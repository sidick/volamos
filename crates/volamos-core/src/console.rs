//! `console.device`, library unit only: `OpenDevice("console.device", -1)`
//! and `RawKeyConvert` (issue #425).
//!
//! # Why only unit -1
//!
//! Programs such as the Amiga Installer open `console.device` with unit
//! `-1` (`CONU_LIBRARY`) and no window, purely to get the device base
//! and call `RawKeyConvert` through it: "Used to get a pointer to the
//! device library vector which is returned in the io_Device field of
//! the IOStdReq block. No actual console is opened." (NDK 3.2
//! `console.doc`, `OpenDevice`). That part needs no display. The window
//! units (`0`, `1`, `3`) bind a console to an Intuition window and draw
//! into it, which a runtime with no display can't do, so they still fail
//! to open with `IOERR_OPENFAIL`, like every other device volamos can't
//! back (see [`crate::exectask`]'s `OpenDevice`).
//!
//! # `RawKeyConvert` (LVO -48)
//!
//! `actual = RawKeyConvert(event, buffer, length, keyMap)`, `A0`/`A1`/
//! `D1`/`A2`: converts an `IECLASS_RAWKEY` input event to the bytes the
//! keymap gives it, or returns `-1` "if a buffer overflow was about to
//! occur" (`console.doc`). Everything else here was measured, because the
//! autodoc says little more: `tools/rawkeyprobe/` on real
//! Kickstart 3.1 (40.63, A600, 68000) under Copperline, with the ROM's
//! default (usa) keymap. Its output is committed as
//! `tests/data/console-rawkeyconvert-ks31.txt`, and this module's tests
//! check [`KEYS`] against every row of it. What it showed:
//!
//! - A key-up event (`ie_Code` with `IECODE_UP_PREFIX`), any `ie_Code`
//!   above `$FF`, or an event of any class other than `IECLASS_RAWKEY`
//!   gives `0` bytes. `ie_SubClass` is ignored.
//! - Capacity is checked before anything is written: `F1` (`CSI 0 ~`,
//!   three bytes) into a one- or two-byte buffer returns `-1` with the
//!   buffer untouched.
//! - Caps Lock acts as Shift for the letter keys only (`km_LoCapsable`,
//!   NDK `libraries/keymap.h`); Shift+Caps still gives upper case.
//! - Ctrl is per key, not a fixed bit mask: Ctrl+`6` is `$1E` (from `^`),
//!   Ctrl+`` ` `` is `$00`, and Ctrl+`1` is still `1`.
//! - With `IEQUALIFIER_REPEAT`, a key that isn't repeatable
//!   (`km_LoRepeatable`: Return, Enter, Esc, Help, some function keys)
//!   gives nothing; a repeatable one gives what it would without it.
//! - Right Shift acts as Left Shift and Right Alt as Left Alt; the Amiga
//!   keys and the numeric-pad qualifier change nothing.
//! - Alt+`F`/`G`/`H`/`J`/`K` are dead keys: they give nothing themselves,
//!   and when the event's `ie_Prev1DownCode`/`ie_Prev1DownQual` name one
//!   (with either Alt), the next vowel, `y`, `n` or space comes out
//!   accented in Latin-1 (`'` then `e` is `$E9`, `é`). Every other key is
//!   unchanged, and `ie_Prev2DownCode`/`ie_Prev2DownQual` don't matter on
//!   this keymap: a dead key there alone does nothing, and two acute dead
//!   keys in a row still give one `é`.
//!
//! [`KEYS`] holds, per raw code, what the probe recorded for no
//! qualifier, Shift, Ctrl, Shift+Ctrl, Alt and Alt+Shift, plus whether
//! Caps Lock and key repeat apply; [`convert`] picks the column.
//! [`DEAD_KEYS`] holds what each dead key does to the keys it changes,
//! with and without Shift.
//!
//! # What fails the run instead
//!
//! Each of these stops the run with a [`DispatchError::HandlerFailed`]
//! naming the call, rather than returning bytes volamos can't vouch for:
//!
//! - **An event chain** (`ie_NextEvent` non-`NULL`). `console.doc` doesn't
//!   say whether the list is walked (`keymap.library`'s `MapRawKey` says
//!   it is not). Real Kickstart 3.1 handles chains erratically: every
//!   shape tried returned `-1`, some after writing one event's bytes, and
//!   a key event followed by an `IECLASS_NULL` event with code `0` hung
//!   the machine. No program can depend on that, so this fails loudly
//!   rather than imitate it.
//! - **A keymap other than the default** (`keyMap` non-`NULL`).
//! - **A dead key followed by a key it changes, with any qualifier but
//!   Shift** (Caps Lock, Ctrl, Alt, repeat...), which the probe didn't
//!   measure.
//! - **Alt together with Ctrl or Caps Lock**, likewise.
//!
//! `CDInputHandler` (LVO -42) feeds input to console *windows*, so it
//! has nothing to act on here and fails as well, as does `DoIO`/`SendIO`
//! of any command on the unit -1 request (see `crate::exectask`).

use crate::cpu::{AddressRegister, Cpu, DataRegister};
use crate::dispatch::{CONSOLE_DEVICE_BASE, DispatchError, HandlerContext, LibraryTable};
use crate::memory::AddressSpace;

/// The device's name, for `OpenDevice` and error reports.
pub(crate) const CONSOLE_NAME: &str = "console.device";

/// `_LVOCDInputHandler` (NDK `console_lib.i`).
const LVO_CD_INPUT_HANDLER: i32 = -42;
/// `_LVORawKeyConvert` (NDK `console_lib.i`).
const LVO_RAW_KEY_CONVERT: i32 = -48;

/// `CONU_LIBRARY` (NDK `devices/conunit.h`), as `OpenDevice`'s `D0`.
/// Real Kickstart stores it in `io_Unit` unchanged (`$FFFFFFFF`).
pub(crate) const CONU_LIBRARY: u32 = -1i32 as u32;

// `struct InputEvent` (NDK `devices/inputevent.h`).
const IE_NEXT_EVENT: u32 = 0;
const IE_CLASS: u32 = 4;
const IE_CODE: u32 = 6;
const IE_QUALIFIER: u32 = 8;
/// `ie_Prev1DownCode` and `ie_Prev1DownQual`, the first two of the
/// `ie_position` union's dead-key bytes.
const IE_PREV1_DOWN_CODE: u32 = 10;
const IE_PREV1_DOWN_QUAL: u32 = 11;

const IECLASS_RAWKEY: u8 = 0x01;
const IEQUALIFIER_SHIFT: u16 = 0x0001 | 0x0002;
const IEQUALIFIER_CAPSLOCK: u16 = 0x0004;
const IEQUALIFIER_CONTROL: u16 = 0x0008;
const IEQUALIFIER_ALT: u16 = 0x0010 | 0x0020;
const IEQUALIFIER_REPEAT: u16 = 0x0200;

/// One raw key in the default keymap: its bytes under each measured
/// qualifier set, and whether Caps Lock and key repeat apply to it.
struct Key {
    plain: &'static [u8],
    shift: &'static [u8],
    ctrl: &'static [u8],
    shift_ctrl: &'static [u8],
    alt: &'static [u8],
    alt_shift: &'static [u8],
    capsable: bool,
    repeatable: bool,
}

#[allow(clippy::too_many_arguments)] // one per table column
const fn key(
    plain: &'static [u8],
    shift: &'static [u8],
    ctrl: &'static [u8],
    shift_ctrl: &'static [u8],
    alt: &'static [u8],
    alt_shift: &'static [u8],
    capsable: bool,
    repeatable: bool,
) -> Key {
    Key {
        plain,
        shift,
        ctrl,
        shift_ctrl,
        alt,
        alt_shift,
        capsable,
        repeatable,
    }
}

/// A raw code that produces nothing under any qualifier.
const NONE: Key = key(b"", b"", b"", b"", b"", b"", false, true);

/// The default keymap, indexed by raw key code, as measured on Kickstart
/// 3.1 (see the module docs). Columns: plain, Shift, Ctrl, Shift+Ctrl,
/// Alt, Alt+Shift, capsable, repeatable.
#[rustfmt::skip]
const KEYS: [Key; 0x80] = [
    key(b"`", b"~", b"\x00", b"\x1e", b"`", b"~", false, true), // 0x00
    key(b"1", b"!", b"1", b"!", b"\xb9", b"!", false, true), // 0x01
    key(b"2", b"@", b"\x00", b"\x00", b"\xb2", b"@", false, true), // 0x02
    key(b"3", b"#", b"3", b"#", b"\xb3", b"#", false, true), // 0x03
    key(b"4", b"$", b"4", b"$", b"\xa2", b"$", false, true), // 0x04
    key(b"5", b"%", b"5", b"%", b"\xbc", b"%", false, true), // 0x05
    key(b"6", b"^", b"\x1e", b"\x1e", b"\xbd", b"^", false, true), // 0x06
    key(b"7", b"&", b"7", b"&", b"\xbe", b"&", false, true), // 0x07
    key(b"8", b"*", b"8", b"*", b"\xb7", b"*", false, true), // 0x08
    key(b"9", b"(", b"9", b"(", b"\xab", b"(", false, true), // 0x09
    key(b"0", b")", b"0", b")", b"\xbb", b")", false, true), // 0x0a
    key(b"-", b"_", b"\x1f", b"\x1f", b"-", b"_", false, true), // 0x0b
    key(b"=", b"+", b"=", b"+", b"=", b"+", false, true), // 0x0c
    key(b"\x5c", b"|", b"\x1c", b"\x1c", b"\x5c", b"|", false, true), // 0x0d
    NONE, // 0x0e
    key(b"0", b"0", b"0", b"0", b"0", b"0", false, true), // 0x0f
    key(b"q", b"Q", b"\x11", b"\x11", b"\xe5", b"\xc5", true, true), // 0x10
    key(b"w", b"W", b"\x17", b"\x17", b"\xb0", b"\xb0", true, true), // 0x11
    key(b"e", b"E", b"\x05", b"\x05", b"\xa9", b"\xa9", true, true), // 0x12
    key(b"r", b"R", b"\x12", b"\x12", b"\xae", b"\xae", true, true), // 0x13
    key(b"t", b"T", b"\x14", b"\x14", b"\xfe", b"\xde", true, true), // 0x14
    key(b"y", b"Y", b"\x19", b"\x19", b"\xa4", b"\xa5", true, true), // 0x15
    key(b"u", b"U", b"\x15", b"\x15", b"\xb5", b"\xb5", true, true), // 0x16
    key(b"i", b"I", b"\x09", b"\x09", b"\xa1", b"\xa6", true, true), // 0x17
    key(b"o", b"O", b"\x0f", b"\x0f", b"\xf8", b"\xd8", true, true), // 0x18
    key(b"p", b"P", b"\x10", b"\x10", b"\xb6", b"\xb6", true, true), // 0x19
    key(b"[", b"{", b"\x1b", b"\x1b", b"[", b"{", false, true), // 0x1a
    key(b"]", b"}", b"\x1d", b"\x1d", b"]", b"}", false, true), // 0x1b
    NONE, // 0x1c
    key(b"1", b"1", b"1", b"1", b"1", b"1", false, true), // 0x1d
    key(b"2", b"2", b"2", b"2", b"2", b"2", false, true), // 0x1e
    key(b"3", b"3", b"3", b"3", b"3", b"3", false, true), // 0x1f
    key(b"a", b"A", b"\x01", b"\x01", b"\xe6", b"\xc6", true, true), // 0x20
    key(b"s", b"S", b"\x13", b"\x13", b"\xdf", b"\xa7", true, true), // 0x21
    key(b"d", b"D", b"\x04", b"\x04", b"\xf0", b"\xd0", true, true), // 0x22
    key(b"f", b"F", b"\x06", b"\x06", b"", b"", true, true), // 0x23
    key(b"g", b"G", b"\x07", b"\x07", b"", b"", true, true), // 0x24
    key(b"h", b"H", b"\x08", b"\x08", b"", b"", true, true), // 0x25
    key(b"j", b"J", b"\x0a", b"\x0a", b"", b"", true, true), // 0x26
    key(b"k", b"K", b"\x0b", b"\x0b", b"", b"", true, true), // 0x27
    key(b"l", b"L", b"\x0c", b"\x0c", b"\xa3", b"\xa3", true, true), // 0x28
    key(b";", b":", b";", b":", b";", b":", false, true), // 0x29
    key(b"'", b"\x22", b"'", b"\x22", b"'", b"\x22", false, true), // 0x2a
    NONE, // 0x2b
    NONE, // 0x2c
    key(b"4", b"4", b"4", b"4", b"4", b"4", false, true), // 0x2d
    key(b"5", b"5", b"5", b"5", b"5", b"5", false, true), // 0x2e
    key(b"6", b"6", b"6", b"6", b"6", b"6", false, true), // 0x2f
    key(b"<", b">", b"<", b">", b"\xab", b"\xbb", false, true), // 0x30
    key(b"z", b"Z", b"\x1a", b"\x1a", b"\xb1", b"\xac", true, true), // 0x31
    key(b"x", b"X", b"\x18", b"\x18", b"\xd7", b"\xf7", true, true), // 0x32
    key(b"c", b"C", b"\x03", b"\x03", b"\xe7", b"\xc7", true, true), // 0x33
    key(b"v", b"V", b"\x16", b"\x16", b"\xaa", b"\xaa", true, true), // 0x34
    key(b"b", b"B", b"\x02", b"\x02", b"\xba", b"\xba", true, true), // 0x35
    key(b"n", b"N", b"\x0e", b"\x0e", b"\xad", b"\xaf", true, true), // 0x36
    key(b"m", b"M", b"\x0d", b"\x0d", b"\xb8", b"\xbf", true, true), // 0x37
    key(b",", b"<", b",", b"<", b",", b"<", false, true), // 0x38
    key(b".", b">", b".", b">", b".", b">", false, true), // 0x39
    key(b"/", b"?", b"/", b"?", b"/", b"?", false, true), // 0x3a
    NONE, // 0x3b
    key(b".", b".", b".", b".", b".", b".", false, true), // 0x3c
    key(b"7", b"7", b"7", b"7", b"7", b"7", false, true), // 0x3d
    key(b"8", b"8", b"8", b"8", b"8", b"8", false, true), // 0x3e
    key(b"9", b"9", b"9", b"9", b"9", b"9", false, true), // 0x3f
    key(b" ", b" ", b" ", b" ", b"\xa0", b"\xa0", false, true), // 0x40
    key(b"\x08", b"\x08", b"\x08", b"\x08", b"\x08", b"\x08", false, true), // 0x41
    key(b"\x09", b"\x9bZ", b"\x09", b"\x9bZ", b"\x09", b"\x9bZ", false, true), // 0x42
    key(b"\x0d", b"\x0d", b"\x0d", b"\x0d", b"\x0d", b"\x0d", false, false), // 0x43
    key(b"\x0d", b"\x0d", b"\x0a", b"\x0a", b"\x0d", b"\x0d", false, false), // 0x44
    key(b"\x1b", b"\x1b", b"\x1b", b"\x1b", b"\x9b", b"\x9b", false, false), // 0x45
    key(b"\x7f", b"\x7f", b"\x7f", b"\x7f", b"\x7f", b"\x7f", false, true), // 0x46
    key(b"\x9b40~", b"\x9b50~", b"\x9b40~", b"\x9b50~", b"\x9b40~", b"\x9b50~", false, false), // 0x47
    key(b"\x9b41~", b"\x9b51~", b"\x9b41~", b"\x9b51~", b"\x9b41~", b"\x9b51~", false, true), // 0x48
    key(b"\x9b42~", b"\x9b52~", b"\x9b42~", b"\x9b52~", b"\x9b42~", b"\x9b52~", false, true), // 0x49
    key(b"-", b"-", b"-", b"-", b"-", b"-", false, true), // 0x4a
    key(b"\x9b20~", b"\x9b30~", b"\x9b20~", b"\x9b30~", b"\x9b20~", b"\x9b30~", false, true), // 0x4b
    key(b"\x9bA", b"\x9bT", b"\x9bA", b"\x9bT", b"\x9bA", b"\x9bT", false, true), // 0x4c
    key(b"\x9bB", b"\x9bS", b"\x9bB", b"\x9bS", b"\x9bB", b"\x9bS", false, true), // 0x4d
    key(b"\x9bC", b"\x9b @", b"\x9bC", b"\x9b @", b"\x9bC", b"\x9b @", false, true), // 0x4e
    key(b"\x9bD", b"\x9b A", b"\x9bD", b"\x9b A", b"\x9bD", b"\x9b A", false, true), // 0x4f
    key(b"\x9b0~", b"\x9b10~", b"\x9b0~", b"\x9b10~", b"\x9b0~", b"\x9b10~", false, true), // 0x50
    key(b"\x9b1~", b"\x9b11~", b"\x9b1~", b"\x9b11~", b"\x9b1~", b"\x9b11~", false, true), // 0x51
    key(b"\x9b2~", b"\x9b12~", b"\x9b2~", b"\x9b12~", b"\x9b2~", b"\x9b12~", false, true), // 0x52
    key(b"\x9b3~", b"\x9b13~", b"\x9b3~", b"\x9b13~", b"\x9b3~", b"\x9b13~", false, true), // 0x53
    key(b"\x9b4~", b"\x9b14~", b"\x9b4~", b"\x9b14~", b"\x9b4~", b"\x9b14~", false, true), // 0x54
    key(b"\x9b5~", b"\x9b15~", b"\x9b5~", b"\x9b15~", b"\x9b5~", b"\x9b15~", false, true), // 0x55
    key(b"\x9b6~", b"\x9b16~", b"\x9b6~", b"\x9b16~", b"\x9b6~", b"\x9b16~", false, true), // 0x56
    key(b"\x9b7~", b"\x9b17~", b"\x9b7~", b"\x9b17~", b"\x9b7~", b"\x9b17~", false, true), // 0x57
    key(b"\x9b8~", b"\x9b18~", b"\x9b8~", b"\x9b18~", b"\x9b8~", b"\x9b18~", false, true), // 0x58
    key(b"\x9b9~", b"\x9b19~", b"\x9b9~", b"\x9b19~", b"\x9b9~", b"\x9b19~", false, true), // 0x59
    key(b"(", b"(", b"(", b"(", b"(", b"(", false, true), // 0x5a
    key(b")", b")", b")", b")", b")", b")", false, true), // 0x5b
    key(b"/", b"/", b"/", b"/", b"/", b"/", false, true), // 0x5c
    key(b"*", b"*", b"*", b"*", b"*", b"*", false, true), // 0x5d
    key(b"+", b"+", b"+", b"+", b"+", b"+", false, true), // 0x5e
    key(b"\x9b?~", b"\x9b?~", b"\x9b?~", b"\x9b?~", b"\x9b?~", b"\x9b?~", false, false), // 0x5f
    NONE, // 0x60
    NONE, // 0x61
    NONE, // 0x62
    NONE, // 0x63
    NONE, // 0x64
    NONE, // 0x65
    NONE, // 0x66
    NONE, // 0x67
    NONE, // 0x68
    NONE, // 0x69
    NONE, // 0x6a
    NONE, // 0x6b
    NONE, // 0x6c
    NONE, // 0x6d
    key(b"\x9b43~", b"\x9b53~", b"\x9b43~", b"\x9b53~", b"\x9b43~", b"\x9b53~", false, false), // 0x6e
    key(b"\x9b21~", b"\x9b31~", b"\x9b21~", b"\x9b31~", b"\x9b21~", b"\x9b31~", false, true), // 0x6f
    key(b"\x9b44~", b"\x9b54~", b"\x9b44~", b"\x9b54~", b"\x9b44~", b"\x9b54~", false, false), // 0x70
    key(b"\x9b45~", b"\x9b55~", b"\x9b45~", b"\x9b55~", b"\x9b45~", b"\x9b55~", false, false), // 0x71
    NONE, // 0x72
    NONE, // 0x73
    NONE, // 0x74
    NONE, // 0x75
    NONE, // 0x76
    NONE, // 0x77
    NONE, // 0x78
    NONE, // 0x79
    NONE, // 0x7a
    NONE, // 0x7b
    NONE, // 0x7c
    NONE, // 0x7d
    NONE, // 0x7e
    NONE, // 0x7f
];

/// A key a dead key changes: its raw code, then what it becomes without
/// and with Shift.
type Composed = (u8, &'static [u8], &'static [u8]);

/// The default keymap's dead keys (Alt with the raw code in the first
/// field: `F` acute, `G` grave, `H` circumflex, `J` tilde, `K`
/// diaeresis) and, for each key one changes, what it becomes without and
/// with Shift, as measured on Kickstart 3.1. Keys not listed are
/// unchanged by a dead key.
#[rustfmt::skip]
const DEAD_KEYS: [(u8, &[Composed]); 5] = [
    (0x23, &[(0x12, b"\xe9", b"\xc9"), (0x15, b"\xfd", b"\xdd"), (0x16, b"\xfa", b"\xda"), (0x17, b"\xed", b"\xcd"), (0x18, b"\xf3", b"\xd3"), (0x20, b"\xe1", b"\xc1"), (0x40, b"\xb4", b"\xb4")]),
    (0x24, &[(0x12, b"\xe8", b"\xc8"), (0x16, b"\xf9", b"\xd9"), (0x17, b"\xec", b"\xcc"), (0x18, b"\xf2", b"\xd2"), (0x20, b"\xe0", b"\xc0"), (0x40, b"`", b"`")]),
    (0x25, &[(0x12, b"\xea", b"\xca"), (0x16, b"\xfb", b"\xdb"), (0x17, b"\xee", b"\xce"), (0x18, b"\xf4", b"\xd4"), (0x20, b"\xe2", b"\xc2"), (0x40, b"^", b"^")]),
    (0x26, &[(0x18, b"\xf5", b"\xd5"), (0x20, b"\xe3", b"\xc3"), (0x36, b"\xf1", b"\xd1"), (0x40, b"~", b"~")]),
    (0x27, &[(0x12, b"\xeb", b"\xcb"), (0x15, b"\xff", b"Y"), (0x16, b"\xfc", b"\xdc"), (0x17, b"\xef", b"\xcf"), (0x18, b"\xf6", b"\xd6"), (0x20, b"\xe4", b"\xc4"), (0x40, b"\xa8", b"\xa8")]),
];

/// The bytes `RawKeyConvert` gives a single event, or why volamos won't
/// convert it (see the module docs). `prev1` is the event's
/// `ie_Prev1DownCode` and `ie_Prev1DownQual`.
fn convert(class: u8, code: u16, qualifier: u16, prev1: (u8, u8)) -> Result<&'static [u8], String> {
    // Codes from IECODE_UP_PREFIX ($80) up are key-ups, or not keys at all.
    if class != IECLASS_RAWKEY || code >= 0x80 {
        return Ok(b"");
    }
    if let Some(composed) = compose(code as u8, qualifier, prev1)? {
        return Ok(composed);
    }
    let key = &KEYS[usize::from(code)];
    if qualifier & IEQUALIFIER_REPEAT != 0 && !key.repeatable {
        return Ok(b"");
    }
    let alt = qualifier & IEQUALIFIER_ALT != 0;
    if alt && qualifier & (IEQUALIFIER_CONTROL | IEQUALIFIER_CAPSLOCK) != 0 {
        return Err(format!(
            "qualifier {qualifier:#06x}: Alt with Ctrl or Caps Lock is not implemented"
        ));
    }
    let shifted = qualifier & IEQUALIFIER_SHIFT != 0
        || (qualifier & IEQUALIFIER_CAPSLOCK != 0 && key.capsable);
    Ok(match (alt, qualifier & IEQUALIFIER_CONTROL != 0, shifted) {
        (true, _, false) => key.alt,
        (true, _, true) => key.alt_shift,
        (false, true, false) => key.ctrl,
        (false, true, true) => key.shift_ctrl,
        (false, false, false) => key.plain,
        (false, false, true) => key.shift,
    })
}

/// What `code` becomes after the dead key in `prev1`, if that is a dead
/// key and `code` one of the keys it changes; `None` otherwise.
fn compose(
    code: u8,
    qualifier: u16,
    (prev_code, prev_qual): (u8, u8),
) -> Result<Option<&'static [u8]>, String> {
    if u16::from(prev_qual) & IEQUALIFIER_ALT == 0 {
        return Ok(None);
    }
    let Some((_, keys)) = DEAD_KEYS.iter().find(|(dead, _)| *dead == prev_code) else {
        return Ok(None);
    };
    let Some(&(_, plain, shifted)) = keys.iter().find(|(key, ..)| *key == code) else {
        return Ok(None);
    };
    if qualifier & !IEQUALIFIER_SHIFT != 0 {
        return Err(format!(
            "raw key {code:#04x} after dead key {prev_code:#04x} with qualifier \
             {qualifier:#06x}: only Shift is implemented with a dead key"
        ));
    }
    Ok(Some(if qualifier & IEQUALIFIER_SHIFT != 0 {
        shifted
    } else {
        plain
    }))
}

/// A failure of `handler`, as the call that hit it.
fn handler_failed(lvo: i32, handler: &str, message: String) -> DispatchError {
    DispatchError::HandlerFailed {
        library: CONSOLE_NAME.to_string(),
        lvo,
        handler_name: handler.to_string(),
        message,
    }
}

/// `RawKeyConvert` (`A0` = event, `A1` = buffer, `D1` = length, `A2` =
/// keymap). `D0` = the number of bytes written, or `-1` if they wouldn't
/// fit (nothing written then). See the module docs.
fn raw_key_convert_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let fail = |message: String| handler_failed(LVO_RAW_KEY_CONVERT, "RawKeyConvert", message);
    let event = ctx.cpu.address_register(AddressRegister(0));
    let buffer = ctx.cpu.address_register(AddressRegister(1));
    let length = ctx.cpu.data_register(DataRegister(1)) as i32;
    let keymap = ctx.cpu.address_register(AddressRegister(2));

    if keymap != 0 {
        return Err(fail(format!(
            "keyMap {keymap:#010x}: only the default keymap (NULL) is implemented"
        )));
    }
    if event == 0 {
        return Err(fail("event is NULL".to_string()));
    }
    let next = ctx.mem.read_u32(event + IE_NEXT_EVENT);
    if next != 0 {
        return Err(fail(format!(
            "event {event:#010x} has ie_NextEvent {next:#010x}: converting an event chain \
             is not implemented (real Kickstart 3.1 returns -1 for one, or hangs)"
        )));
    }
    let prev1 = (
        ctx.mem.read_u8(event + IE_PREV1_DOWN_CODE),
        ctx.mem.read_u8(event + IE_PREV1_DOWN_QUAL),
    );
    let bytes = convert(
        ctx.mem.read_u8(event + IE_CLASS),
        ctx.mem.read_u16(event + IE_CODE),
        ctx.mem.read_u16(event + IE_QUALIFIER),
        prev1,
    )
    .map_err(fail)?;

    let actual = if bytes.len() as i64 > i64::from(length) {
        -1
    } else {
        for (i, &byte) in bytes.iter().enumerate() {
            ctx.mem.write_u8(buffer + i as u32, byte);
        }
        bytes.len() as i32
    };
    ctx.cpu.set_data_register(DataRegister(0), actual as u32);
    Ok(())
}

/// `CDInputHandler` (`A0` = events, `A1` = device base): feeds input
/// events to console windows, which volamos doesn't have. Fails the run.
fn cd_input_handler<C: Cpu>(_ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    Err(handler_failed(
        LVO_CD_INPUT_HANDLER,
        "CDInputHandler",
        "console windows are not implemented, so there is nothing to feed input to".to_string(),
    ))
}

/// Registers `console.device`'s library vectors on
/// [`CONSOLE_DEVICE_BASE`]. Called from [`crate::dispatch::Runtime::new`].
pub fn register_console_handlers<C: Cpu + 'static>(
    table: &mut LibraryTable<C>,
    mem: &mut C::Memory,
) {
    table.register(
        mem,
        CONSOLE_DEVICE_BASE,
        LVO_CD_INPUT_HANDLER,
        CONSOLE_NAME,
        "CDInputHandler",
        cd_input_handler::<C>,
    );
    table.register(
        mem,
        CONSOLE_DEVICE_BASE,
        LVO_RAW_KEY_CONVERT,
        CONSOLE_NAME,
        "RawKeyConvert",
        raw_key_convert_handler::<C>,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{EXEC_LIBRARY_BASE, Runtime, RuntimeError, StartConfig};
    use crate::exectask::{IO_DEVICE_OFFSET, IO_ERROR_OFFSET, IO_UNIT_OFFSET};
    use crate::memory::FlatMemory;

    /// The probe's output on real Kickstart 3.1 (see the module docs).
    const KS31: &str = include_str!("../tests/data/console-rawkeyconvert-ks31.txt");

    /// Every row of the probe, by label: the bytes it wrote (only the
    /// first `actual`; some rows dump more), or `None` for `-1`.
    fn measured() -> std::collections::HashMap<&'static str, Option<Vec<u8>>> {
        KS31.lines()
            .filter(|line| !line.starts_with('#'))
            .filter_map(|line| {
                let mut fields = line.split(' ');
                let label = fields.next()?;
                let actual: i32 = fields.next()?.parse().ok()?;
                let bytes = fields.map(|b| u8::from_str_radix(b, 16).unwrap());
                let written = usize::try_from(actual)
                    .ok()
                    .map(|n| bytes.take(n).collect());
                Some((label, written))
            })
            .collect()
    }

    fn hex(field: &str) -> u16 {
        u16::from_str_radix(field, 16).unwrap()
    }

    #[test]
    fn every_measured_key_and_qualifier_converts_as_on_kickstart_3_1() {
        let mut checked = 0;
        for (label, want) in measured() {
            // kCC/qQQQQ, or dDD/kCC/qQQQQ after dead key DD.
            let fields: Vec<&str> = label.split('/').collect();
            let (prev1, code, qualifier) = match fields[..] {
                [k, q] if k.starts_with('k') && q.starts_with('q') => {
                    ((0, 0), hex(&k[1..]), hex(&q[1..]))
                }
                [d, k, q] if d.starts_with('d') => {
                    ((hex(&d[1..]) as u8, 0x10), hex(&k[1..]), hex(&q[1..]))
                }
                _ => continue,
            };
            let got = convert(IECLASS_RAWKEY, code, qualifier, prev1)
                .unwrap_or_else(|e| panic!("{label}: {e}"));
            assert_eq!(Some(got.to_vec()), want, "{label}");
            checked += 1;
        }
        // 20 qualifier sets and 5 dead keys x 2, each x 128 raw codes.
        assert_eq!(checked, 0x80 * 30, "the whole probe table");
    }

    #[test]
    fn dead_key_variants_compose_as_on_kickstart_3_1() {
        let rows = measured();
        let e = |label: &str, prev1| {
            let got = convert(IECLASS_RAWKEY, 0x12, 0, prev1).unwrap();
            assert_eq!(Some(got.to_vec()), rows[label], "{label}");
        };
        e("ralt-d23-e", (0x23, 0x20));
        e("shiftalt-d23-e", (0x23, 0x11));
        // Only ie_Prev1 counts: Prev2 alone, or both, act as Prev1 alone.
        e("prev2-d23-e", (0, 0));
        e("double-d23-e", (0x23, 0x10));
        e("alta-e", (0x20, 0x10));
    }

    #[test]
    fn key_up_high_byte_and_non_key_events_give_nothing() {
        let rows = measured();
        for code in [0x0120, 0x01a0, 0x2120, 0xff20] {
            let got = convert(IECLASS_RAWKEY, code, 0, (0, 0)).unwrap();
            assert_eq!(Some(got.to_vec()), rows[format!("hi{code:04x}").as_str()]);
        }
        assert_eq!(rows["up-a"], Some(vec![]));
        assert_eq!(
            convert(IECLASS_RAWKEY, 0x20 | 0x80, 0, (0, 0)),
            Ok(&b""[..])
        );
        assert_eq!(rows["null-a"], Some(vec![]));
        assert_eq!(convert(0x00, 0x20, 0, (0, 0)), Ok(&b""[..]));
    }

    #[test]
    fn a_dead_key_with_an_unmeasured_qualifier_fails() {
        // Alt+F, then Caps Lock+e: not measured.
        let err = convert(IECLASS_RAWKEY, 0x12, 0x0004, (0x23, 0x10)).unwrap_err();
        assert!(err.contains("dead key"), "{err}");
        // A key the dead key doesn't change converts normally whatever
        // the qualifier, as does any key after a non-dead Alt key.
        assert_eq!(
            convert(IECLASS_RAWKEY, 0x01, 0x0008, (0x23, 0x10)),
            Ok(&b"1"[..])
        );
        assert_eq!(
            convert(IECLASS_RAWKEY, 0x12, 0x0004, (0x20, 0x10)),
            Ok(&b"E"[..])
        );
    }

    #[test]
    fn alt_with_ctrl_or_caps_lock_fails() {
        for qualifier in [0x0010 | 0x0008, 0x0020 | 0x0004] {
            assert!(convert(IECLASS_RAWKEY, 0x20, qualifier, (0, 0)).is_err());
        }
    }

    // --- the device through real trap dispatch ---

    const NAME: u32 = 0x1_8000;
    const IOREQ: u32 = 0x1_8100;
    const EVENT: u32 = 0x1_8200;
    const NEXT_EVENT: u32 = 0x1_8240;
    const BUF: u32 = 0x1_8300;

    /// `move.l #imm32,An` / `move.l #imm32,Dn`, each with its operand.
    fn move_a(words: &mut Vec<u16>, n: u16, imm: u32) {
        words.extend([0x207C | (n << 9), (imm >> 16) as u16, imm as u16]);
    }
    fn move_d(words: &mut Vec<u16>, n: u16, imm: u32) {
        words.extend([0x203C | (n << 9), (imm >> 16) as u16, imm as u16]);
    }

    /// `OpenDevice("console.device", unit, IOREQ, 0)`, then, if
    /// `convert_len` is set, `RawKeyConvert(EVENT, BUF, len, keymap)`
    /// through the `io_Device` the open returned. Exits with the last
    /// call's `D0`.
    fn program(unit: u32, convert: Option<(u32, u32)>) -> Vec<u16> {
        let mut words = Vec::new();
        move_a(&mut words, 6, EXEC_LIBRARY_BASE);
        move_a(&mut words, 0, NAME);
        move_d(&mut words, 0, unit);
        move_a(&mut words, 1, IOREQ);
        move_d(&mut words, 1, 0);
        words.extend([0x4EAE, (-444i16) as u16]); // jsr OpenDevice(a6)
        if let Some((len, keymap)) = convert {
            // movea.l IOREQ+io_Device,a6
            let io_device = IOREQ + IO_DEVICE_OFFSET;
            words.extend([0x2C79, (io_device >> 16) as u16, io_device as u16]);
            move_a(&mut words, 0, EVENT);
            move_a(&mut words, 1, BUF);
            move_d(&mut words, 1, len);
            move_a(&mut words, 2, keymap);
            words.extend([0x4EAE, LVO_RAW_KEY_CONVERT as i16 as u16]);
        }
        words.push(0x4E75); // rts
        words
    }

    /// Runs `words` with `EVENT` set up as a key-down of `code` under
    /// `qualifier` and `BUF` filled with `0xEE`.
    fn run(
        words: &[u16],
        code: u16,
        qualifier: u16,
    ) -> (Result<i32, RuntimeError>, Runtime<M68kCpu>) {
        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        for (i, &w) in words.iter().enumerate() {
            mem.write_u16(entry + 2 * i as u32, w);
        }
        crate::guestmem::write_c_string(&mut mem, NAME, b"console.device");
        mem.write_u8(EVENT + IE_CLASS, IECLASS_RAWKEY);
        mem.write_u16(EVENT + IE_CODE, code);
        mem.write_u16(EVENT + IE_QUALIFIER, qualifier);
        for i in 0..8 {
            mem.write_u8(BUF + i, 0xEE);
        }
        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: entry + 0x400,
                ..StartConfig::default()
            },
        );
        let result = rt.run(&mut Vec::new(), None);
        (result, rt)
    }

    fn buf(rt: &Runtime<M68kCpu>, n: u32) -> Vec<u8> {
        (0..n).map(|i| rt.memory().read_u8(BUF + i)).collect()
    }

    #[test]
    fn opening_unit_minus_one_hands_out_the_device_base() {
        let (result, rt) = run(&program(CONU_LIBRARY, None), 0, 0);
        assert_eq!(result.unwrap(), 0);
        assert_eq!(
            rt.memory().read_u32(IOREQ + IO_DEVICE_OFFSET),
            CONSOLE_DEVICE_BASE
        );
        // As on real Kickstart 3.1: io_Unit holds the unit, -1.
        assert_eq!(rt.memory().read_u32(IOREQ + IO_UNIT_OFFSET), CONU_LIBRARY);
        assert_eq!(rt.memory().read_u8(IOREQ + IO_ERROR_OFFSET), 0);
    }

    #[test]
    fn a_window_unit_still_fails_to_open() {
        let (result, rt) = run(&program(0, None), 0, 0);
        assert_eq!(result.unwrap(), -1, "IOERR_OPENFAIL");
        assert_eq!(rt.memory().read_u8(IOREQ + IO_ERROR_OFFSET) as i8, -1);
    }

    #[test]
    fn raw_key_convert_writes_the_keys_bytes() {
        let (result, rt) = run(&program(CONU_LIBRARY, Some((64, 0))), 0x20, 0x0001);
        assert_eq!(result.unwrap(), 1);
        assert_eq!(buf(&rt, 2), b"A\xee");
        // F1: CSI 0 ~.
        let (result, rt) = run(&program(CONU_LIBRARY, Some((3, 0))), 0x50, 0);
        assert_eq!(result.unwrap(), 3);
        assert_eq!(buf(&rt, 4), b"\x9b0~\xee");
    }

    #[test]
    fn raw_key_convert_returns_minus_one_and_writes_nothing_when_it_would_overflow() {
        // As measured: F1 (three bytes) into a two-byte buffer, 'a' into none.
        let (result, rt) = run(&program(CONU_LIBRARY, Some((2, 0))), 0x50, 0);
        assert_eq!(result.unwrap(), -1);
        assert_eq!(buf(&rt, 3), [0xEE; 3]);
        let (result, _) = run(&program(CONU_LIBRARY, Some((0, 0))), 0x20, 0);
        assert_eq!(result.unwrap(), -1);
    }

    fn failure_message(result: Result<i32, RuntimeError>, handler: &str) -> String {
        match result {
            Err(RuntimeError::Dispatch(DispatchError::HandlerFailed {
                handler_name,
                message,
                ..
            })) if handler_name == handler => message,
            other => panic!("expected a loud {handler} failure, got {other:?}"),
        }
    }

    #[test]
    fn an_event_chain_fails_the_run() {
        let mut words = program(CONU_LIBRARY, Some((64, 0)));
        // Link EVENT to a second event before the call: insert
        // `move.l #NEXT_EVENT,EVENT` (0x23FC imm32 abs32) at the start.
        let link = [
            0x23FC,
            (NEXT_EVENT >> 16) as u16,
            NEXT_EVENT as u16,
            (EVENT >> 16) as u16,
            EVENT as u16,
        ];
        words.splice(0..0, link);
        let (result, _) = run(&words, 0x20, 0);
        let message = failure_message(result, "RawKeyConvert");
        assert!(message.contains("ie_NextEvent"), "{message}");
    }

    #[test]
    fn a_command_on_the_library_unit_fails_the_run() {
        let mut words = program(CONU_LIBRARY, None);
        words.pop(); // rts
        // movea.l #IOREQ,a1; jsr DoIO(a6); rts -- A6 is still exec.
        words.extend([0x227C, (IOREQ >> 16) as u16, IOREQ as u16]);
        words.extend([0x4EAE, (-456i16) as u16, 0x4E75]);
        let (result, _) = run(&words, 0, 0);
        let message = failure_message(result, "DoIO");
        assert!(message.contains("console.device"), "{message}");
    }

    #[test]
    fn a_non_default_keymap_fails_the_run() {
        let (result, _) = run(&program(CONU_LIBRARY, Some((64, 0x1_8400))), 0x20, 0);
        let message = failure_message(result, "RawKeyConvert");
        assert!(message.contains("keyMap"), "{message}");
    }
}
