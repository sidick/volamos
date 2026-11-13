//! End-to-end tests of volamos's own exit statuses against the real
//! binary: the guest's own code on a normal run, `98` when volamos
//! couldn't carry the run to the guest's `rts`, `2` for a usage error.
//!
//! The point of the `98` cases is that they are *distinguishable* from
//! a guest that returned 1 on purpose. Before this, both were `1`,
//! which forced test programs to avoid returning 1 at all to keep a
//! harness able to tell a crash from a deliberate failure status.
//!
//! The guest programs are tiny hunk executables assembled by hand here
//! (same approach as `uae_debug_e2e.rs`), so these tests need no
//! fixture file and no m68k toolchain.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// `moveq #N,d0 ; rts` -- a guest that deliberately returns `N`.
fn returns(n: u8) -> Vec<u8> {
    let words: [u16; 2] = [0x7000 | n as u16, 0x4E75];
    hunk(words.iter().flat_map(|w| w.to_be_bytes()).collect())
}

/// `dc.w $4AFC` -- the 68000 `ILLEGAL` instruction, which no guest
/// reaches by accident and which volamos reports rather than runs.
fn illegal_instruction() -> Vec<u8> {
    hunk(0x4AFCu16.to_be_bytes().to_vec())
}

/// Wraps `code` in a one-hunk executable.
fn hunk(mut code: Vec<u8>) -> Vec<u8> {
    while !code.len().is_multiple_of(4) {
        code.push(0);
    }
    let longs = (code.len() / 4) as u32;
    let mut out = Vec::new();
    let mut long = |v: u32| out.extend_from_slice(&v.to_be_bytes());
    long(0x3F3); // HUNK_HEADER
    long(0); // no resident libraries
    long(1); // table size
    long(0); // first
    long(0); // last
    long(longs);
    long(0x3E9); // HUNK_CODE
    long(longs);
    out.extend_from_slice(&code);
    out.extend_from_slice(&0x3F2u32.to_be_bytes()); // HUNK_END
    out
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("volamos-exitcodes-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }
    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run(program: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_volamos"))
        .arg(program)
        .output()
        .expect("run volamos")
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn a_guests_own_exit_code_is_propagated_including_one() {
    let dir = TempDir::new("guest");
    for code in [0u8, 1, 5, 10, 20] {
        let prog = dir.write(&format!("ret{code}"), &returns(code));
        let out = run(&prog);
        assert_eq!(
            out.status.code(),
            Some(code as i32),
            "returns({code}): {}",
            stderr_of(&out)
        );
        assert_eq!(stderr_of(&out), "", "returns({code}) should be silent");
    }
}

#[test]
fn a_runtime_crash_exits_98_not_1_so_a_harness_can_tell_it_from_a_guest_failure() {
    let dir = TempDir::new("crash");
    let prog = dir.write("crash", &illegal_instruction());
    let out = run(&prog);
    assert_eq!(out.status.code(), Some(98), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).starts_with("volamos: "),
        "a 98 always comes with a diagnostic naming what went wrong, got {:?}",
        stderr_of(&out)
    );
}

#[test]
fn an_unreadable_or_unloadable_program_exits_98() {
    let dir = TempDir::new("load");
    let missing = dir.0.join("no-such-program");
    let out = run(&missing);
    assert_eq!(out.status.code(), Some(98), "{}", stderr_of(&out));

    let not_a_hunk = dir.write("not-a-hunk", b"this is not an executable\n");
    let out = run(&not_a_hunk);
    assert_eq!(out.status.code(), Some(98), "{}", stderr_of(&out));
}

#[test]
fn a_usage_error_still_exits_2() {
    let out = Command::new(env!("CARGO_BIN_EXE_volamos"))
        .arg("--no-such-flag")
        .output()
        .expect("run volamos");
    assert_eq!(out.status.code(), Some(2), "{}", stderr_of(&out));
}
