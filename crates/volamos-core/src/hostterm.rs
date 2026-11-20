//! The host terminal behind `Input()`: `SetMode` raw/cooked and
//! `WaitForChar` (issue #315).
//!
//! `Input()` and `Output()` are backed directly by host stdin/stdout, not
//! by a console handler, so there is no handler to send
//! `ACTION_SCREEN_MODE` to. When stdin is a TTY this module does what the
//! console handler does for it instead: `SetMode(fh, 1)` puts the host
//! terminal in raw mode and `SetMode(fh, 0)` puts it back, and
//! `WaitForChar` asks the terminal whether a key is waiting. When stdin is
//! not a TTY (a pipe, a file, CI, the comparison harnesses) nothing here
//! does anything and every caller behaves as it did before.
//!
//! # What "raw" means
//!
//! The console documentation (the AmigaDOS Manual's console-handler
//! chapter, and the `SetMode` autodoc: 1 = `RAW:`, 0 = `CON:`) says that
//! in raw mode "the console reports every keystroke individually to its
//! clients, without interpreting them", does not echo, and satisfies a
//! pending read for every single keystroke. The one exception is
//! `Ctrl+C` to `Ctrl+F`, "always interpreted" as break signals, and
//! `Ctrl+\` does not make an EOF condition but arrives as the byte `0x1c`.
//! The host equivalent, applied to a copy of the settings saved at the
//! first switch:
//!
//! - `ICANON` and `ECHO` (with its variants) off, `VMIN` 1 and `VTIME` 0:
//!   a read returns as soon as one key has arrived, and nothing is echoed.
//! - `ICRNL`/`INLCR`/`IGNCR` off: Return arrives as `CR` (`0x0d`), as the
//!   Amiga keyboard sends it, not as the host's `LF`.
//! - `IXON`/`IXOFF` off, `IEXTEN` off: `Ctrl+S`, `Ctrl+Q`, `Ctrl+V`, ...
//!   arrive as bytes.
//! - `ISIG` **stays on**, so `Ctrl+C` is still the host `SIGINT` that
//!   [`crate::exectask`] turns into `SIGBREAKF_CTRL_C`, and a runaway
//!   guest can still be interrupted. As ever, the break reaches the guest
//!   at its next library call: one blocked in `Read` sees it when the
//!   read returns, and only a second `Ctrl+C`, which forces the exit,
//!   stops it before that. `VQUIT` and `VSUSP` (and `VDSUSP`
//!   where it exists) are disabled instead, so `Ctrl+\` and `Ctrl+Z`
//!   arrive as bytes, as they do from the Amiga console. `Ctrl+D`,
//!   `Ctrl+E` and `Ctrl+F` do not arrive at all: as on the real console
//!   they are taken out of the input and raised as `SIGBREAKF_CTRL_D`,
//!   `SIGBREAKF_CTRL_E` and `SIGBREAKF_CTRL_F` ([`strip_console_breaks`]).
//!   A guest that wants `Ctrl+D` as a key (an end-of-input convention) has
//!   to watch for the signal instead, as it would on an Amiga.
//! - Output processing (`OPOST`) is left alone, so a guest's `LF` still
//!   moves to the start of the next line.
//!
//! `SetMode(fh, 0)` restores the settings saved at the first switch.
//! (`SetMode(fh, 2)`, the V47 medium mode, is treated as cooked.) A guest
//! that talks to the console by hand gets the same through
//! `ACTION_SCREEN_MODE` and `ACTION_WAIT_CHAR` packets
//! (`crate::dospkt`): every handle shares one packet port, so a packet
//! cannot name its console and acts on the host's.
//!
//! # Leaving the terminal as it was
//!
//! A failed run must never leave the user's terminal raw. The original
//! settings are put back on every way out of the process:
//!
//! - an ordinary exit, `process::exit`, and a panic that reaches `main`:
//!   an `atexit` hook, registered at the first switch to raw;
//! - a host `SIGTERM` or a second `SIGINT`: the forced-exit thread in
//!   [`crate::exectask`] calls [`restore_stdin_mode`] before it `_exit`s.
//!
//! The one path that does not is a forced exit when that thread could not
//! be started (the signal handler then calls `_exit` directly); there the
//! terminal stays raw and `stty sane` is the way out.
//!
//! # Reading
//!
//! When stdin is a TTY its reads bypass `std`'s stdin buffer
//! ([`read_stdin`]): `WaitForChar` asks the kernel whether input is
//! waiting, which would not see bytes `std` had already pulled into its
//! own buffer. Non-TTY stdin keeps `std`'s buffered reads, which matter
//! for a piped guest reading it a byte at a time.
//!
//! # Other platforms
//!
//! There is no termios outside Unix. Elsewhere stdin is never a TTY as
//! far as this module is concerned, so `SetMode` and `WaitForChar` keep
//! their old answers.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

/// Whether `SetMode` has the console in raw mode right now.
static RAW: AtomicBool = AtomicBool::new(false);

/// Break signals (`SIGBREAKF_CTRL_D`/`E`/`F`) raised by keys typed in raw
/// mode and not yet delivered: [`crate::exectask::fold_pending_host_break`]
/// takes them, as it does a host `SIGINT`'s `SIGBREAKF_CTRL_C`.
static PENDING_CONSOLE_BREAKS: AtomicU32 = AtomicU32::new(0);

/// Whether the console is in raw mode (`SetMode(fh, 1)`).
pub(crate) fn stdin_is_raw() -> bool {
    RAW.load(Ordering::SeqCst)
}

/// Takes and clears the pending console break signals.
pub(crate) fn take_console_breaks() -> u32 {
    PENDING_CONSOLE_BREAKS.swap(0, Ordering::SeqCst)
}

/// Removes `Ctrl+D`, `Ctrl+E` and `Ctrl+F` from raw-mode input and raises
/// the break signals they stand for instead, as the console does ("the
/// keystrokes `Ctrl+C` to `Ctrl+F` are always interpreted and send break
/// signals instead of their ASCII codes" -- `Ctrl+C` is the host `SIGINT`
/// here). Compacts `buf` and returns how many bytes are left.
pub(crate) fn strip_console_breaks(buf: &mut [u8]) -> usize {
    let (kept, raised) = strip_breaks(buf);
    if raised != 0 {
        PENDING_CONSOLE_BREAKS.fetch_or(raised, Ordering::SeqCst);
    }
    kept
}

/// [`strip_console_breaks`] without the side effect: the bytes kept and
/// the break signals the removed ones stand for.
fn strip_breaks(buf: &mut [u8]) -> (usize, u32) {
    use crate::exectask::{SIGBREAKF_CTRL_D, SIGBREAKF_CTRL_E, SIGBREAKF_CTRL_F};
    let mut raised = 0;
    let mut kept = 0;
    for i in 0..buf.len() {
        let byte = buf[i];
        match byte {
            0x04 => raised |= SIGBREAKF_CTRL_D,
            0x05 => raised |= SIGBREAKF_CTRL_E,
            0x06 => raised |= SIGBREAKF_CTRL_F,
            _ => {
                buf[kept] = byte;
                kept += 1;
            }
        }
    }
    (kept, raised)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exectask::{SIGBREAKF_CTRL_D, SIGBREAKF_CTRL_E, SIGBREAKF_CTRL_F};

    #[test]
    fn ctrl_d_e_f_become_break_signals_and_leave_the_input_315() {
        let mut buf = *b"a\x04b\x05\x06c";
        let (kept, raised) = strip_breaks(&mut buf);
        assert_eq!(&buf[..kept], b"abc");
        assert_eq!(
            raised,
            SIGBREAKF_CTRL_D | SIGBREAKF_CTRL_E | SIGBREAKF_CTRL_F
        );
    }

    #[test]
    fn input_without_those_keys_is_untouched_315() {
        // Ctrl-C (0x03) never arrives as a byte (ISIG); Ctrl-\ and Ctrl-Z
        // (0x1c, 0x1a) are ordinary raw input, and so is CR.
        let mut buf = *b"\x03\x1c\x1a\rx";
        let (kept, raised) = strip_breaks(&mut buf);
        assert_eq!((kept, raised), (5, 0));
        assert_eq!(&buf, b"\x03\x1c\x1a\rx");
    }

    #[test]
    fn a_read_of_only_break_keys_keeps_nothing_315() {
        let mut buf = [0x04, 0x04];
        assert_eq!(strip_breaks(&mut buf), (0, SIGBREAKF_CTRL_D));
    }
}

#[cfg(unix)]
pub use unix::restore_stdin_mode;
#[cfg(unix)]
pub(crate) use unix::{
    pump_console_breaks, pump_console_breaks_now, read_stdin, set_stdin_raw, stdin_has_key,
    stdin_is_tty, stdio_is_console, wait_for_console_input_until,
};

#[cfg(not(unix))]
mod fallback {
    use super::Duration;

    /// Nothing to restore: no terminal mode is ever changed here.
    pub fn restore_stdin_mode() {}
    pub(crate) fn stdin_is_tty() -> bool {
        false
    }
    pub(crate) fn stdio_is_console(_stdout: bool) -> bool {
        false
    }
    pub(crate) fn set_stdin_raw(_raw: bool) -> Result<(), i32> {
        Ok(())
    }
    pub(crate) fn stdin_has_key(_timeout: Duration) -> bool {
        false
    }
    pub(crate) fn pump_console_breaks() {}
    pub(crate) fn pump_console_breaks_now() {}
    pub(crate) fn wait_for_console_input_until(_deadline: Option<std::time::Instant>) -> bool {
        false
    }
    pub(crate) fn read_stdin(_buf: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
    }
}
#[cfg(not(unix))]
pub use fallback::restore_stdin_mode;
#[cfg(not(unix))]
pub(crate) use fallback::{
    pump_console_breaks, pump_console_breaks_now, read_stdin, set_stdin_raw, stdin_has_key,
    stdin_is_tty, stdio_is_console, wait_for_console_input_until,
};

#[cfg(unix)]
mod unix {
    use super::Duration;
    use std::sync::Mutex;

    /// A terminal's saved settings, and whether it is currently raw.
    pub(super) struct Term {
        fd: i32,
        saved: Option<libc::termios>,
    }

    impl Term {
        pub(super) const fn new(fd: i32) -> Self {
            Self { fd, saved: None }
        }

        /// Saves the current settings (the first time only) and applies
        /// raw or the saved settings. `Err` is an `errno`.
        pub(super) fn set_raw(&mut self, raw: bool) -> Result<(), i32> {
            let saved = match self.saved {
                Some(saved) => saved,
                None => {
                    // SAFETY: an all-zero termios is a valid out-parameter.
                    let mut current: libc::termios = unsafe { std::mem::zeroed() };
                    if unsafe { libc::tcgetattr(self.fd, &mut current) } != 0 {
                        return Err(last_errno());
                    }
                    self.saved = Some(current);
                    current
                }
            };
            let wanted = if raw { raw_settings(&saved) } else { saved };
            apply(self.fd, &wanted)
        }

        /// Puts back the settings saved by the first [`Self::set_raw`], if
        /// there were any.
        pub(super) fn restore(&self) {
            if let Some(saved) = &self.saved {
                let _ = apply(self.fd, saved);
            }
        }
    }

    /// `tcsetattr(fd, TCSANOW, settings)`, which is what a switch needs to
    /// be to run under [`STDIN_TERM`]'s lock and in the forced-exit thread:
    ///
    /// - `TCSANOW`, not `TCSADRAIN`: draining waits for queued output, and
    ///   a terminal whose output is stalled (`Ctrl+S`, a hung ssh session)
    ///   would hold the lock -- and so a forced exit -- up behind it. The
    ///   flags raw mode changes do not touch output.
    /// - `SIGTTOU` blocked meanwhile, as shells do: from a background
    ///   process group the call would stop the whole process, the `atexit`
    ///   restore included.
    /// - Retried after a signal.
    fn apply(fd: i32, settings: &libc::termios) -> Result<(), i32> {
        // `SIGTTOU` blocked for this thread only (`pthread_sigmask`, not
        // `signal`: that would change the disposition for every thread, and
        // race with the others doing the same). A blocked `SIGTTOU` lets
        // the call through from a background process group.
        let mut block: libc::sigset_t = unsafe { std::mem::zeroed() };
        let mut previous: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::sigemptyset(&mut block);
            libc::sigaddset(&mut block, libc::SIGTTOU);
            libc::pthread_sigmask(libc::SIG_BLOCK, &block, &mut previous);
        }
        let result = loop {
            if unsafe { libc::tcsetattr(fd, libc::TCSANOW, settings) } == 0 {
                break Ok(());
            }
            let errno = last_errno();
            if errno != libc::EINTR {
                break Err(errno);
            }
        };
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut()) };
        result
    }

    fn last_errno() -> i32 {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    }

    /// `saved` with the raw-mode changes the module docs list.
    pub(super) fn raw_settings(saved: &libc::termios) -> libc::termios {
        let mut t = *saved;
        t.c_lflag &=
            !(libc::ICANON | libc::ECHO | libc::ECHOE | libc::ECHOK | libc::ECHONL | libc::IEXTEN);
        t.c_iflag &= !(libc::ICRNL | libc::INLCR | libc::IGNCR | libc::IXON | libc::IXOFF);
        t.c_cc[libc::VMIN] = 1;
        t.c_cc[libc::VTIME] = 0;
        t.c_cc[libc::VQUIT] = libc::_POSIX_VDISABLE;
        t.c_cc[libc::VSUSP] = libc::_POSIX_VDISABLE;
        #[cfg(target_vendor = "apple")]
        {
            t.c_cc[libc::VDSUSP] = libc::_POSIX_VDISABLE;
        }
        t
    }

    /// Stdin's terminal. Locked from the guest's thread and, at exit,
    /// from whichever thread exits.
    static STDIN_TERM: Mutex<Term> = Mutex::new(Term::new(libc::STDIN_FILENO));

    /// Whether stdin is a terminal. Constant for the life of the process.
    pub(crate) fn stdin_is_tty() -> bool {
        unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
    }

    /// Whether the console handles can be switched: stdin is a terminal
    /// and, for the `Output()` handle (which doubles as the console `*`),
    /// so is stdout -- a program whose output is redirected to a file has
    /// no console on that handle.
    pub(crate) fn stdio_is_console(stdout: bool) -> bool {
        stdin_is_tty() && (!stdout || unsafe { libc::isatty(libc::STDOUT_FILENO) == 1 })
    }

    /// `SetMode(fh, raw)` on a console handle: raw (`true`) or cooked.
    /// `Ok` and nothing done when stdin is not a terminal. `Err` is an
    /// `errno` from the terminal driver.
    pub(crate) fn set_stdin_raw(raw: bool) -> Result<(), i32> {
        if !stdin_is_tty() {
            return Ok(());
        }
        let mut term = STDIN_TERM.lock().unwrap_or_else(|e| e.into_inner());
        let first = term.saved.is_none();
        term.set_raw(raw)?;
        super::RAW.store(raw, std::sync::atomic::Ordering::SeqCst);
        if first {
            register_exit_hook();
        }
        Ok(())
    }

    /// Puts stdin's terminal back as it was before the first `SetMode`.
    /// Safe to call any number of times, and when nothing was changed.
    /// Called at exit and by the forced-exit thread; see the module docs.
    pub fn restore_stdin_mode() {
        STDIN_TERM
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .restore();
        super::RAW.store(false, std::sync::atomic::Ordering::SeqCst);
    }

    extern "C" fn restore_at_exit() {
        restore_stdin_mode();
    }

    fn register_exit_hook() {
        static REGISTERED: std::sync::Once = std::sync::Once::new();
        REGISTERED.call_once(|| {
            unsafe { libc::atexit(restore_at_exit) };
        });
    }

    /// `timeout` in whole milliseconds for `poll(2)`, rounded up: a 100 us
    /// wait must not become a 0 ms poll that never waits.
    pub(super) fn poll_millis(timeout: Duration) -> i32 {
        timeout.as_micros().div_ceil(1_000).min(i32::MAX as u128) as i32
    }

    /// Whether `fd` has input to read within `timeout`.
    pub(super) fn fd_ready(fd: i32, timeout: Duration) -> bool {
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = poll_millis(timeout);
        loop {
            let n = unsafe { libc::poll(&mut pollfd, 1, millis) };
            if n >= 0 {
                // POLLHUP/POLLERR also mean a read would not block.
                return n > 0;
            }
            if last_errno() != libc::EINTR {
                return false;
            }
        }
    }

    /// Whether a key is waiting on stdin, or arrives within `timeout`.
    pub(crate) fn stdin_ready(timeout: Duration) -> bool {
        fd_ready(libc::STDIN_FILENO, timeout)
    }

    /// One `read(2)` of `fd`, retried after a signal.
    pub(super) fn read_fd(fd: i32, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n >= 0 {
                return Ok(n as usize);
            }
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }

    /// Keys read ahead of the guest while it was not reading, with
    /// `Ctrl+D`/`E`/`F` already taken out: see [`pump_console_breaks`].
    static LOOKAHEAD: Mutex<std::collections::VecDeque<u8>> =
        Mutex::new(std::collections::VecDeque::new());

    fn lookahead() -> std::sync::MutexGuard<'static, std::collections::VecDeque<u8>> {
        LOOKAHEAD.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Reads from stdin without `std`'s buffer; see the module docs. In raw
    /// mode `Ctrl+D`/`E`/`F` are not input but break signals
    /// ([`super::strip_console_breaks`]), so a read that finds only those
    /// goes on waiting for a key; keys [`pump_console_breaks`] already read
    /// ahead come first.
    pub(crate) fn read_stdin(buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            {
                let mut ahead = lookahead();
                if !ahead.is_empty() && !buf.is_empty() {
                    let n = buf.len().min(ahead.len());
                    for slot in &mut buf[..n] {
                        *slot = ahead.pop_front().unwrap_or(0);
                    }
                    return Ok(n);
                }
            }
            let n = read_fd(libc::STDIN_FILENO, buf)?;
            if n == 0 || !super::stdin_is_raw() {
                return Ok(n);
            }
            let kept = super::strip_console_breaks(&mut buf[..n]);
            if kept > 0 {
                return Ok(kept);
            }
        }
    }

    /// In raw mode, reads whatever is waiting on `fd` into `ahead` with
    /// `Ctrl+D`/`E`/`F` taken out, and returns the break signals they
    /// stand for. Never blocks.
    pub(super) fn pump_from(fd: i32, ahead: &mut std::collections::VecDeque<u8>) -> u32 {
        if !fd_ready(fd, Duration::ZERO) {
            return 0;
        }
        let mut buf = [0u8; 256];
        let Ok(n) = read_fd(fd, &mut buf) else {
            return 0;
        };
        let (kept, raised) = super::strip_breaks(&mut buf[..n]);
        ahead.extend(&buf[..kept]);
        raised
    }

    /// Notices `Ctrl+D`/`E`/`F` typed while the guest is not reading, so a
    /// guest that `Wait`s for the signal gets it (the real console raises
    /// it whatever the guest is doing). Called from
    /// [`crate::exectask::fold_pending_host_break`], i.e. at every library
    /// call; does nothing unless the console is raw, and at most every
    /// [`PUMP_INTERVAL`] (a `poll(2)` per call would cost a busy guest
    /// dearly). The other keys read are kept, in order, for the guest's
    /// next read.
    pub(crate) fn pump_console_breaks() {
        if !super::stdin_is_raw() {
            return;
        }
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        static LAST_NANOS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let now = START
            .get_or_init(std::time::Instant::now)
            .elapsed()
            .as_nanos() as u64;
        let last = LAST_NANOS.load(std::sync::atomic::Ordering::Relaxed);
        if last != 0 && now.saturating_sub(last) < PUMP_INTERVAL.as_nanos() as u64 {
            return;
        }
        LAST_NANOS.store(now.max(1), std::sync::atomic::Ordering::Relaxed);
        pump_console_breaks_now();
    }

    /// How often [`pump_console_breaks`] looks at the terminal.
    const PUMP_INTERVAL: Duration = Duration::from_millis(2);

    /// [`pump_console_breaks`] without the rate limit, for a caller that has
    /// just been told input is waiting (a limited pump could skip it and
    /// spin).
    pub(crate) fn pump_console_breaks_now() {
        if !super::stdin_is_raw() {
            return;
        }
        let raised = pump_from(libc::STDIN_FILENO, &mut lookahead());
        if raised != 0 {
            super::PENDING_CONSOLE_BREAKS.fetch_or(raised, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Blocks until stdin has input, `deadline` passes (`None`: never), or
    /// a signal arrives; the caller then looks again. Polls in short slices
    /// so a host `SIGINT` that landed just before the `poll` is noticed
    /// within one rather than waiting for the next key. `false` if the
    /// terminal has hung up -- it would never block again, so a caller
    /// must not wait on it in a loop. For a `Wait` on a console break key
    /// ([`crate::exectask`]).
    pub(crate) fn wait_for_console_input_until(deadline: Option<std::time::Instant>) -> bool {
        const SLICE: Duration = Duration::from_millis(50);
        let mut pollfd = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let mut slice = SLICE;
        if let Some(deadline) = deadline {
            slice = slice.min(deadline.saturating_duration_since(std::time::Instant::now()));
        }
        pollfd.revents = 0;
        let n = unsafe { libc::poll(&mut pollfd, 1, poll_millis(slice)) };
        if n < 0 {
            return last_errno() == libc::EINTR;
        }
        pollfd.revents & libc::POLLIN != 0
            || pollfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) == 0
    }

    /// Whether a key can be read from stdin now or arrives within
    /// `timeout`. In raw mode a `Ctrl+D`/`E`/`F` is a break signal, not a
    /// key, so those do not count (and are raised as they are noticed).
    pub(crate) fn stdin_has_key(timeout: Duration) -> bool {
        if !super::stdin_is_raw() {
            return stdin_ready(timeout);
        }
        let deadline = std::time::Instant::now() + timeout;
        loop {
            pump_console_breaks_now();
            if !lookahead().is_empty() {
                return true;
            }
            let now = std::time::Instant::now();
            if now >= deadline || !stdin_ready(deadline - now) {
                return false;
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// A pseudo-terminal: `(master, slave)`.
        fn pty() -> (i32, i32) {
            unsafe {
                let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
                assert!(master >= 0, "posix_openpt");
                assert_eq!(libc::grantpt(master), 0);
                assert_eq!(libc::unlockpt(master), 0);
                let name = libc::ptsname(master);
                assert!(!name.is_null());
                let slave = libc::open(name, libc::O_RDWR | libc::O_NOCTTY);
                assert!(slave >= 0, "open slave");
                (master, slave)
            }
        }

        fn attrs(fd: i32) -> libc::termios {
            let mut t: libc::termios = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { libc::tcgetattr(fd, &mut t) }, 0);
            t
        }

        /// The `c_lflag` bits raw mode changes. Not the whole word: the
        /// kernel keeps bits of its own there (BSD's `NOKERNINFO`) that
        /// read back differently after any `tcsetattr`.
        const LFLAG_BITS: libc::tcflag_t = libc::ICANON
            | libc::ECHO
            | libc::ECHOE
            | libc::ECHOK
            | libc::ECHONL
            | libc::ISIG
            | libc::IEXTEN;

        fn close(fds: &[i32]) {
            for &fd in fds {
                unsafe { libc::close(fd) };
            }
        }

        #[test]
        fn raw_mode_applies_the_documented_settings_and_cooked_restores_them_315() {
            let (master, slave) = pty();
            let original = attrs(slave);
            assert_ne!(original.c_lflag & libc::ICANON, 0, "a pty starts cooked");

            let mut term = Term::new(slave);
            term.set_raw(true).expect("raw");
            let raw = attrs(slave);
            assert_eq!(raw.c_lflag & (libc::ICANON | libc::ECHO | libc::IEXTEN), 0);
            assert_eq!(raw.c_iflag & (libc::ICRNL | libc::IXON), 0);
            assert_ne!(raw.c_lflag & libc::ISIG, 0, "Ctrl-C must still break");
            assert_eq!((raw.c_cc[libc::VMIN], raw.c_cc[libc::VTIME]), (1, 0));
            assert_eq!(raw.c_cc[libc::VQUIT], libc::_POSIX_VDISABLE);
            assert_eq!(raw.c_cc[libc::VSUSP], libc::_POSIX_VDISABLE);
            assert_ne!(raw.c_oflag & libc::OPOST, 0, "output is left alone");

            term.set_raw(false).expect("cooked");
            let back = attrs(slave);
            assert_eq!(back.c_lflag & LFLAG_BITS, original.c_lflag & LFLAG_BITS);
            assert_eq!(back.c_iflag, original.c_iflag);
            assert_eq!(back.c_cc[libc::VQUIT], original.c_cc[libc::VQUIT]);
            close(&[master, slave]);
        }

        #[test]
        fn restore_puts_back_what_the_first_switch_saved_even_after_raw_315() {
            // The exit hook's job: whatever state the guest left it in.
            let (master, slave) = pty();
            let original = attrs(slave);
            let mut term = Term::new(slave);
            term.set_raw(true).expect("raw");
            term.restore();
            assert_eq!(
                attrs(slave).c_lflag & LFLAG_BITS,
                original.c_lflag & LFLAG_BITS
            );
            assert_eq!(attrs(slave).c_iflag, original.c_iflag);
            // And with nothing saved it changes nothing.
            Term::new(slave).restore();
            assert_eq!(
                attrs(slave).c_lflag & LFLAG_BITS,
                original.c_lflag & LFLAG_BITS
            );
            close(&[master, slave]);
        }

        #[test]
        fn raw_mode_delivers_a_key_without_a_newline_and_ready_sees_it_315() {
            let (master, slave) = pty();
            let mut term = Term::new(slave);
            term.set_raw(true).expect("raw");

            assert!(
                !fd_ready(slave, Duration::from_millis(20)),
                "nothing typed yet"
            );
            assert_eq!(unsafe { libc::write(master, b"a".as_ptr().cast(), 1) }, 1);
            assert!(
                fd_ready(slave, Duration::from_secs(2)),
                "a single key, no Return, is available in raw mode"
            );
            let mut buf = [0u8; 16];
            assert_eq!(read_fd(slave, &mut buf).expect("read"), 1);
            assert_eq!(buf[0], b'a');

            // Return arrives as CR, as the Amiga keyboard sends it.
            assert_eq!(unsafe { libc::write(master, b"\r".as_ptr().cast(), 1) }, 1);
            assert_eq!(read_fd(slave, &mut buf).expect("read"), 1);
            assert_eq!(buf[0], b'\r');
            close(&[master, slave]);
        }

        #[test]
        fn cooked_mode_holds_a_key_until_return_315() {
            // The contrast that makes raw mode matter.
            let (master, slave) = pty();
            assert_eq!(unsafe { libc::write(master, b"a".as_ptr().cast(), 1) }, 1);
            assert!(
                !fd_ready(slave, Duration::from_millis(100)),
                "cooked: a partial line is not readable"
            );
            assert_eq!(unsafe { libc::write(master, b"\n".as_ptr().cast(), 1) }, 1);
            assert!(fd_ready(slave, Duration::from_secs(2)));
            close(&[master, slave]);
        }

        #[test]
        fn pump_takes_break_keys_out_and_keeps_the_rest_in_order_315() {
            let (master, slave) = pty();
            let mut term = Term::new(slave);
            term.set_raw(true).expect("raw");
            let mut ahead = std::collections::VecDeque::new();

            assert_eq!(pump_from(slave, &mut ahead), 0, "nothing typed: no wait");
            assert_eq!(
                unsafe { libc::write(master, b"a\x04b\x06".as_ptr().cast(), 4) },
                4
            );
            let raised = pump_from(slave, &mut ahead);
            assert_eq!(
                raised,
                crate::exectask::SIGBREAKF_CTRL_D | crate::exectask::SIGBREAKF_CTRL_F
            );
            assert_eq!(ahead.iter().copied().collect::<Vec<_>>(), b"ab");
            close(&[master, slave]);
        }

        #[test]
        fn a_short_timeout_is_not_rounded_down_to_a_zero_poll_315() {
            assert_eq!(poll_millis(Duration::ZERO), 0);
            assert_eq!(poll_millis(Duration::from_micros(100)), 1);
            assert_eq!(poll_millis(Duration::from_micros(1_000)), 1);
            assert_eq!(poll_millis(Duration::from_micros(1_001)), 2);
            assert_eq!(poll_millis(Duration::from_secs(u64::MAX / 2)), i32::MAX);
        }
    }
}
