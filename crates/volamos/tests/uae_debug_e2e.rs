//! End-to-end tests of `--uae-debug` (issue #140) against the real
//! `volamos` binary, using a tiny hunk executable assembled by hand in
//! the test (no fixture file): it stores to UAE's debug sprintf port
//! at 0xBFFF00 and exits 0.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// `fixtures/systest`: runs `TEST:echoargs sys arg` via `System()` and
/// exits 42 if the nested program returned 0.
const SYSTEST_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/systest");

/// The guest program, as 68000 machine code:
///
/// ```text
/// move.l #$FFFE,$BFFF00     ; push a long (printed by %d as -2)
/// lea    fmt(pc),a0
/// move.l a0,$BFFF04         ; print "n=%d\n"
/// moveq  #0,d0
/// rts
/// fmt:   dc.b "n=%d",10,0
/// ```
fn program_bytes() -> Vec<u8> {
    let words: [u16; 12] = [
        0x23FC, 0x0000, 0xFFFE, 0x00BF, 0xFF00, // move.l #$FFFE,$BFFF00
        0x41FA, 0x000C, // lea fmt(pc),a0
        0x23C8, 0x00BF, 0xFF04, // move.l a0,$BFFF04
        0x7000, 0x4E75, // moveq #0,d0 ; rts
    ];
    let mut code: Vec<u8> = words.iter().flat_map(|w| w.to_be_bytes()).collect();
    assert_eq!(code.len(), 24);
    code.extend_from_slice(b"n=%d\n\0");
    hunk(code)
}

/// The uaelib program (`calltrap(86, "hi")`), whose exit code is the
/// call's result:
///
/// ```text
/// lea    msg(pc),a0
/// move.l a0,-(sp)
/// move.l #86,-(sp)
/// jsr    $F0FF60
/// addq.l #8,sp
/// rts                       ; D0 = calltrap's result = exit code
/// msg:   dc.b "hi",0
/// ```
fn uaelib_program_bytes() -> Vec<u8> {
    let words: [u16; 11] = [
        0x41FA, 0x0014, // lea msg(pc),a0
        0x2F08, // move.l a0,-(sp)
        0x2F3C, 0x0000, 0x0056, // move.l #86,-(sp)
        0x4EB9, 0x00F0, 0xFF60, // jsr $F0FF60
        0x508F, 0x4E75, // addq.l #8,sp ; rts
    ];
    let mut code: Vec<u8> = words.iter().flat_map(|w| w.to_be_bytes()).collect();
    assert_eq!(code.len(), 22);
    code.extend_from_slice(b"hi\0");
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
            std::env::temp_dir().join(format!("volamos-uaedebug-{tag}-{}", std::process::id()));
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

fn write_uaelib_program(dir: &TempDir, name: &str) -> PathBuf {
    let path = dir.path().join(name);
    fs::write(&path, uaelib_program_bytes()).unwrap();
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
fn the_port_prints_to_stderr_when_enabled() {
    let dir = TempDir::new("on");
    let prog = write_program(&dir, "prog");
    let out = run(&prog, &["--uae-debug"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    assert_eq!(stderr_of(&out), "n=-2\n");
    assert!(out.stdout.is_empty());
}

#[test]
fn nothing_happens_without_the_flag() {
    let dir = TempDir::new("off");
    let prog = write_program(&dir, "prog");
    let out = run(&prog, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    assert_eq!(stderr_of(&out), "");
}

#[test]
fn a_config_file_cannot_turn_it_on() {
    // The key isn't a config key at all, so a stale file naming it is
    // rejected rather than silently enabling the port.
    let dir = TempDir::new("config");
    let prog = write_program(&dir, "prog");
    fs::write(dir.path().join(".volamos"), "UAE_DEBUG=true\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_volamos"))
        .current_dir(dir.path())
        .arg(&prog)
        .output()
        .unwrap();
    assert_ne!(out.status.code(), Some(0));
    assert!(
        stderr_of(&out).contains("unknown key"),
        "{}",
        stderr_of(&out)
    );
    assert!(!stderr_of(&out).contains("n=-2"));
}

#[test]
fn it_works_across_cpu_models_ram_sizes_and_the_jit_flag() {
    let dir = TempDir::new("matrix");
    let prog = write_program(&dir, "prog");
    for flags in [
        &["--uae-debug", "--cpu", "68020"][..],
        &["--uae-debug", "--cpu", "68060", "--fpu"],
        &["--uae-debug", "--ram", "1M"],
        &["--uae-debug", "--ram", "32M", "--cpu", "68020"],
        // The JIT would bypass the port; the flag forces it off.
        &["--uae-debug", "--jit"],
        &["--uae-debug", "--jit", "--cpu", "68020"],
    ] {
        let out = run(&prog, flags);
        assert_eq!(out.status.code(), Some(0), "{flags:?}: {}", stderr_of(&out));
        // A --ram below 16M also gets the (separate) uaelib warning line.
        assert!(stderr_of(&out).ends_with("n=-2\n"), "{flags:?}");
    }
    // --clock-mhz adds its own exit report after the guest's text.
    let out = run(&prog, &["--uae-debug", "--clock-mhz", "25"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    assert!(stderr_of(&out).starts_with("n=-2\n"), "{}", stderr_of(&out));
}

#[test]
fn port_stores_are_not_sanitizer_violations() {
    let dir = TempDir::new("sanitize");
    let prog = write_program(&dir, "prog");
    let out = run(&prog, &["--uae-debug", "--sanitize"]);
    // 99 would mean a violation was reported.
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    assert_eq!(stderr_of(&out), "n=-2\n");
}

#[test]
fn a_stack_that_covers_the_port_is_a_clear_error() {
    let dir = TempDir::new("stack");
    let prog = write_program(&dir, "prog");
    let out = run(&prog, &["--uae-debug", "--stack", "8M"]);
    assert_ne!(out.status.code(), Some(0));
    assert!(
        stderr_of(&out).contains("UAE debug port"),
        "{}",
        stderr_of(&out)
    );
}

#[test]
fn nested_system_runs_inherit_the_port() {
    // systest System()s "TEST:echoargs"; make that file our program.
    let dir = TempDir::new("nested");
    write_program(&dir, "echoargs");
    for flags in [&["--uae-debug"][..], &["--uae-debug", "--jit"]] {
        let out = Command::new(env!("CARGO_BIN_EXE_volamos"))
            .args(flags)
            .arg("-V")
            .arg(format!("TEST:{}", dir.path().display()))
            .arg(SYSTEST_PATH)
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(42),
            "{flags:?}: {}",
            stderr_of(&out)
        );
        assert_eq!(stderr_of(&out), "n=-2\n", "{flags:?}");
    }
    // Without the flag the nested program prints nothing either.
    let out = Command::new(env!("CARGO_BIN_EXE_volamos"))
        .arg("-V")
        .arg(format!("TEST:{}", dir.path().display()))
        .arg(SYSTEST_PATH)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(42));
    assert_eq!(stderr_of(&out), "");
}

// --- uaelib debug-log trap at 0xF0FF60 (mode 86) ---

#[test]
fn the_uaelib_trap_logs_and_returns_1_on_either_cpu_and_with_the_jit_flag() {
    let dir = TempDir::new("uaelib");
    let prog = write_uaelib_program(&dir, "prog");
    for flags in [
        &["--uae-debug"][..],
        &["--uae-debug", "--cpu", "68020"],
        &["--uae-debug", "--cpu", "68010"],
        &["--uae-debug", "--jit", "--cpu", "68020"],
        &["--uae-debug", "--sanitize"],
        &["--uae-debug", "--emu68-debug", "--cpu", "68020"],
        &["--uae-debug", "--ram", "32M", "--cpu", "68020"],
    ] {
        let out = run(&prog, flags);
        // The exit code is calltrap's return value.
        assert_eq!(out.status.code(), Some(1), "{flags:?}: {}", stderr_of(&out));
        assert_eq!(stderr_of(&out), "DBG: hi\n", "{flags:?}");
    }
}

#[test]
fn a_stack_that_covers_the_uaelib_trap_is_a_clear_error() {
    // 16M - 1M = 0xF00000 < 0xF0FF60, but the sprintf port (0xBFFF00) is
    // still clear of it, so this isolates the uaelib check.
    let dir = TempDir::new("uaelib-stack");
    let prog = write_uaelib_program(&dir, "prog");
    let out = run(&prog, &["--uae-debug", "--stack", "1M"]);
    assert!(
        stderr_of(&out).contains("debug-log trap") && !stderr_of(&out).contains("DBG:"),
        "{}",
        stderr_of(&out)
    );
}

#[test]
fn the_uaelib_trap_is_absent_without_the_flag() {
    let dir = TempDir::new("uaelib-off");
    let prog = write_uaelib_program(&dir, "prog");
    let out = run(&prog, &["--cpu", "68020"]);
    // Whatever the zeroed bytes at 0xF0FF60 do, they do not log.
    assert!(!stderr_of(&out).contains("DBG:"), "{}", stderr_of(&out));
}

#[test]
fn a_small_ram_warns_that_the_trap_is_inactive_but_keeps_the_sprintf_port() {
    let dir = TempDir::new("uaelib-ram");
    let uaelib = write_uaelib_program(&dir, "uaelib");
    let out = run(&uaelib, &["--uae-debug", "--ram", "1M"]);
    let err = stderr_of(&out);
    assert!(
        err.contains("debug-log trap") && err.contains("inactive"),
        "{err}"
    );
    assert!(!err.contains("DBG:"), "{err}");
    // The sprintf port needs no RAM; the warning is printed exactly once.
    let sprintf = write_program(&dir, "sprintf");
    let out = run(&sprintf, &["--uae-debug", "--ram", "1M"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    assert!(stderr_of(&out).contains("n=-2\n"));
    assert_eq!(stderr_of(&out).matches("inactive").count(), 1);
    // Enough RAM: no warning.
    let out = run(&uaelib, &["--uae-debug"]);
    assert!(!stderr_of(&out).contains("inactive"));
}

#[test]
fn nested_system_runs_inherit_the_uaelib_trap() {
    let dir = TempDir::new("uaelib-nested");
    write_uaelib_program(&dir, "echoargs");
    let out = Command::new(env!("CARGO_BIN_EXE_volamos"))
        .arg("--uae-debug")
        .arg("-V")
        .arg(format!("TEST:{}", dir.path().display()))
        .arg(SYSTEST_PATH)
        .output()
        .unwrap();
    assert!(stderr_of(&out).contains("DBG: hi\n"), "{}", stderr_of(&out));
}
