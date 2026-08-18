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
