//! End-to-end tests for `--var NAME=VALUE` and the `.volamos` `VAR` key
//! (issue #428): the initial process starts with the given local
//! variables. Driven through `fixtures/vartest`'s child mode, which
//! prints `VARTEST` (or `unset`) -- see `fixtures/gen_vartest.py`.
//!
//! Each test runs in its own temp directory, used as `$HOME`, the
//! working directory and the program's directory alike, so no
//! developer's own `.volamos` can leak in.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

const VARTEST_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/vartest");

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("volamos-var-e2e-{tag}-{pid}-{n}"));
        fs::create_dir_all(&path).expect("create temp dir");
        fs::copy(VARTEST_PATH, path.join("vartest")).expect("copy vartest fixture");
        TempDir { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Runs `vartest child` with `flags` before the program, in `dir`.
fn run_child(dir: &TempDir, flags: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_volamos"))
        .env("HOME", dir.path())
        .current_dir(dir.path())
        .args(flags)
        .arg("vartest")
        .arg("child")
        .output()
        .expect("failed to run volamos")
}

fn stdout_of(output: &Output) -> String {
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).expect("stdout should be valid UTF-8")
}

#[test]
fn without_var_the_variable_is_unset() {
    let dir = TempDir::new("unset");
    assert_eq!(stdout_of(&run_child(&dir, &[])), "unset\n");
}

#[test]
fn var_flag_sets_a_local_variable_matched_case_insensitively() {
    let dir = TempDir::new("flag");
    let output = run_child(&dir, &["--var", "vartest=a=b c"]);
    assert_eq!(stdout_of(&output), "a=b c\n");
}

#[test]
fn repeated_var_flag_last_one_wins() {
    let dir = TempDir::new("repeat");
    let output = run_child(&dir, &["--var", "VARTEST=first", "--var", "VARTEST=second"]);
    assert_eq!(stdout_of(&output), "second\n");
}

#[test]
fn var_key_in_dot_volamos_sets_it_and_the_flag_overrides_it() {
    let dir = TempDir::new("config");
    fs::write(dir.path().join(".volamos"), "VAR=VARTEST=from config\n").unwrap();
    assert_eq!(stdout_of(&run_child(&dir, &[])), "from config\n");
    assert_eq!(
        stdout_of(&run_child(&dir, &["--var", "VARTEST=from flag"])),
        "from flag\n"
    );
}

#[test]
fn malformed_var_flag_is_a_usage_error() {
    let dir = TempDir::new("malformed");
    for bad in ["VARTEST", "=value"] {
        let output = run_child(&dir, &["--var", bad]);
        assert!(!output.status.success(), "--var {bad:?} should be rejected");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("--var expects NAME=VALUE"),
            "--var {bad:?}: {stderr}"
        );
    }
}
