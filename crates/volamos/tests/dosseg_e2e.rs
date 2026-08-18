//! Phase 3 stage 7 end-to-end test: drives the actual `volamos` binary
//! (same pattern as `phase2_e2e.rs`/`hello_cli.rs`) against
//! `fixtures/systest`, proving `SystemTagList()` (`System()`'s underlying
//! LVO) really resolves and runs a *nested* guest program
//! (`fixtures/echoargs`) to completion: its output appears on stdout
//! (interleaved correctly, before the parent's own trailing message), its
//! propagated exit code is observed by the parent (`tst.l d0` on
//! `SystemTagList`'s `D0`), and control returns cleanly to the parent
//! guest program afterward (able to make further dos.library calls and
//! exit with its own, distinctive code).
//!
//! Every test gets its own unique temp directory tree (via [`TempDir`]),
//! cleaned up on drop, so tests never share host state -- same
//! convention as `phase2_e2e.rs`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

const SYSTEST_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/systest");
const ECHOARGS_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/echoargs");

/// `fixtures/systest`'s distinctive success exit code (see
/// `fixtures/gen_systest.py`'s `SUCCESS_EXIT_CODE`).
const SYSTEST_SUCCESS_EXIT_CODE: i32 = 42;

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = env::temp_dir().join(format!("volamos-dosseg-e2e-{tag}-{pid}-{n}"));
        fs::create_dir_all(&path).expect("create temp dir");
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

fn volamos() -> Command {
    Command::new(env!("CARGO_BIN_EXE_volamos"))
}

fn stdout_of(output: &std::process::Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout should be valid UTF-8")
}

/// `SystemTagList("TEST:echoargs sys arg", NULL)` resolves "TEST:echoargs"
/// through the same `-V TEST:` volume mapping the parent process was
/// given, runs it as a nested guest program with args "sys"/"arg",
/// and observes its exit code (0) before printing its own trailing
/// message and exiting 42 -- see `fixtures/systest.s`/`gen_systest.py`
/// for the exact program.
#[test]
fn systest_runs_nested_echoargs_and_propagates_output_and_control() {
    let tmp = TempDir::new("systest-basic");
    // "TEST:echoargs" must resolve to a real file under the TEST: volume
    // mapping -- copy the existing echoargs fixture binary there.
    fs::copy(ECHOARGS_PATH, tmp.path().join("echoargs")).expect("copy echoargs fixture");

    let output = volamos()
        .arg("-V")
        .arg(format!("TEST:{}", tmp.path().display()))
        .arg(SYSTEST_PATH)
        .output()
        .expect("failed to run volamos");

    // systest's own "success" exit code is the distinctive sentinel 42,
    // not 0 -- see fixtures/gen_systest.py -- so this checks the exit
    // code directly rather than `output.status.success()` (which only
    // ever means "exited 0").
    assert_eq!(
        output.status.code(),
        Some(SYSTEST_SUCCESS_EXIT_CODE),
        "systest should exit 42 after observing the nested program's exit code as 0; \
         stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = stdout_of(&output);
    // Nested echoargs' own output ("sys arg\n" -- it PutStrs its command
    // line verbatim) must appear, and it must appear *before* systest's
    // own trailing message, proving the nested run actually completed
    // (synchronously) before the parent continued.
    let nested_pos = stdout
        .find("sys arg\n")
        .expect("nested echoargs output should appear on stdout");
    let trailing_pos = stdout
        .find("after system\n")
        .expect("parent's trailing message should appear on stdout");
    assert!(
        nested_pos < trailing_pos,
        "nested program's output should appear before the parent's own trailing message; \
         stdout was: {stdout:?}"
    );
}

/// Without any volume mapping at all, `SystemTagList` can't resolve
/// "TEST:echoargs" (no `Vfs` installed -- see `crate::dosseg`'s "No VFS
/// configured"-style failure path), so `D0` comes back `-1` and systest
/// takes its documented failure branch (exit 99), never printing its
/// trailing "after system" message.
#[test]
fn systest_without_vfs_fails_cleanly_and_never_reaches_the_success_path() {
    let output = volamos()
        .arg(SYSTEST_PATH)
        .output()
        .expect("failed to run volamos");

    assert_eq!(
        output.status.code(),
        Some(99),
        "with no Vfs installed, SystemTagList can't resolve the nested program, so systest \
         should take its documented D0 != 0 failure branch"
    );
    let stdout = stdout_of(&output);
    assert!(
        !stdout.contains("after system\n"),
        "the success-path message shouldn't print when SystemTagList failed to run anything"
    );
}
