//! Real `SIGTERM`/`SIGINT` delivery to the actual `volamos` binary
//! (issue #151: "SIGTERM and SIGINT do not stop a guest that ignores
//! breaks"), driven against `fixtures/looper` (`fixtures/gen_looper.py`)
//! -- a guest with no startup, no library calls, and no return at all,
//! just an infinite `bra` back to itself. That shape is deliberate: it
//! is the worst case the issue describes, where
//! `exectask::fold_pending_host_break` never runs because the guest
//! never reaches a single dispatched-trap boundary, so a cooperative
//! break can never be delivered no matter how long it runs.
//!
//! Unix-only (`#[cfg(unix)]` for the whole file): sending real signals
//! and reading exit codes by signal-equivalent convention is a POSIX
//! concept, and `volamos_core::exectask`'s host-break handling is
//! itself `#[cfg(unix)]`-gated to begin with.
//!
//! Signals are sent via the `kill` command-line utility rather than a
//! new `libc`/`nix` dev-dependency, matching
//! `volamos_core::exectask::install_host_break_handler`'s own choice to
//! hand-declare the POSIX `signal`/`_exit` FFI surface instead of
//! pulling in a crate for it.
//!
//! [`STARTUP_GRACE`] exists because sending a signal races a freshly
//! spawned process's own startup: `install_host_break_handler` runs at
//! the very top of `main`, but a signal delivered before the kernel
//! finishes `exec`ing the new image falls back to the *default*
//! disposition (immediate termination for both signals, without going
//! through this crate's handler at all), which would make
//! [`sigterm_force_exits_a_guest_that_never_calls_a_library`] pass for
//! the wrong reason and would make
//! [`a_single_sigint_does_not_force_exit_a_guest_that_never_calls_a_library`]
//! fail spuriously. Measured empirically against this binary on the
//! development machine (200ms was not reliably enough, 1000ms was);
//! kept generous for slower CI hardware.

#![cfg(unix)]

use std::process::{Child, Command, Stdio};
use std::time::Duration;

const LOOPER_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/looper");

/// See the module docs.
const STARTUP_GRACE: Duration = Duration::from_millis(1500);

/// Starts `fixtures/looper` and waits out [`STARTUP_GRACE`] before
/// returning, so callers can send a signal immediately.
fn spawn_looper() -> Child {
    let child = Command::new(env!("CARGO_BIN_EXE_volamos"))
        .arg(LOOPER_PATH)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn the volamos binary");
    std::thread::sleep(STARTUP_GRACE);
    child
}

/// Sends `kill -s <signal> <pid>` to `child`.
fn kill(child: &Child, signal: &str) {
    let status = Command::new("kill")
        .arg("-s")
        .arg(signal)
        .arg(child.id().to_string())
        .status()
        .expect("failed to run the `kill` utility");
    assert!(status.success(), "`kill -s {signal}` itself failed");
}

/// Waits up to `timeout` for `child` to exit, polling rather than
/// blocking forever so a regression hangs this test instead of the
/// whole suite. Force-kills and panics if it doesn't exit in time.
fn wait_with_timeout(mut child: Child, timeout: Duration) -> std::process::ExitStatus {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("try_wait failed") {
            return status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("looper did not exit within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `SIGTERM` ends a guest that never makes a library call and so could
/// never observe a cooperative break -- `SIGTERM` has no cooperative
/// path at all, it always terminates immediately (issue #151's primary
/// ask).
#[test]
fn sigterm_force_exits_a_guest_that_never_calls_a_library() {
    let child = spawn_looper();
    kill(&child, "TERM");
    let status = wait_with_timeout(child, Duration::from_secs(5));
    assert_eq!(
        status.code(),
        Some(143),
        "SIGTERM should force-exit with 128+SIGTERM (143); got {status:?}"
    );
}

/// A *second* `SIGINT` forces the same exit, matching the
/// double-Ctrl-C "I mean it" convention most interpreters use -- see
/// issue #151's "perhaps a second SIGINT ... could force the exit".
#[test]
fn second_sigint_force_exits_a_guest_that_never_calls_a_library() {
    let child = spawn_looper();
    kill(&child, "INT");
    kill(&child, "INT");
    let status = wait_with_timeout(child, Duration::from_secs(5));
    assert_eq!(
        status.code(),
        Some(130),
        "a second SIGINT should force-exit with 128+SIGINT (130); got {status:?}"
    );
}

/// A *single* `SIGINT` stays purely cooperative: it only asks for a
/// break at the guest's next dispatched library call, which `looper`
/// never makes, so it must *not* force an exit on its own -- otherwise
/// this fix would have quietly turned every first Ctrl-C into a hard
/// kill, even for a guest that was about to handle it gracefully.
#[test]
fn a_single_sigint_does_not_force_exit_a_guest_that_never_calls_a_library() {
    let mut child = spawn_looper();
    kill(&child, "INT");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        child.try_wait().expect("try_wait failed"),
        None,
        "a single SIGINT must not force-exit a guest that hasn't had a \
         chance to observe it cooperatively"
    );
    child.kill().expect("cleanup kill failed");
    let _ = child.wait();
}
