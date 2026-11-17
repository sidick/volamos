//! End-to-end test of the 24-bit address mask (the follow-up to issue #98
//! in #332) against the real `volamos` binary, using a tiny hunk
//! executable assembled by hand in the test: `PutStr()` of a pointer with
//! stray bits in its top byte.
//!
//! A 68000/68010 has a 24-bit address bus and ignores the top byte of
//! every address, so the real `dos.library` printed the string (checked
//! on an A500 under Kickstart 3.2). Volamos's *native* `PutStr`
//! dereferences the pointer for the CPU and has to see the same address.
//! On a 32-bit CPU the top byte is a real address bit, so the same
//! pointer is somewhere else and nothing is printed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The guest program, as 68000 machine code (`A6` already holds
/// `dos.library` at entry, as for every fixture):
///
/// ```text
/// lea     msg(pc),a0
/// move.l  a0,d1
/// ori.l   #$AB000000,d1      ; stray top byte
/// jsr     -948(a6)           ; PutStr
/// moveq   #0,d0
/// rts
/// msg:    dc.b "tagged ok",10,0
/// ```
fn program_bytes() -> Vec<u8> {
    let words: [u16; 10] = [
        0x41FA, 0x0012, // lea msg(pc),a0 (msg is 18 bytes past the extension word)
        0x2208, // move.l a0,d1
        0x0081, 0xAB00, 0x0000, // ori.l #$AB000000,d1
        0x4EAE, 0xFC4C, // jsr -948(a6)
        0x7000, 0x4E75, // moveq #0,d0 ; rts
    ];
    let mut code: Vec<u8> = words.iter().flat_map(|w| w.to_be_bytes()).collect();
    assert_eq!(code.len(), 20);
    code.extend_from_slice(b"tagged ok\n\0");
    hunk(code)
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
            std::env::temp_dir().join(format!("volamos-addrmask-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run(cpu: &str, tag: &str) -> Output {
    let dir = TempDir::new(tag);
    let prog = dir.path().join("prog");
    fs::write(&prog, program_bytes()).unwrap();
    Command::new(env!("CARGO_BIN_EXE_volamos"))
        .args(["--cpu", cpu])
        .arg(&prog)
        .output()
        .expect("failed to run the volamos binary")
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn a_24_bit_cpu_prints_through_a_pointer_with_a_stray_top_byte() {
    for cpu in ["68000", "68010"] {
        let out = run(cpu, cpu);
        assert_eq!(out.status.code(), Some(0), "{cpu}");
        assert_eq!(stdout_of(&out), "tagged ok\n", "--cpu {cpu}");
    }
}

#[test]
fn a_32_bit_cpu_keeps_the_top_byte_so_the_pointer_points_elsewhere() {
    for cpu in ["68020", "68030", "68040"] {
        let out = run(cpu, cpu);
        assert_eq!(out.status.code(), Some(0), "{cpu}");
        assert_eq!(stdout_of(&out), "", "--cpu {cpu}");
    }
}
