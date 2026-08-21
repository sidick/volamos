//! Phase L3 end-to-end test: drives the actual `volamos` binary (same
//! pattern as `dosseg_e2e.rs`/`runcmdtest_e2e.rs`) against
//! `fixtures/libcall`, proving `OpenLibrary` genuinely loads a real
//! disk-based `RTF_AUTOINIT` library end to end at the CLI level -- not
//! just via `volamos-core`'s in-process tests
//! (`crates/volamos-core/src/execlib.rs`'s `loaded_library_e2e` module) --
//! covering both name-resolution paths (bare name via `LIBS:`, and a
//! full path) that `OpenLibrary`'s real resolution logic tells apart.
//! See `fixtures/libcall.s`'s header comment for the exact program flow.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

const LIBCALL_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/libcall");
const TESTLIB_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/testlib");

/// `fixtures/libcall`'s success output -- see `fixtures/libcall.s`'s
/// header comment for what each line proves.
const EXPECTED_STDOUT: &str = "user ok\nadd ok\ncnt ok\n";

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = env::temp_dir().join(format!("volamos-libcall-e2e-{tag}-{pid}-{n}"));
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

/// Sets up a temp dir with `libs/test.library` (a copy of
/// `fixtures/testlib`) and a `Vfs` mapping `SYS:` to it and `LIBS:` to
/// `SYS:libs` -- the same shape `execlib.rs`'s own `vfs_with_libs_file`
/// test helper uses, but via the real CLI's `-V`/`-a` flags (see
/// `phase2_e2e.rs`'s `dirtest_resolves_a_simple_assign` for that flag
/// pattern).
fn setup_libs_dir(tag: &str) -> TempDir {
    let tmp = TempDir::new(tag);
    fs::create_dir(tmp.path().join("libs")).expect("create libs dir");
    fs::copy(TESTLIB_PATH, tmp.path().join("libs").join("test.library"))
        .expect("copy testlib fixture");
    tmp
}

/// Test A: bare-name open ("test.library"), resolved via the `LIBS:`
/// assign.
#[test]
fn opens_library_by_bare_name_via_libs_assign() {
    let tmp = setup_libs_dir("bare-name");

    let output = volamos()
        .arg("-V")
        .arg(format!("SYS:{}", tmp.path().display()))
        .arg("-a")
        .arg("LIBS:SYS:libs")
        .arg(LIBCALL_PATH)
        .arg("test.library")
        .output()
        .expect("failed to run volamos");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(stdout_of(&output), EXPECTED_STDOUT);
}

/// Test B: the same fixture, same library, but opened via a full path
/// ("SYS:libs/test.library") instead of a bare name -- exercises
/// `OpenLibrary`'s other name-resolution branch with the same binary.
#[test]
fn opens_library_by_full_path() {
    let tmp = setup_libs_dir("full-path");

    let output = volamos()
        .arg("-V")
        .arg(format!("SYS:{}", tmp.path().display()))
        .arg("-a")
        .arg("LIBS:SYS:libs")
        .arg(LIBCALL_PATH)
        .arg("SYS:libs/test.library")
        .output()
        .expect("failed to run volamos");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(stdout_of(&output), EXPECTED_STDOUT);
}

/// Test C: no library file present at all -- `OpenLibrary("test.library",
/// 0)` returns NULL, so `libcall` takes its documented failure branch.
#[test]
fn missing_library_prints_open_failed_and_exits_ten() {
    let tmp = TempDir::new("missing-lib");
    fs::create_dir(tmp.path().join("libs")).expect("create empty libs dir");

    let output = volamos()
        .arg("-V")
        .arg(format!("SYS:{}", tmp.path().display()))
        .arg("-a")
        .arg("LIBS:SYS:libs")
        .arg(LIBCALL_PATH)
        .arg("test.library")
        .output()
        .expect("failed to run volamos");

    assert_eq!(
        output.status.code(),
        Some(10),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(stdout_of(&output), "open failed\n");
}
