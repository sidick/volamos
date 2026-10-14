//! UAE's "debug sprintf" port at `0xBFFF00` (`--uae-debug`, issue #140).
//!
//! A memory-mapped debug channel that WinUAE/amiberry expose when their
//! `debug_mem` option is on: a guest emits formatted text using nothing
//! but stores, no library call. This module is a port of
//! `debug_sprintf()`/`debug_sprintf_do()` from amiberry's `src/debug.cpp`
//! (with the address routing of `cia_debug()` in `src/cia.cpp`) -- that
//! source is the specification; where this file deliberately departs from
//! it, the departure is listed under "Deviations" below.
//!
//! # Protocol
//!
//! All stores, relative to [`DEBUG_SPRINTF_ADDRESS`]:
//!
//! - `+0`: push an argument (value and access size) onto a 32-entry
//!   stack. Stores beyond the 32nd are dropped. Also switches argument
//!   fetching back to the stack (see `+8`).
//! - `+4`, long: the value is a pointer to a NUL-terminated format
//!   string. It is formatted using the stacked arguments, the text is
//!   written to the sink, and the stack is cleared. A non-long store here
//!   is ignored.
//! - `+8`, long: the value is a pointer to a `va_list`-style argument
//!   block in guest memory. From now on (until the next `+0` push) every
//!   format starts reading its arguments from that pointer, packed with
//!   no padding and no promotion: a plain integer argument occupies 2
//!   bytes, an `l` integer 4, a string/BSTR/`%p` pointer 4. A non-long
//!   store here is ignored.
//!
//! On a 68000/68010 a long store is two word bus cycles, and UAE sees
//! them as such: the word at `+0`/`+4`/`+8` is latched, and the word at
//! the following `+2`/`+6`/`+10` is combined with it. [`DebugSprintf`]
//! reproduces that (the CPU model decides, see
//! [`DebugSprintf::new`]); on a 68020 or later only the exact offsets
//! `+0`/`+4`/`+8` are routed and a long arrives whole.
//!
//! # Format language
//!
//! `%%`, `%c`, `%s` (C string), `%b` (BSTR), `%p` (printed `$%08x`),
//! `%x %X %d %i %u %o`, with C-style flags (`-+ #0`), width and precision
//! in between. **Integers are 16-bit unless an `l` appears in the
//! directive** (`%d` of a word sign-extends it). `%c`/`%s`/`%b` and
//! friends obey the same flags/width/precision as C `printf`. `h` is
//! accepted and ignored (16-bit is already the default).
//!
//! Strings are read through the checked memory API, at most
//! [`MAX_DPATH`]` - 1` bytes, and an unmapped byte reads as the
//! terminator. The formatted output is capped at the same length.
//!
//! # Deviations from UAE
//!
//! - **Missing arguments read as 0.** UAE walks off the end of its
//!   argument stack and prints whatever stale values earlier messages
//!   left there (and past 32 entries, host memory). Here a directive
//!   with no argument left formats `0` and a warning is emitted.
//! - **Malformed directives are rejected loudly.** UAE appends every
//!   unrecognised character to a host `printf` format string and
//!   hands it to `snprintf` (undefined behaviour for `%z`, `%*d`, ...).
//!   Here a directive containing anything other than the characters above
//!   is printed literally (the `%` and everything up to the offending
//!   character) with a warning, consuming no argument.
//! - **`%[...]` custom directives are not supported.** UAE implements
//!   exactly one, `%[CYCLES]x`, which prints emulated CPU cycles since the
//!   previous message; volamos has no cycle counter on the execution path
//!   this mode uses. A recognised `%[name]c` form is consumed and prints
//!   nothing (as UAE does for an unknown name), plus a warning. A `%[`
//!   with no closing `]` and a following character is dropped like UAE
//!   drops it.
//! - **Output is capped at `MAX_DPATH - 1` bytes.** UAE bounds its
//!   conversions but not literal characters, so a long format string
//!   can overflow its buffer; here everything is simply truncated.
//! - **68000 `+8` store:** UAE stores the raw second word (`val`) as the
//!   `va_list` pointer instead of the combined long; here the combined
//!   value is used. The `+10` word also doesn't pop a stack entry (UAE
//!   pops one, an artefact of sharing the `+0` code path).
//! - **Latch hygiene:** a completed `+8` pair clears the word latch,
//!   as `+0`/`+4` pairs do.
//!
//! # Reads
//!
//! The port is write-only, as in UAE (`cia_debug` only hooks the
//! `*_put` handlers): reads of the region fall through to whatever is in
//! memory there. Stores are intercepted by [`crate::memory::FlatMemory`]'s
//! [`m68k::AddressBus`] implementation (see `crate::backend`) *before* any
//! bounds or sanitizer check, so the port exists at the same address
//! whatever `--ram` is, and is never reported as a violation.

use std::cell::RefCell;
use std::rc::Rc;

pub use crate::memory::StoreSize;
use crate::memory::{AddressSpace, StorePort};

/// Base address of the port (`DEBUG_SPRINTF_ADDRESS` in UAE's
/// `debug.h`).
pub const DEBUG_SPRINTF_ADDRESS: u32 = 0x00BF_FF00;

/// Bytes of address space the port claims: three longs at `+0`, `+4`,
/// `+8`. The heap keeps this range away from guests, see
/// [`crate::guestmem::GuestHeap::reserve_range`].
pub const DEBUG_SPRINTF_SIZE: u32 = 12;

/// UAE's `MAX_DPATH`: the cap on format strings, `%s`/`%b` strings and
/// output length.
pub const MAX_DPATH: usize = 4096;

/// Entries in the argument stack (`DEBUGSPRINTF_SIZE`).
pub const STACK_ENTRIES: usize = 32;

/// One stacked argument: the stored value and how wide the store was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StackEntry {
    /// The value written to the port.
    pub value: u32,
    /// The width of the store that pushed it.
    pub size: StoreSize,
}

/// Where formatted text and diagnostics go.
pub trait DebugSink {
    /// Formatted guest output, exactly as formatted (no prefix, no
    /// newline added).
    fn text(&mut self, bytes: &[u8]);
    /// A diagnostic about a directive the formatter could not honour.
    fn warning(&mut self, message: &str);
}

/// The production sink: guest text to stderr verbatim, warnings as
/// `volamos: uae-debug: ...` lines.
#[derive(Debug, Default)]
pub struct StderrSink;

impl DebugSink for StderrSink {
    fn text(&mut self, bytes: &[u8]) {
        use std::io::Write;
        let _ = std::io::stderr().write_all(bytes);
    }
    fn warning(&mut self, message: &str) {
        eprintln!("volamos: uae-debug: {message}");
    }
}

/// What a [`CaptureSink`] has received.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Captured {
    /// Concatenated guest output.
    pub text: Vec<u8>,
    /// Warnings, in order.
    pub warnings: Vec<String>,
}

/// A sink that records everything into shared storage, for tests (clone
/// the handle before boxing it into the memory).
#[derive(Debug, Default, Clone)]
pub struct CaptureSink(pub Rc<RefCell<Captured>>);

impl CaptureSink {
    /// The output so far, lossily as UTF-8.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.borrow().text).into_owned()
    }
    /// The warnings so far.
    pub fn warnings(&self) -> Vec<String> {
        self.0.borrow().warnings.clone()
    }
}

impl DebugSink for CaptureSink {
    fn text(&mut self, bytes: &[u8]) {
        self.0.borrow_mut().text.extend_from_slice(bytes);
    }
    fn warning(&mut self, message: &str) {
        self.0.borrow_mut().warnings.push(message.to_string());
    }
}

/// Where a format's arguments come from.
#[derive(Debug, Clone, Copy)]
pub enum Args<'a> {
    /// The pushed-argument stack (already truncated to its live entries).
    Stack(&'a [StackEntry]),
    /// A packed `va_list`-style block at this guest address.
    VaList(u32),
}

/// The port's state machine. Owned by [`crate::memory::FlatMemory`]
/// when `--uae-debug` is on.
pub struct DebugSprintf {
    /// CPU is a 68000/68010: long stores are word pairs.
    word_pairs: bool,
    stack: [StackEntry; STACK_ENTRIES],
    count: usize,
    latch: u16,
    latched: bool,
    va: u32,
    va_mode: bool,
    sink: Box<dyn DebugSink>,
}

impl std::fmt::Debug for DebugSprintf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DebugSprintf")
            .field("word_pairs", &self.word_pairs)
            .field("count", &self.count)
            .field("va_mode", &self.va_mode)
            .finish_non_exhaustive()
    }
}

const EMPTY_ENTRY: StackEntry = StackEntry {
    value: 0,
    size: StoreSize::Long,
};

impl DebugSprintf {
    /// Creates the port. `word_pairs` is true for CPUs whose long stores
    /// reach the bus as two word cycles (68000/68010, see
    /// [`crate::backend::splits_long_stores`]); it also widens the set
    /// of routed offsets to `+2/+6/+10`.
    pub fn new(word_pairs: bool, sink: Box<dyn DebugSink>) -> Self {
        Self {
            word_pairs,
            stack: [EMPTY_ENTRY; STACK_ENTRIES],
            count: 0,
            latch: 0,
            latched: false,
            va: 0,
            va_mode: false,
            sink,
        }
    }

    /// Whether long stores are word pairs (see [`Self::new`]).
    pub fn word_pairs(&self) -> bool {
        self.word_pairs
    }

    /// One routed store, after a long on a word-pair CPU has been split.
    fn store_one(&mut self, mem: &dyn AddressSpace, addr: u32, value: u32, size: StoreSize) {
        let off = addr.wrapping_sub(DEBUG_SPRINTF_ADDRESS);
        let mut v = value;
        let mut size = size;
        if size == StoreSize::Word && self.word_pairs {
            v &= 0xFFFF;
            if off & 2 == 0 {
                self.latch = v as u16;
                self.latched = true;
            } else if self.latched {
                v |= u32::from(self.latch) << 16;
                size = StoreSize::Long;
                if off & 12 == 0 && self.count > 0 {
                    // The first half was pushed as a word; the combined
                    // long replaces it.
                    self.count -= 1;
                }
            }
        }
        if size != StoreSize::Word {
            self.latched = false;
        }
        match off & 12 {
            4 => {
                if size != StoreSize::Long {
                    return;
                }
                self.print(mem, v);
                self.count = 0;
            }
            8 => {
                if size != StoreSize::Long {
                    return;
                }
                self.va = v;
                self.va_mode = true;
            }
            _ => {
                if self.count < STACK_ENTRIES {
                    self.stack[self.count] = StackEntry { value: v, size };
                    self.count += 1;
                }
                self.va_mode = false;
            }
        }
    }

    fn print(&mut self, mem: &dyn AddressSpace, fmt_ptr: u32) {
        let format = read_string(mem, fmt_ptr, MAX_DPATH - 1);
        let args = if self.va_mode {
            Args::VaList(self.va)
        } else {
            Args::Stack(&self.stack[..self.count])
        };
        let mut warnings = Vec::new();
        let out = format_message(&format, mem, args, &mut warnings);
        for w in &warnings {
            self.sink.warning(w);
        }
        if !out.is_empty() {
            self.sink.text(&out);
        }
    }
}

impl StorePort for DebugSprintf {
    /// UAE's `cia_debug` address test: `+0/+4/+8`, plus `+2/+6/+10` on a
    /// word-pair CPU.
    #[inline]
    fn claims(&self, addr: u32) -> bool {
        match addr.wrapping_sub(DEBUG_SPRINTF_ADDRESS) {
            0 | 4 | 8 => true,
            2 | 6 | 10 => self.word_pairs,
            _ => false,
        }
    }

    /// `mem` is read for format strings and `%s`/`%b` arguments. On a
    /// word-pair CPU a long store is delivered as UAE would see it, two
    /// word stores.
    fn store(&mut self, mem: &dyn AddressSpace, addr: u32, value: u32, size: StoreSize) {
        if size == StoreSize::Long && self.word_pairs {
            self.store_one(mem, addr, value >> 16, StoreSize::Word);
            let low = addr.wrapping_add(2);
            if self.claims(low) {
                self.store_one(mem, low, value & 0xFFFF, StoreSize::Word);
            }
        } else {
            self.store_one(mem, addr, value, size);
        }
    }
}

/// Installs the debug sprintf port on `mem` for a CPU of `cpu_type`,
/// writing to `sink`. Pair with
/// [`crate::dispatch::Runtime::reserve_uae_debug_port`] on the runtime.
pub fn install(
    mem: &mut crate::memory::FlatMemory,
    cpu_type: m68k::CpuType,
    sink: Box<dyn DebugSink>,
) {
    let word_pairs = crate::backend::splits_long_stores(cpu_type);
    mem.add_store_port(Box::new(DebugSprintf::new(word_pairs, sink)));
}

/// Reads a NUL-terminated string of at most `max` bytes through the
/// checked memory API; an unmapped byte reads as 0 and so terminates it.
pub fn read_string(mem: &dyn AddressSpace, addr: u32, max: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..max {
        let c = mem.read_u8(addr.wrapping_add(i as u32));
        if c == 0 {
            break;
        }
        out.push(c);
    }
    out
}

/// Reads a BSTR (length byte, then that many characters), mapping NUL
/// characters to `.` as UAE does.
pub fn read_bstring(mem: &dyn AddressSpace, addr: u32, max: usize) -> Vec<u8> {
    let len = usize::from(mem.read_u8(addr)).min(max);
    (1..=len)
        .map(|i| match mem.read_u8(addr.wrapping_add(i as u32)) {
            0 => b'.',
            c => c,
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Want {
    Word,
    Long,
}

struct ArgReader<'a> {
    mem: &'a dyn AddressSpace,
    args: Args<'a>,
    next: usize,
    ptr: u32,
    starved: bool,
}

impl ArgReader<'_> {
    fn get(&mut self, want: Want) -> u32 {
        match self.args {
            Args::VaList(_) => {
                let (v, step) = match want {
                    Want::Long => (self.mem.read_u32(self.ptr), 4),
                    Want::Word => (u32::from(self.mem.read_u16(self.ptr)), 2),
                };
                self.ptr = self.ptr.wrapping_add(step);
                v
            }
            Args::Stack(entries) => {
                let Some(e) = entries.get(self.next) else {
                    self.starved = true;
                    return 0;
                };
                self.next += 1;
                let mut v = e.value;
                match e.size {
                    StoreSize::Byte => v &= 0xFF,
                    StoreSize::Word => v &= 0xFFFF,
                    StoreSize::Long => {}
                }
                if want == Want::Word {
                    v &= 0xFFFF;
                }
                v
            }
        }
    }
}

/// A parsed `%` directive's flags, width and precision.
#[derive(Debug, Default)]
struct Spec {
    minus: bool,
    plus: bool,
    space: bool,
    hash: bool,
    zero: bool,
    width: usize,
    precision: Option<usize>,
}

fn parse_spec(raw: &[u8]) -> Option<Spec> {
    let mut spec = Spec::default();
    let mut i = 0;
    while let Some(&c) = raw.get(i) {
        match c {
            b'-' => spec.minus = true,
            b'+' => spec.plus = true,
            b' ' => spec.space = true,
            b'#' => spec.hash = true,
            b'0' => spec.zero = true,
            _ => break,
        }
        i += 1;
    }
    let digits = |i: &mut usize| {
        let mut n = 0usize;
        while let Some(d) = raw.get(*i).filter(|c| c.is_ascii_digit()) {
            n = (n * 10 + usize::from(d - b'0')).min(MAX_DPATH);
            *i += 1;
        }
        n
    };
    spec.width = digits(&mut i);
    if raw.get(i) == Some(&b'.') {
        i += 1;
        spec.precision = Some(digits(&mut i));
    }
    (i == raw.len()).then_some(spec)
}

fn pad(spec: &Spec, body: Vec<u8>, zero_ok: bool, sign_len: usize) -> Vec<u8> {
    if body.len() >= spec.width {
        return body;
    }
    let fill = spec.width - body.len();
    let mut out = Vec::with_capacity(spec.width);
    if spec.minus {
        out.extend(body);
        out.resize(spec.width, b' ');
    } else if spec.zero && zero_ok {
        // Zeros go between the sign/radix prefix and the digits.
        out.extend_from_slice(&body[..sign_len]);
        out.resize(sign_len + fill, b'0');
        out.extend_from_slice(&body[sign_len..]);
    } else {
        out.resize(fill, b' ');
        out.extend(body);
    }
    out
}

fn format_int(spec: &Spec, conv: u8, value: u32, signed_value: i32) -> Vec<u8> {
    let (mut digits, negative) = match conv {
        b'd' | b'i' => (
            signed_value.unsigned_abs().to_string().into_bytes(),
            signed_value < 0,
        ),
        b'u' => (value.to_string().into_bytes(), false),
        b'x' => (format!("{value:x}").into_bytes(), false),
        b'X' => (format!("{value:X}").into_bytes(), false),
        _ => (format!("{value:o}").into_bytes(), false),
    };
    if spec.precision == Some(0) && value == 0 && signed_value == 0 {
        digits.clear();
    }
    if let Some(p) = spec.precision
        && digits.len() < p
    {
        let mut zeros = vec![b'0'; p - digits.len()];
        zeros.extend(digits);
        digits = zeros;
    }
    let mut prefix = Vec::new();
    if matches!(conv, b'd' | b'i') {
        if negative {
            prefix.push(b'-');
        } else if spec.plus {
            prefix.push(b'+');
        } else if spec.space {
            prefix.push(b' ');
        }
    } else if spec.hash {
        match conv {
            b'x' if value != 0 => prefix.extend_from_slice(b"0x"),
            b'X' if value != 0 => prefix.extend_from_slice(b"0X"),
            b'o' if digits.first() != Some(&b'0') => digits.insert(0, b'0'),
            _ => {}
        }
    }
    let sign_len = prefix.len();
    let mut body = prefix;
    body.extend(digits);
    pad(spec, body, spec.precision.is_none(), sign_len)
}

fn format_str(spec: &Spec, s: &[u8]) -> Vec<u8> {
    let s = match spec.precision {
        Some(p) if p < s.len() => &s[..p],
        _ => s,
    };
    pad(spec, s.to_vec(), false, 0)
}

/// Formats `format` (the bytes of a guest format string) with `args`,
/// returning the text. Anything the formatter could not honour is
/// described in `warnings`. See the module docs for the language.
pub fn format_message(
    format: &[u8],
    mem: &dyn AddressSpace,
    args: Args<'_>,
    warnings: &mut Vec<String>,
) -> Vec<u8> {
    const CAP: usize = MAX_DPATH - 1;
    let mut rd = ArgReader {
        mem,
        args,
        next: 0,
        ptr: match args {
            Args::VaList(p) => p,
            Args::Stack(_) => 0,
        },
        starved: false,
    };
    let mut out: Vec<u8> = Vec::new();
    // `Some(raw)` while inside a directive, `raw` being the flag/width/
    // precision characters seen so far.
    let mut spec: Option<Vec<u8>> = None;
    let mut long = false;
    let mut i = 0;
    while i < format.len() {
        let c = format[i];
        i += 1;
        let Some(raw) = spec.as_mut() else {
            if c == b'%' {
                spec = Some(Vec::new());
                long = false;
            } else if out.len() < CAP {
                out.push(c);
            }
            continue;
        };
        if out.len() >= CAP {
            break;
        }
        let piece: Vec<u8> = match c {
            b'%' => b"%".to_vec(),
            b'l' => {
                long = true;
                continue;
            }
            b'h' => continue,
            b'-' | b'+' | b' ' | b'#' | b'.' | b'0'..=b'9' => {
                raw.push(c);
                continue;
            }
            b'[' => {
                match format[i..].iter().position(|&b| b == b']') {
                    Some(close) if i + close + 1 < format.len() => {
                        let name = String::from_utf8_lossy(&format[i..i + close]).into_owned();
                        warnings.push(format!(
                            "unsupported custom directive %[{name}]{}",
                            char::from(format[i + close + 1])
                        ));
                        i += close + 2;
                    }
                    _ => warnings.push("%[ without a closing ] and conversion dropped".into()),
                }
                spec = None;
                continue;
            }
            b'c' | b's' | b'b' | b'p' | b'd' | b'i' | b'u' | b'x' | b'X' | b'o' => {
                let Some(parsed) = parse_spec(raw) else {
                    let literal = format!("%{}{}", String::from_utf8_lossy(raw), char::from(c));
                    warnings.push(format!("malformed directive {literal:?} printed literally"));
                    let bytes = literal.into_bytes();
                    out.extend(bytes);
                    out.truncate(CAP);
                    spec = None;
                    continue;
                };
                let want = if long { Want::Long } else { Want::Word };
                match c {
                    b'c' => {
                        let v = rd.get(want);
                        // snprintf into a C string: a NUL ends the piece.
                        let mut p = pad(&parsed, vec![v as u8], false, 0);
                        if let Some(nul) = p.iter().position(|&b| b == 0) {
                            p.truncate(nul);
                        }
                        p
                    }
                    b'b' => {
                        let ptr = rd.get(Want::Long);
                        format_str(&parsed, &read_bstring(mem, ptr, CAP))
                    }
                    b's' => {
                        let ptr = rd.get(Want::Long);
                        format_str(&parsed, &read_string(mem, ptr, CAP))
                    }
                    b'p' => format!("${:08x}", rd.get(Want::Long)).into_bytes(),
                    _ => {
                        let v = rd.get(want);
                        let signed = if want == Want::Word {
                            i32::from(v as u16 as i16)
                        } else {
                            v as i32
                        };
                        format_int(&parsed, c, v, signed)
                    }
                }
            }
            _ => {
                let literal = format!("%{}{}", String::from_utf8_lossy(raw), char::from(c));
                warnings.push(format!("unknown directive {literal:?} printed literally"));
                literal.into_bytes()
            }
        };
        out.extend(piece);
        out.truncate(CAP);
        spec = None;
    }
    if rd.starved {
        warnings.push("format used more arguments than were pushed; missing ones read as 0".into());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::FlatMemory;

    fn fmt_stack(format: &str, args: &[StackEntry], mem: &FlatMemory) -> (String, Vec<String>) {
        let mut w = Vec::new();
        let out = format_message(format.as_bytes(), mem, Args::Stack(args), &mut w);
        (String::from_utf8_lossy(&out).into_owned(), w)
    }
    fn word(v: u32) -> StackEntry {
        StackEntry {
            value: v,
            size: StoreSize::Word,
        }
    }
    fn long(v: u32) -> StackEntry {
        StackEntry {
            value: v,
            size: StoreSize::Long,
        }
    }
    fn byte(v: u32) -> StackEntry {
        StackEntry {
            value: v,
            size: StoreSize::Byte,
        }
    }
    fn mem_with(strings: &[(u32, &[u8])]) -> FlatMemory {
        let mut m = FlatMemory::new(0x1000);
        for (a, s) in strings {
            for (i, b) in s.iter().enumerate() {
                m.write_u8(a + i as u32, *b);
            }
        }
        m
    }
    fn f(format: &str, args: &[StackEntry]) -> String {
        let m = mem_with(&[
            (0x100, b"hello\0"),
            (0x200, b"\x03abc"),
            (0x300, b"\x04a\0bc"),
        ]);
        let (s, w) = fmt_stack(format, args, &m);
        assert!(w.is_empty(), "unexpected warnings {w:?} for {format:?}");
        s
    }

    #[test]
    fn literals_and_percent_percent() {
        assert_eq!(f("plain text", &[]), "plain text");
        assert_eq!(f("100%%", &[]), "100%");
        assert_eq!(f("a%%b%%c", &[]), "a%b%c");
    }

    #[test]
    fn integers_default_to_16_bits() {
        assert_eq!(f("%d", &[long(0x0001_2345)]), "9029"); // 0x2345
        assert_eq!(f("%x", &[long(0xABCD_1234)]), "1234");
        assert_eq!(f("%u", &[word(0xFFFF)]), "65535");
        assert_eq!(f("%o", &[word(8)]), "10");
        assert_eq!(f("%X", &[word(0xBEEF)]), "BEEF");
    }

    #[test]
    fn l_modifier_makes_integers_32_bit() {
        assert_eq!(f("%lx", &[long(0xABCD_1234)]), "abcd1234");
        assert_eq!(f("%lu", &[long(0xFFFF_FFFF)]), "4294967295");
        assert_eq!(f("%ld", &[long(0xFFFF_FFFF)]), "-1");
        assert_eq!(f("%li", &[long(5)]), "5");
        assert_eq!(f("%lo", &[long(0o777_777)]), "777777");
    }

    #[test]
    fn l_may_appear_between_flags_and_conversion() {
        assert_eq!(f("%08lx", &[long(0xAB)]), "000000ab");
        assert_eq!(f("%l08x", &[long(0xAB)]), "000000ab");
    }

    #[test]
    fn d_and_i_sign_extend_word_values() {
        assert_eq!(f("%d", &[word(0xFFFF)]), "-1");
        assert_eq!(f("%i", &[word(0x8000)]), "-32768");
        assert_eq!(f("%d", &[word(0x7FFF)]), "32767");
        // u/x are not sign-extended.
        assert_eq!(f("%x", &[word(0xFFFF)]), "ffff");
    }

    #[test]
    fn a_byte_push_is_masked_to_8_bits_and_not_sign_extended() {
        assert_eq!(f("%d", &[byte(0xFF)]), "255");
        assert_eq!(f("%x", &[byte(0x1AB)]), "ab");
    }

    #[test]
    fn a_word_push_read_as_long_is_zero_extended() {
        assert_eq!(f("%ld", &[word(0xFFFF)]), "65535");
    }

    #[test]
    fn flags_width_and_precision() {
        assert_eq!(f("%5d|", &[word(42)]), "   42|");
        assert_eq!(f("%-5d|", &[word(42)]), "42   |");
        assert_eq!(f("%05d", &[word(42)]), "00042");
        assert_eq!(f("%+d", &[word(42)]), "+42");
        assert_eq!(f("% d", &[word(42)]), " 42");
        assert_eq!(f("%05d", &[word(0xFFFE)]), "-0002");
        assert_eq!(f("%.4d", &[word(42)]), "0042");
        assert_eq!(f("%8.4d", &[word(42)]), "    0042");
        assert_eq!(f("%08.4d", &[word(42)]), "    0042"); // precision cancels 0
        assert_eq!(f("%.0d|", &[word(0)]), "|");
        assert_eq!(f("%#x", &[word(0xAB)]), "0xab");
        assert_eq!(f("%#X", &[word(0xAB)]), "0XAB");
        assert_eq!(f("%#x", &[word(0)]), "0");
        assert_eq!(f("%#o", &[word(8)]), "010");
        assert_eq!(f("%#08x", &[word(0xAB)]), "0x0000ab");
    }

    #[test]
    fn c_prints_the_low_byte() {
        assert_eq!(f("%c", &[word(0x41)]), "A");
        assert_eq!(f("%c", &[word(0x1_41)]), "A");
        assert_eq!(f("%lc", &[long(0x4142)]), "B");
        assert_eq!(f("%3c|", &[word(0x41)]), "  A|");
        assert_eq!(f("%-3c|", &[word(0x41)]), "A  |");
    }

    #[test]
    fn c_of_zero_prints_nothing_like_a_c_string_would() {
        assert_eq!(f("[%c]", &[word(0)]), "[]");
        assert_eq!(f("[%3c]", &[word(0)]), "[  ]");
        assert_eq!(f("[%-3c]", &[word(0)]), "[]");
    }

    #[test]
    fn s_reads_a_c_string() {
        assert_eq!(f("<%s>", &[long(0x100)]), "<hello>");
        assert_eq!(f("<%8s>", &[long(0x100)]), "<   hello>");
        assert_eq!(f("<%-8s>", &[long(0x100)]), "<hello   >");
        assert_eq!(f("<%.2s>", &[long(0x100)]), "<he>");
        assert_eq!(f("<%s>", &[long(0x800)]), "<>"); // zeroed memory
        assert_eq!(f("<%s>", &[long(0xFFFF_0000)]), "<>"); // unmapped
    }

    #[test]
    fn s_ignores_a_word_sized_push_width_by_masking_to_16_bits() {
        // A pointer pushed as a word loses its top half, like UAE.
        assert_eq!(f("<%s>", &[word(0x100)]), "<hello>");
        assert_eq!(f("<%s>", &[word(0x1_0100)]), "<hello>");
    }

    #[test]
    fn b_reads_a_bstring_and_maps_nul_to_dot() {
        assert_eq!(f("<%b>", &[long(0x200)]), "<abc>");
        assert_eq!(f("<%b>", &[long(0x300)]), "<a.bc>");
        assert_eq!(f("<%5b>", &[long(0x200)]), "<  abc>");
        assert_eq!(f("<%b>", &[long(0xFFFF_0000)]), "<>");
    }

    #[test]
    fn p_prints_dollar_and_eight_hex_digits() {
        assert_eq!(f("%p", &[long(0xBEEF)]), "$0000beef");
        assert_eq!(f("%p", &[long(0xDEAD_BEEF)]), "$deadbeef");
        // Flags are ignored for %p, as in UAE.
        assert_eq!(f("%20p", &[long(1)]), "$00000001");
    }

    #[test]
    fn arguments_are_consumed_in_order() {
        assert_eq!(
            f("%d-%lx-%s", &[word(1), long(0x20), long(0x100)]),
            "1-20-hello"
        );
    }

    #[test]
    fn missing_arguments_read_as_zero_and_warn_once() {
        let m = FlatMemory::new(16);
        let (s, w) = fmt_stack("%d %x %s|", &[], &m);
        assert_eq!(s, "0 0 |");
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("missing"));
    }

    #[test]
    fn extra_arguments_are_ignored() {
        assert_eq!(f("%d", &[word(1), word(2)]), "1");
    }

    #[test]
    fn h_is_accepted_and_ignored() {
        assert_eq!(f("%hd", &[word(0xFFFF)]), "-1");
    }

    #[test]
    fn malformed_directives_print_literally_without_consuming_an_argument() {
        let m = FlatMemory::new(16);
        for (fmt, expect) in [
            ("%z|%d", "%z|7"),
            ("%*d|%d", "%*d|7"),
            ("%5-d|", "%5-d|"),
            ("%1.2.3d|", "%1.2.3d|"),
        ] {
            let (s, w) = fmt_stack(fmt, &[word(7)], &m);
            assert_eq!(s, expect, "{fmt:?}");
            assert!(!w.is_empty(), "{fmt:?} should warn");
        }
    }

    #[test]
    fn trailing_lone_percent_prints_nothing() {
        // The directive never completes, as in UAE.
        assert_eq!(f("abc%", &[]), "abc");
        assert_eq!(f("abc%5", &[]), "abc");
    }

    #[test]
    fn custom_directive_is_consumed_prints_nothing_and_warns() {
        let m = FlatMemory::new(16);
        let (s, w) = fmt_stack("a%[CYCLES]xb", &[], &m);
        assert_eq!(s, "ab");
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("%[CYCLES]x"), "{w:?}");
        let (s, w) = fmt_stack("a%[NOPE]db", &[], &m);
        assert_eq!(s, "ab");
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn unterminated_custom_directive_is_dropped_like_uae() {
        let m = FlatMemory::new(16);
        let (s, _) = fmt_stack("a%[CYCLESb", &[], &m);
        assert_eq!(s, "aCYCLESb");
        // ']' is the last character: nothing follows to convert with.
        let (s, _) = fmt_stack("a%[X]", &[], &m);
        assert_eq!(s, "aX]");
    }

    #[test]
    fn va_list_reads_packed_unpromoted_arguments() {
        let mut m = FlatMemory::new(0x1000);
        m.write_u8(0x100, b'h');
        m.write_u8(0x101, b'i');
        // word 5, long 0x12345678, pointer to "hi", word 0xFFFF
        m.write_u16(0x400, 5);
        m.write_u32(0x402, 0x1234_5678);
        m.write_u32(0x406, 0x100);
        m.write_u16(0x40A, 0xFFFF);
        let mut w = Vec::new();
        let out = format_message(b"%d %lx %s %d", &m, Args::VaList(0x400), &mut w);
        assert_eq!(String::from_utf8_lossy(&out), "5 12345678 hi -1");
        assert!(w.is_empty());
    }

    #[test]
    fn va_list_read_beyond_memory_is_zero() {
        let m = FlatMemory::new(16);
        let mut w = Vec::new();
        let out = format_message(b"%d|%lx|%s", &m, Args::VaList(0xFFFF_FFF0), &mut w);
        assert_eq!(String::from_utf8_lossy(&out), "0|0|");
    }

    #[test]
    fn output_is_capped_at_max_dpath_minus_one() {
        let m = FlatMemory::new(16);
        let long_fmt = "x".repeat(MAX_DPATH + 100);
        let (s, _) = fmt_stack(&long_fmt, &[], &m);
        assert_eq!(s.len(), MAX_DPATH - 1);
        let (s, _) = fmt_stack("%99999d", &[word(1)], &m);
        assert_eq!(s.len(), MAX_DPATH - 1);
    }

    #[test]
    fn read_string_stops_at_max_and_at_nul() {
        let mut m = FlatMemory::new(0x2000);
        for i in 0..0x1800u32 {
            m.write_u8(i, b'z');
        }
        assert_eq!(read_string(&m, 0, 10).len(), 10);
        assert_eq!(read_string(&m, 0, MAX_DPATH - 1).len(), MAX_DPATH - 1);
        m.write_u8(5, 0);
        assert_eq!(read_string(&m, 0, 100), b"zzzzz");
    }

    // --- the store protocol ---

    fn port(word_pairs: bool) -> (DebugSprintf, CaptureSink, FlatMemory) {
        let cap = CaptureSink::default();
        let mem = mem_with(&[
            (0x100, b"hello\0"),
            (0x140, b"v=%d s=%s\0"),
            (0x180, b"%d%lx\0"),
            (0x1C0, b"A\0"),
        ]);
        (
            DebugSprintf::new(word_pairs, Box::new(cap.clone())),
            cap,
            mem,
        )
    }
    const A: u32 = DEBUG_SPRINTF_ADDRESS;

    #[test]
    fn claims_matches_cia_debug_routing() {
        let p020 = DebugSprintf::new(false, Box::new(CaptureSink::default()));
        let p000 = DebugSprintf::new(true, Box::new(CaptureSink::default()));
        for off in 0..16u32 {
            let want020 = matches!(off, 0 | 4 | 8);
            let want000 = matches!(off, 0 | 2 | 4 | 6 | 8 | 10);
            assert_eq!(p020.claims(A + off), want020, "68020 +{off}");
            assert_eq!(p000.claims(A + off), want000, "68000 +{off}");
        }
        assert!(!p020.claims(A - 4));
    }

    #[test]
    fn long_stores_push_and_print_on_a_68020() {
        let (mut p, cap, mem) = port(false);
        p.store(&mem, A, 0xFFFE, StoreSize::Long);
        p.store(&mem, A, 0x100, StoreSize::Long);
        p.store(&mem, A + 4, 0x140, StoreSize::Long);
        assert_eq!(cap.text(), "v=-2 s=hello");
        assert!(cap.warnings().is_empty());
    }

    #[test]
    fn printing_clears_the_stack() {
        let (mut p, cap, mem) = port(false);
        p.store(&mem, A, 7, StoreSize::Word);
        p.store(&mem, A + 4, 0x180, StoreSize::Long);
        p.store(&mem, A + 4, 0x180, StoreSize::Long);
        // First print: %d=7, %lx=0 (starved). Second: both starved, so
        // the first print's argument did not survive.
        assert_eq!(cap.text(), "7000");
        assert_eq!(cap.warnings().len(), 2);
    }

    #[test]
    fn non_long_stores_to_plus_4_and_plus_8_are_ignored() {
        let (mut p, cap, mem) = port(false);
        p.store(&mem, A + 4, 0x100, StoreSize::Word);
        p.store(&mem, A + 4, 0x100, StoreSize::Byte);
        p.store(&mem, A + 8, 0x100, StoreSize::Word);
        assert_eq!(cap.text(), "");
        assert!(!p.va_mode);
    }

    #[test]
    fn stack_holds_32_entries_and_drops_the_rest() {
        let (mut p, _cap, mem) = port(false);
        for i in 0..40 {
            p.store(&mem, A, i, StoreSize::Long);
        }
        assert_eq!(p.count, STACK_ENTRIES);
        assert_eq!(p.stack[31].value, 31);
    }

    #[test]
    fn va_mode_reads_from_memory_until_the_next_push() {
        let (mut p, cap, mut mem) = port(false);
        mem.write_u16(0x500, 11);
        mem.write_u32(0x502, 0x1C0);
        p.store(&mem, A + 8, 0x500, StoreSize::Long);
        p.store(&mem, A + 4, 0x140, StoreSize::Long);
        // Sticky: a second print restarts from the same pointer.
        p.store(&mem, A + 4, 0x140, StoreSize::Long);
        assert_eq!(cap.text(), "v=11 s=Av=11 s=A");
        // A push returns to stack mode.
        p.store(&mem, A, 3, StoreSize::Word);
        p.store(&mem, A + 4, 0x180, StoreSize::Long);
        assert!(cap.text().ends_with("30"));
    }

    #[test]
    fn word_pairs_combine_into_a_long_on_a_68000() {
        let (mut p, cap, mem) = port(true);
        // push long 0x0001FFFE as two words, then print
        p.store(&mem, A, 0x0001, StoreSize::Word);
        p.store(&mem, A + 2, 0xFFFE, StoreSize::Word);
        assert_eq!(p.count, 1, "the first half is replaced, not kept");
        assert_eq!(p.stack[0].size, StoreSize::Long);
        assert_eq!(p.stack[0].value, 0x0001_FFFE);
        p.store(&mem, A, 0x0000, StoreSize::Word);
        p.store(&mem, A + 2, 0x0100, StoreSize::Word);
        p.store(&mem, A + 4, 0x0000, StoreSize::Word);
        p.store(&mem, A + 6, 0x0140, StoreSize::Word);
        assert_eq!(cap.text(), "v=-2 s=hello");
    }

    #[test]
    fn a_va_pair_on_a_68000_uses_the_combined_long() {
        let (mut p, _cap, mem) = port(true);
        p.store(&mem, A + 8, 0x0001, StoreSize::Word);
        p.store(&mem, A + 10, 0x0500, StoreSize::Word);
        assert_eq!(
            p.va, 0x0001_0500,
            "the combined long, not just the low word"
        );
        assert!(p.va_mode);
    }

    #[test]
    fn a_long_store_on_a_68000_is_split_into_its_word_pair() {
        let (mut p, cap, mem) = port(true);
        p.store(&mem, A, 0x0001_FFFE, StoreSize::Long);
        assert_eq!(p.count, 1);
        assert_eq!(p.stack[0].size, StoreSize::Long);
        assert_eq!(p.stack[0].value, 0x0001_FFFE);
        p.store(&mem, A, 0x100, StoreSize::Long);
        p.store(&mem, A + 4, 0x140, StoreSize::Long);
        assert_eq!(cap.text(), "v=-2 s=hello");
        assert_eq!(p.count, 0);
    }

    #[test]
    fn a_lone_word_at_plus_6_is_not_a_print() {
        let (mut p, cap, mem) = port(true);
        p.store(&mem, A + 6, 0x0140, StoreSize::Word);
        assert_eq!(cap.text(), "");
    }

    #[test]
    fn plus_4_word_half_alone_never_prints() {
        let (mut p, cap, mem) = port(true);
        p.store(&mem, A + 4, 0x0000, StoreSize::Word);
        assert_eq!(cap.text(), "");
    }

    #[test]
    fn the_va_pair_does_not_pop_the_argument_stack() {
        let (mut p, _cap, mem) = port(true);
        p.store(&mem, A, 1, StoreSize::Word); // push {word 1}... then +2 below
        p.store(&mem, A + 8, 0, StoreSize::Word);
        p.store(&mem, A + 10, 0x500, StoreSize::Word);
        assert_eq!(p.count, 1);
    }

    #[test]
    fn empty_output_is_not_emitted() {
        let (mut p, cap, mut mem) = port(false);
        mem.write_u8(0x600, 0);
        p.store(&mem, A + 4, 0x600, StoreSize::Long);
        assert!(cap.0.borrow().text.is_empty());
    }
}

#[cfg(test)]
mod runtime_tests {
    //! The port driven by real (hand-assembled) guest code through
    //! `Runtime::run`, so the whole path is exercised: `AddressBus` stores
    //! -> interception -> state machine -> sink.
    use super::*;
    use crate::backend::{CpuType, M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{Runtime, StartConfig};
    use crate::memory::FlatMemory;
    use crate::sanitize::PoisonReason;
    use m68k::AddressBus;

    const FMT: u32 = 0x1_0000;
    const STR: u32 = 0x1_0100;
    const VA: u32 = 0x1_0200;
    const DEFAULT_RAM: usize = 16 * 1024 * 1024;
    const SMALL_RAM: usize = 0x2_0000;

    fn hi(v: u32) -> u16 {
        (v >> 16) as u16
    }
    fn lo(v: u32) -> u16 {
        v as u16
    }
    /// `move.l #imm,abs.l`
    fn mov_l(imm: u32, addr: u32) -> Vec<u16> {
        vec![0x23FC, hi(imm), lo(imm), hi(addr), lo(addr)]
    }
    /// `move.w #imm,abs.l`
    fn mov_w(imm: u16, addr: u32) -> Vec<u16> {
        vec![0x33FC, imm, hi(addr), lo(addr)]
    }
    /// `move.b #imm,abs.l`
    fn mov_b(imm: u8, addr: u32) -> Vec<u16> {
        vec![0x13FC, u16::from(imm), hi(addr), lo(addr)]
    }
    fn finish(mut words: Vec<Vec<u16>>) -> Vec<u16> {
        words.push(vec![0x7000, 0x4E75]); // moveq #0,d0 ; rts
        words.concat()
    }
    fn put(mem: &mut FlatMemory, addr: u32, bytes: &[u8]) {
        for (i, b) in bytes.iter().enumerate() {
            mem.write_u8(addr + i as u32, *b);
        }
    }

    struct Guest {
        cpu: CpuType,
        ram: usize,
        port: bool,
        jit: bool,
        sanitize: bool,
    }
    impl Guest {
        fn new(cpu: CpuType) -> Self {
            Self {
                cpu,
                ram: SMALL_RAM,
                port: true,
                jit: false,
                sanitize: false,
            }
        }
        fn run(self, program: &[u16]) -> (CaptureSink, Runtime<M68kCpu>) {
            self.run_with(program, |_| {})
        }
        fn run_with(
            self,
            program: &[u16],
            setup: impl FnOnce(&mut FlatMemory),
        ) -> (CaptureSink, Runtime<M68kCpu>) {
            let cap = CaptureSink::default();
            let mut mem = FlatMemory::new(self.ram);
            if self.sanitize {
                mem.enable_sanitizer();
            }
            if self.port {
                install(&mut mem, self.cpu, Box::new(cap.clone()));
            }
            let entry = TRAP_TABLE_END;
            for (i, w) in program.iter().enumerate() {
                mem.write_u16(entry + 2 * i as u32, *w);
            }
            put(&mut mem, FMT, b"v=%d s=%s\0");
            put(&mut mem, STR, b"hello\0");
            setup(&mut mem);
            let mut cpu = M68kCpu::with_config(self.cpu, false);
            cpu.set_jit(self.jit);
            let mut rt = Runtime::new(
                cpu,
                mem,
                StartConfig {
                    entry,
                    load_end: entry + 0x400,
                    ..StartConfig::default()
                },
            );
            if self.port {
                rt.reserve_uae_debug_port().expect("reserve");
            }
            let mut out = Vec::new();
            rt.run(&mut out, None).expect("guest run");
            (cap, rt)
        }
    }

    fn stack_program() -> Vec<u16> {
        finish(vec![
            mov_l(0xFFFE, DEBUG_SPRINTF_ADDRESS),
            mov_l(STR, DEBUG_SPRINTF_ADDRESS),
            mov_l(FMT, DEBUG_SPRINTF_ADDRESS + 4),
        ])
    }

    #[test]
    fn stack_arguments_on_a_68020() {
        let (cap, _) = Guest::new(CpuType::M68020).run(&stack_program());
        assert_eq!(cap.text(), "v=-2 s=hello");
        assert!(cap.warnings().is_empty());
    }

    #[test]
    fn stack_arguments_on_a_68000_via_long_stores() {
        // The core hands the bus a whole long; the port splits it into
        // the word pair UAE would have seen.
        let (cap, _) = Guest::new(CpuType::M68000).run(&stack_program());
        assert_eq!(cap.text(), "v=-2 s=hello");
    }

    #[test]
    fn stack_arguments_on_a_68000_via_explicit_word_pairs() {
        let a = DEBUG_SPRINTF_ADDRESS;
        let prog = finish(vec![
            mov_w(0, a),
            mov_w(0xFFFE, a + 2),
            mov_w(hi(STR), a),
            mov_w(lo(STR), a + 2),
            mov_w(hi(FMT), a + 4),
            mov_w(lo(FMT), a + 6),
        ]);
        let (cap, _) = Guest::new(CpuType::M68000).run(&prog);
        assert_eq!(cap.text(), "v=-2 s=hello");
    }

    #[test]
    fn va_list_path_on_both_cpu_families() {
        let prog = finish(vec![
            mov_l(VA, DEBUG_SPRINTF_ADDRESS + 8),
            mov_l(FMT, DEBUG_SPRINTF_ADDRESS + 4),
        ]);
        for cpu in [CpuType::M68000, CpuType::M68020] {
            let (cap, _) = Guest::new(cpu).run_with(&prog, |m| {
                m.write_u16(VA, 5);
                m.write_u32(VA + 2, STR);
            });
            assert_eq!(cap.text(), "v=5 s=hello", "{cpu:?}");
        }
    }

    #[test]
    fn va_list_path_via_explicit_word_pairs_on_a_68000() {
        let a = DEBUG_SPRINTF_ADDRESS;
        let prog = finish(vec![
            mov_w(hi(VA), a + 8),
            mov_w(lo(VA), a + 10),
            mov_w(hi(FMT), a + 4),
            mov_w(lo(FMT), a + 6),
        ]);
        let (cap, _) = Guest::new(CpuType::M68000).run_with(&prog, |m| {
            m.write_u16(VA, 9);
            m.write_u32(VA + 2, STR);
        });
        assert_eq!(cap.text(), "v=9 s=hello");
    }

    #[test]
    fn byte_and_word_pushes_keep_their_width() {
        let a = DEBUG_SPRINTF_ADDRESS;
        let prog = finish(vec![
            mov_b(0xFF, a),
            mov_w(0xFFFF, a),
            mov_l(FMT + 0x40, a + 4),
        ]);
        // On a 68020 +0 takes the byte as a byte and the word as a word.
        let (cap, _) = Guest::new(CpuType::M68020).run_with(&prog, |m| {
            put(m, FMT + 0x40, b"%d %d\0");
        });
        assert_eq!(cap.text(), "255 -1");
    }

    #[test]
    fn works_with_the_default_16_mib_ram() {
        let mut g = Guest::new(CpuType::M68020);
        g.ram = DEFAULT_RAM;
        let (cap, _) = g.run(&stack_program());
        assert_eq!(cap.text(), "v=-2 s=hello");
    }

    #[test]
    fn works_when_the_port_is_beyond_the_end_of_ram() {
        let mut g = Guest::new(CpuType::M68020);
        g.ram = 1 << 20; // --ram 1M
        assert!(g.ram < DEBUG_SPRINTF_ADDRESS as usize);
        let (cap, rt) = g.run(&stack_program());
        assert_eq!(cap.text(), "v=-2 s=hello");
        assert_eq!(
            rt.memory().read_u32(DEBUG_SPRINTF_ADDRESS),
            0,
            "no RAM there"
        );
    }

    #[test]
    fn a_claimed_store_never_reaches_ram() {
        let mut g = Guest::new(CpuType::M68020);
        g.ram = DEFAULT_RAM;
        let prog = finish(vec![mov_l(0x1234_5678, DEBUG_SPRINTF_ADDRESS)]);
        let (_, rt) = g.run(&prog);
        assert_eq!(rt.memory().read_u32(DEBUG_SPRINTF_ADDRESS), 0);
    }

    #[test]
    fn plus_2_is_only_routed_on_a_68000() {
        let a = DEBUG_SPRINTF_ADDRESS;
        let prog = finish(vec![mov_w(0xABCD, a + 2)]);
        let mut g = Guest::new(CpuType::M68020);
        g.ram = DEFAULT_RAM;
        let (_, rt) = g.run(&prog);
        assert_eq!(rt.memory().read_u16(a + 2), 0xABCD, "68020: plain RAM");
        let mut g = Guest::new(CpuType::M68000);
        g.ram = DEFAULT_RAM;
        let (_, rt) = g.run(&prog);
        assert_eq!(
            rt.memory().read_u16(a + 2),
            0,
            "68000: swallowed by the port"
        );
    }

    #[test]
    fn without_the_flag_a_store_is_plain_memory_exactly_as_before() {
        let prog = finish(vec![mov_l(0x1234_5678, DEBUG_SPRINTF_ADDRESS + 4)]);
        let mut g = Guest::new(CpuType::M68020);
        g.ram = DEFAULT_RAM;
        g.port = false;
        let (cap, rt) = g.run(&prog);
        assert_eq!(cap.text(), "");
        assert_eq!(rt.memory().read_u32(DEBUG_SPRINTF_ADDRESS + 4), 0x1234_5678);
        // ...and with RAM too small to contain it the store is dropped.
        let mut g = Guest::new(CpuType::M68020);
        g.ram = 1 << 20;
        g.port = false;
        let (_, rt) = g.run(&prog);
        assert_eq!(rt.memory().read_u32(DEBUG_SPRINTF_ADDRESS + 4), 0);
    }

    #[test]
    fn handler_writes_through_address_space_are_not_intercepted() {
        let cap = CaptureSink::default();
        let mut mem = FlatMemory::new(DEFAULT_RAM);
        install(&mut mem, CpuType::M68020, Box::new(cap.clone()));
        mem.write_u32(DEBUG_SPRINTF_ADDRESS, 0xCAFE_F00D);
        assert_eq!(mem.read_u32(DEBUG_SPRINTF_ADDRESS), 0xCAFE_F00D);
        assert_eq!(cap.text(), "");
    }

    #[test]
    fn reads_of_the_port_fall_through() {
        let mut mem = FlatMemory::new(DEFAULT_RAM);
        install(&mut mem, CpuType::M68020, Box::new(CaptureSink::default()));
        mem.write_u32(DEBUG_SPRINTF_ADDRESS + 4, 0x0102_0304);
        assert_eq!(
            AddressBus::read_long(&mut mem, DEBUG_SPRINTF_ADDRESS + 4),
            0x0102_0304
        );
    }

    #[test]
    fn jit_is_not_used_when_the_port_is_installed() {
        let mut mem = FlatMemory::new(SMALL_RAM);
        assert!(mem.fast_mem().is_some(), "control: fast path normally on");
        install(&mut mem, CpuType::M68020, Box::new(CaptureSink::default()));
        assert!(mem.fast_mem().is_none());
    }

    #[test]
    fn a_jit_cpu_still_reaches_the_port() {
        // Requesting the JIT must make no difference: the run loop drops
        // to batches of one and the fast path is off.
        let mut g = Guest::new(CpuType::M68020);
        g.jit = true;
        let (cap, _) = g.run(&stack_program());
        assert_eq!(cap.text(), "v=-2 s=hello");
    }

    #[test]
    fn a_loop_of_messages_with_the_jit_requested_prints_every_one() {
        // dbra loop: 3 x { push; print } -- the shape a JIT would trace.
        let a = DEBUG_SPRINTF_ADDRESS;
        let mut prog = vec![0x7402]; // moveq #2,d2
        let body: Vec<u16> = [mov_w(7, a), mov_l(FMT + 0x40, a + 4)].concat();
        prog.extend(&body);
        // dbra d2, back to the first store
        let back = -((body.len() as i32 + 1) * 2) as i16;
        prog.extend([0x51CA, back as u16]);
        prog.extend([0x7000, 0x4E75]);
        let mut g = Guest::new(CpuType::M68020);
        g.jit = true;
        let (cap, _) = g.run_with(&prog, |m| put(m, FMT + 0x40, b"<%d>\0"));
        assert_eq!(cap.text(), "<7><7><7>");
    }

    #[test]
    fn port_stores_are_not_sanitizer_violations() {
        let a = DEBUG_SPRINTF_ADDRESS;
        let mut g = Guest::new(CpuType::M68020);
        g.ram = DEFAULT_RAM;
        g.sanitize = true;
        // Poison the port bytes themselves: a plain store here would be
        // reported, a port store must not be.
        let (cap, rt) = g.run_with(&stack_program(), |m| {
            m.shadow_mut()
                .unwrap()
                .mark_unaddressable(a, 12, PoisonReason::Redzone);
        });
        assert_eq!(cap.text(), "v=-2 s=hello");
        assert_eq!(rt.memory().shadow().unwrap().violation_count(), 0);
    }

    #[test]
    fn control_the_same_poisoned_store_is_a_violation_without_the_port() {
        let a = DEBUG_SPRINTF_ADDRESS;
        let mut g = Guest::new(CpuType::M68020);
        g.ram = DEFAULT_RAM;
        g.sanitize = true;
        g.port = false;
        let (_, rt) = g.run_with(&stack_program(), |m| {
            m.shadow_mut()
                .unwrap()
                .mark_unaddressable(a, 12, PoisonReason::Redzone);
        });
        assert!(rt.memory().shadow().unwrap().violation_count() > 0);
    }

    #[test]
    fn unclaimed_neighbouring_bytes_still_get_sanitizer_checks() {
        let a = DEBUG_SPRINTF_ADDRESS;
        let mut g = Guest::new(CpuType::M68020);
        g.ram = DEFAULT_RAM;
        g.sanitize = true;
        let prog = finish(vec![mov_b(1, a + 1)]);
        let (_, rt) = g.run_with(&prog, |m| {
            m.shadow_mut()
                .unwrap()
                .mark_unaddressable(a + 1, 1, PoisonReason::Redzone);
        });
        assert_eq!(rt.memory().shadow().unwrap().violation_count(), 1);
    }

    #[test]
    fn string_reads_go_through_the_checked_api() {
        // A format string living in poisoned memory is reported, rather
        // than the port reading around the sanitizer.
        let mut g = Guest::new(CpuType::M68020);
        g.ram = DEFAULT_RAM;
        g.sanitize = true;
        let prog = finish(vec![mov_l(FMT + 0x80, DEBUG_SPRINTF_ADDRESS + 4)]);
        let (cap, rt) = g.run_with(&prog, |m| {
            put(m, FMT + 0x80, b"zz\0");
            m.shadow_mut()
                .unwrap()
                .mark_unaddressable(FMT + 0x80, 1, PoisonReason::Freed);
        });
        assert_eq!(cap.text(), "zz");
        assert!(rt.memory().shadow().unwrap().violation_count() > 0);
    }

    /// Records the addresses it is asked about, for the masking test.
    struct Probe(Rc<RefCell<Vec<(u32, StoreSize)>>>);
    impl StorePort for Probe {
        fn claims(&self, addr: u32) -> bool {
            addr & 0x00FF_FFFF == 0x00AD_BEEF
        }
        fn store(&mut self, _: &dyn AddressSpace, addr: u32, _: u32, size: StoreSize) {
            self.0.borrow_mut().push((addr, size));
        }
    }

    #[test]
    fn store_ports_see_the_address_the_core_presents() {
        // What `move.b d0,$DEADBEEF` puts on the bus: the 68000/68010
        // cores present it masked to 24 bits, later models do not. A
        // port at a >24-bit address must therefore match per CPU model.
        let prog = finish(vec![vec![0x13C0, 0xDEAD, 0xBEEF]]);
        for (cpu, want) in [
            (CpuType::M68000, 0x00AD_BEEF),
            (CpuType::M68010, 0x00AD_BEEF),
            (CpuType::M68020, 0xDEAD_BEEF),
            (CpuType::M68060, 0xDEAD_BEEF),
        ] {
            let seen = Rc::new(RefCell::new(Vec::new()));
            let mut mem = FlatMemory::new(SMALL_RAM);
            mem.add_store_port(Box::new(Probe(seen.clone())));
            for (i, w) in prog.iter().enumerate() {
                mem.write_u16(TRAP_TABLE_END + 2 * i as u32, *w);
            }
            let mut rt = Runtime::new(
                M68kCpu::with_config(cpu, false),
                mem,
                StartConfig {
                    entry: TRAP_TABLE_END,
                    load_end: TRAP_TABLE_END + 0x400,
                    ..StartConfig::default()
                },
            );
            rt.run(&mut Vec::new(), None).expect("run");
            assert_eq!(*seen.borrow(), vec![(want, StoreSize::Byte)], "{cpu:?}");
        }
    }

    // --- heap reservation ---

    #[test]
    fn reserve_is_a_no_op_when_the_port_is_beyond_ram() {
        let mut g = Guest::new(CpuType::M68020);
        g.ram = 1 << 20;
        // run_with() already calls reserve_uae_debug_port().expect().
        let (cap, _) = g.run(&stack_program());
        assert_eq!(cap.text(), "v=-2 s=hello");
    }

    #[test]
    fn reserve_fails_when_the_stack_covers_the_port() {
        let mut mem = FlatMemory::new(DEFAULT_RAM);
        install(&mut mem, CpuType::M68020, Box::new(CaptureSink::default()));
        let mut rt = Runtime::new(
            M68kCpu::with_config(CpuType::M68020, false),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: TRAP_TABLE_END + 0x400,
                stack_size: 6 * 1024 * 1024, // 16M - 6M = 0xA00000 < 0xBFFF00
                ..StartConfig::default()
            },
        );
        let err = rt.reserve_uae_debug_port().unwrap_err();
        assert!(err.contains("0xbfff00"), "{err}");
    }
}
