//! End-to-end tests of `--workbench` (issue #345) against the real
//! binary, using `fixtures/wbtest`: it takes the Workbench path of the C
//! runtimes' startup code by hand (no `pr_CLI`, `WaitPort` for the
//! `WBStartup` message, reply to it) and returns `sm_NumArgs`.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

const WBTEST: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/wbtest");

/// A scratch volume holding a copy of `wbtest` and a project file.
struct Volume(PathBuf);

impl Volume {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("volamos-wb-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::copy(WBTEST, dir.join("wbtest")).unwrap();
        fs::write(dir.join("notes.txt"), b"").unwrap();
        Volume(dir)
    }

    fn run(&self, flags: &[&str], args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_volamos"));
        cmd.arg("-V").arg(format!("SYS:{}", self.0.display()));
        cmd.args(flags).arg(self.0.join("wbtest"));
        for a in args {
            cmd.arg(self.0.join(a));
        }
        cmd.output().expect("failed to run the volamos binary")
    }
}

impl Drop for Volume {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn workbench_start_posts_a_wbstartup_message_with_the_tool_and_project() {
    let vol = Volume::new("project");
    let out = vol.run(&["--workbench"], &["notes.txt"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "wbtest\nnotes.txt\n");
    assert_eq!(out.status.code(), Some(2), "sm_NumArgs is the return code");
    assert!(
        out.stderr.is_empty(),
        "a program that replied must not be reported: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn workbench_start_without_projects_has_only_the_tool_entry() {
    let vol = Volume::new("tool-only");
    let out = vol.run(&["--workbench"], &[]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "wbtest\n");
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn shell_start_still_has_a_cli() {
    // wbtest exits 99 when pr_CLI is non-zero: the default start is
    // unchanged by --workbench existing.
    let vol = Volume::new("shell");
    let out = vol.run(&[], &[]);
    assert_eq!(out.status.code(), Some(99));
}

#[test]
fn workbench_project_outside_every_volume_gets_a_volume_of_its_own() {
    // The project's directory is under no `-V`, so volamos mounts it.
    let vol = Volume::new("outside");
    let elsewhere = Volume::new("elsewhere");
    let out = Command::new(env!("CARGO_BIN_EXE_volamos"))
        .arg("-V")
        .arg(format!("SYS:{}", vol.0.display()))
        .arg("--workbench")
        .arg(vol.0.join("wbtest"))
        .arg(elsewhere.0.join("notes.txt"))
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "wbtest\nnotes.txt\n",
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn workbench_works_as_typed_with_no_volume_configured() {
    // No -V at all (and no defaults): the program's own directory and the
    // project's become volumes, so the command line needs nothing extra.
    let vol = Volume::new("asis");
    let out = Command::new(env!("CARGO_BIN_EXE_volamos"))
        .arg("--no-defaults")
        .current_dir(&vol.0)
        .arg("--workbench")
        .arg("wbtest")
        .arg("notes.txt")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "wbtest\nnotes.txt\n",
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn workbench_project_that_does_not_exist_is_a_clean_error() {
    let vol = Volume::new("missing");
    let out = vol.run(&["--workbench"], &["typo.txt"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no such file"), "stderr was: {stderr}");
    assert_ne!(out.status.code(), Some(0));
}
