//! End-to-end tests for `--term-escapes`/`--term-charset` and their
//! `.volamos` keys (issues #426/#427), driven through
//! `fixtures/termtest`, which writes single-byte CSI sequences, an
//! Amiga-only one, `SO`/`SI` and Latin-1 text in two `Write` calls that
//! split a sequence -- see `fixtures/gen_termtest.py`.
//!
//! `cargo test` has no terminal on stdout, so `auto` (the default) must
//! leave the bytes alone, and `on` is what exercises the translation.
//! Each test runs in its own temp directory, used as `$HOME`, the
//! working directory and the program's directory alike, so no
//! developer's own `.volamos` can leak in.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

const TERMTEST_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/termtest");
const ECHOARGS_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/echoargs");
const RAWTEST_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/rawtest");

/// What `termtest` writes, untouched.
const RAW: &[u8] = b"ab\x9bKcd\x9b1;32m caf\xe9 \x0ei\x0f\x9b0 p\n";

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("volamos-term-e2e-{tag}-{pid}-{n}"));
        fs::create_dir_all(&path).expect("create temp dir");
        for fixture in [TERMTEST_PATH, ECHOARGS_PATH, RAWTEST_PATH] {
            let name = Path::new(fixture).file_name().unwrap();
            fs::copy(fixture, path.join(name)).expect("copy fixture");
        }
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

/// Runs `termtest` with `flags` before the program, in `dir`, on a
/// UTF-8 locale.
fn run(dir: &TempDir, flags: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_volamos"))
        .env("HOME", dir.path())
        .env("LC_ALL", "en_US.UTF-8")
        .current_dir(dir.path())
        .args(flags)
        .arg("termtest")
        .output()
        .expect("failed to run volamos")
}

/// `termtest`'s stdout, after checking it exited 0 (both `Write`s
/// reported their full Amiga length).
fn stdout_of(output: Output) -> Vec<u8> {
    assert!(
        output.status.success(),
        "exit {:?}, stderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[test]
fn output_to_a_pipe_stays_byte_exact_by_default() {
    let dir = TempDir::new("auto");
    assert_eq!(stdout_of(run(&dir, &[])), RAW);
}

#[test]
fn term_escapes_on_translates_control_sequences_only() {
    let dir = TempDir::new("escapes");
    let out = stdout_of(run(&dir, &["--term-escapes", "on"]));
    // The split SGR is reassembled; SO-shifted 'i' is Latin-1 0xE9; the
    // Amiga cursor-off becomes the host's.
    assert_eq!(out, b"ab\x1b[Kcd\x1b[1;32m caf\xe9 \xe9\x1b[?25l\n");
}

#[test]
fn term_charset_on_converts_latin1_only() {
    let dir = TempDir::new("charset");
    let out = stdout_of(run(&dir, &["--term-charset", "on"]));
    assert_eq!(
        out,
        "ab\u{9b}Kcd\u{9b}1;32m café \u{e}i\u{f}\u{9b}0 p\n"
            .chars()
            .map(|c| match c {
                // C1 bytes are left as the raw single byte, not encoded.
                '\u{80}'..='\u{9f}' => vec![c as u8],
                _ => c.to_string().into_bytes(),
            })
            .collect::<Vec<_>>()
            .concat()
    );
}

#[test]
fn both_on_give_terminal_ready_utf8() {
    let dir = TempDir::new("both");
    let out = stdout_of(run(&dir, &["--term-escapes", "on", "--term-charset", "on"]));
    assert_eq!(out, "ab\x1b[Kcd\x1b[1;32m café é\x1b[?25l\n".as_bytes());
}

#[test]
fn dot_volamos_keys_set_them_and_flags_override() {
    let dir = TempDir::new("config");
    fs::write(
        dir.path().join(".volamos"),
        "TERM_ESCAPES=on\nTERM_CHARSET=on\n",
    )
    .unwrap();
    assert_eq!(
        stdout_of(run(&dir, &[])),
        "ab\x1b[Kcd\x1b[1;32m café é\x1b[?25l\n".as_bytes()
    );
    assert_eq!(
        stdout_of(run(
            &dir,
            &["--term-escapes", "off", "--term-charset", "off"]
        )),
        RAW
    );
}

#[test]
fn a_bad_setting_is_a_usage_error() {
    let dir = TempDir::new("bad");
    for flag in ["--term-escapes", "--term-charset"] {
        let output = run(&dir, &[flag, "yes"]);
        assert!(!output.status.success(), "{flag} yes should be rejected");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("auto, on or off"), "{stderr}");
    }
}

/// Runs fixture `program` with `flags` before it and `args` after, on a
/// UTF-8 locale, with `stdin` piped in.
fn run_program(
    dir: &TempDir,
    flags: &[&str],
    program: &str,
    args: &[&std::ffi::OsStr],
    stdin: &[u8],
) -> Output {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_volamos"))
        .env("HOME", dir.path())
        .env("LC_ALL", "en_US.UTF-8")
        .current_dir(dir.path())
        .args(flags)
        .arg(program)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to run volamos");
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().expect("wait for volamos")
}

#[test]
fn utf8_arguments_reach_the_guest_as_latin1() {
    let dir = TempDir::new("args");
    let arg = std::ffi::OsStr::new("café €");
    let out = stdout_of(run_program(&dir, &[], "echoargs", &[arg], b""));
    // Quoted for the space; the euro sign has no Latin-1 byte.
    assert_eq!(out, b"\"caf\xe9 ?\" \n");
    let out = stdout_of(run_program(
        &dir,
        &["--term-charset", "off"],
        "echoargs",
        &[arg],
        b"",
    ));
    assert_eq!(out, "\"café €\" \n".as_bytes());
}

#[cfg(unix)]
#[test]
fn an_argument_that_is_not_utf8_passes_through_unchanged() {
    // Issue #449: this used to panic in std::env::args.
    use std::os::unix::ffi::OsStrExt;
    let dir = TempDir::new("latin1-arg");
    let arg = std::ffi::OsStr::from_bytes(b"caf\xe9");
    let out = stdout_of(run_program(&dir, &[], "echoargs", &[arg], b""));
    assert_eq!(out, b"caf\xe9 \n");
}

#[cfg(unix)]
#[test]
fn a_volamos_argument_that_is_not_utf8_is_a_usage_error() {
    use std::os::unix::ffi::OsStrExt;
    let dir = TempDir::new("bad-own-arg");
    let output = Command::new(env!("CARGO_BIN_EXE_volamos"))
        .env("HOME", dir.path())
        .current_dir(dir.path())
        .arg(std::ffi::OsStr::from_bytes(b"caf\xe9"))
        .output()
        .expect("failed to run volamos");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not valid UTF-8"), "{stderr}");
}

#[test]
fn piped_utf8_input_is_converted_only_when_charset_is_on() {
    // rawtest exits with the first byte it reads. A pipe isn't a
    // terminal, so `auto` leaves the input alone.
    let dir = TempDir::new("stdin");
    let code = |flags: &[&str]| {
        run_program(&dir, flags, "rawtest", &[], "é".as_bytes())
            .status
            .code()
    };
    assert_eq!(code(&[]), Some(0xC3));
    assert_eq!(code(&["--term-charset", "on"]), Some(0xE9));
}

#[test]
fn a_bad_key_setting_is_rejected() {
    let dir = TempDir::new("bad-key");
    let output = run(&dir, &["--key", "F12=NOPE"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown Amiga key"), "{stderr}");
    fs::write(dir.path().join(".volamos"), "KEY=Nope=HELP\n").unwrap();
    let output = run(&dir, &[]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown host key"), "{stderr}");
}
