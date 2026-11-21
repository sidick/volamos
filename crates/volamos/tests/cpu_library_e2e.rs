//! `--cpu-library`: a real library opened before the program runs, as
//! `SetPatch` does at boot. Drives the actual `volamos` binary with the
//! repo's own `testlib` fixtures standing in for a real
//! `68040.library`/`68060.library` (the mechanism is the same: guest code
//! opened by a prologue, then the program starts); the real libraries are
//! checked by hand, see `userdocs/CLI-Reference.md`.

use std::process::Command;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures");

fn volamos() -> Command {
    Command::new(env!("CARGO_BIN_EXE_volamos"))
}

fn fixture(name: &str) -> String {
    format!("{FIXTURES}/{name}")
}

/// `fixtures/libcall` opens the library it is given and calls it. Opened by
/// the prologue first, the library is there for the program's own open by
/// the same path; its last check (the open count is exactly 1) then sees
/// the extra open and says `bad`, which is how this shows the open came
/// before the program ran.
#[test]
fn the_library_is_opened_before_the_program_starts() {
    let with = volamos()
        .args(["--cpu-library", &fixture("testlib"), &fixture("libcall")])
        .arg("CPULIB:testlib")
        .output()
        .expect("failed to run volamos");
    let stdout = String::from_utf8_lossy(&with.stdout);
    assert!(
        stdout.starts_with("user ok\nadd ok\n"),
        "stdout: {stdout}, stderr: {}",
        String::from_utf8_lossy(&with.stderr)
    );
    assert_eq!(
        stdout, "user ok\nadd ok\nbad\n",
        "open count 2: opened before"
    );

    // Without the flag there is no such volume or library.
    let without = volamos()
        .args([&fixture("libcall"), "CPULIB:testlib"])
        .output()
        .expect("failed to run volamos");
    assert_eq!(String::from_utf8_lossy(&without.stdout), "open failed\n");
}

/// A library whose init refuses stops the run with exit 98 and a message
/// that says why, and the program is never started.
#[test]
fn a_library_that_will_not_open_ends_the_run_before_the_program() {
    let output = volamos()
        .args([
            "--cpu-library",
            &fixture("testlib_initfail"),
            &fixture("hello"),
        ])
        .output()
        .expect("failed to run volamos");
    assert_eq!(output.status.code(), Some(98), "volamos's own failure code");
    assert!(
        output.stdout.is_empty(),
        "the program must not run: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--cpu-library") && stderr.contains("OpenLibrary failed"),
        "stderr: {stderr}"
    );
}

#[test]
fn a_missing_library_file_is_one_clear_error() {
    let output = volamos()
        .args([
            "--cpu-library",
            "/nonexistent/68060.library",
            &fixture("hello"),
        ])
        .output()
        .expect("failed to run volamos");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not a file"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn it_cannot_be_combined_with_fpsp_emulate() {
    let output = volamos()
        .args([
            "--fpsp-emulate",
            "--cpu-library",
            &fixture("testlib"),
            &fixture("hello"),
        ])
        .output()
        .expect("failed to run volamos");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("cannot be used with"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
