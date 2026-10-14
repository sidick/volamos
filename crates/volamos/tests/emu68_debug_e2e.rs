//! End-to-end tests of `--emu68-debug` (issue #140) against the real
//! `volamos` binary, using a tiny hunk executable assembled by hand in
//! the test: it stores "Hi\n" one byte at a time to Emu68's debug port at
//! 0xDEADBEEF and exits 0.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// `fixtures/systest`: runs `TEST:echoargs sys arg` via `System()` and
/// exits 42 if the nested program returned 0.
const SYSTEST_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/systest");

const WARNING: &str = "inactive with --cpu";

/// ```text
/// move.b #'H',$DEADBEEF   ; x3, for 'H' 'i' '\n'
/// moveq  #0,d0
/// rts
/// ```
fn program_bytes() -> Vec<u8> {
    let mut words: Vec<u16> = Vec::new();
    for c in *b"Hi\n" {
        words.extend([0x13FC, u16::from(c), 0xDEAD, 0xBEEF]);
    }
    words.extend([0x7000, 0x4E75]);
    let code: Vec<u8> = words.iter().flat_map(|w| w.to_be_bytes()).collect();
    assert_eq!(code.len(), 28);
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
            std::env::temp_dir().join(format!("volamos-emu68debug-{tag}-{}", std::process::id()));
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

fn write_program(dir: &TempDir, name: &str) -> PathBuf {
    let path = dir.path().join(name);
    fs::write(&path, program_bytes()).unwrap();
    path
}

fn run(program: &Path, flags: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_volamos"))
        .args(flags)
        .arg(program)
        .output()
        .expect("failed to run the volamos binary")
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn the_port_prints_to_stderr_on_a_32_bit_cpu() {
    let dir = TempDir::new("on");
    let prog = write_program(&dir, "prog");
    let out = run(&prog, &["--emu68-debug", "--cpu", "68020"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    assert_eq!(stderr_of(&out), "Hi\n");
    assert!(out.stdout.is_empty());
}

#[test]
fn nothing_happens_without_the_flag() {
    let dir = TempDir::new("off");
    let prog = write_program(&dir, "prog");
    for flags in [
        &["--cpu", "68020"][..],
        &[],
        &["--cpu", "68020", "--sanitize"],
    ] {
        let out = run(&prog, flags);
        assert_eq!(out.status.code(), Some(0), "{flags:?}: {}", stderr_of(&out));
        assert_eq!(stderr_of(&out), "", "{flags:?}");
    }
}

#[test]
fn on_a_24_bit_cpu_it_warns_and_stays_silent() {
    let dir = TempDir::new("24bit");
    let prog = write_program(&dir, "prog");
    // The default CPU is a 68000.
    for flags in [
        &["--emu68-debug"][..],
        &["--emu68-debug", "--cpu", "68010"],
        &["--emu68-debug", "--cpu", "68000", "--jit"],
    ] {
        let out = run(&prog, flags);
        assert_eq!(out.status.code(), Some(0), "{flags:?}: {}", stderr_of(&out));
        let err = stderr_of(&out);
        assert!(err.contains(WARNING), "{flags:?}: {err}");
        assert!(!err.contains("Hi"), "{flags:?}: {err}");
    }
}

#[test]
fn no_warning_on_a_32_bit_cpu() {
    let dir = TempDir::new("nowarn");
    let prog = write_program(&dir, "prog");
    let out = run(&prog, &["--emu68-debug", "--cpu", "68040"]);
    assert!(!stderr_of(&out).contains(WARNING), "{}", stderr_of(&out));
}

#[test]
fn a_config_file_cannot_turn_it_on() {
    let dir = TempDir::new("config");
    let prog = write_program(&dir, "prog");
    fs::write(dir.path().join(".volamos"), "EMU68_DEBUG=true\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_volamos"))
        .current_dir(dir.path())
        .args(["--cpu", "68020"])
        .arg(&prog)
        .output()
        .unwrap();
    assert_ne!(out.status.code(), Some(0));
    assert!(
        stderr_of(&out).contains("unknown key"),
        "{}",
        stderr_of(&out)
    );
    assert!(!stderr_of(&out).contains("Hi"));
}

#[test]
fn it_works_across_32_bit_cpu_models_ram_sizes_and_the_jit_flag() {
    let dir = TempDir::new("matrix");
    let prog = write_program(&dir, "prog");
    for flags in [
        &["--emu68-debug", "--cpu", "68030"][..],
        &["--emu68-debug", "--cpu", "68060", "--fpu"],
        &["--emu68-debug", "--cpu", "68020", "--ram", "1M"],
        &["--emu68-debug", "--cpu", "68020", "--ram", "32M"],
        // The JIT would bypass the port; the flag forces it off.
        &["--emu68-debug", "--cpu", "68020", "--jit"],
    ] {
        let out = run(&prog, flags);
        assert_eq!(out.status.code(), Some(0), "{flags:?}: {}", stderr_of(&out));
        assert_eq!(stderr_of(&out), "Hi\n", "{flags:?}");
    }
}

#[test]
fn port_stores_are_not_sanitizer_violations() {
    let dir = TempDir::new("sanitize");
    let prog = write_program(&dir, "prog");
    let out = run(&prog, &["--emu68-debug", "--cpu", "68020", "--sanitize"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    assert_eq!(stderr_of(&out), "Hi\n");
}

#[test]
fn nested_system_runs_inherit_the_port() {
    // systest System()s "TEST:echoargs"; make that file our program.
    let dir = TempDir::new("nested");
    write_program(&dir, "echoargs");
    let nested = |flags: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_volamos"))
            .args(flags)
            .arg("-V")
            .arg(format!("TEST:{}", dir.path().display()))
            .arg(SYSTEST_PATH)
            .output()
            .unwrap()
    };
    for flags in [
        &["--emu68-debug", "--cpu", "68020"][..],
        &["--emu68-debug", "--cpu", "68020", "--jit"],
    ] {
        let out = nested(flags);
        assert_eq!(
            out.status.code(),
            Some(42),
            "{flags:?}: {}",
            stderr_of(&out)
        );
        assert_eq!(stderr_of(&out), "Hi\n", "{flags:?}");
    }
    // Without the flag the nested program prints nothing either.
    let out = nested(&["--cpu", "68020"]);
    assert_eq!(out.status.code(), Some(42));
    assert_eq!(stderr_of(&out), "");
}

#[test]
fn both_debug_ports_can_be_enabled_together() {
    // The UAE port's own e2e program is not reused; this only checks the
    // two flags parse and run together, with the Emu68 port still live.
    let dir = TempDir::new("both");
    let prog = write_program(&dir, "prog");
    let out = run(&prog, &["--uae-debug", "--emu68-debug", "--cpu", "68020"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    assert_eq!(stderr_of(&out), "Hi\n");
}
