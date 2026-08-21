//! `dos.library` `ReadArgs`/`FreeArgs`: the standard AmigaOS command-line
//! template parser. Every real Workbench/`C:` command uses this instead of
//! hand-rolled argv parsing (see `docs/plan.md`'s empirical-corpus
//! decision), which is why it's implemented as its own module rather than
//! left a stretch goal.
//!
//! # Scope and deviations from real `ReadArgs`
//!
//! - **Default source only.** `rdargs == NULL` reads from
//!   [`DosState::cmdline`] -- the same `A0`/`D0` command-line buffer
//!   [`crate::dispatch::Runtime::new`] builds, exactly mirroring how real
//!   AmigaOS delivers the CLI command tail through the process's buffered
//!   `Input()`. [`DosState::cmdline_pos`] persists across calls, so
//!   repeated `ReadArgs(NULL)` calls in one process walk forward through
//!   the same buffer rather than each re-parsing it from the start --
//!   real `ReadArgs(NULL)` shares this property because it reads off a
//!   stateful buffered stream. A caller-supplied `rdargs` with its own
//!   `RDA_Source` (a custom string to parse instead of the command line)
//!   is not implemented: `rdargs` is accepted only as an *identity* for
//!   `FreeArgs` bookkeeping (see below), and parsing still always reads
//!   from the shared command-line cursor. No real `C:` command corpus
//!   command supplies its own `RDArgs`, so this doesn't block T-corpus
//!   coverage; `RDA_ExtHelp`/interactive `?`-help and `RDAF_*` flags are
//!   likewise not implemented.
//! - **`/T` (toggle) is approximated as `/S`-with-memory**: each time the
//!   keyword appears, the boolean flips (starting `false`). The NDK
//!   autodoc describes exactly this; an older RKRM passage describes `/T`
//!   instead expecting an explicit `On`/`Off` value, which no known real
//!   `C:` command template uses -- deviation recorded here rather than
//!   implementing both.
//! - The returned `struct RDArgs*` is an opaque host-side anchor (a
//!   4-byte heap block when `ReadArgs` allocates its own, or the
//!   caller-supplied address when one was passed in) rather than a real
//!   `struct RDArgs`; nothing in this runtime reads its fields directly,
//!   only [`FreeArgs`] looks it up by address in [`DosState::rdargs`].
//!
//! # Template syntax
//!
//! `NAME[=ABBREV][/mod[/mod...]],NAME2...` -- keywords comma-separated;
//! each keyword optionally has a second name (order between the two
//! doesn't matter: either can appear on the command line); modifiers
//! (order-independent, at most one type-modifier per keyword): `/A`
//! required, `/K` keyword-only (excluded from positional fill), `/S`
//! switch, `/T` toggle, `/N` number, `/M` multiple strings (at most one
//! per template), `/F` rest of line (at most one per template). No
//! modifier = a plain optional string, matched by keyword or position.
//!
//! # Matching algorithm
//!
//! Tokens come from [`Reader`] (a from-scratch, template-scoped
//! reimplementation of `ReadItem`'s quoting/escaping rules -- double
//! quotes, and *inside* them the `*"`/`*n`/`*e`/`**` escapes -- since
//! `ReadItem` itself isn't a separately callable LVO here). Each
//! unquoted token is checked against every template keyword name
//! (case-insensitively); a match runs that keyword's type-specific
//! value-consumption (switch/toggle flip in place, `/N`/`/M`/plain
//! string read one more token as the value, `/F` swallows the raw rest
//! of the line verbatim). An unmatched token fills the next unfilled
//! *positional-eligible* slot (every non-`/K`, non-switch, non-toggle
//! keyword, in template order); once the cursor reaches a `/M` slot it
//! stays there, soaking up every further positional token (matching the
//! documented `Dir/M,All/S` example). After parsing, per the documented
//! `/M`+`/A` interaction (the `Copy` command's `FROM/A/M,TO/A`), any
//! still-unfilled required non-`/M` slot *after* the template's `/M`
//! slot steals one value off the end of the `/M` list.

use crate::cpu::{Cpu, DataRegister};
use crate::dispatch::{DOS_LIBRARY_BASE, DispatchError, HandlerContext, LibraryTable};
use crate::dosfile::DosState;
use crate::guestmem::{GuestHeap, GuestHeapError, read_c_string, write_c_string};
use crate::lvos::dos::DOS_LVOS;
use crate::memory::AddressSpace;

/// The argument template was syntactically invalid.
pub const ERROR_BAD_TEMPLATE: i32 = 114;
/// An `/N` argument's value didn't parse as a number.
pub const ERROR_BAD_NUMBER: i32 = 115;
/// An `/A` (required) argument was never given a value.
pub const ERROR_REQUIRED_ARG_MISSING: i32 = 116;
/// A keyword was recognized on the command line but no value token
/// followed it.
pub const ERROR_KEY_NEEDS_ARG: i32 = 117;
/// More positional arguments were supplied than the template (and any
/// `/M` slot) could absorb.
pub const ERROR_TOO_MANY_ARGS: i32 = 118;
/// A quoted argument had no matching closing quote before end of line.
pub const ERROR_UNMATCHED_QUOTES: i32 = 119;

/// Size in bytes of the opaque anchor block [`read_args`] allocates for
/// its own `struct RDArgs*` when the caller passes `rdargs == 0`. Never
/// read as a real `struct RDArgs` -- see the module docs.
const RDARGS_ANCHOR_SIZE: u32 = 4;

/// Bookkeeping for one live `ReadArgs` result, keyed by the anchor
/// address returned in `D0` (see [`DosState::rdargs`]).
#[derive(Debug, Default)]
pub struct RdArgsEntry {
    /// Every guest-heap block this call allocated (parsed strings, `/N`
    /// longwords, `/M` string arrays) -- freed by `FreeArgs`.
    allocations: Vec<u32>,
    /// Whether the anchor block itself was allocated by `ReadArgs` (and
    /// so must also be freed by `FreeArgs`), as opposed to being a
    /// caller-supplied `rdargs` address, which `FreeArgs` must leave
    /// alone per the real contract.
    owns_anchor: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArgKind {
    String,
    Switch,
    Toggle,
    Number,
    Multi,
    RestOfLine,
}

#[derive(Debug, Clone)]
struct TemplateArg {
    /// One or two upper-cased match names (`NAME` and, if given,
    /// `ABBREV`).
    names: Vec<Vec<u8>>,
    kind: ArgKind,
    keyword_only: bool,
    required: bool,
}

fn parse_template(template: &[u8]) -> Result<Vec<TemplateArg>, i32> {
    if template.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut have_multi = false;
    let mut have_rest_of_line = false;
    for part in template.split(|&b| b == b',') {
        let mut pieces = part.split(|&b| b == b'/');
        let name_part = pieces.next().unwrap_or(b"");
        // An empty name (a template item that's just modifiers, e.g.
        // the real `C:/Wait` binary's own template "/N,SEC=SECS/S,...")
        // is legal real AmigaDOS syntax: the item can only ever be
        // filled positionally, never matched by a `NAME=value` keyword
        // (there's no name to match against) -- see the module docs.
        let names: Vec<Vec<u8>> = if name_part.is_empty() {
            Vec::new()
        } else {
            let names: Vec<Vec<u8>> = name_part
                .splitn(2, |&b| b == b'=')
                .map(|n| n.to_ascii_uppercase())
                .collect();
            if names.iter().any(|n| n.is_empty()) {
                return Err(ERROR_BAD_TEMPLATE);
            }
            names
        };

        let mut kind = ArgKind::String;
        let mut kind_set = false;
        let mut keyword_only = false;
        let mut required = false;
        for modifier in pieces {
            if modifier.len() != 1 {
                return Err(ERROR_BAD_TEMPLATE);
            }
            match modifier[0].to_ascii_uppercase() {
                b'A' => required = true,
                b'K' => keyword_only = true,
                b'S' if !kind_set => {
                    kind = ArgKind::Switch;
                    kind_set = true;
                }
                b'T' if !kind_set => {
                    kind = ArgKind::Toggle;
                    kind_set = true;
                }
                b'N' if !kind_set => {
                    kind = ArgKind::Number;
                    kind_set = true;
                }
                b'M' if !kind_set && !have_multi => {
                    kind = ArgKind::Multi;
                    kind_set = true;
                    have_multi = true;
                }
                b'F' if !kind_set && !have_rest_of_line => {
                    kind = ArgKind::RestOfLine;
                    kind_set = true;
                    have_rest_of_line = true;
                }
                _ => return Err(ERROR_BAD_TEMPLATE),
            }
        }
        out.push(TemplateArg {
            names,
            kind,
            keyword_only,
            required,
        });
    }
    Ok(out)
}

fn matches_keyword(arg: &TemplateArg, token_upper: &[u8]) -> bool {
    arg.names.iter().any(|n| n.as_slice() == token_upper)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemKind {
    Nothing,
    Equal,
    Unquoted,
    Quoted,
    Error,
}

/// A from-scratch, `ReadArgs`-scoped reimplementation of `ReadItem`'s
/// tokenizing rules (see the module docs' "Matching algorithm" section).
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    /// Position (after whitespace-skipping, before consuming the token
    /// itself) of the most recent item read by [`Reader::read_item`] --
    /// used by `/F` to recover the *raw* rest of the line starting at a
    /// token, since `/F` takes the text verbatim rather than
    /// re-tokenizing it.
    last_item_start: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8], pos: usize) -> Self {
        Self {
            buf,
            pos: pos.min(buf.len()),
            last_item_start: pos.min(buf.len()),
        }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.buf.len() && matches!(self.buf[self.pos], b' ' | b'\t') {
            self.pos += 1;
        }
    }

    fn read_item(&mut self) -> (ItemKind, Vec<u8>) {
        self.skip_ws();
        self.last_item_start = self.pos;
        if self.pos >= self.buf.len() || self.buf[self.pos] == b'\n' || self.buf[self.pos] == b';' {
            return (ItemKind::Nothing, Vec::new());
        }
        if self.buf[self.pos] == b'=' {
            self.pos += 1;
            return (ItemKind::Equal, Vec::new());
        }
        if self.buf[self.pos] == b'"' {
            self.pos += 1;
            let mut out = Vec::new();
            loop {
                if self.pos >= self.buf.len() || self.buf[self.pos] == b'\n' {
                    return (ItemKind::Error, out);
                }
                let c = self.buf[self.pos];
                if c == b'"' {
                    self.pos += 1;
                    break;
                }
                if c == b'*' && self.pos + 1 < self.buf.len() {
                    let escaped = match self.buf[self.pos + 1] {
                        b'"' => Some(b'"'),
                        b'n' | b'N' => Some(b'\n'),
                        b'e' | b'E' => Some(0x1b),
                        b'*' => Some(b'*'),
                        _ => None,
                    };
                    if let Some(byte) = escaped {
                        out.push(byte);
                        self.pos += 2;
                        continue;
                    }
                }
                out.push(c);
                self.pos += 1;
            }
            return (ItemKind::Quoted, out);
        }
        let mut out = Vec::new();
        while self.pos < self.buf.len() {
            let c = self.buf[self.pos];
            if matches!(c, b' ' | b'\t' | b'\n' | b';' | b'=') {
                break;
            }
            out.push(c);
            self.pos += 1;
        }
        (ItemKind::Unquoted, out)
    }

    /// Skips one immediately-following `=`-kind pseudo-item (if any),
    /// then reads the item after it -- so `FROM=org` and `FROM org` both
    /// deliver `org` as the next value regardless of which separator was
    /// used, per the documented equivalence.
    fn read_value(&mut self) -> (ItemKind, Vec<u8>) {
        let (kind, text) = self.read_item();
        if kind == ItemKind::Equal {
            self.read_item()
        } else {
            (kind, text)
        }
    }

    /// The raw (unescaped, unquoted) text from the start of the last
    /// token read up to (not including) the line's terminating `\n`, for
    /// `/F`. Also advances `pos` to that `\n`.
    fn rest_of_line_from_last_item(&mut self) -> Vec<u8> {
        let start = self.last_item_start;
        let end = self.buf[start..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|i| start + i)
            .unwrap_or(self.buf.len());
        let text = self.buf[start..end].to_vec();
        self.pos = end;
        text
    }
}

/// In-progress parse result for one template keyword, before
/// materialization into guest memory.
#[derive(Debug, Default)]
struct Slot {
    string: Option<Vec<u8>>,
    number: Option<i32>,
    bool_val: bool,
    multi: Vec<Vec<u8>>,
}

fn parse_number(text: &[u8]) -> Result<i32, i32> {
    std::str::from_utf8(text)
        .ok()
        .and_then(|s| s.parse::<i32>().ok())
        .ok_or(ERROR_BAD_NUMBER)
}

/// Runs the matching algorithm (module docs) over `reader` against
/// `template`, returning one filled-in [`Slot`] per template keyword.
fn match_args(reader: &mut Reader<'_>, template: &[TemplateArg]) -> Result<Vec<Slot>, i32> {
    let mut slots: Vec<Slot> = template.iter().map(|_| Slot::default()).collect();
    let positional: Vec<usize> = template
        .iter()
        .enumerate()
        .filter(|(_, a)| !a.keyword_only && !matches!(a.kind, ArgKind::Switch | ArgKind::Toggle))
        .map(|(i, _)| i)
        .collect();
    let mut cursor = 0usize;

    loop {
        let (kind, text) = reader.read_item();
        match kind {
            ItemKind::Nothing => break,
            ItemKind::Error => return Err(ERROR_UNMATCHED_QUOTES),
            ItemKind::Equal => continue,
            ItemKind::Unquoted | ItemKind::Quoted => {
                let matched = if kind == ItemKind::Unquoted {
                    let upper = text.to_ascii_uppercase();
                    template.iter().position(|a| matches_keyword(a, &upper))
                } else {
                    None
                };

                if let Some(i) = matched {
                    match template[i].kind {
                        ArgKind::Switch => slots[i].bool_val = true,
                        ArgKind::Toggle => slots[i].bool_val = !slots[i].bool_val,
                        ArgKind::Number => {
                            let (vk, vtext) = reader.read_value();
                            if vk == ItemKind::Nothing {
                                return Err(ERROR_KEY_NEEDS_ARG);
                            }
                            slots[i].number = Some(parse_number(&vtext)?);
                        }
                        ArgKind::Multi => {
                            let (vk, vtext) = reader.read_value();
                            if vk == ItemKind::Nothing {
                                return Err(ERROR_KEY_NEEDS_ARG);
                            }
                            slots[i].multi.push(vtext);
                        }
                        ArgKind::RestOfLine => {
                            let (vk, _) = reader.read_value();
                            if vk == ItemKind::Nothing {
                                return Err(ERROR_KEY_NEEDS_ARG);
                            }
                            // read_value already consumed the first
                            // value token; the rest-of-line starts back
                            // at that token, not after it.
                            let rest = reader.rest_of_line_from_last_item();
                            slots[i].string = Some(rest);
                        }
                        ArgKind::String => {
                            let (vk, vtext) = reader.read_value();
                            if vk == ItemKind::Nothing {
                                return Err(ERROR_KEY_NEEDS_ARG);
                            }
                            slots[i].string = Some(vtext);
                        }
                    }
                    continue;
                }

                // Positional fill.
                if cursor >= positional.len() {
                    return Err(ERROR_TOO_MANY_ARGS);
                }
                let i = positional[cursor];
                match template[i].kind {
                    ArgKind::Multi => slots[i].multi.push(text),
                    ArgKind::RestOfLine => {
                        let rest = reader.rest_of_line_from_last_item();
                        slots[i].string = Some(rest);
                        cursor += 1;
                    }
                    ArgKind::Number => {
                        slots[i].number = Some(parse_number(&text)?);
                        cursor += 1;
                    }
                    ArgKind::String => {
                        slots[i].string = Some(text);
                        cursor += 1;
                    }
                    ArgKind::Switch | ArgKind::Toggle => unreachable!(
                        "switches/toggles are excluded from the positional-eligible list"
                    ),
                }
            }
        }
    }

    // /M + trailing /A borrowing (module docs).
    if let Some(multi_idx) = template.iter().position(|a| a.kind == ArgKind::Multi) {
        for i in (multi_idx + 1)..template.len() {
            let a = &template[i];
            if !a.required || a.kind == ArgKind::Multi {
                continue;
            }
            let already_filled = match a.kind {
                ArgKind::Number => slots[i].number.is_some(),
                _ => slots[i].string.is_some(),
            };
            if already_filled {
                continue;
            }
            let Some(borrowed) = slots[multi_idx].multi.pop() else {
                continue;
            };
            match a.kind {
                ArgKind::Number => slots[i].number = Some(parse_number(&borrowed)?),
                _ => slots[i].string = Some(borrowed),
            }
        }
    }

    for (i, a) in template.iter().enumerate() {
        if !a.required {
            continue;
        }
        let filled = match a.kind {
            ArgKind::Switch | ArgKind::Toggle => true,
            ArgKind::Multi => !slots[i].multi.is_empty(),
            ArgKind::Number => slots[i].number.is_some(),
            ArgKind::String | ArgKind::RestOfLine => slots[i].string.is_some(),
        };
        if !filled {
            return Err(ERROR_REQUIRED_ARG_MISSING);
        }
    }

    Ok(slots)
}

/// Allocates `bytes` plus a `NUL` terminator on `heap`, writes it, and
/// records the allocation in `allocations` for later `FreeArgs` cleanup.
fn alloc_c_string(
    heap: &mut GuestHeap,
    mem: &mut dyn AddressSpace,
    allocations: &mut Vec<u32>,
    bytes: &[u8],
) -> Result<u32, GuestHeapError> {
    let addr = heap.alloc(bytes.len() as u32 + 1)?;
    allocations.push(addr);
    write_c_string(mem, addr, bytes);
    Ok(addr)
}

/// Materializes `slots` into guest memory at `array_addr` (one `LONG`
/// per template keyword), returning every heap block allocated along the
/// way. On [`GuestHeapError::OutOfMemory`], everything allocated so far
/// for this call is freed before returning.
fn materialize(
    heap: &mut GuestHeap,
    mem: &mut dyn AddressSpace,
    template: &[TemplateArg],
    slots: Vec<Slot>,
    array_addr: u32,
) -> Result<Vec<u32>, i32> {
    let mut allocations = Vec::new();
    let result = (|| -> Result<(), GuestHeapError> {
        for (i, (arg, slot)) in template.iter().zip(slots).enumerate() {
            let slot_addr = array_addr.wrapping_add((i as u32) * 4);
            let value = match arg.kind {
                ArgKind::Switch | ArgKind::Toggle => u32::from(slot.bool_val),
                ArgKind::Number => match slot.number {
                    Some(n) => {
                        let addr = heap.alloc(4)?;
                        allocations.push(addr);
                        mem.write_u32(addr, n as u32);
                        addr
                    }
                    None => 0,
                },
                ArgKind::String | ArgKind::RestOfLine => match slot.string {
                    Some(bytes) => alloc_c_string(heap, mem, &mut allocations, &bytes)?,
                    None => 0,
                },
                ArgKind::Multi => {
                    if slot.multi.is_empty() {
                        0
                    } else {
                        let mut ptrs = Vec::with_capacity(slot.multi.len() + 1);
                        for item in &slot.multi {
                            ptrs.push(alloc_c_string(heap, mem, &mut allocations, item)?);
                        }
                        ptrs.push(0);
                        let table_addr = heap.alloc(ptrs.len() as u32 * 4)?;
                        allocations.push(table_addr);
                        for (j, ptr) in ptrs.iter().enumerate() {
                            mem.write_u32(table_addr.wrapping_add((j as u32) * 4), *ptr);
                        }
                        table_addr
                    }
                }
            };
            mem.write_u32(slot_addr, value);
        }
        Ok(())
    })();

    match result {
        Ok(()) => Ok(allocations),
        Err(_) => {
            for addr in allocations {
                let _ = heap.free(addr);
            }
            Err(crate::dosfile::ERROR_NO_FREE_STORE)
        }
    }
}

/// Implements `ReadArgs(template, array, rdargs)`: parses `dos`'s
/// command-line cursor per `template` into `array_addr`, returning the
/// `struct RDArgs*` anchor address to report in `D0`, or an `IoErr()`
/// code on failure. See the module docs for scope/deviations.
fn read_args(
    heap: &mut GuestHeap,
    mem: &mut dyn AddressSpace,
    dos: &mut DosState,
    template_bytes: &[u8],
    array_addr: u32,
    rdargs_ptr: u32,
) -> Result<u32, i32> {
    let template = parse_template(template_bytes)?;

    let (cmdline_addr, cmdline_len) = dos.cmdline.unwrap_or((0, 0));
    let mut buf = Vec::with_capacity(cmdline_len as usize);
    for offset in 0..cmdline_len {
        buf.push(mem.read_u8(cmdline_addr.wrapping_add(offset)));
    }

    let mut reader = Reader::new(&buf, dos.cmdline_pos as usize);
    let result = match_args(&mut reader, &template);
    dos.cmdline_pos = reader.pos as u32;

    let slots = result?;
    let allocations = materialize(heap, mem, &template, slots, array_addr)?;

    let (anchor_addr, owns_anchor) = if rdargs_ptr != 0 {
        (rdargs_ptr, false)
    } else {
        let addr = heap.alloc(RDARGS_ANCHOR_SIZE).map_err(|_| {
            for a in &allocations {
                let _ = heap.free(*a);
            }
            crate::dosfile::ERROR_NO_FREE_STORE
        })?;
        (addr, true)
    };

    dos.rdargs.insert(
        anchor_addr,
        RdArgsEntry {
            allocations,
            owns_anchor,
        },
    );
    Ok(anchor_addr)
}

/// Implements `FreeArgs(rdargs)`: frees every resource a matching
/// [`read_args`] call recorded for `rdargs`, plus the anchor block
/// itself if `ReadArgs` allocated it. A no-op for `rdargs == 0` or an
/// address this runtime never returned from `ReadArgs`.
fn free_args(heap: &mut GuestHeap, dos: &mut DosState, rdargs_ptr: u32) {
    if rdargs_ptr == 0 {
        return;
    }
    let Some(entry) = dos.rdargs.remove(&rdargs_ptr) else {
        return;
    };
    for addr in entry.allocations {
        let _ = heap.free(addr);
    }
    if entry.owns_anchor {
        let _ = heap.free(rdargs_ptr);
    }
}

/// `ReadArgs` (`D1` = template `CString*`, `D2` = result array `LONG*`,
/// `D3` = optional `struct RDArgs*`). `D0` = anchor `struct RDArgs*` or
/// `0` (+ `IoErr()` set).
fn read_args_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let template_ptr = ctx.cpu.data_register(DataRegister(1));
    let array_addr = ctx.cpu.data_register(DataRegister(2));
    let rdargs_ptr = ctx.cpu.data_register(DataRegister(3));
    let template_bytes = read_c_string(ctx.mem, template_ptr);

    match read_args(
        ctx.heap,
        ctx.mem,
        ctx.dos,
        &template_bytes,
        array_addr,
        rdargs_ptr,
    ) {
        Ok(anchor_addr) => ctx.cpu.set_data_register(DataRegister(0), anchor_addr),
        Err(code) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), 0);
        }
    }
    Ok(())
}

/// `FreeArgs` (`D1` = `struct RDArgs*` from `ReadArgs`). No return value.
fn free_args_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let rdargs_ptr = ctx.cpu.data_register(DataRegister(1));
    free_args(ctx.heap, ctx.dos, rdargs_ptr);
    Ok(())
}

/// Registers `ReadArgs`/`FreeArgs` onto [`DOS_LIBRARY_BASE`], looked up
/// by name through [`DOS_LVOS`]. Called from [`crate::dispatch::
/// Runtime::new`] alongside the other `dos.library` registrations; these
/// handlers need [`DosState::cmdline`] (always set by `Runtime::new`),
/// not a `Vfs`, so they work regardless of whether one is installed.
pub fn register_dosargs_handlers<C: Cpu + 'static>(
    table: &mut LibraryTable<C>,
    mem: &mut C::Memory,
) {
    table
        .register_by_name(
            mem,
            DOS_LIBRARY_BASE,
            DOS_LVOS,
            "dos.library",
            "ReadArgs",
            read_args_handler::<C>,
        )
        .unwrap_or_else(|e| panic!("ReadArgs should be in DOS_LVOS: {e}"));
    table
        .register_by_name(
            mem,
            DOS_LIBRARY_BASE,
            DOS_LVOS,
            "dos.library",
            "FreeArgs",
            free_args_handler::<C>,
        )
        .unwrap_or_else(|e| panic!("FreeArgs should be in DOS_LVOS: {e}"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::FlatMemory;

    /// Builds a fresh `(heap, mem, dos)` triple with `dos.cmdline`
    /// pointing at `cmdline` (space-joined-and-newline-terminated, same
    /// framing `Runtime::new` gives the real `A0`/`D0` buffer).
    fn setup(cmdline: &[&str]) -> (GuestHeap, FlatMemory, DosState) {
        let mut mem = FlatMemory::new(0x4000);
        let mut heap = GuestHeap::new(0x100, 0x3000);
        let mut line = cmdline.join(" ").into_bytes();
        line.push(b'\n');
        let addr = heap.alloc(line.len() as u32).unwrap();
        for (i, &b) in line.iter().enumerate() {
            mem.write_u8(addr + i as u32, b);
        }
        let mut dos = DosState::new(None);
        dos.cmdline = Some((addr, line.len() as u32));
        (heap, mem, dos)
    }

    /// Calls `ReadArgs(template, array, 0)` and returns `(anchor_addr,
    /// array_addr)`, panicking (with the `IoErr()` code) on failure.
    fn call_read_args(
        heap: &mut GuestHeap,
        mem: &mut FlatMemory,
        dos: &mut DosState,
        template: &str,
        slot_count: u32,
    ) -> (u32, u32) {
        let array_addr = heap.alloc(slot_count * 4).unwrap();
        for i in 0..slot_count {
            mem.write_u32(array_addr + i * 4, 0);
        }
        let anchor = read_args(heap, mem, dos, template.as_bytes(), array_addr, 0)
            .unwrap_or_else(|e| panic!("ReadArgs failed: IoErr {e}"));
        (anchor, array_addr)
    }

    /// Like [`call_read_args`] but returns the `IoErr()` code instead of
    /// panicking, for error-path tests.
    fn call_read_args_err(
        heap: &mut GuestHeap,
        mem: &mut FlatMemory,
        dos: &mut DosState,
        template: &str,
        slot_count: u32,
    ) -> i32 {
        let array_addr = heap.alloc(slot_count * 4).unwrap();
        for i in 0..slot_count {
            mem.write_u32(array_addr + i * 4, 0);
        }
        read_args(heap, mem, dos, template.as_bytes(), array_addr, 0).unwrap_err()
    }

    #[test]
    fn plain_string_and_switch_by_keyword_and_position() {
        let (mut heap, mut mem, mut dos) = setup(&["SYS:", "ALL"]);
        let (_, array) = call_read_args(&mut heap, &mut mem, &mut dos, "DIR,ALL/S", 2);
        let dir_ptr = mem.read_u32(array);
        assert_eq!(read_c_string(&mem, dir_ptr), b"SYS:");
        assert_eq!(mem.read_u32(array + 4), 1);
    }

    #[test]
    fn keyword_with_equals_and_with_space() {
        let (mut heap, mut mem, mut dos) = setup(&["FROM=org.txt", "TO", "dst.txt"]);
        let (_, array) = call_read_args(&mut heap, &mut mem, &mut dos, "FROM/K,TO/K", 2);
        let from_ptr = mem.read_u32(array);
        let to_ptr = mem.read_u32(array + 4);
        assert_eq!(read_c_string(&mem, from_ptr), b"org.txt");
        assert_eq!(read_c_string(&mem, to_ptr), b"dst.txt");
    }

    #[test]
    fn number_argument() {
        let (mut heap, mut mem, mut dos) = setup(&["BUF=8192"]);
        let (_, array) = call_read_args(&mut heap, &mut mem, &mut dos, "BUF/K/N", 1);
        let ptr = mem.read_u32(array);
        assert_ne!(ptr, 0);
        assert_eq!(mem.read_u32(ptr), 8192);
    }

    #[test]
    fn bad_number_sets_io_err() {
        let (mut heap, mut mem, mut dos) = setup(&["BUF=notanumber"]);
        let err = call_read_args_err(&mut heap, &mut mem, &mut dos, "BUF/K/N", 1);
        assert_eq!(err, ERROR_BAD_NUMBER);
    }

    #[test]
    fn required_arg_missing_is_an_error() {
        let (mut heap, mut mem, mut dos) = setup(&[]);
        let err = call_read_args_err(&mut heap, &mut mem, &mut dos, "NAME/A", 1);
        assert_eq!(err, ERROR_REQUIRED_ARG_MISSING);
    }

    #[test]
    fn multi_and_trailing_required_borrows_from_its_tail() {
        // The Copy command's classic template: one or more sources, one
        // destination, with no explicit TO= needed.
        let (mut heap, mut mem, mut dos) = setup(&["a", "b", "c", "dest"]);
        let (_, array) = call_read_args(&mut heap, &mut mem, &mut dos, "FROM/A/M,TO/A", 2);
        let from_list_ptr = mem.read_u32(array);
        let to_ptr = mem.read_u32(array + 4);
        assert_eq!(read_c_string(&mem, to_ptr), b"dest");
        let mut items = Vec::new();
        let mut p = from_list_ptr;
        loop {
            let s = mem.read_u32(p);
            if s == 0 {
                break;
            }
            items.push(read_c_string(&mem, s));
            p += 4;
        }
        assert_eq!(items, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
    }

    /// Regression test for issue #13: real `C:Search`'s full 9-item
    /// template (`FROM/M` followed by a trailing required `SEARCH/A`)
    /// parses a plain two-token command line fine -- confirming the
    /// bug wasn't in `/M`'s borrow-from-tail logic (as first suspected)
    /// but in `Search` itself picking the wrong template at runtime,
    /// due to an unpopulated `cli_StandardInput` (see
    /// `exectask::CLI_STANDARD_INPUT_OFFSET`).
    #[test]
    fn optional_multi_with_trailing_required_parses_plain_input() {
        let (mut heap, mut mem, mut dos) = setup(&["SYS:S/Shell-Startup", "Alias"]);
        call_read_args(
            &mut heap,
            &mut mem,
            &mut dos,
            "FROM/M,SEARCH/A,ALL/S,NONUM/S,QUIET/S,QUICK/S,FILE/S,PATTERN/S,CASE/S",
            9,
        );
    }

    #[test]
    fn quoted_string_with_escapes() {
        let (mut heap, mut mem, mut dos) = setup(&[r#""Hello*NThere""#]);
        let (_, array) = call_read_args(&mut heap, &mut mem, &mut dos, "TEXT", 1);
        let ptr = mem.read_u32(array);
        assert_eq!(read_c_string(&mem, ptr), b"Hello\nThere");
    }

    #[test]
    fn unmatched_quote_is_an_error() {
        let (mut heap, mut mem, mut dos) = setup(&[r#""unterminated"#]);
        let err = call_read_args_err(&mut heap, &mut mem, &mut dos, "TEXT", 1);
        assert_eq!(err, ERROR_UNMATCHED_QUOTES);
    }

    #[test]
    fn rest_of_line_takes_everything_verbatim() {
        let (mut heap, mut mem, mut dos) = setup(&["Echo", "hello", "FROM=x"]);
        let (_, array) = call_read_args(&mut heap, &mut mem, &mut dos, "NAME,REST/F", 2);
        let name_ptr = mem.read_u32(array);
        let rest_ptr = mem.read_u32(array + 4);
        assert_eq!(read_c_string(&mem, name_ptr), b"Echo");
        assert_eq!(read_c_string(&mem, rest_ptr), b"hello FROM=x");
    }

    #[test]
    fn too_many_positional_args_is_an_error() {
        let (mut heap, mut mem, mut dos) = setup(&["one", "two"]);
        let err = call_read_args_err(&mut heap, &mut mem, &mut dos, "ONLY", 1);
        assert_eq!(err, ERROR_TOO_MANY_ARGS);
    }

    #[test]
    fn free_args_returns_heap_to_prior_state() {
        let (mut heap, mut mem, mut dos) = setup(&["hello", "world"]);
        let free_before = heap.free_bytes();
        let (anchor, array_addr) = call_read_args(&mut heap, &mut mem, &mut dos, "A,B", 2);
        assert!(heap.free_bytes() < free_before);
        free_args(&mut heap, &mut dos, anchor);
        heap.free(array_addr).unwrap();
        assert_eq!(heap.free_bytes(), free_before);
    }

    #[test]
    fn bad_template_is_an_error() {
        let (mut heap, mut mem, mut dos) = setup(&["x"]);
        let err = call_read_args_err(&mut heap, &mut mem, &mut dos, "NAME/Z", 1);
        assert_eq!(err, ERROR_BAD_TEMPLATE);
    }

    #[test]
    fn template_item_with_an_empty_name_is_legal_and_positional_only() {
        // The real Workbench 3.1.4 C:/Wait binary's own template:
        // an anonymous /N item (numeric, no name -- can only ever be
        // filled positionally) followed by ordinary named items.
        let (mut heap, mut mem, mut dos) = setup(&["5"]);
        let (_, array_addr) = call_read_args(
            &mut heap,
            &mut mem,
            &mut dos,
            "/N,SEC=SECS/S,MIN=MINS/S,UNTIL/K,FILE=DIR/K",
            5,
        );
        let num_ptr = mem.read_u32(array_addr);
        assert_ne!(num_ptr, 0, "the anonymous /N slot should be filled");
        assert_eq!(mem.read_u32(num_ptr), 5);
    }

    #[test]
    fn template_item_with_an_empty_name_cannot_be_matched_by_keyword() {
        // Since it has no name, "5" can only bind positionally; a
        // template with just the anonymous item and nothing else still
        // parses and works the same way.
        let (mut heap, mut mem, mut dos) = setup(&["5"]);
        let (_, array_addr) = call_read_args(&mut heap, &mut mem, &mut dos, "/N", 1);
        let num_ptr = mem.read_u32(array_addr);
        assert_ne!(num_ptr, 0);
        assert_eq!(mem.read_u32(num_ptr), 5);
    }

    #[test]
    fn caller_supplied_rdargs_anchor_is_not_freed() {
        let (mut heap, mut mem, mut dos) = setup(&["value"]);
        let array_addr = heap.alloc(4).unwrap();
        mem.write_u32(array_addr, 0);
        let caller_rdargs = 0x2000; // not a heap allocation; FreeArgs must not touch it
        let anchor = read_args(
            &mut heap,
            &mut mem,
            &mut dos,
            b"A",
            array_addr,
            caller_rdargs,
        )
        .expect("ok");
        assert_eq!(anchor, caller_rdargs);
        free_args(&mut heap, &mut dos, anchor);
        // No panic/double-free from freeing a non-heap address confirms
        // FreeArgs left it alone.
    }

    // --- End-to-end: real A-line trap dispatch, not a direct call ---

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
    fn end_to_end_read_args_via_trap_dispatch() {
        // D1 = template CString*, D2 = result array, D3 = 0 (rdargs);
        // jsr ReadArgs(a6); D0 (== the exit code, since nothing after
        // touches it) should be the nonzero RDArgs anchor, and the
        // result array should hold a real guest string pointer.
        let mut words = Vec::new();
        let template_idx = push_move_imm_to_d(&mut words, 1, 0);
        let array_idx = push_move_imm_to_d(&mut words, 2, 0);
        push_move_imm_to_d(&mut words, 3, 0);
        push_jsr(&mut words, 6, -798); // ReadArgs(a6)
        words.push(RTS);

        let template = b"DIR\0";
        let template_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        let array_addr = (template_addr + template.len() as u32 + 3) & !3;
        patch_imm32(&mut words, template_idx, template_addr);
        patch_imm32(&mut words, array_idx, array_addr);

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);
        for (i, &b) in template.iter().enumerate() {
            mem.write_u8(template_addr + i as u32, b);
        }
        mem.write_u32(array_addr, 0);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: array_addr + 4,
                args: vec!["SYS:".to_string()],
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_ne!(code, 0, "ReadArgs should return a nonzero RDArgs anchor");

        let dir_ptr = rt.memory().read_u32(array_addr);
        assert_eq!(read_c_string(rt.memory(), dir_ptr), b"SYS:");
    }
}
