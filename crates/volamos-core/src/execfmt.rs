//! `exec.library` `RawDoFmt`: the "C-`printf`-like" formatter every
//! AmigaOS C startup library builds its own `sprintf`/`Printf` on top
//! of, per the NDK autodoc's own worked example. Real `RawDoFmt` calls
//! back into a guest-supplied `PutChProc` once per output character
//! (including a final `NUL`), so this handler is one of two in the
//! runtime that step the CPU itself mid-handler, rather than only
//! reading/writing registers and memory -- see [`call_put_ch_proc`].
//! `Supervisor` (below) is the other, for the same underlying reason:
//! both need to run a guest-supplied routine synchronously and get its
//! result back before the enclosing library call can itself return.
//!
//! # Format string syntax
//!
//! `%[flags][width[.limit]][length]type`, matching the NDK autodoc:
//! flag `-` (left justify; default is right justify, space-padded, or
//! zero-padded if `width` starts with `0`), `.limit` (max characters
//! taken from a `%s` string), length `l` (32-bit input; the default for
//! `d`/`u`/`x`/`c` is 16-bit -- `%s`/`%b` pointers are always 32-bit
//! regardless), and types `b` (`BSTR`-ish: a pointer to a length byte
//! followed by that many raw characters, no `NUL`; `NULL` is an empty
//! string), `d`/`u`/`x` (signed/unsigned decimal, hex), `s` (a `NUL`-
//! terminated string pointer, `NULL` is an empty string), `c`
//! (character), and `%%` for a literal `%`.
//!
//! `%b`'s exact semantics are the one place the NDK autodoc itself
//! hedges ("32-bit BPTR to byte count followed by byte string, *or*
//! NULL terminated byte string") -- implemented here as the more
//! commonly documented length-prefixed reading, since no known real
//! `C:` command corpus target uses `%b` at all.
//!
//! # Calling back into guest code from a handler
//!
//! Every character `RawDoFmt` produces -- literal format-string bytes,
//! substituted values, and the final `NUL` -- goes through the guest's
//! own `PutChProc(Char, PutChData)` (`D0` = char, `A3` = the pass-
//! through `PutChData`), a real m68k subroutine (`stuffChar` in the NDK
//! example: `move.b d0,(a3)+; rts`). [`call_put_ch_proc`] executes it
//! for real: push a sentinel return address ([`crate::dispatch::
//! EXIT_STUB_ADDR`], reused here purely as an address this runtime never
//! writes real code at -- not for its `Runtime::run` "whole program
//! done" meaning, since this is a raw `Cpu::step` loop this module
//! drives itself, not `Runtime::run`'s dispatch loop), set up `D0`/`A3`/
//! `PC`, then step until `PC` reaches that sentinel (i.e. the callee's
//! own `rts` returned) or a step budget is exhausted. If the callee
//! itself traps into another library call, that's out of scope --
//! real-world `PutChProc`s (`stuffChar` and dos.library's own file-
//! writing variants used by `Printf`/`VPrintf`) are simple, no known
//! corpus target's `RawDoFmt`-supplied callback calls back out further
//! -- and is reported as a clean [`DispatchError`] rather than silently
//! hanging or corrupting guest state.

use crate::cpu::{AddressRegister, Cpu, DataRegister, StopReason};
use crate::dispatch::{
    DispatchError, EXEC_LIBRARY_BASE, EXIT_STUB_ADDR, HandlerContext, LibraryTable,
};
use crate::guestmem::read_c_string;
use crate::lvos::exec::EXEC_LVOS;
use crate::memory::AddressSpace;

/// Hard cap on how many instructions one `PutChProc` callout may take
/// before this runtime gives up and reports a stuck callback rather than
/// hanging forever on a guest bug.
const CALLOUT_STEP_BUDGET: u32 = 100_000;

/// Executes the guest's `PutChProc(char, put_ch_data)` for real (see the
/// module docs), returning the (possibly mutated, e.g. `(a3)+`-style
/// pointer bumps) value of `A3` afterward so the caller can thread it
/// into the next callout.
fn call_put_ch_proc<C: Cpu>(
    cpu: &mut C,
    mem: &mut C::Memory,
    put_ch_proc: u32,
    char_byte: u8,
    put_ch_data: u32,
) -> Result<u32, DispatchError> {
    let saved_pc = cpu.pc();
    let saved_sp = cpu.address_register(AddressRegister(7));
    let saved_d0 = cpu.data_register(DataRegister(0));
    let saved_a3 = cpu.address_register(AddressRegister(3));

    let sp = saved_sp.wrapping_sub(4);
    mem.write_u32(sp, EXIT_STUB_ADDR);
    cpu.set_address_register(AddressRegister(7), sp);
    cpu.set_data_register(DataRegister(0), u32::from(char_byte));
    cpu.set_address_register(AddressRegister(3), put_ch_data);
    cpu.set_pc(put_ch_proc);

    let mut steps = 0u32;
    while cpu.pc() != EXIT_STUB_ADDR {
        if steps >= CALLOUT_STEP_BUDGET {
            return Err(DispatchError::HandlerFailed {
                library: "exec.library".to_string(),
                lvo: -522,
                handler_name: "RawDoFmt".to_string(),
                message: format!(
                    "PutChProc at {put_ch_proc:#010x} did not return within \
                     {CALLOUT_STEP_BUDGET} steps -- possible infinite loop \
                     or missing rts"
                ),
            });
        }
        match cpu.step(mem) {
            StopReason::Step => {}
            StopReason::Halted => {
                return Err(DispatchError::HandlerFailed {
                    library: "exec.library".to_string(),
                    lvo: -522,
                    handler_name: "RawDoFmt".to_string(),
                    message: format!("PutChProc at {put_ch_proc:#010x} halted the CPU"),
                });
            }
            StopReason::Trap(info) => {
                return Err(DispatchError::HandlerFailed {
                    library: "exec.library".to_string(),
                    lvo: -522,
                    handler_name: "RawDoFmt".to_string(),
                    message: format!(
                        "PutChProc at {put_ch_proc:#010x} trapped at \
                         {:#010x} ({:?}) -- calling back into another \
                         library call from a RawDoFmt PutChProc isn't \
                         supported",
                        info.pc, info.kind
                    ),
                });
            }
            StopReason::PcOutOfBounds { pc } => {
                return Err(DispatchError::HandlerFailed {
                    library: "exec.library".to_string(),
                    lvo: -522,
                    handler_name: "RawDoFmt".to_string(),
                    message: format!(
                        "PutChProc at {put_ch_proc:#010x} ran the program \
                         counter off the end of guest memory (reached {pc:#010x})"
                    ),
                });
            }
        }
        steps += 1;
    }

    let result_a3 = cpu.address_register(AddressRegister(3));
    cpu.set_pc(saved_pc);
    cpu.set_address_register(AddressRegister(7), saved_sp);
    cpu.set_data_register(DataRegister(0), saved_d0);
    cpu.set_address_register(AddressRegister(3), saved_a3);
    Ok(result_a3)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FmtType {
    Bstr,
    Decimal,
    Unsigned,
    Hex,
    Str,
    Char,
}

struct Directive {
    left_justify: bool,
    zero_pad: bool,
    width: usize,
    limit: Option<usize>,
    is_long: bool,
    ty: FmtType,
}

/// Parses one `%...` directive starting right after the `%` at
/// `fmt[*pos]`, advancing `*pos` past it. `None` if `fmt[*pos]` isn't a
/// recognized type character at all (treated as a literal `%` followed
/// by that character, matching a lenient real-world reading rather than
/// erroring out).
fn parse_directive(fmt: &[u8], pos: &mut usize) -> Option<Directive> {
    let start = *pos;
    let mut left_justify = false;
    if fmt.get(*pos) == Some(&b'-') {
        left_justify = true;
        *pos += 1;
    }
    let zero_pad = fmt.get(*pos) == Some(&b'0');
    let mut width = 0usize;
    while let Some(&c) = fmt.get(*pos) {
        if c.is_ascii_digit() {
            width = width * 10 + (c - b'0') as usize;
            *pos += 1;
        } else {
            break;
        }
    }
    let mut limit = None;
    if fmt.get(*pos) == Some(&b'.') {
        *pos += 1;
        let mut n = 0usize;
        let mut any = false;
        while let Some(&c) = fmt.get(*pos) {
            if c.is_ascii_digit() {
                n = n * 10 + (c - b'0') as usize;
                *pos += 1;
                any = true;
            } else {
                break;
            }
        }
        if any {
            limit = Some(n);
        }
    }
    let is_long = if fmt.get(*pos) == Some(&b'l') {
        *pos += 1;
        true
    } else {
        false
    };
    let ty = match fmt.get(*pos) {
        Some(b'b') => FmtType::Bstr,
        Some(b'd') => FmtType::Decimal,
        Some(b'u') => FmtType::Unsigned,
        Some(b'x') => FmtType::Hex,
        Some(b's') => FmtType::Str,
        Some(b'c') => FmtType::Char,
        _ => {
            *pos = start;
            return None;
        }
    };
    *pos += 1;
    Some(Directive {
        left_justify,
        zero_pad,
        width,
        limit,
        is_long,
        ty,
    })
}

/// Formats one directive's substituted text (the raw characters,
/// *before* width padding) given the raw value/bytes already read off
/// the data stream.
fn format_value(
    ty: FmtType,
    is_long: bool,
    raw: u32,
    string_bytes: &[u8],
    limit: Option<usize>,
) -> Vec<u8> {
    match ty {
        FmtType::Decimal => {
            let v: i32 = if is_long {
                raw as i32
            } else {
                (raw as u16) as i16 as i32
            };
            v.to_string().into_bytes()
        }
        FmtType::Unsigned => {
            let v: u32 = if is_long { raw } else { u32::from(raw as u16) };
            v.to_string().into_bytes()
        }
        FmtType::Hex => {
            let v: u32 = if is_long { raw } else { u32::from(raw as u16) };
            format!("{v:x}").into_bytes()
        }
        FmtType::Char => {
            vec![raw as u8]
        }
        FmtType::Str | FmtType::Bstr => {
            let mut bytes = string_bytes.to_vec();
            if let Some(n) = limit {
                bytes.truncate(n);
            }
            bytes
        }
    }
}

fn pad(mut text: Vec<u8>, width: usize, left_justify: bool, zero_pad: bool) -> Vec<u8> {
    if text.len() >= width {
        return text;
    }
    let fill = if zero_pad && !left_justify {
        b'0'
    } else {
        b' '
    };
    let pad_len = width - text.len();
    if left_justify {
        text.extend(std::iter::repeat_n(b' ', pad_len));
        text
    } else {
        let mut out = vec![fill; pad_len];
        out.append(&mut text);
        out
    }
}

/// The pure formatting core shared by `RawDoFmt` (which then streams the
/// result through a real guest `PutChProc` callback -- see
/// [`raw_do_fmt_handler`]) and `dos.library`'s `VPrintf`/`VFPrintf`
/// (`crate::dosprintf`, which just writes the result to a file/`Output()`
/// directly -- no guest callback involved for those). Renders `fmt`
/// against the data stream starting at `data_ptr`, returning the
/// rendered bytes (*not* including the final `NUL` `RawDoFmt` itself
/// always sends through `PutChProc` -- callers that need that behavior
/// add it themselves) and the data-stream pointer one past the last byte
/// consumed (`RawDoFmt`'s own `D0` result).
pub(crate) fn render_format(
    mem: &dyn AddressSpace,
    fmt: &[u8],
    mut data_ptr: u32,
) -> (Vec<u8>, u32) {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < fmt.len() {
        if fmt[i] != b'%' {
            out.push(fmt[i]);
            i += 1;
            continue;
        }
        i += 1;
        if fmt.get(i) == Some(&b'%') {
            out.push(b'%');
            i += 1;
            continue;
        }
        let Some(dir) = parse_directive(fmt, &mut i) else {
            out.push(b'%');
            continue;
        };

        let (raw, string_bytes) = match dir.ty {
            FmtType::Str => {
                let ptr = mem.read_u32(data_ptr);
                data_ptr = data_ptr.wrapping_add(4);
                let bytes = if ptr == 0 {
                    Vec::new()
                } else {
                    read_c_string(mem, ptr)
                };
                (0, bytes)
            }
            FmtType::Bstr => {
                let ptr = mem.read_u32(data_ptr);
                data_ptr = data_ptr.wrapping_add(4);
                let bytes = if ptr == 0 {
                    Vec::new()
                } else {
                    let len = u32::from(mem.read_u8(ptr));
                    (0..len).map(|j| mem.read_u8(ptr + 1 + j)).collect()
                };
                (0, bytes)
            }
            _ => {
                let value = if dir.is_long {
                    let v = mem.read_u32(data_ptr);
                    data_ptr = data_ptr.wrapping_add(4);
                    v
                } else {
                    let v = mem.read_u16(data_ptr);
                    data_ptr = data_ptr.wrapping_add(2);
                    u32::from(v)
                };
                (value, Vec::new())
            }
        };

        let text = format_value(dir.ty, dir.is_long, raw, &string_bytes, dir.limit);
        let text = pad(text, dir.width, dir.left_justify, dir.zero_pad);
        out.extend(text);
    }
    (out, data_ptr)
}

/// `RawDoFmt` (`A0` = format string, `A1` = data stream, `A2` =
/// `PutChProc` (may be legally `0` on `NULL`-tolerant `PutChProc`
/// callers -- but this runtime has no "default stuffChar" of its own to
/// fall back to, so a `NULL` `PutChProc` is reported as a
/// [`DispatchError`] rather than silently doing nothing), `A3` =
/// `PutChData`). `D0` = one past the last data-stream byte consumed
/// (matching real V36+ `RawDoFmt`).
fn raw_do_fmt_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let fmt_addr = ctx.cpu.address_register(AddressRegister(0));
    let mut data_ptr = ctx.cpu.address_register(AddressRegister(1));
    let put_ch_proc = ctx.cpu.address_register(AddressRegister(2));
    let mut put_ch_data = ctx.cpu.address_register(AddressRegister(3));

    if put_ch_proc == 0 {
        return Err(DispatchError::HandlerFailed {
            library: "exec.library".to_string(),
            lvo: -522,
            handler_name: "RawDoFmt".to_string(),
            message: "PutChProc is NULL; this runtime doesn't implement exec's \
                       V45.1+ default stuffChar fallback"
                .to_string(),
        });
    }

    let fmt = read_c_string(ctx.mem, fmt_addr);
    let (rendered, final_data_ptr) = render_format(ctx.mem, &fmt, data_ptr);
    data_ptr = final_data_ptr;

    for &byte in &rendered {
        put_ch_data = call_put_ch_proc(ctx.cpu, ctx.mem, put_ch_proc, byte, put_ch_data)?;
    }
    // "the procedure is called with a NULL Char at the end of the
    // format string" -- always, even though the NUL itself is never
    // part of `rendered`.
    call_put_ch_proc(ctx.cpu, ctx.mem, put_ch_proc, 0, put_ch_data)?;

    ctx.cpu.set_data_register(DataRegister(0), data_ptr);
    Ok(())
}

/// Hard cap on how many instructions one `Supervisor` callout may take
/// -- see [`CALLOUT_STEP_BUDGET`]'s use for `PutChProc`.
const SUPERVISOR_STEP_BUDGET: u32 = 100_000;

/// `exec.library`'s `Supervisor` (LVO -30: `A5` = a routine to call in
/// supervisor mode, `struct { LONG (*)() }`). `D0` = whatever the
/// routine itself left in `D0`.
///
/// Real `Supervisor` elevates to supervisor mode, calls the routine
/// (preserving every register the caller had set except `PC`/`SP`,
/// which the call/return sequence naturally handles), and drops back to
/// the caller's original privilege level on return -- the routine sees
/// (and, other than `D0`, can freely leave changed in) the same
/// registers the caller had, unlike [`call_put_ch_proc`]'s `RawDoFmt`
/// callback, which restores everything but its own `A3` contract
/// afterward.
///
/// **The routine must terminate with `RTE`, not `RTS`** -- the real,
/// documented `Supervisor` contract (confirmed the hard way: an early
/// implementation here pushed a plain `RTS`-style return address, and
/// the real `CPU` command's routine promptly executed a bare `rte`
/// with nothing valid on the stack, sending the program counter
/// straight off the end of guest memory). This runtime has no user/
/// supervisor privilege distinction at all (every guest instruction
/// already executes as if privileged), so there's nothing to actually
/// elevate: this pushes a real exception-style stack frame (`SR` then
/// `PC`, 6 bytes, matching every other real 68000 exception this
/// runtime delivers -- see [`crate::cpu::Cpu::take_hardware_exception`]'s
/// doc) with `PC` = [`EXIT_STUB_ADDR`], sets `PC` to the routine, then
/// steps until the routine's own `RTE` pops that frame back off, the
/// same technique `call_put_ch_proc` uses for `RawDoFmt`'s `PutChProc`.
/// Found missing while running the real `CPU` command (Workbench 3.1.4
/// `C:`), which wraps its direct `CACR`-register access in `Supervisor`
/// (a real, privileged `movec` instruction on 68020+) even though
/// `CacheControl` already answers its query -- defensive real-world
/// code, not a bug.
fn supervisor_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let routine = ctx.cpu.address_register(AddressRegister(5));
    let saved_sp = ctx.cpu.address_register(AddressRegister(7));
    let saved_pc = ctx.cpu.pc();
    let saved_sr = ctx.cpu.sr();

    let sp = saved_sp.wrapping_sub(6);
    ctx.mem.write_u16(sp, saved_sr);
    ctx.mem.write_u32(sp.wrapping_add(2), EXIT_STUB_ADDR);
    ctx.cpu.set_address_register(AddressRegister(7), sp);
    ctx.cpu.set_pc(routine);

    let mut steps = 0u32;
    while ctx.cpu.pc() != EXIT_STUB_ADDR {
        if steps >= SUPERVISOR_STEP_BUDGET {
            return Err(DispatchError::HandlerFailed {
                library: "exec.library".to_string(),
                lvo: -30,
                handler_name: "Supervisor".to_string(),
                message: format!(
                    "routine at {routine:#010x} did not return within \
                     {SUPERVISOR_STEP_BUDGET} steps -- possible infinite loop \
                     or missing rts"
                ),
            });
        }
        match ctx.cpu.step(ctx.mem) {
            StopReason::Step => {}
            StopReason::Halted => {
                return Err(DispatchError::HandlerFailed {
                    library: "exec.library".to_string(),
                    lvo: -30,
                    handler_name: "Supervisor".to_string(),
                    message: format!("routine at {routine:#010x} halted the CPU"),
                });
            }
            StopReason::Trap(info) => {
                return Err(DispatchError::HandlerFailed {
                    library: "exec.library".to_string(),
                    lvo: -30,
                    handler_name: "Supervisor".to_string(),
                    message: format!(
                        "routine at {routine:#010x} trapped at {:#010x} ({:?}) \
                         -- calling back into another library call from a \
                         Supervisor routine isn't supported",
                        info.pc, info.kind
                    ),
                });
            }
            StopReason::PcOutOfBounds { pc } => {
                return Err(DispatchError::HandlerFailed {
                    library: "exec.library".to_string(),
                    lvo: -30,
                    handler_name: "Supervisor".to_string(),
                    message: format!(
                        "routine at {routine:#010x} ran the program counter \
                         off the end of guest memory (reached {pc:#010x})"
                    ),
                });
            }
        }
        steps += 1;
    }

    ctx.cpu.set_pc(saved_pc);
    ctx.cpu.set_address_register(AddressRegister(7), saved_sp);
    Ok(())
}

/// Registers `RawDoFmt`/`Supervisor` onto [`EXEC_LIBRARY_BASE`], looked
/// up by name through [`EXEC_LVOS`]. Called from
/// [`crate::dispatch::Runtime::new`] alongside the other `exec.library`
/// registrations.
pub fn register_execfmt_handlers<C: Cpu + 'static>(
    table: &mut LibraryTable<C>,
    mem: &mut C::Memory,
) {
    table
        .register_by_name(
            mem,
            EXEC_LIBRARY_BASE,
            EXEC_LVOS,
            "exec.library",
            "RawDoFmt",
            raw_do_fmt_handler::<C>,
        )
        .unwrap_or_else(|e| panic!("RawDoFmt should be in EXEC_LVOS: {e}"));
    table
        .register_by_name(
            mem,
            EXEC_LIBRARY_BASE,
            EXEC_LVOS,
            "exec.library",
            "Supervisor",
            supervisor_handler::<C>,
        )
        .unwrap_or_else(|e| panic!("Supervisor should be in EXEC_LVOS: {e}"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{Runtime, StartConfig};
    use crate::guestmem::write_c_string;
    use crate::memory::FlatMemory;

    fn move_imm_to_a(n: u16) -> u16 {
        0x207C | (n << 9)
    }
    fn move_imm_to_d(n: u16) -> u16 {
        0x203C | (n << 9)
    }
    fn movea_exec_base_to_a6() -> [u16; 3] {
        [
            move_imm_to_a(6),
            (EXEC_LIBRARY_BASE >> 16) as u16,
            EXEC_LIBRARY_BASE as u16,
        ]
    }
    fn jsr_disp16_a6(disp: i32) -> [u16; 2] {
        [0x4EAE, disp as u16]
    }
    const RTS: u16 = 0x4E75;
    const STUFF_CHAR: [u16; 2] = [0x16C0, 0x4E75]; // move.b d0,(a3)+ ; rts

    fn runtime_with_program(words: &[u16]) -> Runtime<M68kCpu> {
        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, words);
        Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: TRAP_TABLE_END + (words.len() as u32) * 2 + 64,
                args: Vec::new(),
                ..StartConfig::default()
            },
        )
    }

    fn load_words(mem: &mut FlatMemory, addr: u32, words: &[u16]) {
        let mut offset = addr;
        for &w in words {
            mem.write_u16(offset, w);
            offset += 2;
        }
    }

    /// `sprintf("%s have %ld eyes.", "Fish", 2)` into a guest buffer,
    /// via the exact `stuffChar` idiom the NDK autodoc's own example
    /// uses -- this is the shape virtually every real AmigaOS C
    /// program's RawDoFmt usage takes.
    #[test]
    fn end_to_end_sprintf_style_formatting() {
        let mut words = movea_exec_base_to_a6().to_vec();

        let fmt_idx = words.len();
        words.push(move_imm_to_a(0)); // A0 = format string
        words.push(0);
        words.push(0);
        let data_idx = words.len();
        words.push(move_imm_to_a(1)); // A1 = data stream
        words.push(0);
        words.push(0);
        let putch_idx = words.len();
        words.push(move_imm_to_a(2)); // A2 = PutChProc (stuffChar)
        words.push(0);
        words.push(0);
        let out_idx = words.len();
        words.push(move_imm_to_a(3)); // A3 = PutChData (output buffer)
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-522)); // RawDoFmt(a6)
        words.push(RTS);

        // stuffChar lives in dead-code space *after* the main flow's own
        // RTS -- it must only ever be reached via RawDoFmt's callout,
        // never by falling into it from sequential execution.
        let stuff_char_idx = words.len();
        words.extend_from_slice(&STUFF_CHAR);
        let stuff_char_addr = TRAP_TABLE_END + (stuff_char_idx as u32) * 2;

        let fmt_str = b"%s have %ld eyes.";
        let name_str = b"Fish";
        let fmt_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        let name_addr = fmt_addr + fmt_str.len() as u32 + 1;
        let data_addr = (name_addr + name_str.len() as u32 + 1 + 3) & !3;
        let out_addr = data_addr + 16;

        words[fmt_idx + 1] = (fmt_addr >> 16) as u16;
        words[fmt_idx + 2] = fmt_addr as u16;
        words[data_idx + 1] = (data_addr >> 16) as u16;
        words[data_idx + 2] = data_addr as u16;
        words[putch_idx + 1] = (stuff_char_addr >> 16) as u16;
        words[putch_idx + 2] = stuff_char_addr as u16;
        words[out_idx + 1] = (out_addr >> 16) as u16;
        words[out_idx + 2] = out_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);
        write_c_string(&mut mem, fmt_addr, fmt_str);
        write_c_string(&mut mem, name_addr, name_str);
        // Data stream: %s reads a 4-byte pointer, %ld reads a 4-byte long.
        mem.write_u32(data_addr, name_addr);
        mem.write_u32(data_addr + 4, 2);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: out_addr + 64,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(read_c_string(rt.memory(), out_addr), b"Fish have 2 eyes.");
    }

    // --- Supervisor ---

    #[test]
    fn supervisor_runs_the_routine_and_returns_its_d0_via_rte() {
        // The real, documented contract: the routine must end with RTE,
        // not RTS (see supervisor_handler's doc comment for why -- an
        // early implementation here got this wrong and the real CPU
        // command's own Supervisor routine sent PC off the end of guest
        // memory as a result).
        let mut words = movea_exec_base_to_a6().to_vec();
        let routine_idx = words.len();
        words.push(move_imm_to_a(5)); // A5 = routine (patched)
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-30)); // Supervisor(a6)
        words.push(RTS);

        // moveq #42,d0 ; rte
        const ROUTINE: [u16; 2] = [0x702A, 0x4E73];
        let routine_idx_words = words.len();
        words.extend_from_slice(&ROUTINE);
        let routine_addr = TRAP_TABLE_END + (routine_idx_words as u32) * 2;
        words[routine_idx + 1] = (routine_addr >> 16) as u16;
        words[routine_idx + 2] = routine_addr as u16;

        let mut rt = runtime_with_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 42);
    }

    #[test]
    fn supervisor_preserves_registers_the_routine_did_not_touch() {
        let mut words = movea_exec_base_to_a6().to_vec();
        words.push(move_imm_to_d(1)); // D1 = 0x1234 -- must survive
        words.push(0);
        words.push(0x1234);
        let routine_idx = words.len();
        words.push(move_imm_to_a(5)); // A5 = routine (patched)
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-30)); // Supervisor(a6)
        words.push(0x2001); // move.l d1,d0 -- exit code proves D1 survived
        words.push(RTS);

        // A no-op routine that only RTEs straight back.
        const ROUTINE: [u16; 1] = [0x4E73]; // rte
        let routine_idx_words = words.len();
        words.extend_from_slice(&ROUTINE);
        let routine_addr = TRAP_TABLE_END + (routine_idx_words as u32) * 2;
        words[routine_idx + 1] = (routine_addr >> 16) as u16;
        words[routine_idx + 2] = routine_addr as u16;

        let mut rt = runtime_with_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 0x1234, "D1 set before Supervisor should survive it");
    }

    #[test]
    fn supervisor_routine_that_never_returns_is_a_clean_error_not_a_hang() {
        let mut words = movea_exec_base_to_a6().to_vec();
        let routine_idx = words.len();
        words.push(move_imm_to_a(5)); // A5 = routine (patched)
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-30)); // Supervisor(a6)
        words.push(RTS);

        // bra.s $ (an infinite self-loop, never RTEs).
        const ROUTINE: [u16; 1] = [0x60FE];
        let routine_idx_words = words.len();
        words.extend_from_slice(&ROUTINE);
        let routine_addr = TRAP_TABLE_END + (routine_idx_words as u32) * 2;
        words[routine_idx + 1] = (routine_addr >> 16) as u16;
        words[routine_idx + 2] = routine_addr as u16;

        let mut rt = runtime_with_program(&words);
        let mut out = Vec::new();
        let err = rt.run(&mut out, None).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("did not return"),
            "unexpected message: {message}"
        );
    }
}
