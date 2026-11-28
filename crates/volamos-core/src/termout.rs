//! The guest's console output, made readable on a host terminal: Amiga
//! control sequences become their ANSI equivalents (issue #427) and
//! Latin-1 text becomes UTF-8 (issue #426, the output half).
//!
//! # Why
//!
//! `Output()` is host stdout (see [`crate::hostterm`]), with no console
//! handler between them, so whatever bytes a guest writes for the Amiga
//! console reach the host terminal unchanged. Two things then go wrong
//! on a modern terminal:
//!
//! - The Amiga console's single-byte control sequence introducer is
//!   `$9B` (C1 `CSI`). A UTF-8 terminal doesn't treat a lone `$9B` as
//!   anything, so "erase to end of line" (`9B 4B`) or a colour change
//!   (`9B 33 31 6D`) shows as garbage. Several of the Amiga's sequences
//!   are its own, too, with no ANSI meaning.
//! - Amiga text is ISO-8859-1 ("G1: ECMA 94 Latin 1 graphic characters",
//!   `console.doc`). A byte such as `$E9` (`é`) isn't valid UTF-8.
//!
//! [`TermFilter`] fixes both, one switch each. The CLI decides whether
//! either applies (normally only when stdout is a terminal, so a
//! redirected stream stays byte-exact); this module only translates.
//!
//! # Character conversion
//!
//! `$A0`..`$FF` become the same code points in UTF-8 (Latin-1 is the
//! first 256 code points of Unicode). `$80`..`$9F` are control codes,
//! not characters, so this never touches them: they belong to escape
//! translation, and with that off they pass through unchanged. That
//! keeps the two switches independent.
//!
//! # Escape translation
//!
//! The ground truth for the Amiga side is the `CMD_WRITE` entry of the
//! NDK 3.2 `console.doc` ("ANSI CODES SUPPORTED"), plus the RKRM Devices
//! console chapter for `SET CURSOR RENDITION`'s parameters. The parser
//! follows the usual ECMA-48 shape (parameter bytes `$30`..`$3F`, then
//! intermediates `$20`..`$2F`, then a final `$40`..`$7E`). It keeps its
//! state between writes, since a guest can split a sequence across two
//! `Write` calls.
//!
//! Byte by byte:
//!
//! - `$9B` and `ESC [` both start a control sequence ("CSI is either 9B
//!   or Esc[", `console.doc`). Sequences the host understands the same
//!   way are written out with `ESC [`. The rest are dropped.
//! - `$84` (IND), `$85` (NEL), `$88` (HTS) and `$8D` (RI), the single-byte
//!   C1 functions the doc lists, become their two-byte `ESC D`/`E`/`H`/`M`
//!   forms, as do those `ESC` forms themselves. Every other C1 byte is
//!   dropped.
//! - `SO` (`$0E`) and `SI` (`$0F`) shift ASCII into Latin-1's upper half
//!   and back on the Amiga. On a host terminal they would select a
//!   different character set (often line drawing), so they are consumed,
//!   and while shifted, `$20`..`$7E` are written as `$A0`..`$FE`.
//! - `ESC c` (RIS, "reset the console display") passes through.
//!
//! Control sequences, by final byte:
//!
//! | Amiga | Written out as |
//! | --- | --- |
//! | `@ A B C D E F H I L M P S T W Z f g n` | `ESC [`, same numeric parameters |
//! | `J` (ED), `K` (EL) | `ESC [J` / `ESC [K`: the Amiga erases "only to end of display/line", whatever the parameter |
//! | `m` (SGR) | `ESC [`, the numeric parameters; the Amiga-only `>`*n* (background colour) ones are removed, and if nothing is left the sequence is dropped, so it doesn't become a reset |
//! | `h`, `l` (SM/RM) | only `?7` (auto-wrap, same as DEC's) is kept; `20` (LNM) and `>1` (auto-scroll) are dropped |
//! | ` p` (aSCR) | `0` hides the cursor (`ESC [?25l`), no parameter shows it (`ESC [?25h`) |
//! | anything else | dropped |
//!
//! "Anything else" covers the private Amiga sequences with no host
//! equivalent (`t u x y { }`, ` q`, ` s`), the ones the doc lists as
//! coming only from the console (`R | ~`, ` r`, ` v`), and any sequence
//! with a parameter the Amiga doesn't define (a `?` or `>` prefix outside
//! the cases above). SGR colour numbers are passed on as they are: the
//! Amiga's `30`..`37` pick palette pens, not fixed colours, so they come
//! out in the host terminal's colours instead.
//!
//! A sequence the guest waits for an answer to (` q`, the window status
//! request) is dropped here. Answering it needs the input side, which is
//! not done yet.
//!
//! Following ECMA-48, a C0 control inside a sequence is carried out at
//! once without ending the sequence; `CAN` (`$18`) or `SUB` (`$1A`) ends
//! it, as does any byte from `$80` up, and a new `ESC`/`$9B` starts a
//! fresh one. A sequence longer than [`MAX_SEQUENCE`] is dropped rather
//! than buffered without limit.

/// Longest parameter-and-intermediate run kept for one sequence; past it
/// the sequence is dropped. Amiga sequences are far shorter (the longest
/// documented, `aIER`, has 8 parameters).
const MAX_SEQUENCE: usize = 64;

const ESC: u8 = 0x1B;
const CSI: u8 = 0x9B;
const SO: u8 = 0x0E;
const SI: u8 = 0x0F;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1A;

#[derive(Debug)]
enum State {
    Ground,
    /// After `ESC`.
    Escape,
    /// In a control sequence: the parameter and intermediate bytes so
    /// far.
    Sequence(Vec<u8>),
    /// In a control sequence that grew past [`MAX_SEQUENCE`]: skipping
    /// to its final byte.
    Discard,
}

/// Translates guest console output for a host terminal; see the module
/// docs. Keep one per output stream, so state survives between writes.
#[derive(Debug)]
pub struct TermFilter {
    escapes: bool,
    charset: bool,
    state: State,
    shifted: bool,
}

impl TermFilter {
    /// `escapes`: translate control sequences. `charset`: convert
    /// Latin-1 to UTF-8.
    pub fn new(escapes: bool, charset: bool) -> Self {
        Self {
            escapes,
            charset,
            state: State::Ground,
            shifted: false,
        }
    }

    /// Whether this filter changes anything at all.
    pub fn is_active(&self) -> bool {
        self.escapes || self.charset
    }

    /// Appends the translation of `input` to `out`. An unfinished
    /// sequence at the end of `input` is held until the next call.
    pub fn filter(&mut self, input: &[u8], out: &mut Vec<u8>) {
        for &byte in input {
            if self.escapes {
                self.escape_byte(byte, out);
            } else {
                self.text_byte(byte, out);
            }
        }
    }

    /// A byte of plain text: Latin-1 to UTF-8 if enabled.
    fn text_byte(&self, byte: u8, out: &mut Vec<u8>) {
        if self.charset && byte >= 0xA0 {
            let mut utf8 = [0; 2];
            out.extend_from_slice(char::from(byte).encode_utf8(&mut utf8).as_bytes());
        } else {
            out.push(byte);
        }
    }

    fn escape_byte(&mut self, byte: u8, out: &mut Vec<u8>) {
        match std::mem::replace(&mut self.state, State::Ground) {
            State::Ground => self.ground(byte, out),
            State::Escape => match byte {
                b'[' => self.state = State::Sequence(Vec::new()),
                b'c' | b'D' | b'E' | b'H' | b'M' => out.extend_from_slice(&[ESC, byte]),
                // Not an Amiga escape sequence: leave it as it was.
                _ if (0x20..0x7F).contains(&byte) => out.extend_from_slice(&[ESC, byte]),
                _ => {
                    out.push(ESC);
                    self.ground(byte, out);
                }
            },
            State::Discard => match byte {
                0x20..=0x3F | 0x7F => self.state = State::Discard,
                0x40..=0x7E | CAN | SUB => {}
                0x00..=0x1F if byte != ESC => {
                    self.ground(byte, out);
                    self.state = State::Discard;
                }
                _ => self.ground(byte, out),
            },
            State::Sequence(mut seq) => match byte {
                0x20..=0x3F if seq.len() < MAX_SEQUENCE => {
                    seq.push(byte);
                    self.state = State::Sequence(seq);
                }
                0x20..=0x3F => self.state = State::Discard,
                0x40..=0x7E => emit_sequence(&seq, byte, out),
                CAN | SUB => {}
                ESC | CSI => self.ground(byte, out),
                0x00..=0x1F => {
                    self.ground(byte, out);
                    self.state = State::Sequence(seq);
                }
                // DEL inside a sequence is ignored, as in ECMA-48.
                0x7F => self.state = State::Sequence(seq),
                _ => self.ground(byte, out),
            },
        }
    }

    fn ground(&mut self, byte: u8, out: &mut Vec<u8>) {
        match byte {
            ESC => self.state = State::Escape,
            CSI => self.state = State::Sequence(Vec::new()),
            SO => self.shifted = true,
            SI => self.shifted = false,
            0x84 => out.extend_from_slice(&[ESC, b'D']),
            0x85 => out.extend_from_slice(&[ESC, b'E']),
            0x88 => out.extend_from_slice(&[ESC, b'H']),
            0x8D => out.extend_from_slice(&[ESC, b'M']),
            0x80..=0x9F => {}
            0x20..=0x7E if self.shifted => self.text_byte(byte | 0x80, out),
            _ => self.text_byte(byte, out),
        }
    }
}

/// Writes the host form of the control sequence `params` + `last`, or
/// nothing (see the module docs' table).
fn emit_sequence(params: &[u8], last: u8, out: &mut Vec<u8>) {
    let split = params
        .iter()
        .position(|b| (0x20..=0x2F).contains(b))
        .unwrap_or(params.len());
    let (numbers, intermediates) = params.split_at(split);
    let plain = numbers.iter().all(|&b| b.is_ascii_digit() || b == b';');
    let mut csi = |body: &[u8]| {
        out.extend_from_slice(&[ESC, b'[']);
        out.extend_from_slice(body);
    };
    match (intermediates, last) {
        ([], b'@' | b'A'..=b'I' | b'L' | b'M' | b'P' | b'S' | b'T' | b'W' | b'Z')
        | ([], b'f' | b'g' | b'n')
            if plain =>
        {
            csi(numbers);
            out.push(last);
        }
        ([], b'J' | b'K') if plain => {
            csi(&[]);
            out.push(last);
        }
        ([], b'm') => {
            let kept: Vec<&[u8]> = numbers
                .split(|&b| b == b';')
                .filter(|p| !p.starts_with(b">"))
                .collect();
            let all_plain = kept.iter().all(|p| p.iter().all(u8::is_ascii_digit));
            let dropped_all = !numbers.is_empty() && kept.iter().all(|p| p.is_empty());
            if all_plain && !dropped_all {
                csi(&kept.join(&b';'));
                out.push(b'm');
            }
        }
        ([], b'h' | b'l') => {
            if numbers.split(|&b| b == b';').any(|p| p == b"?7") {
                csi(b"?7");
                out.push(last);
            }
        }
        (b" ", b'p') => match numbers {
            b"0" => csi(b"?25l"),
            b"" => csi(b"?25h"),
            _ => {}
        },
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(escapes: bool, charset: bool, chunks: &[&[u8]]) -> Vec<u8> {
        let mut filter = TermFilter::new(escapes, charset);
        let mut out = Vec::new();
        for chunk in chunks {
            filter.filter(chunk, &mut out);
        }
        out
    }

    fn both(input: &[u8]) -> Vec<u8> {
        run(true, true, &[input])
    }

    #[test]
    fn latin1_text_becomes_utf8() {
        // The issue's own example: "café naïve £5".
        assert_eq!(
            run(false, true, &[b"caf\xe9 na\xefve \xa35\n"]),
            "café naïve £5\n".as_bytes()
        );
        assert_eq!(run(false, true, &[b"\xa0\xff"]), "\u{a0}ÿ".as_bytes());
    }

    #[test]
    fn character_conversion_leaves_c1_controls_alone() {
        // So that with escapes off, 0x9B stays one raw byte.
        assert_eq!(run(false, true, &[b"a\x9bKb\x85"]), b"a\x9bKb\x85");
    }

    #[test]
    fn both_off_is_byte_exact() {
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(run(false, false, &[&all]), all);
    }

    #[test]
    fn the_single_byte_csi_becomes_esc_bracket() {
        // The issue's example: "ab", erase to end of line, "cd", SGR 0.
        assert_eq!(both(b"ab\x9bKcd\x9b0m"), b"ab\x1b[Kcd\x1b[0m");
        assert_eq!(both(b"\x9b14;80H\x9b3A"), b"\x1b[14;80H\x1b[3A");
        // The two-byte form is accepted too.
        assert_eq!(both(b"\x1b[1;31mx\x1b[0m"), b"\x1b[1;31mx\x1b[0m");
    }

    #[test]
    fn erase_only_reaches_the_end_as_on_the_amiga() {
        assert_eq!(both(b"\x9b2J\x9b1K"), b"\x1b[J\x1b[K");
    }

    #[test]
    fn sgr_drops_the_amiga_background_parameter() {
        assert_eq!(both(b"\x9b1;32;>3m"), b"\x1b[1;32m");
        // Only Amiga-only parameters: dropped, not turned into a reset.
        assert_eq!(both(b"\x9b>3mx"), b"x");
        assert_eq!(both(b"\x9bm"), b"\x1b[m");
    }

    #[test]
    fn modes_keep_only_auto_wrap() {
        assert_eq!(both(b"\x9b?7l\x9b20h\x9b>1l"), b"\x1b[?7l");
        assert_eq!(both(b"\x9b20;?7h"), b"\x1b[?7h");
    }

    #[test]
    fn cursor_rendition_hides_and_shows_the_cursor() {
        // RKRM Devices: invisible 9B 30 20 70, visible 9B 20 70.
        assert_eq!(both(b"\x9b0 p\x9b p"), b"\x1b[?25l\x1b[?25h");
        assert_eq!(both(b"\x9b5 p"), b"");
    }

    #[test]
    fn amiga_private_and_report_sequences_are_dropped() {
        for seq in [
            &b"\x9b20t"[..],
            b"\x9b80u",
            b"\x9b4x",
            b"\x9b4y",
            b"\x9b1;2{",
            b"\x9b1}",
            b"\x9b q",
            b"\x9b s",
            b"\x9b1;1;24;80 r",
            b"\x9b0 v",
            b"\x9b1;1R",
            b"\x9b0~",
            b"\x9b?1A",
        ] {
            assert_eq!(both(&[b"<", seq, b">"].concat()), b"<>", "{seq:?}");
        }
    }

    #[test]
    fn single_byte_c1_functions_get_their_esc_forms() {
        assert_eq!(both(b"\x84\x85\x88\x8d"), b"\x1bD\x1bE\x1bH\x1bM");
        assert_eq!(both(b"a\x80\x9f\x90b"), b"ab");
        assert_eq!(both(b"\x1bc\x1bM"), b"\x1bc\x1bM");
    }

    #[test]
    fn shift_out_maps_ascii_into_latin1() {
        assert_eq!(both(b"a\x0ei\x0fi"), "aéi".as_bytes());
        // Without character conversion the shifted byte is raw Latin-1.
        assert_eq!(run(true, false, &[b"\x0ei\x0f"]), b"\xe9");
    }

    #[test]
    fn a_sequence_split_across_writes_still_translates() {
        let chunks: [&[u8]; 4] = [b"a\x9b", b"1;", b"3", b"1mb"];
        assert_eq!(run(true, true, &chunks), b"a\x1b[1;31mb");
        assert_eq!(run(true, true, &[b"\x1b", b"[K"]), b"\x1b[K");
    }

    #[test]
    fn controls_inside_a_sequence_follow_ecma_48() {
        // A C0 control runs at once; the sequence carries on.
        assert_eq!(both(b"\x9b1\n2H"), b"\n\x1b[12H");
        // CAN cancels it; a new introducer restarts it.
        assert_eq!(both(b"\x9b12\x18x"), b"x");
        assert_eq!(both(b"\x9b12\x9bK"), b"\x1b[K");
        // A high byte ends it and is handled as text.
        assert_eq!(both(b"\x9b1\xe9"), "é".as_bytes());
    }

    #[test]
    fn an_overlong_sequence_is_dropped() {
        let mut long = vec![CSI];
        long.extend(std::iter::repeat_n(b'1', MAX_SEQUENCE + 10));
        long.extend_from_slice(b"Hx");
        assert_eq!(both(&long), b"x");
    }

    #[test]
    fn unknown_escapes_pass_through() {
        assert_eq!(both(b"\x1b(B"), b"\x1b(B");
        assert_eq!(both(b"\x1b\nx"), b"\x1b\nx");
    }
}
