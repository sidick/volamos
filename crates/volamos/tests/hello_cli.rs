//! End-to-end test of the actual `volamos` binary: runs
//! `fixtures/hello` as a real subprocess and checks its captured stdout
//! and process exit status, rather than calling any library code
//! directly.

use std::process::Command;

/// Path to `fixtures/hello` relative to this crate's manifest directory
/// (`crates/volamos`), resolved at compile time so the test works
/// regardless of the working directory `cargo test` is invoked from.
const HELLO_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/hello");

#[test]
fn running_hello_prints_greeting_and_exits_zero() {
    let output = Command::new(env!("CARGO_BIN_EXE_volamos"))
        .arg(HELLO_PATH)
        .output()
        .expect("failed to run the volamos binary");

    assert!(
        output.status.success(),
        "volamos exited with {:?}; stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "Hello from volamos\n"
    );
}

#[test]
fn verbose_flag_logs_the_putstr_call_to_stderr() {
    let output = Command::new(env!("CARGO_BIN_EXE_volamos"))
        .arg("--verbose")
        .arg(HELLO_PATH)
        .output()
        .expect("failed to run the volamos binary");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "Hello from volamos\n"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("PutStr"),
        "expected --verbose output to mention PutStr, got: {stderr}"
    );
}

#[test]
fn missing_file_reports_a_clean_error_and_nonzero_exit() {
    let output = Command::new(env!("CARGO_BIN_EXE_volamos"))
        .arg("/nonexistent/path/to/nothing")
        .output()
        .expect("failed to run the volamos binary");

    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).is_empty());
}

#[test]
fn trailing_guest_args_are_accepted_and_ignored() {
    let output = Command::new(env!("CARGO_BIN_EXE_volamos"))
        .arg(HELLO_PATH)
        .arg("some")
        .arg("extra")
        .arg("args")
        .output()
        .expect("failed to run the volamos binary");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "Hello from volamos\n"
    );
}

/// A unique temp directory, cleaned up on drop -- used here as a fake
/// `$HOME` so these tests can observe (or assert the absence of)
/// `~/.volamos.d/volumes` without ever touching the real developer
/// machine's actual home directory.
struct FakeHome {
    path: std::path::PathBuf,
}

impl FakeHome {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("volamos-fakehome-{tag}-{pid}-{n}"));
        std::fs::create_dir_all(&path).expect("create fake $HOME");
        FakeHome { path }
    }
}

impl Drop for FakeHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn running_hello_with_zero_flags_touches_nothing_under_home() {
    // Issue #43's core "lazy, not eager" property, at the full binary
    // level: hello never opens/locks anything, so even though the
    // built-in SYS:/RAM: defaults are active by default (no flags
    // given at all), nothing should be created on the host.
    let home = FakeHome::new("zero-touch");
    let output = Command::new(env!("CARGO_BIN_EXE_volamos"))
        .env("HOME", &home.path)
        .arg(HELLO_PATH)
        .output()
        .expect("failed to run the volamos binary");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "Hello from volamos\n"
    );
    assert!(
        !home.path.join(".volamos.d").exists(),
        "a program that never touches the filesystem must leave no trace on the host"
    );
}

#[test]
fn no_defaults_flag_disables_the_standard_volumes_layer() {
    // --no-defaults doesn't change hello's own behavior (it doesn't
    // need a Vfs either way), but confirms the flag parses and the run
    // still succeeds -- the actual "SYS: stops resolving" behavior is
    // covered directly (Lock/Examine through real trap dispatch) by
    // volamos-core's own doslock.rs end-to-end tests.
    let home = FakeHome::new("no-defaults");
    let output = Command::new(env!("CARGO_BIN_EXE_volamos"))
        .env("HOME", &home.path)
        .arg("--no-defaults")
        .arg(HELLO_PATH)
        .output()
        .expect("failed to run the volamos binary");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "Hello from volamos\n"
    );
    assert!(!home.path.join(".volamos.d").exists());
}
