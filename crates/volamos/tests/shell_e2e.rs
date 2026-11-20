//! `--shell` (issue #20): the real AmigaDOS Shell running under volamos.
//!
//! No Shell is shipped with volamos: it is a module of a Kickstart ROM,
//! which `tools/extract_rom_shell.py` cuts out of a ROM image of your own
//! as a hunk executable. These tests need that file, so they skip --
//! passing, with a note on stderr -- unless `VOLAMOS_SHELL_SEG` names it:
//!
//! ```sh
//! python3 tools/extract_rom_shell.py kick314.rom /tmp/Shell-Seg
//! VOLAMOS_SHELL_SEG=/tmp/Shell-Seg cargo test -p volamos --test shell_e2e
//! ```
//!
//! They drive the Shell through its stdin, as a script would, and check
//! what it prints and the exit code (the last command's return code).
//!
//! The first two tests need nothing: `fixtures/minishell`
//! (`fixtures/gen_minishell.py`) stands in for the Shell, so CI covers how
//! `--shell` starts a Shell segment -- the entry, the startup packet and
//! the exit code -- without a ROM.

use std::io::Write;
use std::process::{Command, Stdio};

const MINISHELL_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/minishell");

#[test]
fn a_shell_segment_is_started_at_its_call_in_with_a_valid_startup_packet() {
    // minishell checks the packet (dp_Type non-zero, dp_Res1 = dp_Res2 = 0)
    // and replies it with 7, or 99 if it was wrong: the Shell's reply code
    // is the exit code.
    let status = Command::new(env!("CARGO_BIN_EXE_volamos"))
        .args(["--shell", "--cpu", "68020", MINISHELL_PATH])
        .stdin(Stdio::null())
        .status()
        .expect("failed to spawn the volamos binary");
    assert_eq!(status.code(), Some(7));
}

#[test]
fn shell_and_workbench_start_modes_cannot_be_combined() {
    let output = Command::new(env!("CARGO_BIN_EXE_volamos"))
        .args(["--shell", "--workbench", MINISHELL_PATH])
        .output()
        .expect("failed to spawn the volamos binary");
    assert_eq!(output.status.code(), Some(2), "a usage error");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("cannot be used with"),
        "clap names the conflict: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The extracted Shell, if the environment names one that exists.
fn shell_seg() -> Option<String> {
    let path = std::env::var("VOLAMOS_SHELL_SEG").ok()?;
    std::path::Path::new(&path).is_file().then_some(path)
}

/// Runs `volamos --shell` on the extracted Shell with `input` on stdin;
/// returns its stdout and exit code, or `None` to skip.
fn run_shell(input: &str) -> Option<(String, i32)> {
    let Some(seg) = shell_seg() else {
        eprintln!("skipped: VOLAMOS_SHELL_SEG is not set to an extracted Shell-Seg");
        return None;
    };
    let mut child = Command::new(env!("CARGO_BIN_EXE_volamos"))
        .args(["--shell", "--cpu", "68020", &seg])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn the volamos binary");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write the commands");
    let output = child.wait_with_output().expect("wait");
    Some((
        String::from_utf8_lossy(&output.stdout).into_owned(),
        output.status.code().unwrap_or(-1),
    ))
}

#[test]
fn the_shell_runs_its_built_in_commands_and_ends_at_eof() {
    let Some((out, code)) = run_shell("echo hello there\nstack\n") else {
        return;
    };
    assert!(out.contains("hello there"), "Echo ran: {out:?}");
    assert!(
        out.contains("Current stack size is"),
        "Stack answered with the Shell's own message: {out:?}"
    );
    assert!(
        out.contains("Process 0 ending"),
        "EOF ends the Shell: {out:?}"
    );
    assert_eq!(code, 0, "no command failed");
}

#[test]
fn an_unknown_command_is_reported_by_the_shell() {
    let Some((out, _)) = run_shell("nosuchcommand\n") else {
        return;
    };
    assert!(
        out.contains("nosuchcommand: Unknown command"),
        "the Shell's own message, from Fault's table: {out:?}"
    );
}

#[test]
fn the_prompt_can_be_changed_and_redirection_works() {
    let Some((out, _)) =
        run_shell("prompt \"vol> \"\necho redirected >RAM:t.txt\ntype RAM:t.txt\n")
    else {
        return;
    };
    assert!(out.contains("vol> "), "the new prompt is used: {out:?}");
    assert!(
        out.contains("redirected"),
        "Echo's output went to a file and Type read it back: {out:?}"
    );
}

#[test]
fn the_exit_code_is_the_last_commands_return_code() {
    // `Type` of a missing file ends with RETURN_ERROR (10), which the
    // Shell replies its startup packet with as it ends.
    let Some((_, code)) = run_shell("type RAM:nothere.txt\n") else {
        return;
    };
    assert_eq!(code, 10);
}
