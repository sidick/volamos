//! End-to-end test for `console.device`'s library unit (issue #425):
//! `fixtures/rawkeytest` opens unit -1 without a window, as the Amiga
//! Installer does, and converts keys with `RawKeyConvert` through the
//! `io_Device` it got back. See `fixtures/gen_rawkeytest.py`.

use std::process::Command;

const RAWKEYTEST_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/rawkeytest");

#[test]
fn rawkeytest_converts_keys_through_console_device_unit_minus_one() {
    let output = Command::new(env!("CARGO_BIN_EXE_volamos"))
        .arg(RAWKEYTEST_PATH)
        .output()
        .expect("failed to run volamos");
    // 20: OpenDevice failed; 10: a key didn't give one byte; 11: F1 into
    // two bytes didn't return -1.
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "Hi!\n");
}
