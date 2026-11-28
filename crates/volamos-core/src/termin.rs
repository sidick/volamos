//! What the host terminal types, made into what an Amiga console would
//! deliver: keys become the Amiga's key sequences, and UTF-8 becomes
//! Latin-1 (issue #426, the input half). The counterpart of
//! [`crate::termout`].
//!
//! # Keys
//!
//! In raw mode (`SetMode(fh, 1)`) an Amiga program reads the console's
//! key reports directly: cursor up is `9B 41`, F1 is `9B 30 7E`, Help is
//! `9B 3F 7E`. A host terminal sends the same keys in its own forms
//! (cursor up is `ESC [ A`, F1 is `ESC O P`), so [`TermInput`] maps each
//! one it recognizes to an Amiga raw key and writes that key's bytes as
//! Kickstart 3.1 does ([`crate::console`]'s measured table, which is also
//! where `RawKeyConvert` gets them). The host side follows xterm's
//! conventions, which most terminals share:
//!
//! | Host key | Host sends | Amiga key |
//! | --- | --- | --- |
//! | cursor keys | `ESC [ A`..`D`, `ESC O A`..`D` | `CRSRUP` .. `CRSRLEFT` |
//! | F1-F4 | `ESC O P`..`S`, `ESC [ 11 ~`..`14 ~` | `F1`-`F4` |
//! | F5-F12 | `ESC [ 15 ~`, `17`-`21 ~`, `23 ~`, `24 ~` | `F5`-`F10`, `F11`, `F12` |
//! | Insert, Delete | `ESC [ 2 ~`, `ESC [ 3 ~` | `INSERT`, `DEL` |
//! | Home, End | `ESC [ H`/`F`, `ESC O H`/`F`, `ESC [ 1 ~`/`4 ~` (also `7 ~`/`8 ~`) | `HOME`, `END` |
//! | Page Up, Page Down | `ESC [ 5 ~`, `ESC [ 6 ~` | `PAGEUP`, `PAGEDOWN` |
//! | Shift+Tab | `ESC [ Z` | Shift+`TAB` |
//! | Backspace | `$7F` | `BACKSPACE` (`$08`) |
//!
//! `INSERT`, `PAGEUP`, `PAGEDOWN`, `F11`, `F12`, `HOME` and `END` are
//! "not on classic keyboards" (NDK 3.2 `libraries/keymap.h`'s
//! `RAWKEY_*` list), but Kickstart 3.1's default keymap already has
//! sequences for them. Shift with any of these (xterm's modifier
//! parameter `2`, as in `ESC [ 1 ; 2 A`) gives the Amiga key with Shift.
//! The key the Amiga has and a PC keyboard doesn't, Help, has no default:
//! every common host key already stands for a real Amiga key. A
//! [`KeyMap`] built from the user's `--key` settings can give it one, or
//! remap anything else.
//!
//! Any other sequence (F13 and up unless mapped, Alt or Ctrl with a
//! special key, keys no Amiga has) is dropped, as no Amiga key would
//! produce it.
//!
//! The terminal's answer to a cursor position request (`ESC [ row ; col
//! R`, asked for by [`crate::termout`] on the guest's behalf) becomes
//! the Amiga's `9B row ; col R`. It looks like Shift+F3 (`ESC [ 1 ; 2
//! R`), so it is only read as an answer while one is expected
//! ([`TermInput::expect_cursor_report`]).
//!
//! A lone `ESC` is the Esc key. A key's sequence comes from the terminal
//! in one burst, so [`TermInput`] holds an `ESC` (or a half sequence)
//! back when it ends a read, and the caller lets it go
//! ([`TermInput::flush`]) if nothing follows within a moment; an `ESC`
//! followed by an ordinary character (Alt+key on a terminal that sends
//! Meta as `ESC`) passes through as both bytes.
//!
//! Keys are only translated in raw mode. In cooked mode the host
//! terminal's own line editing has already used the keys it knows, and
//! the line is delivered as typed.
//!
//! # Characters
//!
//! UTF-8 becomes Latin-1: a character up to U+00FF is its one byte, and
//! one beyond that (no Amiga character set has it) becomes `?`. Bytes that
//! aren't valid UTF-8 pass through unchanged, so a Latin-1 terminal still
//! works. A character split between two reads is held until the rest
//! arrives.

use std::collections::HashMap;

const ESC: u8 = 0x1B;
const CSI: u8 = 0x9B;
const DEL: u8 = 0x7F;

/// Longest host key sequence kept while waiting for its final byte.
const MAX_SEQUENCE: usize = 16;

/// The Amiga raw keys a host key can stand for, by their NDK 3.2
/// `RAWKEY_*` names (`libraries/keymap.h`) without the prefix.
pub const AMIGA_KEYS: &[(&str, u8)] = &[
    ("SPACE", 0x40),
    ("BACKSPACE", 0x41),
    ("TAB", 0x42),
    ("ENTER", 0x43),
    ("RETURN", 0x44),
    ("ESC", 0x45),
    ("DEL", 0x46),
    ("INSERT", 0x47),
    ("PAGEUP", 0x48),
    ("PAGEDOWN", 0x49),
    ("F11", 0x4B),
    ("CRSRUP", 0x4C),
    ("CRSRDOWN", 0x4D),
    ("CRSRRIGHT", 0x4E),
    ("CRSRLEFT", 0x4F),
    ("F1", 0x50),
    ("F2", 0x51),
    ("F3", 0x52),
    ("F4", 0x53),
    ("F5", 0x54),
    ("F6", 0x55),
    ("F7", 0x56),
    ("F8", 0x57),
    ("F9", 0x58),
    ("F10", 0x59),
    ("HELP", 0x5F),
    ("BREAK", 0x6E),
    ("F12", 0x6F),
    ("HOME", 0x70),
    ("END", 0x71),
];

/// The host keys [`TermInput`] recognizes, and the Amiga key each stands
/// for by default (`None`: no default, see the module docs).
pub const HOST_KEYS: &[(&str, Option<&str>)] = &[
    ("Up", Some("CRSRUP")),
    ("Down", Some("CRSRDOWN")),
    ("Right", Some("CRSRRIGHT")),
    ("Left", Some("CRSRLEFT")),
    ("Home", Some("HOME")),
    ("End", Some("END")),
    ("Insert", Some("INSERT")),
    ("Delete", Some("DEL")),
    ("PageUp", Some("PAGEUP")),
    ("PageDown", Some("PAGEDOWN")),
    ("Tab", Some("TAB")),
    ("Backspace", Some("BACKSPACE")),
    ("F1", Some("F1")),
    ("F2", Some("F2")),
    ("F3", Some("F3")),
    ("F4", Some("F4")),
    ("F5", Some("F5")),
    ("F6", Some("F6")),
    ("F7", Some("F7")),
    ("F8", Some("F8")),
    ("F9", Some("F9")),
    ("F10", Some("F10")),
    ("F11", Some("F11")),
    ("F12", Some("F12")),
    ("F13", None),
    ("F14", None),
    ("F15", None),
    ("F16", None),
    ("F17", None),
    ("F18", None),
    ("F19", None),
    ("F20", None),
];

fn amiga_code(name: &str) -> Option<u8> {
    AMIGA_KEYS
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|&(_, code)| code)
}

fn host_key(name: &str) -> Option<&'static str> {
    HOST_KEYS
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|&(n, _)| n)
}

/// Splits an optional `Shift+` prefix off a key name.
fn split_shift(name: &str) -> (bool, &str) {
    match name.get(..6) {
        Some(prefix) if prefix.eq_ignore_ascii_case("shift+") => (true, &name[6..]),
        _ => (false, name),
    }
}

/// Which Amiga key, with or without Shift, each host key (with or
/// without Shift) stands for: the defaults in [`HOST_KEYS`], plus any
/// `--key` settings.
#[derive(Debug, Clone)]
pub struct KeyMap {
    keys: HashMap<(&'static str, bool), (u8, bool)>,
}

impl Default for KeyMap {
    fn default() -> Self {
        let mut keys = HashMap::new();
        for &(host, amiga) in HOST_KEYS {
            if let Some(code) = amiga.and_then(amiga_code) {
                keys.insert((host, false), (code, false));
                keys.insert((host, true), (code, true));
            }
        }
        Self { keys }
    }
}

impl KeyMap {
    /// Applies one `HOSTKEY=AMIGAKEY` setting, e.g. `F12=HELP` or
    /// `Shift+F12=Shift+HELP`; either side may carry `Shift+`. Mapping a
    /// host key without `Shift+` maps its Shift form too, to the Amiga
    /// key with Shift, unless that has a setting of its own.
    pub fn set(&mut self, setting: &str) -> Result<(), String> {
        let (host, amiga) = setting
            .split_once('=')
            .ok_or_else(|| format!("{setting:?} is not HOSTKEY=AMIGAKEY"))?;
        let (host_shift, host_name) = split_shift(host.trim());
        let (amiga_shift, amiga_name) = split_shift(amiga.trim());
        let host = host_key(host_name).ok_or_else(|| {
            let names: Vec<&str> = HOST_KEYS.iter().map(|(n, _)| *n).collect();
            format!(
                "unknown host key {host_name:?} (one of {}, optionally with Shift+)",
                names.join(", ")
            )
        })?;
        let code = amiga_code(amiga_name).ok_or_else(|| {
            let names: Vec<&str> = AMIGA_KEYS.iter().map(|(n, _)| *n).collect();
            format!(
                "unknown Amiga key {amiga_name:?} (one of {}, optionally with Shift+)",
                names.join(", ")
            )
        })?;
        self.keys.insert((host, host_shift), (code, amiga_shift));
        if !host_shift {
            self.keys.insert((host, true), (code, true));
        }
        Ok(())
    }

    /// The bytes host key `host` (with Shift if `shift`) gives, if it is
    /// mapped.
    fn bytes(&self, host: &str, shift: bool) -> Option<&'static [u8]> {
        let &(code, amiga_shift) = self.keys.get(&(host_key(host)?, shift))?;
        Some(crate::console::key_bytes(code, amiga_shift))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    /// After `ESC`.
    Escape,
    /// After `ESC [`; the parameters so far are in [`TermInput::seq`].
    Csi,
    /// After `ESC O`.
    Ss3,
    /// Part way through a UTF-8 character, its bytes in
    /// [`TermInput::seq`], `usize` more to come.
    Utf8(usize),
}

/// Translates host terminal input for the guest; see the module docs.
/// Keep one per input stream, so state survives between reads.
#[derive(Debug)]
pub struct TermInput {
    keys: bool,
    charset: bool,
    map: KeyMap,
    state: State,
    seq: Vec<u8>,
    cursor_reports_expected: u32,
}

impl TermInput {
    /// `keys`: translate key sequences (in raw mode). `charset`: convert
    /// UTF-8 to Latin-1.
    pub fn new(keys: bool, charset: bool, map: KeyMap) -> Self {
        Self {
            keys,
            charset,
            map,
            state: State::Ground,
            seq: Vec::new(),
            cursor_reports_expected: 0,
        }
    }

    /// Whether this translator changes anything at all.
    pub fn is_active(&self) -> bool {
        self.keys || self.charset
    }

    /// Whether key translation is on (it then applies in raw mode).
    pub fn translates_keys(&self) -> bool {
        self.keys
    }

    /// The guest has asked for the cursor position, and the terminal's
    /// answer is on its way: read the next `ESC [ r ; c R` as that.
    pub fn expect_cursor_report(&mut self) {
        self.cursor_reports_expected += 1;
    }

    /// Whether some bytes are held, waiting for the rest of a sequence
    /// or character; see [`Self::flush`].
    pub fn has_pending(&self) -> bool {
        self.state != State::Ground
    }

    /// Appends the translation of `input` to `out`. `raw`: the console is
    /// in raw mode, so keys are translated (if that is on).
    pub fn feed(&mut self, input: &[u8], raw: bool, out: &mut Vec<u8>) {
        for &byte in input {
            self.byte(byte, raw && self.keys, out);
        }
    }

    /// Lets go of whatever is held: a lone `ESC` is the Esc key, a half
    /// key sequence is dropped, and the bytes of a half UTF-8 character
    /// pass through as they are. For when nothing more has arrived in a
    /// moment, or at end of input.
    pub fn flush(&mut self, out: &mut Vec<u8>) {
        match self.state {
            State::Escape => out.push(ESC),
            State::Utf8(_) => out.extend_from_slice(&self.seq),
            State::Ground | State::Csi | State::Ss3 => {}
        }
        self.state = State::Ground;
        self.seq.clear();
    }

    fn byte(&mut self, byte: u8, keys: bool, out: &mut Vec<u8>) {
        match self.state {
            State::Ground => self.ground(byte, keys, out),
            State::Escape => match byte {
                b'[' => {
                    self.seq.clear();
                    self.state = State::Csi;
                }
                b'O' => self.state = State::Ss3,
                ESC => out.push(ESC),
                _ => {
                    // Not a key sequence: the Esc key, then whatever
                    // came after it.
                    out.push(ESC);
                    self.state = State::Ground;
                    self.ground(byte, keys, out);
                }
            },
            State::Csi => match byte {
                0x30..=0x3F if self.seq.len() < MAX_SEQUENCE => self.seq.push(byte),
                0x40..=0x7E => {
                    self.state = State::Ground;
                    self.csi_key(byte, out);
                }
                // Malformed or overlong: dropped, and the byte handled
                // afresh.
                _ => {
                    self.state = State::Ground;
                    self.ground(byte, keys, out);
                }
            },
            State::Ss3 => {
                self.state = State::Ground;
                let key = match byte {
                    b'A'..=b'D' | b'H' | b'F' => cursor_key(byte),
                    b'P'..=b'S' => Some(["F1", "F2", "F3", "F4"][usize::from(byte - b'P')]),
                    _ => None,
                };
                if let Some(bytes) = key.and_then(|k| self.map.bytes(k, false)) {
                    out.extend_from_slice(bytes);
                }
            }
            State::Utf8(more) => {
                if byte & 0xC0 == 0x80 {
                    self.seq.push(byte);
                    if more == 1 {
                        self.state = State::Ground;
                        self.utf8_done(out);
                    } else {
                        self.state = State::Utf8(more - 1);
                    }
                } else {
                    // Not a continuation: the bytes so far weren't UTF-8.
                    out.extend_from_slice(&self.seq);
                    self.state = State::Ground;
                    self.ground(byte, keys, out);
                }
            }
        }
    }

    fn ground(&mut self, byte: u8, keys: bool, out: &mut Vec<u8>) {
        match byte {
            ESC if keys => self.state = State::Escape,
            DEL if keys => match self.map.bytes("Backspace", false) {
                Some(bytes) => out.extend_from_slice(bytes),
                None => out.push(DEL),
            },
            0xC2..=0xF4 if self.charset => {
                self.seq.clear();
                self.seq.push(byte);
                self.state = State::Utf8(match byte {
                    0xC2..=0xDF => 1,
                    0xE0..=0xEF => 2,
                    _ => 3,
                });
            }
            _ => out.push(byte),
        }
    }

    fn utf8_done(&mut self, out: &mut Vec<u8>) {
        match std::str::from_utf8(&self.seq)
            .ok()
            .and_then(|s| s.chars().next())
        {
            Some(c) => out.push(u8::try_from(u32::from(c)).unwrap_or(b'?')),
            // Overlong or a surrogate: not UTF-8 after all.
            None => out.extend_from_slice(&self.seq),
        }
        self.seq.clear();
    }

    /// `ESC [` + [`Self::seq`] + `last`: a key, a cursor position report,
    /// or nothing.
    fn csi_key(&mut self, last: u8, out: &mut Vec<u8>) {
        let params = std::str::from_utf8(&self.seq).unwrap_or("");
        let mut fields = params.split(';');
        let first = fields.next().unwrap_or("");
        let second = fields.next();
        if fields.next().is_some() {
            return;
        }
        if last == b'R' && self.cursor_reports_expected > 0 {
            if let Some(col) = second
                && !first.is_empty()
                && first.bytes().chain(col.bytes()).all(|b| b.is_ascii_digit())
            {
                self.cursor_reports_expected -= 1;
                out.push(CSI);
                out.extend_from_slice(self.seq.as_slice());
                out.push(b'R');
            }
            return;
        }
        // xterm's modifier parameter: 1 (or none) is no modifier, 2 Shift.
        let shift = match second {
            None | Some("1") => false,
            Some("2") => true,
            Some(_) => return,
        };
        let key = match (last, first) {
            (b'A'..=b'D' | b'H' | b'F', "" | "1") => cursor_key(last),
            (b'P'..=b'S', "" | "1") => Some(["F1", "F2", "F3", "F4"][usize::from(last - b'P')]),
            (b'Z', "") => {
                if let Some(bytes) = self.map.bytes("Tab", true) {
                    out.extend_from_slice(bytes);
                }
                return;
            }
            (b'~', number) => tilde_key(number),
            _ => None,
        };
        if let Some(bytes) = key.and_then(|k| self.map.bytes(k, shift)) {
            out.extend_from_slice(bytes);
        }
    }
}

fn cursor_key(last: u8) -> Option<&'static str> {
    Some(match last {
        b'A' => "Up",
        b'B' => "Down",
        b'C' => "Right",
        b'D' => "Left",
        b'H' => "Home",
        b'F' => "End",
        _ => return None,
    })
}

/// The `ESC [ n ~` keys, by `n` (xterm's numbering; `7`/`8` are rxvt's
/// Home/End).
fn tilde_key(number: &str) -> Option<&'static str> {
    Some(match number {
        "1" | "7" => "Home",
        "2" => "Insert",
        "3" => "Delete",
        "4" | "8" => "End",
        "5" => "PageUp",
        "6" => "PageDown",
        "11" => "F1",
        "12" => "F2",
        "13" => "F3",
        "14" => "F4",
        "15" => "F5",
        "17" => "F6",
        "18" => "F7",
        "19" => "F8",
        "20" => "F9",
        "21" => "F10",
        "23" => "F11",
        "24" => "F12",
        "25" => "F13",
        "26" => "F14",
        "28" => "F15",
        "29" => "F16",
        "31" => "F17",
        "32" => "F18",
        "33" => "F19",
        "34" => "F20",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `bytes` through a fresh translator in raw mode, then flushed.
    fn raw(bytes: &[u8]) -> Vec<u8> {
        let mut t = TermInput::new(true, true, KeyMap::default());
        let mut out = Vec::new();
        t.feed(bytes, true, &mut out);
        t.flush(&mut out);
        out
    }

    #[test]
    fn host_keys_become_the_amiga_keys_sequences() {
        // The Amiga side is console.rs's measured Kickstart 3.1 table.
        assert_eq!(raw(b"\x1b[A\x1bOB\x1b[C\x1b[D"), b"\x9bA\x9bB\x9bC\x9bD");
        assert_eq!(
            raw(b"\x1bOP\x1b[13~\x1b[15~\x1b[21~"),
            b"\x9b0~\x9b2~\x9b4~\x9b9~"
        );
        assert_eq!(raw(b"\x1b[23~\x1b[24~"), b"\x9b20~\x9b21~");
        assert_eq!(raw(b"\x1b[2~\x1b[5~\x1b[6~"), b"\x9b40~\x9b41~\x9b42~");
        assert_eq!(
            raw(b"\x1b[H\x1bOF\x1b[1~\x1b[4~"),
            b"\x9b44~\x9b45~\x9b44~\x9b45~"
        );
        assert_eq!(raw(b"\x1b[3~\x7f"), b"\x7f\x08");
        assert_eq!(raw(b"\x1b[Z"), b"\x9bZ");
    }

    #[test]
    fn shift_gives_the_amiga_key_with_shift() {
        // Shift+Up is CSI T, Shift+Right CSI SP @, Shift+F1 CSI 10 ~.
        assert_eq!(raw(b"\x1b[1;2A\x1b[1;2C\x1b[1;2P"), b"\x9bT\x9b @\x9b10~");
        assert_eq!(raw(b"\x1b[15;2~\x1b[24;2~"), b"\x9b14~\x9b31~");
    }

    #[test]
    fn unmapped_and_modified_keys_are_dropped() {
        // F13, Ctrl+Up (modifier 5), an unknown final.
        assert_eq!(raw(b"a\x1b[25~b\x1b[1;5Ac\x1b[99~d\x1b[?1;2cE"), b"abcdE");
    }

    #[test]
    fn a_lone_esc_is_the_esc_key_and_esc_plus_a_letter_passes() {
        assert_eq!(raw(b"\x1b"), b"\x1b");
        assert_eq!(raw(b"\x1bx"), b"\x1bx");
        assert_eq!(raw(b"\x1b\x1b[A"), b"\x1b\x9bA");
    }

    #[test]
    fn a_held_escape_waits_for_the_next_read() {
        let mut t = TermInput::new(true, true, KeyMap::default());
        let mut out = Vec::new();
        t.feed(b"a\x1b", true, &mut out);
        assert!(t.has_pending());
        t.feed(b"[A", true, &mut out);
        assert!(!t.has_pending());
        assert_eq!(out, b"a\x9bA");
    }

    #[test]
    fn a_cursor_report_is_only_read_as_one_when_expected() {
        let mut t = TermInput::new(true, true, KeyMap::default());
        let mut out = Vec::new();
        t.expect_cursor_report();
        t.feed(b"\x1b[1;2R\x1b[1;2R", true, &mut out);
        // The first is the answer; the second is Shift+F3.
        assert_eq!(out, b"\x9b1;2R\x9b12~");
    }

    #[test]
    fn keys_are_left_alone_in_cooked_mode_or_when_off() {
        let mut t = TermInput::new(true, true, KeyMap::default());
        let mut out = Vec::new();
        t.feed(b"\x1b[A\x7f", false, &mut out);
        assert_eq!(out, b"\x1b[A\x7f");
        let mut t = TermInput::new(false, false, KeyMap::default());
        let mut out = Vec::new();
        t.feed(b"\x1b[A\xc3\xa9", true, &mut out);
        assert_eq!(out, b"\x1b[A\xc3\xa9");
    }

    #[test]
    fn utf8_becomes_latin1() {
        let mut t = TermInput::new(false, true, KeyMap::default());
        let mut out = Vec::new();
        t.feed("café £5 € 😀".as_bytes(), false, &mut out);
        assert_eq!(out, b"caf\xe9 \xa35 ? ?");
    }

    #[test]
    fn a_character_split_between_reads_is_held() {
        let mut t = TermInput::new(false, true, KeyMap::default());
        let mut out = Vec::new();
        t.feed(b"\xc3", false, &mut out);
        assert!(out.is_empty() && t.has_pending());
        t.feed(b"\xa9", false, &mut out);
        assert_eq!(out, b"\xe9");
    }

    #[test]
    fn bytes_that_are_not_utf8_pass_through() {
        let mut t = TermInput::new(false, true, KeyMap::default());
        let mut out = Vec::new();
        t.feed(b"\xe9t\xe9", false, &mut out);
        t.flush(&mut out);
        assert_eq!(out, b"\xe9t\xe9");
        // Overlong encoding of '/'.
        let mut out = Vec::new();
        t.feed(b"\xe0\x80\xaf", false, &mut out);
        assert_eq!(out, b"\xe0\x80\xaf");
    }

    #[test]
    fn key_settings_remap_and_add_keys() {
        let mut map = KeyMap::default();
        map.set("F12=HELP").unwrap();
        map.set("shift+f13 = Shift+F1").unwrap();
        let mut t = TermInput::new(true, true, map);
        let mut out = Vec::new();
        t.feed(b"\x1b[24~\x1b[24;2~\x1b[25;2~\x1b[25~", true, &mut out);
        // Help, Shift+Help (from the unshifted setting), Shift+F1, and
        // unshifted F13 still has no mapping.
        assert_eq!(out, b"\x9b?~\x9b?~\x9b10~");
    }

    #[test]
    fn bad_key_settings_name_the_choices() {
        let mut map = KeyMap::default();
        let err = map.set("F99=HELP").unwrap_err();
        assert!(
            err.contains("unknown host key") && err.contains("PageUp"),
            "{err}"
        );
        let err = map.set("F12=NOPE").unwrap_err();
        assert!(
            err.contains("unknown Amiga key") && err.contains("HELP"),
            "{err}"
        );
        assert!(map.set("F12").is_err());
    }
}
