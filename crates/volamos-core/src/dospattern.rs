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
//! `ParsePattern`/`MatchPattern` as a pair" and calls the byte encoding
//! internal (ISO-Latin-1 C1 control codes, "should be considered
//! internal"). An earlier version of this module took that at face
//! value and used an arbitrary length-prefixed binary serialization
//! instead -- until the real Workbench 3.1.4 `Rename` binary broke
//! against it: real `ParsePattern`'s actual, empirically-observed
//! property is that for a pattern with **no wildcards at all**, the
//! tokenized output is byte-for-byte identical to the input string
//! (plus a `NUL` terminator) -- i.e. real programs *do* rely on being
//! able to reuse a `ParsePattern` destination buffer as a plain
//! `STRPTR` when there was nothing to tokenize, and `Rename` is exactly
//! such a program (it `ParsePattern`s its source-name argument, then
//! passes that same buffer's pointer straight to `Rename()`'s own
//! `oldName` argument).
//!
//! This module now matches that property directly: [`encode`] is a
//! literal transliteration of the original wildcard syntax --
//! ordinary characters (`Node::Literal`) are written as themselves, and
//! only the wildcard *operators* (`?`, `#`, `~`, `%`, `(`/`|`/`)`,
//! `[`/`]`) become single reserved bytes in the `0x80`-`0x9F` C1
//! control range, which can never legally appear in a real AmigaOS path
//! character (RKRM `paths-and-filenames.md`: printable characters are
//! `0x20`-`0x7e` and `0xa0`-`0xff` only). The encoding is
//! self-terminating on a trailing `0x00` (which also can never appear
//! in a literal, since the source is always read via
//! [`read_c_string`]), so [`decode_from_mem`] never needs a length
//! prefix either -- structurally, this mirrors the original pattern
//! text almost exactly, just with each operator character swapped for
//! its reserved-byte equivalent.

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

// --- Byte encoding: a literal transliteration (module docs) ---

/// `?`.
const C_ANY: u8 = 0x80;
/// `%`.
const C_EMPTY: u8 = 0x81;
/// `#`, prefixing the one atom it repeats.
const C_REPEAT: u8 = 0x82;
/// `~`, prefixing the one atom it negates.
const C_NOT: u8 = 0x83;
/// `(`.
const C_GROUP_START: u8 = 0x84;
/// `|`.
const C_ALT_SEP: u8 = 0x85;
/// `)`.
const C_GROUP_END: u8 = 0x86;
/// `[` (or `[~` -- see [`C_CLASS_NEGATE`]).
const C_CLASS_START: u8 = 0x87;
/// The `~` immediately after `[` negating a class; only ever appears
/// right after [`C_CLASS_START`].
const C_CLASS_NEGATE: u8 = 0x88;
/// `]`.
const C_CLASS_END: u8 = 0x89;
/// The `-` inside a class denoting a range (`lo`-`hi`); a single-byte
/// class member is written without one.
const C_RANGE_DASH: u8 = 0x8A;

fn encode(node: &Node, out: &mut Vec<u8>) {
    match node {
        Node::Literal(c) => out.push(*c),
        Node::Any => out.push(C_ANY),
        Node::Empty => out.push(C_EMPTY),
        Node::Class { negate, ranges } => {
            out.push(C_CLASS_START);
            if *negate {
                out.push(C_CLASS_NEGATE);
            }
            for (lo, hi) in ranges {
                out.push(*lo);
                if hi != lo {
                    out.push(C_RANGE_DASH);
                    out.push(*hi);
                }
            }
            out.push(C_CLASS_END);
        }
        Node::Seq(nodes) => {
            for n in nodes {
                encode(n, out);
            }
        }
        Node::Alt(branches) => {
            out.push(C_GROUP_START);
            for (i, b) in branches.iter().enumerate() {
                if i > 0 {
                    out.push(C_ALT_SEP);
                }
                encode(b, out);
            }
            out.push(C_GROUP_END);
        }
        Node::Not(inner) => {
            out.push(C_NOT);
            encode_prefixed_atom(inner, out);
        }
        Node::Repeat(inner) => {
            out.push(C_REPEAT);
            encode_prefixed_atom(inner, out);
        }
    }
}

/// Encodes the single atom a `#`/`~` prefix applies to. Both operators'
/// grammar (`parse_atom`) only ever consumes exactly one following atom
/// -- the only way that atom ends up being a bare, multi-child
/// `Node::Seq` (rather than some other self-delimiting single node) is
/// via a parenthesized group whose single branch was collapsed
/// (`parse_group`'s "if branches.len() == 1" case, e.g. `~(#?.info)`).
/// [`encode`] alone would concatenate that `Seq`'s children with no
/// boundary, making them indistinguishable from separate atoms
/// *outside* the `#`/`~`'s scope once decoded -- so a bare `Seq` here
/// is re-wrapped in the same group markers the original `(...)` would
/// have produced, giving [`decode_atom`]'s existing `C_GROUP_START`
/// case (which already collapses a single branch back to a bare `Seq`)
/// something it can decode as one atom. Every other node kind already
/// self-delimits and needs no wrapping.
fn encode_prefixed_atom(inner: &Node, out: &mut Vec<u8>) {
    if matches!(inner, Node::Seq(_)) {
        out.push(C_GROUP_START);
        encode(inner, out);
        out.push(C_GROUP_END);
    } else {
        encode(inner, out);
    }
}

/// A one-byte-lookahead source of encoded-pattern bytes, generic over
/// where those bytes actually live ([`SliceSource`] for the round-trip
/// test, [`MemSource`] for [`decode_from_mem`]/`MatchPattern`). Also
/// enforces a byte budget so a corrupt/foreign buffer (not written by
/// this module's own `ParsePattern`) can't send decoding into an
/// unbounded scan of guest memory looking for a terminator that will
/// never come.
trait ByteSource {
    fn next(&mut self) -> u8;
}

#[cfg(test)]
struct SliceSource<'a> {
    buf: &'a [u8],
    pos: usize,
}

#[cfg(test)]
impl ByteSource for SliceSource<'_> {
    fn next(&mut self) -> u8 {
        let b = self.buf.get(self.pos).copied().unwrap_or(0);
        if self.pos < self.buf.len() {
            self.pos += 1;
        }
        b
    }
}

struct MemSource<'a> {
    mem: &'a dyn AddressSpace,
    addr: u32,
}

impl ByteSource for MemSource<'_> {
    fn next(&mut self) -> u8 {
        let b = self.mem.read_u8(self.addr);
        self.addr = self.addr.wrapping_add(1);
        b
    }
}

/// Max bytes a single decode may consume, guarding against a
/// corrupt/foreign buffer with no `0x00` terminator ever turning up.
const DECODE_BYTE_BUDGET: u32 = 4096;

struct Stream<S: ByteSource> {
    src: S,
    peeked: Option<u8>,
    budget: u32,
}

impl<S: ByteSource> Stream<S> {
    fn new(src: S) -> Self {
        Self {
            src,
            peeked: None,
            budget: DECODE_BYTE_BUDGET,
        }
    }

    fn peek(&mut self) -> Option<u8> {
        if self.peeked.is_none() {
            if self.budget == 0 {
                return None;
            }
            self.peeked = Some(self.src.next());
        }
        self.peeked
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.peeked = None;
        self.budget -= 1;
        Some(b)
    }
}

/// Decodes a run of atoms until a `0x00`/[`C_ALT_SEP`]/[`C_GROUP_END`]
/// terminator (consumed), returning the built [`Node`] and which byte
/// ended it.
fn decode_seq<S: ByteSource>(s: &mut Stream<S>) -> Option<(Node, u8)> {
    let mut atoms = Vec::new();
    loop {
        let b = s.peek()?;
        if b == 0 || b == C_ALT_SEP || b == C_GROUP_END {
            s.bump();
            let node = if atoms.len() == 1 {
                atoms.pop().unwrap()
            } else {
                Node::Seq(atoms)
            };
            return Some((node, b));
        }
        atoms.push(decode_atom(s)?);
    }
}

fn decode_atom<S: ByteSource>(s: &mut Stream<S>) -> Option<Node> {
    match s.bump()? {
        C_ANY => Some(Node::Any),
        C_EMPTY => Some(Node::Empty),
        C_REPEAT => decode_atom(s).map(|n| Node::Repeat(Box::new(n))),
        C_NOT => decode_atom(s).map(|n| Node::Not(Box::new(n))),
        C_GROUP_START => {
            let mut branches = Vec::new();
            loop {
                let (branch, term) = decode_seq(s)?;
                branches.push(branch);
                if term == C_GROUP_END {
                    break;
                }
                if term != C_ALT_SEP {
                    return None; // hit 0x00 (unterminated group) -- malformed
                }
            }
            Some(if branches.len() == 1 {
                branches.pop().unwrap()
            } else {
                Node::Alt(branches)
            })
        }
        C_CLASS_START => {
            let negate = if s.peek()? == C_CLASS_NEGATE {
                s.bump();
                true
            } else {
                false
            };
            let mut ranges = Vec::new();
            loop {
                if s.peek()? == C_CLASS_END {
                    s.bump();
                    break;
                }
                let lo = s.bump()?;
                if s.peek()? == C_RANGE_DASH {
                    s.bump();
                    let hi = s.bump()?;
                    ranges.push((lo, hi));
                } else {
                    ranges.push((lo, lo));
                }
            }
            Some(Node::Class { negate, ranges })
        }
        c => Some(Node::Literal(c)),
    }
}

/// Decodes one top-level [`Node`] starting at `buf[*pos]`, advancing
/// `*pos` past it (including the terminating `0x00`). `None` on
/// truncated/corrupt input (should never happen for buffers this
/// module's own `ParsePattern` wrote). Only used by the round-trip test
/// now -- [`decode_from_mem`] is what `MatchPattern` actually calls,
/// since it must read from guest memory rather than a pre-fetched byte
/// slice.
#[cfg(test)]
fn decode(buf: &[u8], pos: &mut usize) -> Option<Node> {
    let mut stream = Stream::new(SliceSource { buf, pos: *pos });
    let (node, _) = decode_seq(&mut stream)?;
    *pos = stream.src.pos;
    Some(node)
}

/// Same decoding as [`decode`], but reading directly from guest memory
/// at `*addr` (advancing it past the node, including the terminating
/// `0x00`) rather than a pre-fetched byte slice.
fn decode_from_mem(mem: &dyn AddressSpace, addr: &mut u32) -> Option<Node> {
    let mut stream = Stream::new(MemSource { mem, addr: *addr });
    let (node, _) = decode_seq(&mut stream)?;
    *addr = stream.src.addr;
    Some(node)
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
    bytes.push(0); // terminator -- see the module docs' "Tokenized encoding" section
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

    #[test]
    fn a_pattern_with_no_wildcard_encodes_to_its_own_literal_bytes() {
        // The real property (confirmed against the real Workbench
        // 3.1.4 Rename binary, which reuses ParsePattern's own output
        // buffer as a plain STRPTR for a non-wildcard name): with no
        // wildcard characters at all, the tokenized encoding is
        // byte-for-byte identical to the source, NUL-terminated.
        let source = b"WORK:hello2.txt";
        let (node, has_wildcard) = parse(source).expect("parse");
        assert!(!has_wildcard);
        let mut bytes = Vec::new();
        encode(&node, &mut bytes);
        bytes.push(0);
        assert_eq!(&bytes[..bytes.len() - 1], source);
        assert_eq!(*bytes.last().unwrap(), 0);
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
