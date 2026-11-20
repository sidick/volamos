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
//!   `Ctrl+E` and `Ctrl+F` also arrive as bytes: the real console turns
//!   them into `SIGBREAKF_CTRL_D`/`E`/`F`, which this does not.
//! - Output processing (`OPOST`) is left alone, so a guest's `LF` still
//!   moves to the start of the next line.
//!
//! `SetMode(fh, 0)` restores the settings saved at the first switch.
//! (`SetMode(fh, 2)`, the V47 medium mode, is treated as cooked.)
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

use std::time::Duration;

#[cfg(unix)]
pub use unix::restore_stdin_mode;
#[cfg(unix)]
pub(crate) use unix::{read_stdin, set_stdin_raw, stdin_is_tty, stdin_ready, stdio_is_console};

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
    pub(crate) fn stdin_ready(_timeout: Duration) -> bool {
        false
    }
    pub(crate) fn read_stdin(_buf: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
    }
}
#[cfg(not(unix))]
pub use fallback::restore_stdin_mode;
#[cfg(not(unix))]
pub(crate) use fallback::{read_stdin, set_stdin_raw, stdin_is_tty, stdin_ready, stdio_is_console};

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
    /// - `SIGTTOU` ignored meanwhile, as shells do: from a background
    ///   process group the call would stop the whole process, the `atexit`
    ///   restore included.
    /// - Retried after a signal.
    fn apply(fd: i32, settings: &libc::termios) -> Result<(), i32> {
        // SAFETY: `signal` with `SIG_IGN` and restoring the old handler.
        let previous = unsafe { libc::signal(libc::SIGTTOU, libc::SIG_IGN) };
        let result = loop {
            if unsafe { libc::tcsetattr(fd, libc::TCSANOW, settings) } == 0 {
                break Ok(());
            }
            let errno = last_errno();
            if errno != libc::EINTR {
                break Err(errno);
            }
        };
        if previous != libc::SIG_ERR {
            unsafe { libc::signal(libc::SIGTTOU, previous) };
        }
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

    /// Reads from stdin without `std`'s buffer; see the module docs.
    pub(crate) fn read_stdin(buf: &mut [u8]) -> std::io::Result<usize> {
        read_fd(libc::STDIN_FILENO, buf)
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
        fn a_short_timeout_is_not_rounded_down_to_a_zero_poll_315() {
            assert_eq!(poll_millis(Duration::ZERO), 0);
            assert_eq!(poll_millis(Duration::from_micros(100)), 1);
            assert_eq!(poll_millis(Duration::from_micros(1_000)), 1);
            assert_eq!(poll_millis(Duration::from_micros(1_001)), 2);
            assert_eq!(poll_millis(Duration::from_secs(u64::MAX / 2)), i32::MAX);
        }
    }
}
