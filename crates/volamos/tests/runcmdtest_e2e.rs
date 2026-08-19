//! `RunCommand()` end-to-end test: drives the actual `volamos` binary
//! (same pattern as `dosseg_e2e.rs`) against `fixtures/runcmdtest`,
//! proving `RunCommand` really `LoadSeg`s and re-runs a *nested* guest
//! program (`fixtures/echoargs`) to completion: its output appears on
//! stdout (interleaved correctly, before the parent's own trailing
//! message), its propagated exit code is observed by the parent (`tst.l
//! d0` on `RunCommand`'s `D0`), and control returns cleanly to the
//! parent guest program afterward (able to make further dos.library
//! calls -- `UnLoadSeg`, then `PutStr` -- and exit with its own,
//! distinctive code).
//!
//! Every test gets its own unique temp directory tree (via [`TempDir`]),
//! cleaned up on drop, so tests never share host state -- same
//! convention as `dosseg_e2e.rs`/`phase2_e2e.rs`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

const RUNCMDTEST_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/runcmdtest");
const ECHOARGS_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/echoargs");

/// `fixtures/runcmdtest`'s distinctive success exit code (see
/// `fixtures/gen_runcmdtest.py`'s `SUCCESS_EXIT_CODE`).
const RUNCMDTEST_SUCCESS_EXIT_CODE: i32 = 43;

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = env::temp_dir().join(format!("volamos-runcmdtest-e2e-{tag}-{pid}-{n}"));
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

/// `LoadSeg("TEST:echoargs")` then `RunCommand(seg, 8192, "run cmd", 7)`
/// resolves "TEST:echoargs" through the same `-V TEST:` volume mapping
/// the parent process was given, runs it as a nested guest program with
/// args "run"/"cmd", and observes its exit code (0) before `UnLoadSeg`ing
/// the seglist, printing its own trailing message, and exiting 43 -- see
/// `fixtures/runcmdtest.s`/`gen_runcmdtest.py` for the exact program.
#[test]
fn runcmdtest_loadseg_runcommand_propagates_output_and_control() {
    let tmp = TempDir::new("basic");
    // "TEST:echoargs" must resolve to a real file under the TEST: volume
    // mapping -- copy the existing echoargs fixture binary there.
    fs::copy(ECHOARGS_PATH, tmp.path().join("echoargs")).expect("copy echoargs fixture");

    let output = volamos()
        .arg("-V")
        .arg(format!("TEST:{}", tmp.path().display()))
        .arg(RUNCMDTEST_PATH)
        .output()
        .expect("failed to run volamos");

    assert_eq!(
        output.status.code(),
        Some(RUNCMDTEST_SUCCESS_EXIT_CODE),
        "runcmdtest should exit 43 after observing the nested program's exit code as 0; \
         stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = stdout_of(&output);
    // Nested echoargs' own output ("run cmd\n" -- it PutStrs its command
    // line verbatim) must appear, and it must appear *before*
    // runcmdtest's own trailing message, proving the nested run actually
    // completed (synchronously) before the parent continued.
    let nested_pos = stdout
        .find("run cmd\n")
        .expect("nested echoargs output should appear on stdout");
    let trailing_pos = stdout
        .find("after runcommand\n")
        .expect("parent's trailing message should appear on stdout");
    assert!(
        nested_pos < trailing_pos,
        "nested program's output should appear before the parent's own trailing message; \
         stdout was: {stdout:?}"
    );
}

/// Without any volume mapping at all, `LoadSeg` can't resolve
/// "TEST:echoargs" (no `Vfs` installed), so `D0` comes back `0` and
/// `RunCommand` is never reached with a valid seglist at all -- D1 stays
/// `0`, `RunCommand(0, ...)` fails cleanly (`D0 = -1`, same
/// `ERROR_OBJECT_NOT_FOUND` "couldn't run it" bucket `System()`/
/// `Execute()` use), and runcmdtest takes its documented failure branch
/// (exit 99), never printing its trailing "after runcommand" message.
#[test]
fn runcmdtest_without_vfs_fails_cleanly_and_never_reaches_the_success_path() {
    let output = volamos()
        .arg(RUNCMDTEST_PATH)
        .output()
        .expect("failed to run volamos");

    assert_eq!(
        output.status.code(),
        Some(99),
        "with no Vfs installed, LoadSeg can't resolve the nested program, so runcmdtest \
         should take its documented D0 != 0 failure branch"
    );
    let stdout = stdout_of(&output);
    assert!(
        !stdout.contains("after runcommand\n"),
        "the success-path message shouldn't print when RunCommand never ran anything"
    );
}
