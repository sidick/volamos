//! `dos.library` `VPrintf`/`VFPrintf`: format a string exactly like
//! `exec.library`'s `RawDoFmt` (see `crate::execfmt`'s module docs for
//! the format syntax and the shared [`crate::execfmt::render_format`]
//! core), then write the result to `Output()` (`VPrintf`) or an
//! explicit file handle (`VFPrintf`) -- no real guest `PutChProc`
//! callback involved for either, since both write straight to a real
//! `dos.library` destination rather than an arbitrary caller-supplied
//! routine. `Printf`/`FPrintf` (the varargs C wrappers shown in the NDK
//! autodoc) aren't separate LVOs -- they're compiled to call these same
//! two entry points with the caller's own stack as the data stream, so
//! implementing `VPrintf`/`VFPrintf` covers both.
//!
//! `VFWritef`/`FWritef` are a *different* formatter, despite the
//! superficially identical `(fh, fmt, argv)` signature: per the RKRM's
//! own description, `VFWritef` follows the BCPL `Writef` directive
//! syntax (`%S`/`%T<w>`/`%C`/`%O<w>`/`%X<w>`/`%D<w>`/`%N`/`%U<w>`/
//! `%*`/`%%`), is not locale-patched, and is unrelated to `RawDoFmt`'s
//! C-`printf`-style directives -- despite an earlier version of this
//! module aliasing it straight onto [`vfprintf_handler`] (wrong: it
//! left a real `Eval`'s `%N` directive completely unsubstituted,
//! printing the literal text `%n` instead of the computed result --
//! issue #12). See [`render_writef_format`] for the real syntax this
//! module now implements instead.

use crate::cpu::{Cpu, DataRegister};
use crate::dispatch::{DOS_LIBRARY_BASE, DispatchError, HandlerContext, LibraryTable};
use crate::dosbuf::{Written, write_bytes};
use crate::execfmt::render_format;
use crate::guestmem::{addr_from_bptr, read_c_string};
use crate::lvos::dos::DOS_LVOS;
use crate::memory::AddressSpace;

const RESULT_ERROR: u32 = 0xFFFF_FFFF;

/// `VPrintf` (`D1` = format string, `D2` = data stream). `D0` = number
/// of characters written, or `-1` (+ `IoErr()` set). Writes to the
/// *current* `Output()` selection, so a preceding `SelectOutput` (e.g.
/// `Type FROM x TO y`) redirects it, exactly like real `VPrintf`.
fn vprintf_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let fmt_ptr = ctx.cpu.data_register(DataRegister(1));
    let data_ptr = ctx.cpu.data_register(DataRegister(2));

    let fmt = read_c_string(ctx.mem, fmt_ptr);
    let (rendered, _) = render_format(ctx.mem, &fmt, data_ptr);

    let out_addr = match crate::dosfile::current_output_addr(ctx) {
        Ok(addr) => addr,
        Err(code) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), RESULT_ERROR);
            return Ok(());
        }
    };
    match write_bytes(ctx, out_addr, &rendered, "VPrintf")? {
        Written::Done(Ok(n)) => ctx.cpu.set_data_register(DataRegister(0), n as u32),
        Written::Done(Err(code)) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), RESULT_ERROR);
        }
        Written::Switched => return Ok(()),
    }
    Ok(())
}

/// `VFPrintf` (`D1` = file handle `BPTR`, `D2` = format string, `D3` =
/// data stream). `D0` = number of characters written, or `-1` (+
/// `IoErr()` set).
fn vfprintf_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let bptr = ctx.cpu.data_register(DataRegister(1));
    let fmt_ptr = ctx.cpu.data_register(DataRegister(2));
    let data_ptr = ctx.cpu.data_register(DataRegister(3));
    let addr = addr_from_bptr(bptr);

    let fmt = read_c_string(ctx.mem, fmt_ptr);
    let (rendered, _) = render_format(ctx.mem, &fmt, data_ptr);

    match write_bytes(ctx, addr, &rendered, "VFPrintf")? {
        Written::Done(Ok(n)) => ctx.cpu.set_data_register(DataRegister(0), n as u32),
        Written::Done(Err(code)) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), RESULT_ERROR);
        }
        Written::Switched => return Ok(()),
    }
    Ok(())
}

/// Decodes a BCPL `Writef` field-width character: `'0'`-`'9'` -> `0`-`9`
/// directly, `'A'`-`'Z'`/`'a'`-`'z'` -> `10`-`35` (per the RKRM: "a
/// digit from 0 to 9 indicates the field widths from 0 to 9 directly;
/// characters A to Z indicate field widths from 10 onward" -- accepting
/// lowercase too since real directive *letters* turned out to be
/// case-insensitive, per this module's own doc note, and there's no
/// documented reason width chars would be stricter).
fn writef_width(c: u8) -> usize {
    match c {
        b'0'..=b'9' => (c - b'0') as usize,
        b'A'..=b'Z' => (c - b'A') as usize + 10,
        b'a'..=b'z' => (c - b'a') as usize + 10,
        _ => 0,
    }
}

/// Renders a BCPL `Writef`-style format string (`VFWritef`/`FWritef` --
/// see this module's doc note) against the `LONG` array at `argv_ptr`,
/// returning the formatted bytes. Every directive consumes exactly one
/// 4-byte array slot; a non-directive `%c` (such as `%%`) consumes none
/// -- `argv` is a plain
/// `LONG*`, unlike [`render_format`]'s mixed 16-/32-bit `RawDoFmt`
/// stream, so slot advancement is always by 4.
///
/// Measured on real Kickstart 3.1 (r40.68) under Copperline (issue #362),
/// and matching the NDK `VFWritef` autodoc:
///
/// - The directives are `%S`, `%T<w>`, `%C`, `%O<w>`, `%X<w>`, `%I<w>`,
///   `%U<w>`, `%N` and `%$` (ignore one argument). Any other `%c`
///   (including `%D` and `%*`, which an earlier version implemented)
///   prints the character `c` with the `%` dropped and consumes no
///   argument; that is also how `%%` prints a single `%`.
/// - `<w>` is one character in base 36 (`'0'`-`'9'` then `'A'`-`'Z'`, so
///   `A` is 10 and `G` is 16).
/// - `%I`/`%U` right-align a signed/unsigned decimal in a field of at
///   least `<w>` spaces and never truncate (`%I2` of 12345 is `12345`).
///   `%O`/`%X` print the low `<w>` digits (at least one), zero-padded:
///   `%X4` of 255 is `00FF` (uppercase), `%X2` of 0x1234 is `34`, `%X0`
///   is `4`. `%N` is any length.
///
/// Directive letters are matched case-insensitively: `%S`/`%T<w>` are
/// confirmed against the real `Which` binary (which literally embeds
/// `%s`), and `%N` against the real `Eval` binary (lowercase `%n`, issue
/// #12).
fn render_writef_format(mem: &dyn AddressSpace, fmt: &[u8], argv_ptr: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut argv = argv_ptr;
    let next_arg = |mem: &dyn AddressSpace, argv: &mut u32| {
        let v = mem.read_u32(*argv);
        *argv = argv.wrapping_add(4);
        v
    };

    let mut i = 0;
    while i < fmt.len() {
        let c = fmt[i];
        if c != b'%' {
            out.push(c);
            i += 1;
            continue;
        }
        i += 1;
        let Some(&directive) = fmt.get(i) else {
            out.push(b'%');
            break;
        };
        i += 1;
        match directive.to_ascii_uppercase() {
            b'$' => {
                next_arg(mem, &mut argv);
            }
            b'S' => {
                let ptr = next_arg(mem, &mut argv);
                out.extend(read_c_string(mem, ptr));
            }
            b'T' => {
                let width = fmt.get(i).copied().map(writef_width).unwrap_or(0);
                if fmt.get(i).is_some() {
                    i += 1;
                }
                let ptr = next_arg(mem, &mut argv);
                let s = read_c_string(mem, ptr);
                out.extend(&s);
                if s.len() < width {
                    out.resize(out.len() + (width - s.len()), b' ');
                }
            }
            b'C' => {
                let v = next_arg(mem, &mut argv);
                out.push(v as u8);
            }
            b'O' | b'X' | b'I' | b'U' => {
                let kind = directive.to_ascii_uppercase();
                let width = fmt.get(i).copied().map(writef_width).unwrap_or(0);
                if fmt.get(i).is_some() {
                    i += 1;
                }
                let v = next_arg(mem, &mut argv);
                let mut digits = match kind {
                    b'O' => format!("{v:o}"),
                    b'X' => format!("{v:X}"),
                    b'I' => format!("{}", v as i32),
                    _ => format!("{v}"),
                };
                if matches!(kind, b'O' | b'X') {
                    // Zero-padded, and cut to the low `width` digits -- at
                    // least one: `%X0` of 0x1234 prints `4` on real
                    // Kickstart 3.1, the same as `%X1`.
                    let width = width.max(1);
                    if digits.len() > width {
                        digits = digits[digits.len() - width..].to_string();
                    }
                    if digits.len() < width {
                        out.resize(out.len() + (width - digits.len()), b'0');
                    }
                } else if digits.len() < width {
                    out.resize(out.len() + (width - digits.len()), b' ');
                }
                out.extend(digits.as_bytes());
            }
            b'N' => {
                let v = next_arg(mem, &mut argv);
                out.extend(format!("{}", v as i32).as_bytes());
            }
            // Not a directive: the character itself, no argument.
            _ => out.push(directive),
        }
    }
    out
}

/// `VFWritef` (`D1` = file handle `BPTR`, `D2` = format string, `D3` =
/// `LONG*` data stream). `D0` = number of characters written, or `-1`
/// (+ `IoErr()` set). See this module's doc note for why this isn't
/// just [`vfprintf_handler`] under another name.
fn vfwritef_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let bptr = ctx.cpu.data_register(DataRegister(1));
    let fmt_ptr = ctx.cpu.data_register(DataRegister(2));
    let data_ptr = ctx.cpu.data_register(DataRegister(3));
    let addr = addr_from_bptr(bptr);

    let fmt = read_c_string(ctx.mem, fmt_ptr);
    let rendered = render_writef_format(ctx.mem, &fmt, data_ptr);

    match write_bytes(ctx, addr, &rendered, "VFWritef")? {
        Written::Done(Ok(n)) => ctx.cpu.set_data_register(DataRegister(0), n as u32),
        Written::Done(Err(code)) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), RESULT_ERROR);
        }
        Written::Switched => return Ok(()),
    }
    Ok(())
}

/// Registers `VPrintf`/`VFPrintf` onto [`DOS_LIBRARY_BASE`], looked up
/// by name through [`DOS_LVOS`]. Called from [`crate::dispatch::
/// Runtime::new`] alongside the other `dos.library` registrations.
pub fn register_dosprintf_handlers<C: Cpu + 'static>(
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
    reg!("VPrintf", vprintf_handler::<C>);
    reg!("VFPrintf", vfprintf_handler::<C>);
    // VFWritef(fh, fmt, argv) has the same D1/D2/D3 signature as
    // VFPrintf, but a different (BCPL Writef) format-string syntax --
    // see this module's doc note and vfwritef_handler.
    reg!("VFWritef", vfwritef_handler::<C>);
}

#[cfg(test)]
mod tests {
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{Runtime, StartConfig};
    use crate::guestmem::write_c_string;
    use crate::memory::{AddressSpace, FlatMemory};

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
    fn end_to_end_vprintf_writes_to_stdout_via_trap_dispatch() {
        let mut words = Vec::new();
        let fmt_idx = push_move_imm_to_d(&mut words, 1, 0);
        let data_idx = push_move_imm_to_d(&mut words, 2, 0);
        push_jsr(&mut words, 6, -954); // VPrintf(a6)
        words.push(RTS);

        let fmt_str = b"%s have %ld eyes.";
        let name_str = b"Fish";
        let fmt_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        let name_addr = fmt_addr + fmt_str.len() as u32 + 1;
        let data_addr = (name_addr + name_str.len() as u32 + 1 + 3) & !3;
        patch_imm32(&mut words, fmt_idx, fmt_addr);
        patch_imm32(&mut words, data_idx, data_addr);

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);
        write_c_string(&mut mem, fmt_addr, fmt_str);
        write_c_string(&mut mem, name_addr, name_str);
        mem.write_u32(data_addr, name_addr);
        mem.write_u32(data_addr + 4, 2);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: data_addr + 16,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 17, "\"Fish have 2 eyes.\" is 17 characters");
        assert_eq!(out, b"Fish have 2 eyes.");
    }

    #[test]
    fn end_to_end_vfwritef_writes_to_the_given_handle_via_trap_dispatch() {
        // VFWritef(fh, fmt, argv) -- same signature as VFPrintf, just
        // reached under its own LVO name (real Which calls it this way,
        // not via VFPrintf).
        let mut words = Vec::new();
        push_jsr(&mut words, 6, -60); // Output(a6): D0 = BPTR
        words.push(0x2200); // move.l d0,d1 (D1 = fh)
        let fmt_idx = push_move_imm_to_d(&mut words, 2, 0);
        let data_idx = push_move_imm_to_d(&mut words, 3, 0);
        push_jsr(&mut words, 6, -348); // VFWritef(a6)
        words.push(RTS);

        let fmt_str = b"found %s";
        let name_str = b"Which";
        let fmt_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        let name_addr = fmt_addr + fmt_str.len() as u32 + 1;
        let data_addr = (name_addr + name_str.len() as u32 + 1 + 3) & !3;
        patch_imm32(&mut words, fmt_idx, fmt_addr);
        patch_imm32(&mut words, data_idx, data_addr);

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);
        write_c_string(&mut mem, fmt_addr, fmt_str);
        write_c_string(&mut mem, name_addr, name_str);
        mem.write_u32(data_addr, name_addr);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: data_addr + 16,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 11, "\"found Which\" is 11 characters");
        assert_eq!(out, b"found Which");
    }

    /// Renders `fmt` against `args` (each a plain `LONG`), with `"abcdef"`
    /// available at 0x2000 for a `%T`/`%S` argument.
    fn writef(fmt: &str, args: &[u32]) -> String {
        let mut mem = FlatMemory::new(0x4000);
        for (i, &a) in args.iter().enumerate() {
            mem.write_u32(0x100 + 4 * i as u32, a);
        }
        crate::guestmem::write_c_string(&mut mem, 0x2000, b"abcdef");
        String::from_utf8(super::render_writef_format(&mem, fmt.as_bytes(), 0x100)).unwrap()
    }

    #[test]
    fn vfwritef_matches_real_kickstart_31_362() {
        // Every expectation below was measured on real Kickstart 3.1
        // (r40.68) under Copperline.
        assert_eq!(writef("%I5|%N", &[42, 43]), "   42|43");
        assert_eq!(
            writef("%IA|%N", &[42, 43]),
            "        42|43",
            "base-36 width: A = 10"
        );
        assert_eq!(writef("%I0|%N", &[42, 43]), "42|43");
        assert_eq!(writef("%I5", &[-42i32 as u32]), "  -42");
        assert_eq!(writef("%U5|%N", &[-42i32 as u32, 43]), "4294967254|43");
        assert_eq!(writef("%U5", &[7]), "    7");
        assert_eq!(writef("%I2", &[12345]), "12345", "I never truncates");
        assert_eq!(writef("%X4", &[255]), "00FF", "uppercase, zero-padded");
        assert_eq!(writef("%O4", &[255]), "0377");
        assert_eq!(
            writef("%X2|%N", &[0x1234, 43]),
            "34|43",
            "X keeps the low digits"
        );
        assert_eq!(writef("%O2", &[0x1234]), "64");
        assert_eq!(
            writef("%X0|%N", &[0x1234, 43]),
            "4|43",
            "width 0 keeps one digit"
        );
        assert_eq!(writef("%O0|%N", &[0x1234, 43]), "4|43");
        assert_eq!(writef("%X1", &[0x1234]), "4");
        assert_eq!(writef("%X8", &[0x1234_ABCD]), "1234ABCD");
        assert_eq!(writef("%XG", &[0x1234_ABCD]), "000000001234ABCD", "G = 16");
        assert_eq!(writef("%T2", &[0x2000]), "abcdef", "T is a minimum width");
        assert_eq!(writef("%T8|", &[0x2000]), "abcdef  |");
        assert_eq!(writef("%C%C", &[b'A' as u32, b'B' as u32]), "AB");
        assert_eq!(writef("%N|%N|%N", &[1, 2, 3]), "1|2|3");
    }

    #[test]
    fn vfwritef_non_directives_print_the_character_and_take_no_argument_362() {
        // %D and %* are not directives on real Kickstart 3.1: the % is
        // dropped, the letter prints, and the next argument is untouched.
        assert_eq!(writef("%D5|%N", &[42, 43]), "D5|42");
        assert_eq!(writef("%*5|%N", &[42, 43]), "*5|42");
        assert_eq!(writef("%Z5|%N", &[42, 43]), "Z5|42");
        assert_eq!(writef("%%", &[]), "%");
        // %$ ignores exactly one argument.
        assert_eq!(writef("%$|%N", &[1, 2]), "|2");
    }
}
