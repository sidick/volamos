//! `SetMode(Input(), 1)` on a real terminal (issue #315), against the
//! actual `volamos` binary on a pseudo-terminal, driven with
//! `fixtures/rawtest` (`fixtures/gen_rawtest.py`): a guest that switches
//! its input to raw, prints `READY`, reads one key and exits with it
//! *without* switching back.
//!
//! What this pins that the unit tests in `volamos_core::hostterm` cannot:
//! that the guest's `SetMode` call really reaches the host terminal, that
//! a key then needs no Return, and that the terminal is back to what it
//! was however the process ends -- here an ordinary exit with the guest
//! still raw, and a host `SIGTERM` while it is.
//!
//! Unix-only: termios and pseudo-terminals are POSIX. A `libc`
//! dev-dependency opens the pty; signals go through the `kill` utility,
//! as `signal_e2e.rs` does.

#![cfg(unix)]

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const RAWTEST_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/rawtest");
const RAWWAIT_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/rawwait");
const RAWBREAK_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/rawbreak");

/// Long enough for a slow CI runner to start the binary, short enough
/// that a hang fails the test instead of the job.
const LIMIT: Duration = Duration::from_secs(30);

struct Pty {
    master: OwnedFd,
    slave: OwnedFd,
}

fn open_pty() -> Pty {
    unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(master >= 0, "posix_openpt");
        assert_eq!(libc::grantpt(master), 0);
        assert_eq!(libc::unlockpt(master), 0);
        let name = libc::ptsname(master);
        assert!(!name.is_null());
        let slave = libc::open(name, libc::O_RDWR | libc::O_NOCTTY);
        assert!(slave >= 0, "open the pty slave");
        Pty {
            master: OwnedFd::from_raw_fd(master),
            slave: OwnedFd::from_raw_fd(slave),
        }
    }
}

impl Pty {
    /// The terminal's `c_lflag`, read through the master (it reports the
    /// slave's settings).
    fn lflag(&self) -> libc::tcflag_t {
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::tcgetattr(self.master.as_raw_fd(), &mut t) },
            0
        );
        t.c_lflag
    }

    fn is_cooked(&self) -> bool {
        let lflag = self.lflag();
        lflag & libc::ICANON != 0 && lflag & libc::ECHO != 0
    }

    fn is_raw(&self) -> bool {
        self.lflag() & (libc::ICANON | libc::ECHO) == 0
    }

    fn write(&self, bytes: &[u8]) {
        let n = unsafe { libc::write(self.master.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
        assert_eq!(n, bytes.len() as isize);
    }

    /// Reads from the master until `marker` has appeared, or fails.
    fn read_until(&self, marker: &str) -> String {
        let mut seen = Vec::new();
        let deadline = Instant::now() + LIMIT;
        while Instant::now() < deadline {
            let mut pollfd = libc::pollfd {
                fd: self.master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut pollfd, 1, 200) } <= 0 {
                continue;
            }
            let mut buf = [0u8; 256];
            let n =
                unsafe { libc::read(self.master.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break;
            }
            seen.extend_from_slice(&buf[..n as usize]);
            if String::from_utf8_lossy(&seen).contains(marker) {
                return String::from_utf8_lossy(&seen).into_owned();
            }
        }
        panic!(
            "never saw {marker:?}; got {:?}",
            String::from_utf8_lossy(&seen)
        );
    }

    fn spawn_rawtest(&self) -> Child {
        self.spawn(RAWTEST_PATH)
    }

    fn spawn(&self, program: &str) -> Child {
        let stdio = || Stdio::from(self.slave.try_clone().expect("dup the slave"));
        Command::new(env!("CARGO_BIN_EXE_volamos"))
            .arg(program)
            .stdin(stdio())
            .stdout(stdio())
            .stderr(stdio())
            .spawn()
            .expect("failed to spawn the volamos binary")
    }
}

fn wait_for_exit(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + LIMIT;
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("volamos did not exit");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn set_mode_raw_delivers_a_key_without_return_and_the_terminal_is_restored_at_exit() {
    let pty = open_pty();
    assert!(pty.is_cooked(), "a fresh pty starts cooked");
    let mut child = pty.spawn_rawtest();

    pty.read_until("READY");
    assert!(
        pty.is_raw(),
        "SetMode(Input(), 1) must put the host terminal in raw mode"
    );

    // One key, no Return: in cooked mode this would sit in the line
    // buffer and the guest's Read would never return.
    pty.write(b"x");
    let status = wait_for_exit(&mut child);
    assert_eq!(
        status.code(),
        Some(i32::from(b'x')),
        "the guest read the key"
    );
    assert!(
        pty.is_cooked(),
        "the guest exited still raw: volamos must put the terminal back"
    );
}

#[test]
fn a_host_sigterm_while_raw_restores_the_terminal() {
    let pty = open_pty();
    let mut child = pty.spawn_rawtest();
    pty.read_until("READY");
    assert!(pty.is_raw());

    let killed = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .expect("kill");
    assert!(killed.success());
    let status = wait_for_exit(&mut child);
    assert_eq!(status.code(), Some(143), "128 + SIGTERM, as a forced exit");
    assert!(
        pty.is_cooked(),
        "a forced exit must not leave the terminal raw"
    );
}

#[test]
fn ctrl_d_e_f_in_raw_mode_are_break_signals_not_input() {
    // fixtures/rawbreak reads ONE key, then exits with the break signals
    // pending (bit 0 = Ctrl+D, 1 = Ctrl+E, 2 = Ctrl+F). Ctrl+D and Ctrl+F
    // typed first must not be that key -- the read goes on to the `x` --
    // and must arrive as signals.
    let pty = open_pty();
    let mut child = pty.spawn(RAWBREAK_PATH);
    pty.read_until("READY");
    pty.write(b"\x04\x06x");
    let status = wait_for_exit(&mut child);
    assert_eq!(
        status.code(),
        Some(0b101),
        "Ctrl+D and Ctrl+F raised as SIGBREAKF_CTRL_D and _F"
    );
    assert!(pty.is_cooked(), "and the terminal is restored");
}

#[test]
fn a_plain_key_raises_no_break_signal() {
    let pty = open_pty();
    let mut child = pty.spawn(RAWBREAK_PATH);
    pty.read_until("READY");
    pty.write(b"z");
    assert_eq!(wait_for_exit(&mut child).code(), Some(0));
}

#[test]
fn a_break_key_reaches_a_guest_that_waits_for_it_without_reading() {
    // fixtures/rawwait never reads: it Waits for SIGBREAKF_CTRL_D. The
    // console raises the break whatever the guest is doing, so the key
    // must be noticed without a Read to find it.
    let pty = open_pty();
    let mut child = pty.spawn(RAWWAIT_PATH);
    pty.read_until("READY");
    pty.write(b"\x04");
    let status = wait_for_exit(&mut child);
    assert_eq!(status.code(), Some(0b001), "SIGBREAKF_CTRL_D received");
    assert!(pty.is_cooked());
}
