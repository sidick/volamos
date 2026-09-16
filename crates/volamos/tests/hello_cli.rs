//! End-to-end test of the actual `volamos` binary: runs
//! `fixtures/hello` as a real subprocess and checks its captured stdout
//! and process exit status, rather than calling any library code
//! directly.

use std::process::Command;

/// Path to `fixtures/hello` relative to this crate's manifest directory
/// (`crates/volamos`), resolved at compile time so the test works
/// regardless of the working directory `cargo test` is invoked from.
const HELLO_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/hello");

/// Path to `fixtures/memtest`, the deliberately-buggy heap fixture
/// (issue #65). See `fixtures/README.md` for its four modes.
const MEMTEST_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/memtest");

/// Runs `fixtures/memtest <mode>` under `--sanitize` and returns its
/// captured stderr, asserting the process itself succeeded. The fixture
/// deliberately misbehaves but always exits 0: noticing the bug is the
/// sanitizer's job, not the guest's (see `fixtures/memtest.s`).
fn sanitize_memtest(mode: &str) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_volamos"))
        .arg("--sanitize")
        .arg(MEMTEST_PATH)
        .arg(mode)
        .output()
        .expect("failed to run the volamos binary");
    assert!(
        output.status.success(),
        "memtest {mode} exited with {:?}; stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stderr).unwrap()
}

/// Path to `fixtures/stacktest`, the stack-bug fixture (issue #65
/// increment 2). See `fixtures/README.md` for its modes.
const STACKTEST_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/stacktest");

/// As [`sanitize_memtest`], for `fixtures/stacktest`.
fn sanitize_stacktest(mode: &str) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_volamos"))
        .arg("--sanitize")
        .arg(STACKTEST_PATH)
        .arg(mode)
        .output()
        .expect("failed to run the volamos binary");
    assert!(
        output.status.success(),
        "stacktest {mode} exited with {:?}; stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stderr).unwrap()
}

#[test]
fn sanitize_reports_nothing_for_legitimate_stack_use() {
    // Three false-positive guards, and they matter more than the two
    // detection tests below. `clean` is ordinary nested calls with
    // LINK/UNLK frames; `deep` is 64 levels of recursion with MOVEM
    // saves, so a lot of stack-pointer movement; `pushret` is the
    // `move.l #target,-(sp)` + `rts` computed-jump idiom, which is
    // legitimate, common Amiga code that has no matching JSR/BSR at
    // all. `pushret` genuinely regressed once during development --
    // volamos performs library-call returns itself, which left a stale
    // shadow frame sitting at exactly the slot the idiom's push reused
    // -- so this is a real guard, not a theoretical one.
    for mode in ["clean", "deep", "pushret"] {
        let stderr = sanitize_stacktest(mode);
        assert!(
            !stderr.contains("sanitizer:"),
            "expected stacktest {mode} to report nothing, got: {stderr}"
        );
    }
}

#[test]
fn sanitize_catches_a_read_below_the_stack_pointer() {
    // 128 bytes below SP -- deliberately past the grace band that
    // forgives the push-writes-below-SP window every call makes (see
    // volamos_core::sanitize::BELOW_SP_GRACE_BYTES).
    let stderr = sanitize_stacktest("below");
    assert!(
        stderr.contains("invalid 1-byte read") && stderr.contains("below stack pointer"),
        "expected a below-stack-pointer read violation, got: {stderr}"
    );
}

#[test]
fn sanitize_catches_a_corrupted_return_address() {
    // Stack smashing, caught precisely -- something valgrind itself
    // does not offer. The report must name both addresses, since
    // "expected X, found Y" is the whole diagnostic value.
    let stderr = sanitize_stacktest("smash");
    assert!(
        stderr.contains("return address corrupted")
            && stderr.contains("expected")
            && stderr.contains("found"),
        "expected a return-address-corruption violation naming both \
         addresses, got: {stderr}"
    );
}

/// As [`sanitize_memtest`], but with `--sanitize-uninit` on top.
fn sanitize_uninit_memtest(mode: &str) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_volamos"))
        .arg("--sanitize-uninit")
        .arg(MEMTEST_PATH)
        .arg(mode)
        .output()
        .expect("failed to run the volamos binary");
    assert!(
        output.status.success(),
        "memtest {mode} exited with {:?}; stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stderr).unwrap()
}

/// Runs `fixtures/memtest <mode>` with the given extra flags, returning
/// its captured stdout. Used by the `--dirty-heap` tests, which assert
/// on what the *guest* printed rather than on a sanitizer report.
fn memtest_stdout(flags: &[&str], mode: &str) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_volamos"))
        .args(flags)
        .arg(MEMTEST_PATH)
        .arg(mode)
        .output()
        .expect("failed to run the volamos binary");
    assert!(
        output.status.success(),
        "memtest {mode} exited {:?}",
        output.status
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn dirty_heap_changes_which_branch_a_zero_dependent_guest_takes() {
    // The whole point of --dirty-heap (issue #80): `zerodep` reads an
    // unwritten AllocMem block and *branches* on the value. Without the
    // flag volamos hands it zeros, so it takes the lucky path and its
    // bug stays invisible -- which is what real hardware would not do.
    // With the flag the debris is real and the other branch runs.
    let plain = memtest_stdout(&[], "zerodep");
    assert!(
        plain.contains("took the zero path"),
        "without --dirty-heap the guest should see zeros, got: {plain}"
    );

    let dirty = memtest_stdout(&["--dirty-heap"], "zerodep");
    assert!(
        dirty.contains("took the garbage path"),
        "with --dirty-heap the guest should see poison, got: {dirty}"
    );
}

#[test]
fn dirty_heap_and_uninit_reporting_compose() {
    // Filling writes through the checked path and so heals the shadow
    // bytes it touches; the poison step re-marks them afterwards. If
    // that ordering were wrong these two flags would cancel out and the
    // report would be empty -- see execmem's own unit test for the
    // shadow-state half of this.
    let output = Command::new(env!("CARGO_BIN_EXE_volamos"))
        .args(["--dirty-heap", "--sanitize-uninit"])
        .arg(MEMTEST_PATH)
        .arg("uninit")
        .output()
        .expect("failed to run the volamos binary");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("uninitialized 1-byte read"),
        "--dirty-heap must not silence --sanitize-uninit, got: {stderr}"
    );
}

#[test]
fn dirty_heap_is_off_by_default_and_memf_clear_still_wins() {
    // MEMF_CLEAR is a documented guarantee; `cleared` reads an
    // unwritten block it asked to be zeroed, so it must take the zero
    // path even with the fill on.
    let dirty = memtest_stdout(&["--dirty-heap"], "cleared");
    assert!(
        !dirty.contains("garbage"),
        "MEMF_CLEAR memory must stay zeroed under --dirty-heap, got: {dirty}"
    );
}

#[test]
fn sanitize_uninit_catches_a_read_of_never_written_heap() {
    let stderr = sanitize_uninit_memtest("uninit");
    assert!(
        stderr.contains("uninitialized 1-byte read"),
        "expected an uninitialized-read violation, got: {stderr}"
    );
}

#[test]
fn sanitize_uninit_is_byte_granular_not_per_allocation() {
    // `uninitpartial` writes the first 16 bytes of a 32-byte block and
    // then reads offset 20. Reporting that proves the detector tracks
    // individual bytes rather than treating a whole allocation as
    // initialised once anything in it is written -- which is the
    // difference between catching a real partial-initialisation bug and
    // catching nothing useful.
    let stderr = sanitize_uninit_memtest("uninitpartial");
    assert!(
        stderr.contains("uninitialized 1-byte read"),
        "expected an uninitialized-read violation for the unwritten half, got: {stderr}"
    );
}

#[test]
fn sanitize_uninit_reports_nothing_for_legitimate_heap_use() {
    // The false-positive guards, and they matter more than the two
    // detection tests above. `written` writes every byte before reading
    // it; `cleared` allocates with MEMF_CLEAR, which genuinely *is*
    // initialised memory. Uninitialised-read detection is the noisiest
    // class in any sanitizer, so these staying silent is what makes the
    // flag worth turning on.
    for mode in ["written", "cleared", "clean"] {
        let stderr = sanitize_uninit_memtest(mode);
        assert!(
            !stderr.contains("sanitizer:"),
            "expected memtest {mode} to report nothing under --sanitize-uninit, got: {stderr}"
        );
    }
}

#[test]
fn uninit_reporting_is_off_unless_asked_for() {
    // The same two bug modes must be silent under plain --sanitize:
    // this is an opt-in extra, and `--sanitize` staying quiet on real
    // software is what makes it trustworthy.
    for mode in ["uninit", "uninitpartial"] {
        let stderr = sanitize_memtest(mode);
        assert!(
            !stderr.contains("sanitizer:"),
            "expected memtest {mode} to be silent under plain --sanitize, got: {stderr}"
        );
    }
}

#[test]
fn sanitize_reports_nothing_for_memtests_clean_mode() {
    // The false-positive guard, and the most important of these four: a
    // program that allocates, writes and reads every byte it asked for,
    // and frees correctly must be silent. A sanitizer that cries wolf
    // on correct code is worse than none, so this failing is a louder
    // signal than any of the three detection tests below.
    let stderr = sanitize_memtest("clean");
    assert!(
        !stderr.contains("sanitizer:"),
        "expected a clean heap run to report nothing, got: {stderr}"
    );
}

#[test]
fn sanitize_catches_a_one_byte_heap_overrun_write() {
    let stderr = sanitize_memtest("overrun");
    assert!(
        stderr.contains("invalid 1-byte write") && stderr.contains("heap redzone"),
        "expected a 1-byte redzone write violation, got: {stderr}"
    );
}

#[test]
fn sanitize_catches_a_one_byte_heap_underrun_read() {
    // The off-by-one *read* MuForce-style page-granular tools can't see
    // at all -- one byte before the block, still on the same page.
    let stderr = sanitize_memtest("underrun");
    assert!(
        stderr.contains("invalid 1-byte read") && stderr.contains("heap redzone"),
        "expected a 1-byte redzone read violation, got: {stderr}"
    );
}

#[test]
fn sanitize_catches_a_use_after_free_read() {
    // Only detectable because guestmem's free quarantine holds the
    // address out of circulation: without it, nothing would have
    // re-marked these bytes, but equally nothing would stop a later
    // allocation from making the read legitimate again.
    let stderr = sanitize_memtest("uaf");
    assert!(
        stderr.contains("invalid 1-byte read") && stderr.contains("freed block"),
        "expected a use-after-free read violation, got: {stderr}"
    );
}

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
fn sanitize_flag_on_a_clean_program_reports_no_violations() {
    // hello never does anything a shadow map would flag (no heap
    // allocation, no freed/redzone memory) -- this is the "default
    // state is Valid, so a normal run is silent" property from
    // volamos_core::sanitize's module doc, exercised through the real
    // binary rather than just the library's own unit tests.
    let output = Command::new(env!("CARGO_BIN_EXE_volamos"))
        .arg("--sanitize")
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
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        !stderr.contains("sanitizer:"),
        "expected a clean --sanitize run to report nothing, got: {stderr}"
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
