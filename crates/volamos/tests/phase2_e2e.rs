//! Phase 2 (T14) end-to-end tests: drives the actual `volamos` binary
//! (like `hello_cli.rs`) against the three Phase 2 fixtures
//! (`fixtures/filetest`, `fixtures/dirtest`, `fixtures/echoargs`), each
//! built from `fixtures/gen_*.py` per `fixtures/README.md`. This is
//! Phase 2's "done" criterion (`docs/plan.md`'s T14 entry): volume
//! mapping, assign resolution (incl. multi-assign search order),
//! case-insensitive lookup, `MODE_NEWFILE` creation, `IoErr` on a
//! missing file, and the guest command-line round trip.
//!
//! Every test gets its own unique temp directory tree (via [`TempDir`]),
//! cleaned up on drop, so tests never share host state.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

const FILETEST_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/filetest");
const DIRTEST_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/dirtest");
const ECHOARGS_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/echoargs");

/// The exact string `filetest` writes then reads back (see
/// `fixtures/gen_filetest.py`'s `MESSAGE`).
const FILETEST_MESSAGE: &str = "hello from filetest\n";

/// A unique temp directory, cleaned up on drop -- same pattern as
/// `crates/volamos-core/src/{dosfile,doslock}.rs`'s own tests.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = env::temp_dir().join(format!("volamos-phase2-e2e-{tag}-{pid}-{n}"));
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

// --- volume mapping + MODE_NEWFILE creation + read-back round trip ---

#[test]
fn filetest_volume_mapping_creates_file_and_reads_it_back() {
    let tmp = TempDir::new("filetest-volume");

    let output = volamos()
        .arg("-V")
        .arg(format!("TEST:{}", tmp.path().display()))
        .arg(FILETEST_PATH)
        .output()
        .expect("failed to run volamos");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        stdout_of(&output),
        FILETEST_MESSAGE,
        "filetest should PutStr back exactly what it wrote"
    );

    // MODE_NEWFILE creation: the host file exists, with the same
    // content filetest wrote via Write() (byte-for-byte, before the
    // reopen/read-back half of the run).
    let host_file = tmp.path().join("out.txt");
    assert!(
        host_file.exists(),
        "Open(MODE_NEWFILE) should create the host file"
    );
    assert_eq!(
        fs::read_to_string(&host_file).unwrap(),
        FILETEST_MESSAGE,
        "host file content should match what Write() sent"
    );
}

// --- IoErr on a missing file/dir: no -V at all, so Open/Lock fail ---

#[test]
fn filetest_without_a_volume_mapping_prints_err_and_exits_nonzero() {
    // No -V flag at all -> no Vfs is installed -> Open("TEST:out.txt",
    // ...) always fails with ERROR_OBJECT_NOT_FOUND (see
    // crates/volamos-core/src/dosfile.rs's "No VFS configured" module
    // docs), driving filetest's `fail:` path.
    let output = volamos()
        .arg(FILETEST_PATH)
        .output()
        .expect("failed to run volamos");

    assert!(
        !output.status.success(),
        "filetest should exit nonzero when Open fails"
    );
    assert_eq!(
        stdout_of(&output),
        "ERR\n",
        "filetest's fail path PutStrs a fixed ERR marker (see fixtures/README.md)"
    );
}

#[test]
fn dirtest_without_a_volume_mapping_prints_err_and_exits_nonzero() {
    let output = volamos()
        .arg(DIRTEST_PATH)
        .output()
        .expect("failed to run volamos");

    assert!(
        !output.status.success(),
        "dirtest should exit nonzero when Lock fails"
    );
    assert_eq!(stdout_of(&output), "ERR\n");
}

// --- assign resolution, including multi-assign search order ---

#[test]
fn dirtest_resolves_a_simple_assign() {
    // -a TEST:SYS:sub (a single-target assign, not multi-) should
    // resolve exactly like a volume mapping would once expanded.
    let tmp = TempDir::new("dirtest-assign-simple");
    fs::create_dir_all(tmp.path().join("sub/dir")).unwrap();
    fs::write(tmp.path().join("sub/dir/entry.txt"), b"x").unwrap();

    let output = volamos()
        .arg("-V")
        .arg(format!("SYS:{}", tmp.path().display()))
        .arg("-a")
        .arg("TEST:SYS:sub")
        .arg(DIRTEST_PATH)
        .output()
        .expect("failed to run volamos");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(stdout_of(&output), "entry.txt\n");
}

#[test]
fn dirtest_multi_assign_search_order_prefers_the_first_target() {
    // Two volumes, A: and B:, each with a "sub/dir" directory
    // containing a *different* file -- TEST: is a multi-assign
    // "A:sub+B:sub". Per vfs.rs's documented multi-assign order (each
    // target tried in list order, first one where the path resolves
    // wins), TEST:dir should resolve to A:sub/dir, not B:sub/dir, so
    // dirtest should print A's entry and never B's.
    let tmp = TempDir::new("dirtest-assign-multi");
    let a_root = tmp.path().join("A");
    let b_root = tmp.path().join("B");
    fs::create_dir_all(a_root.join("sub/dir")).unwrap();
    fs::create_dir_all(b_root.join("sub/dir")).unwrap();
    fs::write(a_root.join("sub/dir/onlyA.txt"), b"from A").unwrap();
    fs::write(b_root.join("sub/dir/onlyB.txt"), b"from B").unwrap();

    let output = volamos()
        .arg("-V")
        .arg(format!("A:{}", a_root.display()))
        .arg("-V")
        .arg(format!("B:{}", b_root.display()))
        .arg("-a")
        .arg("TEST:A:sub+B:sub")
        .arg(DIRTEST_PATH)
        .output()
        .expect("failed to run volamos");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let out = stdout_of(&output);
    assert_eq!(
        out, "onlyA.txt\n",
        "multi-assign should resolve TEST:dir to A's copy (first in search order), not B's"
    );
}

// --- case-insensitive lookup over a case-sensitive host tree ---

#[test]
fn dirtest_resolves_a_differently_cased_directory_name() {
    // dirtest's fixed guest path is lowercase "TEST:dir"; the host
    // directory is created with a different case ("Dir") to exercise
    // vfs.rs's case-insensitive component matching. The entry inside
    // keeps its on-disk case in the printed output (matching real
    // fib_FileName -- Examine/ExNext don't re-case anything).
    let tmp = TempDir::new("dirtest-case-insensitive");
    fs::create_dir_all(tmp.path().join("Dir")).unwrap();
    fs::write(tmp.path().join("Dir/File.TXT"), b"mixed case").unwrap();

    let output = volamos()
        .arg("-V")
        .arg(format!("TEST:{}", tmp.path().display()))
        .arg(DIRTEST_PATH)
        .output()
        .expect("failed to run volamos");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        stdout_of(&output),
        "File.TXT\n",
        "lowercase 'dir' in the guest path should case-insensitively match host 'Dir'"
    );
}

// --- guest command-line ("args") round trip ---

#[test]
fn echoargs_prints_the_joined_command_line_with_args() {
    let output = volamos()
        .arg(ECHOARGS_PATH)
        .arg("foo")
        .arg("bar")
        .output()
        .expect("failed to run volamos");

    assert!(output.status.success());
    assert_eq!(stdout_of(&output), "foo bar\n");
}

#[test]
fn echoargs_prints_just_a_newline_with_no_args() {
    let output = volamos()
        .arg(ECHOARGS_PATH)
        .output()
        .expect("failed to run volamos");

    assert!(output.status.success());
    assert_eq!(stdout_of(&output), "\n");
}
