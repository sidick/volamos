//! `dos.library` pattern matching: `ParsePattern`/`ParsePatternNoCase`
//! (tokenize a wildcard pattern) and `MatchPattern`/`MatchPatternNoCase`
//! (test a string against a tokenized pattern). Every `C:` command that
//! takes a filename argument (`List`, `Copy`, `Delete`, `Dir`, ...)
//! resolves wildcards through these -- see `docs/plan.md`'s
//! empirical-corpus decision.
//!
//! # Scope
//!
//! This module is the string-matching engine only: `ParsePattern`/
//! `MatchPattern` and their `NoCase` counterparts. `MatchFirst`/
//! `MatchNext`/`MatchEnd` (the `AnchorPath`-based recursive directory
//! scanner built on top of this engine) live in `crate::dosanchor`,
//! split out because real `AnchorPath`/`AChain` struct-layout fidelity
//! is a distinct chunk of work from the matcher itself.
//!
//! # Wildcard syntax
//!
//! `?` any one character; `#atom` zero or more repeats of the following
//! atom (a bare group is required for repeats of more than one atom,
//! e.g. `#(ab)`); `~atom` matches iff `atom` does *not* match the same
//! (rest-of-string) position -- see "`~` scope" below; `[...]`/`[~...]`
//! a character class or its negation, with `a-z` ranges, a leading `-`
//! meaning the literal dash, and a trailing `-` meaning "up to `0x7f`";
//! `(a|b|c)` alternation, `%` the empty alternative; `'` escapes the
//! *next* character, but (per the real semantics, not the naive
//! reading) **only** when that next character is itself one of the
//! wildcard characters above -- `'a` where `a` is a plain character is
//! *not* an escape at all, and matches the literal two-character
//! sequence `'a`.
//!
//! `~`'s scope is simplified from the real matcher: `~atom` is treated
//! as matching the *entire remaining* input from the current position
//! (i.e. `atom` is tested against the whole rest of the string, not
//! some prefix of it), which is exactly how every real-world `~` usage
//! this project's corpus is expected to hit looks (`~(#?.info)`, always
//! the final part of a pattern) -- a `~` with trailing atoms after it in
//! the same sequence is not supported.
//!
//! # Tokenized encoding
//!
//! The real dos.library documents its tokenized format only as "use
//! `ParsePattern`/`MatchPattern` as a pair" and explicitly calls the
//! byte encoding internal (ISO-Latin-1 C1 control codes, "should be
//! considered internal"). Nothing in a real program is meant to inspect
//! those bytes -- only `MatchPattern(NoCase)` ever reads them back -- so
//! this runtime is free to use its own self-delimiting recursive
//! encoding (see [`encode`]/[`decode`]) rather than replicate the real
//! byte-for-byte format; the two are only ever exchanged between this
//! module's own `ParsePattern` and `MatchPattern` handlers.

use crate::cpu::{Cpu, DataRegister};
use crate::dispatch::{DOS_LIBRARY_BASE, DispatchError, HandlerContext, LibraryTable};
use crate::dosargs::ERROR_BAD_TEMPLATE;
use crate::dosfile::DosState;
use crate::guestmem::read_c_string;
use crate::lvos::dos::DOS_LVOS;
use crate::memory::AddressSpace;
use crate::utility::amiga_toupper;

/// The tokenized-pattern destination buffer wasn't big enough.
pub const ERROR_LINE_TOO_LONG: i32 = 120;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Node {
    Literal(u8),
    Any,
    Empty,
    Class { negate: bool, ranges: Vec<(u8, u8)> },
    Seq(Vec<Node>),
    Alt(Vec<Node>),
    Not(Box<Node>),
    Repeat(Box<Node>),
}

fn is_wildcard_char(c: u8) -> bool {
    matches!(
        c,
        b'?' | b'#' | b'~' | b'%' | b'(' | b')' | b'[' | b']' | b'\'' | b'|'
    )
}

struct Parser<'a> {
    buf: &'a [u8],
    pos: usize,
    has_wildcard: bool,
}

impl<'a> Parser<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self {
            buf,
            pos: 0,
            has_wildcard: false,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.buf.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn parse_top(&mut self) -> Result<Node, i32> {
        let node = self.parse_seq(false)?;
        if self.pos != self.buf.len() {
            // A ')' or '|' with no enclosing '(' left unconsumed.
            return Err(ERROR_BAD_TEMPLATE);
        }
        Ok(node)
    }

    /// Parses a run of atoms. `in_group` controls whether `)`/`|` end
    /// the run (inside a group) or are themselves errors (top level).
    fn parse_seq(&mut self, in_group: bool) -> Result<Node, i32> {
        let mut atoms = Vec::new();
        loop {
            match self.peek() {
                None => break,
                Some(b')') | Some(b'|') if in_group => break,
                Some(b')') | Some(b'|') => return Err(ERROR_BAD_TEMPLATE),
                _ => atoms.push(self.parse_atom()?),
            }
        }
        Ok(if atoms.len() == 1 {
            atoms.pop().unwrap()
        } else {
            Node::Seq(atoms)
        })
    }

    fn parse_group(&mut self) -> Result<Node, i32> {
        let mut branches = vec![self.parse_seq(true)?];
        loop {
            match self.bump() {
                Some(b'|') => branches.push(self.parse_seq(true)?),
                Some(b')') => break,
                _ => return Err(ERROR_BAD_TEMPLATE),
            }
        }
        self.has_wildcard = true;
        Ok(if branches.len() == 1 {
            branches.pop().unwrap()
        } else {
            Node::Alt(branches)
        })
    }

    fn parse_class(&mut self) -> Result<Node, i32> {
        let negate = if self.peek() == Some(b'~') {
            self.bump();
            true
        } else {
            false
        };
        let mut ranges: Vec<(u8, u8)> = Vec::new();
        let mut first = true;
        loop {
            match self.peek() {
                None => return Err(ERROR_BAD_TEMPLATE),
                Some(b']') => {
                    self.bump();
                    break;
                }
                Some(b'-') if first => {
                    self.bump();
                    ranges.push((b'-', b'-'));
                }
                Some(lo) => {
                    self.bump();
                    if self.peek() == Some(b'-') && self.buf.get(self.pos + 1).is_some() {
                        self.bump();
                        if self.peek() == Some(b']') {
                            // Trailing dash: "up to the end of the ASCII range".
                            ranges.push((lo, 0x7f));
                        } else {
                            let hi = self.bump().ok_or(ERROR_BAD_TEMPLATE)?;
                            ranges.push((lo, hi));
                        }
                    } else {
                        ranges.push((lo, lo));
                    }
                }
            }
            first = false;
        }
        if ranges.is_empty() {
            return Err(ERROR_BAD_TEMPLATE);
        }
        self.has_wildcard = true;
        Ok(Node::Class { negate, ranges })
    }

    fn parse_atom(&mut self) -> Result<Node, i32> {
        match self.bump().ok_or(ERROR_BAD_TEMPLATE)? {
            b'?' => {
                self.has_wildcard = true;
                Ok(Node::Any)
            }
            b'#' => {
                self.has_wildcard = true;
                let inner = self.parse_atom()?;
                Ok(Node::Repeat(Box::new(inner)))
            }
            b'~' => {
                self.has_wildcard = true;
                let inner = self.parse_atom()?;
                Ok(Node::Not(Box::new(inner)))
            }
            b'%' => {
                self.has_wildcard = true;
                Ok(Node::Empty)
            }
            b'(' => self.parse_group(),
            b'[' => self.parse_class(),
            b')' | b']' | b'|' => Err(ERROR_BAD_TEMPLATE),
            b'\'' => match self.peek() {
                Some(c) if is_wildcard_char(c) => {
                    self.bump();
                    Ok(Node::Literal(c))
                }
                // Per the real semantics: an apostrophe not followed by
                // a wildcard character is not an escape at all -- it
                // stands for itself, and the next character (if any) is
                // parsed as its own following atom.
                _ => Ok(Node::Literal(b'\'')),
            },
            c => Ok(Node::Literal(c)),
        }
    }
}

/// Parses `source` into a [`Node`] tree, returning `(node, has_wildcard)`.
pub(crate) fn parse(source: &[u8]) -> Result<(Node, bool), i32> {
    let mut parser = Parser::new(source);
    let node = parser.parse_top()?;
    Ok((node, parser.has_wildcard))
}

// --- Self-delimiting byte encoding (module docs) ---

const OP_LITERAL: u8 = 0x00;
const OP_ANY: u8 = 0x01;
const OP_EMPTY: u8 = 0x02;
const OP_CLASS: u8 = 0x03;
const OP_SEQ: u8 = 0x04;
const OP_ALT: u8 = 0x05;
const OP_NOT: u8 = 0x06;
const OP_REPEAT: u8 = 0x07;

fn encode(node: &Node, out: &mut Vec<u8>) {
    match node {
        Node::Literal(c) => {
            out.push(OP_LITERAL);
            out.push(*c);
        }
        Node::Any => out.push(OP_ANY),
        Node::Empty => out.push(OP_EMPTY),
        Node::Class { negate, ranges } => {
            out.push(OP_CLASS);
            out.push(u8::from(*negate));
            out.push(ranges.len() as u8);
            for (lo, hi) in ranges {
                out.push(*lo);
                out.push(*hi);
            }
        }
        Node::Seq(nodes) => {
            out.push(OP_SEQ);
            out.push(nodes.len() as u8);
            for n in nodes {
                encode(n, out);
            }
        }
        Node::Alt(branches) => {
            out.push(OP_ALT);
            out.push(branches.len() as u8);
            for b in branches {
                encode(b, out);
            }
        }
        Node::Not(inner) => {
            out.push(OP_NOT);
            encode(inner, out);
        }
        Node::Repeat(inner) => {
            out.push(OP_REPEAT);
            encode(inner, out);
        }
    }
}

/// Decodes one [`Node`] starting at `buf[*pos]`, advancing `*pos` past
/// it. `None` on truncated/corrupt input (should never happen for
/// buffers this module's own `ParsePattern` wrote). Only used by the
/// round-trip test now -- [`decode_from_mem`] is what `MatchPattern`
/// actually calls, since it must read from guest memory rather than a
/// pre-fetched byte slice.
#[cfg(test)]
fn decode(buf: &[u8], pos: &mut usize) -> Option<Node> {
    let op = *buf.get(*pos)?;
    *pos += 1;
    match op {
        OP_LITERAL => {
            let c = *buf.get(*pos)?;
            *pos += 1;
            Some(Node::Literal(c))
        }
        OP_ANY => Some(Node::Any),
        OP_EMPTY => Some(Node::Empty),
        OP_CLASS => {
            let negate = *buf.get(*pos)? != 0;
            *pos += 1;
            let n = *buf.get(*pos)? as usize;
            *pos += 1;
            let mut ranges = Vec::with_capacity(n);
            for _ in 0..n {
                let lo = *buf.get(*pos)?;
                let hi = *buf.get(*pos + 1)?;
                *pos += 2;
                ranges.push((lo, hi));
            }
            Some(Node::Class { negate, ranges })
        }
        OP_SEQ => {
            let n = *buf.get(*pos)? as usize;
            *pos += 1;
            let mut nodes = Vec::with_capacity(n);
            for _ in 0..n {
                nodes.push(decode(buf, pos)?);
            }
            Some(Node::Seq(nodes))
        }
        OP_ALT => {
            let n = *buf.get(*pos)? as usize;
            *pos += 1;
            let mut branches = Vec::with_capacity(n);
            for _ in 0..n {
                branches.push(decode(buf, pos)?);
            }
            Some(Node::Alt(branches))
        }
        OP_NOT => decode(buf, pos).map(|n| Node::Not(Box::new(n))),
        OP_REPEAT => decode(buf, pos).map(|n| Node::Repeat(Box::new(n))),
        _ => None,
    }
}

/// Same decoding as [`decode`], but reading directly from guest memory
/// at `*addr` (advancing it past the node) rather than a pre-fetched
/// byte slice. Used instead of `read_c_string` + [`decode`] because the
/// tokenized encoding embeds raw `0x00` bytes ([`OP_LITERAL`]'s tag) that
/// a NUL-terminated read would truncate on.
fn decode_from_mem(mem: &dyn AddressSpace, addr: &mut u32) -> Option<Node> {
    let mut next = || {
        let b = mem.read_u8(*addr);
        *addr = addr.wrapping_add(1);
        b
    };
    let op = next();
    match op {
        OP_LITERAL => Some(Node::Literal(next())),
        OP_ANY => Some(Node::Any),
        OP_EMPTY => Some(Node::Empty),
        OP_CLASS => {
            let negate = next() != 0;
            let n = next() as usize;
            let mut ranges = Vec::with_capacity(n);
            for _ in 0..n {
                let lo = next();
                let hi = next();
                ranges.push((lo, hi));
            }
            Some(Node::Class { negate, ranges })
        }
        OP_SEQ => {
            let n = next() as usize;
            let mut nodes = Vec::with_capacity(n);
            for _ in 0..n {
                nodes.push(decode_from_mem(mem, addr)?);
            }
            Some(Node::Seq(nodes))
        }
        OP_ALT => {
            let n = next() as usize;
            let mut branches = Vec::with_capacity(n);
            for _ in 0..n {
                branches.push(decode_from_mem(mem, addr)?);
            }
            Some(Node::Alt(branches))
        }
        OP_NOT => decode_from_mem(mem, addr).map(|n| Node::Not(Box::new(n))),
        OP_REPEAT => decode_from_mem(mem, addr).map(|n| Node::Repeat(Box::new(n))),
        _ => None,
    }
}

// --- Matching ---

fn byte_eq(a: u8, b: u8, fold: bool) -> bool {
    if fold {
        amiga_toupper(a) == amiga_toupper(b)
    } else {
        a == b
    }
}

fn class_matches(c: u8, negate: bool, ranges: &[(u8, u8)], fold: bool) -> bool {
    let hit = ranges.iter().any(|&(lo, hi)| {
        if fold {
            let u = amiga_toupper(c);
            (amiga_toupper(lo)..=amiga_toupper(hi)).contains(&u) || (lo..=hi).contains(&c)
        } else {
            (lo..=hi).contains(&c)
        }
    });
    hit != negate
}

/// Tries every way `node` can consume input starting at `input[ii..]`,
/// calling `k(j)` for each resulting end position `j`; succeeds if any
/// call to `k` succeeds. Continuation-passing style so `Seq`/`Alt`/
/// `Repeat` compose correctly regardless of nesting.
fn match_node(node: &Node, input: &[u8], ii: usize, fold: bool, k: &dyn Fn(usize) -> bool) -> bool {
    match node {
        Node::Literal(c) => ii < input.len() && byte_eq(input[ii], *c, fold) && k(ii + 1),
        Node::Any => ii < input.len() && k(ii + 1),
        Node::Empty => k(ii),
        Node::Class { negate, ranges } => {
            ii < input.len() && class_matches(input[ii], *negate, ranges, fold) && k(ii + 1)
        }
        Node::Seq(nodes) => match_seq(nodes, 0, input, ii, fold, k),
        Node::Alt(branches) => branches.iter().any(|b| match_node(b, input, ii, fold, k)),
        Node::Not(inner) => {
            // Simplified whole-remainder scope -- see the module docs'
            // "~ scope" note.
            let inner_matches_rest = match_node(inner, input, ii, fold, &|j| j == input.len());
            if inner_matches_rest {
                false
            } else {
                k(input.len())
            }
        }
        Node::Repeat(inner) => {
            let seen = std::cell::RefCell::new(Vec::new());
            match_repeat(inner, input, ii, fold, k, &seen)
        }
    }
}

fn match_repeat(
    inner: &Node,
    input: &[u8],
    pos: usize,
    fold: bool,
    k: &dyn Fn(usize) -> bool,
    seen: &std::cell::RefCell<Vec<usize>>,
) -> bool {
    // Guards against infinite recursion on a zero-width inner match
    // (e.g. `#%`), which would otherwise revisit the same position
    // forever.
    if seen.borrow().contains(&pos) {
        return false;
    }
    seen.borrow_mut().push(pos);
    if k(pos) {
        return true;
    }
    match_node(inner, input, pos, fold, &|j| {
        match_repeat(inner, input, j, fold, k, seen)
    })
}

fn match_seq(
    nodes: &[Node],
    ni: usize,
    input: &[u8],
    ii: usize,
    fold: bool,
    k: &dyn Fn(usize) -> bool,
) -> bool {
    if ni == nodes.len() {
        return k(ii);
    }
    match_node(&nodes[ni], input, ii, fold, &|j| {
        match_seq(nodes, ni + 1, input, j, fold, k)
    })
}

pub(crate) fn full_match(node: &Node, input: &[u8], fold: bool) -> bool {
    match_node(node, input, 0, fold, &|j| j == input.len())
}

// --- LVO handlers ---

/// Shared implementation of `ParsePattern`/`ParsePatternNoCase` (they
/// differ only in whether case folds during a later `MatchPattern`,
/// which the tokenized encoding itself doesn't need to record -- the
/// caller uses the matching `MatchPattern`/`MatchPatternNoCase` half of
/// the pair, exactly as real programs are required to).
fn parse_pattern(
    mem: &mut dyn AddressSpace,
    dos: &mut DosState,
    source_ptr: u32,
    dest_addr: u32,
    dest_len: u32,
) -> i32 {
    let source = read_c_string(mem, source_ptr);
    let (node, has_wildcard) = match parse(&source) {
        Ok(v) => v,
        Err(code) => {
            dos.set_io_err(code);
            return -1;
        }
    };
    let mut bytes = Vec::new();
    encode(&node, &mut bytes);
    if bytes.len() as u32 > dest_len {
        dos.set_io_err(ERROR_LINE_TOO_LONG);
        return -1;
    }
    let mut addr = dest_addr;
    for b in bytes {
        mem.write_u8(addr, b);
        addr = addr.wrapping_add(1);
    }
    i32::from(has_wildcard)
}

/// Shared implementation of `MatchPattern`/`MatchPatternNoCase`.
fn match_pattern(
    mem: &dyn AddressSpace,
    dos: &mut DosState,
    pat_ptr: u32,
    str_ptr: u32,
    fold: bool,
) -> bool {
    let mut addr = pat_ptr;
    let Some(node) = decode_from_mem(mem, &mut addr) else {
        // Corrupt/foreign tokenized buffer -- fail closed rather than
        // panicking; there's no real IoErr() code for "not a pattern I
        // tokenized", so this just reports no-match.
        dos.set_io_err(0);
        return false;
    };
    let input = read_c_string(mem, str_ptr);
    let matched = full_match(&node, &input, fold);
    dos.set_io_err(0);
    matched
}

fn parse_pattern_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let source_ptr = ctx.cpu.data_register(DataRegister(1));
    let dest_addr = ctx.cpu.data_register(DataRegister(2));
    let dest_len = ctx.cpu.data_register(DataRegister(3));
    let result = parse_pattern(ctx.mem, ctx.dos, source_ptr, dest_addr, dest_len);
    ctx.cpu.set_data_register(DataRegister(0), result as u32);
    Ok(())
}

fn match_pattern_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let pat_ptr = ctx.cpu.data_register(DataRegister(1));
    let str_ptr = ctx.cpu.data_register(DataRegister(2));
    let matched = match_pattern(ctx.mem, ctx.dos, pat_ptr, str_ptr, false);
    ctx.cpu
        .set_data_register(DataRegister(0), u32::from(matched));
    Ok(())
}

fn match_pattern_no_case_handler<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
) -> Result<(), DispatchError> {
    let pat_ptr = ctx.cpu.data_register(DataRegister(1));
    let str_ptr = ctx.cpu.data_register(DataRegister(2));
    let matched = match_pattern(ctx.mem, ctx.dos, pat_ptr, str_ptr, true);
    ctx.cpu
        .set_data_register(DataRegister(0), u32::from(matched));
    Ok(())
}

/// Registers `ParsePattern`/`ParsePatternNoCase`/`MatchPattern`/
/// `MatchPatternNoCase` onto [`DOS_LIBRARY_BASE`], looked up by name
/// through [`DOS_LVOS`]. Called from [`crate::dispatch::Runtime::new`]
/// alongside the other `dos.library` registrations; these handlers
/// don't need a `Vfs` or any other runtime state beyond `IoErr()`.
pub fn register_dospattern_handlers<C: Cpu + 'static>(
    table: &mut LibraryTable<C>,
    mem: &mut C::Memory,
) {
    macro_rules! reg {
        ($name:literal, $handler:expr) => {
            table
                .register_by_name(
                    mem,
                    DOS_LIBRARY_BASE,
                    DOS_LVOS,
                    "dos.library",
                    $name,
                    $handler,
                )
                .unwrap_or_else(|e| panic!("{} should be in DOS_LVOS: {e}", $name));
        };
    }
    reg!("ParsePattern", parse_pattern_handler::<C>);
    reg!("ParsePatternNoCase", parse_pattern_handler::<C>);
    reg!("MatchPattern", match_pattern_handler::<C>);
    reg!("MatchPatternNoCase", match_pattern_no_case_handler::<C>);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guestmem::write_c_string;
    use crate::memory::FlatMemory;

    fn m(pattern: &str, s: &str, fold: bool) -> bool {
        let (node, _) = parse(pattern.as_bytes()).unwrap_or_else(|e| panic!("parse failed: {e}"));
        full_match(&node, s.as_bytes(), fold)
    }

    #[test]
    fn any_char_and_literal() {
        assert!(m("a?c", "abc", false));
        assert!(!m("a?c", "ac", false));
        assert!(m("abc", "abc", false));
    }

    #[test]
    fn hash_matches_zero_or_more() {
        assert!(m("#?", "", false));
        assert!(m("#?", "anything", false));
        assert!(m("#?.info", "foo.info", false));
        assert!(!m("#?.info", "foo.info.old", false));
    }

    #[test]
    fn hash_group_repeats_the_whole_group() {
        assert!(m("#(ab)", "", false));
        assert!(m("#(ab)", "ab", false));
        assert!(m("#(ab)", "ababab", false));
        assert!(!m("#(ab)", "aba", false));
    }

    #[test]
    fn alternation_and_empty() {
        assert!(m("(a|b)", "a", false));
        assert!(m("(a|b)", "b", false));
        assert!(!m("(a|b)", "c", false));
        assert!(m("Tool(%|.info)", "Tool", false));
        assert!(m("Tool(%|.info)", "Tool.info", false));
    }

    #[test]
    fn character_class_ranges_and_negation() {
        assert!(m("[a-z]", "m", false));
        assert!(!m("[a-z]", "M", false));
        assert!(m("[ab]", "a", false));
        assert!(m("[a-cx-z]", "y", false));
        assert!(!m("[a-cx-z]", "m", false));
        assert!(m("[~a-z]", "M", false));
        assert!(!m("[~a-z]", "m", false));
        assert!(m("[-a-c]", "-", false));
    }

    #[test]
    fn not_excludes_the_whole_pattern() {
        assert!(m("~(#?.info)", "foo.c", false));
        assert!(!m("~(#?.info)", "foo.info", false));
    }

    #[test]
    fn escape_only_activates_before_a_wildcard_char() {
        // 'a where 'a' is not a wildcard char: the apostrophe is
        // literal too, so the whole thing is a two-char literal match.
        assert!(m("'a", "'a", false));
        assert!(!m("'a", "a", false));
        // '? escapes the question mark into a literal.
        assert!(m("'?", "?", false));
        assert!(!m("'?", "x", false));
    }

    #[test]
    fn no_case_folds_ascii() {
        assert!(m("FOO", "foo", true));
        assert!(!m("FOO", "foo", false));
        assert!(m("[a-z]", "M", true));
    }

    #[test]
    fn bad_template_errors() {
        assert_eq!(parse(b"(a").unwrap_err(), ERROR_BAD_TEMPLATE);
        assert_eq!(parse(b"a)").unwrap_err(), ERROR_BAD_TEMPLATE);
        assert_eq!(parse(b"a|b").unwrap_err(), ERROR_BAD_TEMPLATE);
        assert_eq!(parse(b"[a-").unwrap_err(), ERROR_BAD_TEMPLATE);
    }

    #[test]
    fn has_wildcard_flag() {
        assert!(!parse(b"plain").unwrap().1);
        assert!(parse(b"has?wild").unwrap().1);
        assert!(!parse(b"'?").unwrap().1);
    }

    #[test]
    fn encode_decode_round_trip() {
        // Exercises Not, Repeat, Class and Alt all at once.
        let (node, _) = parse(b"(~(#?.info)|[a-z])").expect("parse");
        let mut bytes = Vec::new();
        encode(&node, &mut bytes);
        let mut pos = 0;
        let decoded = decode(&bytes, &mut pos).expect("decode");
        assert_eq!(decoded, node);
    }

    // --- End-to-end: real A-line trap dispatch ---

    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{Runtime, StartConfig};

    fn move_imm_to_d(n: u16) -> u16 {
        0x203C | (n << 9)
    }
    fn jsr_disp16(an: u16) -> u16 {
        0x4EA8 | an
    }
    const RTS: u16 = 0x4E75;

    fn push_move_imm_to_d(words: &mut Vec<u16>, dn: u16, imm: u32) -> usize {
        let idx = words.len();
        words.push(move_imm_to_d(dn));
        words.push((imm >> 16) as u16);
        words.push(imm as u16);
        idx
    }
    fn push_jsr(words: &mut Vec<u16>, an: u16, disp: i32) {
        words.push(jsr_disp16(an));
        words.push(disp as u16);
    }
    fn patch_imm32(words: &mut [u16], idx: usize, value: u32) {
        words[idx + 1] = (value >> 16) as u16;
        words[idx + 2] = value as u16;
    }
    fn load_words(mem: &mut FlatMemory, addr: u32, words: &[u16]) {
        let mut offset = addr;
        for &w in words {
            mem.write_u16(offset, w);
            offset += 2;
        }
    }

    #[test]
    fn end_to_end_parse_then_match_via_trap_dispatch() {
        // D1=source, D2=dest buf, D3=dest len; jsr ParsePattern(a6);
        // then D1=dest buf, D2=string; jsr MatchPattern(a6); D0 (== the
        // exit code) is the match result.
        let mut words = Vec::new();
        let source_idx = push_move_imm_to_d(&mut words, 1, 0);
        let dest_idx = push_move_imm_to_d(&mut words, 2, 0);
        push_move_imm_to_d(&mut words, 3, 64);
        push_jsr(&mut words, 6, -840); // ParsePattern(a6)
        let dest_reload_idx = push_move_imm_to_d(&mut words, 1, 0);
        let str_idx = push_move_imm_to_d(&mut words, 2, 0);
        push_jsr(&mut words, 6, -846); // MatchPattern(a6)
        words.push(RTS);

        let source = b"#?.info";
        let s = b"icon.info";
        let source_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        let dest_addr = source_addr + source.len() as u32 + 1;
        let str_addr = dest_addr + 64;
        patch_imm32(&mut words, source_idx, source_addr);
        patch_imm32(&mut words, dest_idx, dest_addr);
        patch_imm32(&mut words, dest_reload_idx, dest_addr);
        patch_imm32(&mut words, str_idx, str_addr);

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);
        write_c_string(&mut mem, source_addr, source);
        write_c_string(&mut mem, str_addr, s);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: str_addr + s.len() as u32 + 4,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 1, "icon.info should match #?.info");
    }
}
