//! Phase 3 stage 8 end-to-end tests: drives the actual `volamos` binary
//! (same `CARGO_BIN_EXE_volamos` convention as `phase2_e2e.rs`/
//! `dosseg_e2e.rs`; no host filesystem fixtures are needed here, so
//! there's no `TempDir` in this file) to prove, through real hunk-loaded
//! execution, the Phase 3 features that otherwise only had in-crate unit
//! tests:
//!
//! - `exec.library`'s `AllocMem`/`FreeMem`/`AllocVec`/`FreeVec`
//!   (`execmem.rs`), `utility.library` opened for real via
//!   `OpenLibrary("utility.library", 0)` and its `Stricmp`/`GetTagData`/
//!   `Strnicmp` (`utility.rs`), and `exec.library`'s `FindTask`/
//!   `SetSignal` plus `dos.library`'s `CheckSignal` (`exectask.rs`) --
//!   via `fixtures/exectest` (`fixtures/gen_exectest.py`).
//! - `--stack` end to end: acceptance of a suffixed value, clamping of a
//!   too-small one (both via `echoargs`, which needs no library calls
//!   beyond `OpenLibrary`/`PutStr` to prove the CLI plumbed the flag
//!   through and the run still completes), an invalid value producing
//!   the CLI's usage error, and a real stack-overflow trip (the "vamos
//!   is known to hit" bug class Phase 3 targets) via `fixtures/recurse`
//!   (`fixtures/gen_recurse.py`).
//!
//! **SIGINT end to end: deliberately not covered here.** The unit tests
//! in `crates/volamos-core/src/exectask.rs` already cover the host
//! `SIGINT`/`SIGTERM` -> `SIGBREAKF_CTRL_C` folding logic directly
//! (`PENDING_HOST_BREAK`/`fold_pending_host_break`). An actual CLI-level
//! SIGINT e2e test would need a *long-running* guest process that
//! actively polls/`Wait`s on the signal after receiving it (so there's a
//! window to deliver the host signal into, and an observable guest-side
//! reaction to assert on) -- every fixture in this repo runs to
//! completion near-instantly and none of them call `Wait`/`CheckSignal`
//! in a loop waiting on `SIGBREAKF_CTRL_C` specifically, so there is no
//! fixture this test file could drive to make a real signal delivery
//! observable rather than racy. Building a fixture purely to make this
//! one test possible (an infinite `Wait`/`CheckSignal` polling loop) was
//! judged not worth the added maintenance surface given the unit tests
//! already exercise the exact folding mechanism a real SIGINT would
//! trigger; revisit if a corpus program with this shape shows up.

use std::process::Command;

const EXECTEST_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/exectest");
const ECHOARGS_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/echoargs");
const RECURSE_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/recurse");

fn volamos() -> Command {
    Command::new(env!("CARGO_BIN_EXE_volamos"))
}

fn stdout_of(output: &std::process::Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout should be valid UTF-8")
}

fn stderr_of(output: &std::process::Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr should be valid UTF-8")
}

// --- exectest: execmem + utility (via real OpenLibrary) + exectask/CheckSignal ---

#[test]
fn exectest_runs_every_checked_step_and_exits_zero() {
    let output = volamos()
        .arg(EXECTEST_PATH)
        .output()
        .expect("failed to run volamos");

    assert_eq!(
        output.status.code(),
        Some(0),
        "exectest should exit 0 (all nine checked steps passing); stderr: {}",
        stderr_of(&output)
    );
    assert_eq!(
        stdout_of(&output),
        "exec ok\n",
        "exectest's success path PutStrs exactly this message"
    );
}

// --- --stack: acceptance, clamping, and CLI validation ---

#[test]
fn stack_flag_accepts_a_suffixed_value_and_the_program_still_runs() {
    let output = volamos()
        .arg("--stack")
        .arg("256K")
        .arg(ECHOARGS_PATH)
        .arg("foo")
        .arg("bar")
        .output()
        .expect("failed to run volamos");

    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
    assert_eq!(stdout_of(&output), "foo bar\n");
}

#[test]
fn stack_flag_below_the_minimum_is_silently_clamped_and_still_runs() {
    // volamos_core::MIN_STACK_SIZE is 4096; anything below it is clamped
    // up rather than honored or rejected -- see Runtime::new's docs.
    // echoargs needs only a trivial amount of real stack, so a clamp
    // failure (e.g. if clamping regressed into "reject instead") would
    // show up as a nonzero exit / no output rather than this passing.
    let output = volamos()
        .arg("--stack")
        .arg("1")
        .arg(ECHOARGS_PATH)
        .output()
        .expect("failed to run volamos");

    assert!(
        output.status.success(),
        "a too-small --stack should be clamped up, not rejected; stderr: {}",
        stderr_of(&output)
    );
    assert_eq!(stdout_of(&output), "\n");
}

#[test]
fn stack_flag_with_an_invalid_value_produces_the_usage_error() {
    let output = volamos()
        .arg("--stack")
        .arg("notanumber")
        .arg(ECHOARGS_PATH)
        .output()
        .expect("failed to run volamos");

    assert!(
        !output.status.success(),
        "an invalid --stack value should fail before even trying to run the program"
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("--stack"),
        "expected the --stack parse error in stderr, got: {stderr:?}"
    );
}

// --- real stack overflow: fixtures/recurse trips check_stack_bounds ---

#[test]
fn recurse_with_a_small_stack_trips_the_stack_overflow_guard() {
    // fixtures/recurse (gen_recurse.py) is an infinite bsr loop with one
    // cheap dos.library call (PutStr) per iteration -- see its module
    // docs for why the library call is what actually re-checks the
    // bounds. --stack 4096 (volamos_core::MIN_STACK_SIZE, the CLI's own
    // floor) makes this trip after roughly a thousand iterations, fast
    // enough for a test and with a bounded amount of "x\n" output
    // captured before the guard fires.
    let output = volamos()
        .arg("--stack")
        .arg("4096")
        .arg(RECURSE_PATH)
        .output()
        .expect("failed to run volamos");

    assert!(
        !output.status.success(),
        "recurse should fail once it runs off the bottom of its (tiny) stack"
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("stack overflow"),
        "expected a stack-overflow diagnostic in stderr, got: {stderr:?}"
    );

    // Sanity: it actually ran for a while first (this is a real trip,
    // not an immediate failure on the very first call) -- the loop's one
    // dos.library call per iteration means "some x\n lines got out"
    // proves real guest execution happened before the guard fired.
    let stdout = stdout_of(&output);
    assert!(
        stdout.contains("x\n"),
        "expected at least one loop iteration's output before the overflow, got: {stdout:?}"
    );
}
