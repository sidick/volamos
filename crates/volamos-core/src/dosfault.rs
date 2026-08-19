//! `dos.library` `Fault`/`PrintFault`: render a standard AmigaDOS error
//! message from a secondary error code (an `IoErr()` value), optionally
//! prefixed by a caller-supplied header.
//!
//! Found missing while running the real Workbench 3.1.4 `C:/Type`
//! binary against a directory argument: after printing its own `"TYPE
//! can't open %s"` message, `Type` calls `PrintFault(IoErr(), "Type")`
//! as its final command-level error report -- the same pattern the
//! Shell itself uses (`PrintFault(cli_Result2, cmd)`) to surface a
//! command's overall result. Real AmigaDOS keeps the message table as
//! localized resource strings inside `dos.library` itself
//! (`dl_Errors`/`estr_Strings`, RKRM `dos-library.md`); this runtime
//! hardcodes the (non-localized, `en`) standard English text instead,
//! since that's what every corpus binary actually expects to see.

use crate::cpu::{Cpu, DataRegister};
use crate::dispatch::{DOS_LIBRARY_BASE, DispatchError, HandlerContext, LibraryTable};
use crate::dosbuf::write_bytes;
use crate::guestmem::{read_c_string, write_c_string};
use crate::lvos::dos::DOS_LVOS;

const DOSTRUE: u32 = 0xFFFF_FFFF;
const DOSFALSE: u32 = 0;

/// The standard AmigaDOS secondary error code -> message text table
/// (`dos/dos.h`'s `ERROR_*` constants and their well-known English
/// strings).
const FAULT_MESSAGES: &[(i32, &str)] = &[
    (103, "Out of memory"),
    (114, "Bad template"),
    (115, "Bad number"),
    (116, "Required argument missing"),
    (117, "Argument after key needed"),
    (118, "Too many arguments"),
    (119, "Unmatched quotes"),
    (120, "Line too long"),
    (121, "File is not executable"),
    (122, "Invalid resident library"),
    (202, "Object in use"),
    (203, "Object already exists"),
    (204, "Directory not found"),
    (205, "Object not found"),
    (206, "Invalid window description"),
    (209, "Action not known"),
    (210, "Invalid component name"),
    (211, "Invalid lock"),
    (212, "Object is not of required type"),
    (213, "Disk not validated"),
    (214, "Disk write protected"),
    (215, "Rename across devices attempted"),
    (216, "Directory not empty"),
    (217, "Too many levels"),
    (218, "Device (or volume) not mounted"),
    (219, "Seek error"),
    (220, "Comment too big"),
    (221, "Disk full"),
    (222, "File is protected from deletion"),
    (223, "File is write protected"),
    (224, "File is read protected"),
    (225, "Not a valid DOS disk"),
    (226, "No disk in drive"),
    (232, "No more entries in directory"),
    (233, "Object is a soft link"),
    (234, "Object is linked"),
    (235, "Bad hunk"),
    (236, "Not implemented"),
    (303, "Buffer overflow"),
    (304, "Break"),
    (305, "Not executable"),
];

/// Looks up the standard message text for `code`, if any.
fn fault_message(code: i32) -> Option<&'static str> {
    FAULT_MESSAGES
        .iter()
        .find(|&&(c, _)| c == code)
        .map(|&(_, msg)| msg)
}

/// Renders `header: message\n` (or just `message\n` if `header` is
/// empty), falling back to a generic `"Error N"` if `code` has no known
/// message -- matching `Fault()`'s own documented fallback.
fn render_fault(code: i32, header: &[u8]) -> (Vec<u8>, bool) {
    let mut buf = Vec::new();
    if !header.is_empty() {
        buf.extend_from_slice(header);
        buf.extend_from_slice(b": ");
    }
    match fault_message(code) {
        Some(msg) => {
            buf.extend_from_slice(msg.as_bytes());
            (buf, true)
        }
        None => {
            buf.extend_from_slice(format!("Error {code}").as_bytes());
            (buf, false)
        }
    }
}

/// `Fault` (`D1` = code, `D2` = header, `D3` = buffer, `D4` = buffer
/// size). Fills `buffer` (truncated to fit `size`, including the `NUL`
/// terminator) with the rendered message; leaves it untouched if `code`
/// is 0. `D0` = the number of characters written (0 if `code` was 0) --
/// real `Fault()`'s own documented return value is unreliable, but this
/// is the best-effort convention corpus binaries that do check it
/// expect.
fn fault_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let code = ctx.cpu.data_register(DataRegister(1)) as i32;
    let header_ptr = ctx.cpu.data_register(DataRegister(2));
    let buf_addr = ctx.cpu.data_register(DataRegister(3));
    let size = ctx.cpu.data_register(DataRegister(4)) as usize;

    if code == 0 || size == 0 {
        ctx.cpu.set_data_register(DataRegister(0), 0);
        return Ok(());
    }

    let header = if header_ptr == 0 {
        Vec::new()
    } else {
        read_c_string(ctx.mem, header_ptr)
    };
    let (mut rendered, _) = render_fault(code, &header);
    rendered.truncate(size - 1);
    let written = write_c_string(ctx.mem, buf_addr, &rendered);
    ctx.cpu.set_data_register(DataRegister(0), written - 1);
    Ok(())
}

/// `PrintFault` (`D1` = code, `D2` = header). Prints `header: message\n`
/// (or just `message\n`) to the current `Output()` selection -- this
/// runtime has no separate `pr_CES` error stream, so it always uses
/// `Output()`, matching real AmigaDOS's own pre-V45 (and V45+
/// no-`pr_CES`-configured) fallback behavior. Sets `IoErr()` to `code`
/// unless `code` is 0. `D0` = `DOSTRUE` if `code` was 0 or a message was
/// found, `DOSFALSE` otherwise.
fn print_fault_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let code = ctx.cpu.data_register(DataRegister(1)) as i32;
    let header_ptr = ctx.cpu.data_register(DataRegister(2));

    if code == 0 {
        ctx.cpu.set_data_register(DataRegister(0), DOSTRUE);
        return Ok(());
    }
    ctx.dos.set_io_err(code);

    let header = if header_ptr == 0 {
        Vec::new()
    } else {
        read_c_string(ctx.mem, header_ptr)
    };
    let (mut rendered, found) = render_fault(code, &header);
    rendered.push(b'\n');

    let out_addr = match ctx.dos.output_addr(ctx.heap, ctx.mem) {
        Ok(addr) => addr,
        Err(io_code) => {
            ctx.dos.set_io_err(io_code);
            ctx.cpu.set_data_register(DataRegister(0), DOSFALSE);
            return Ok(());
        }
    };
    let _ = write_bytes(ctx, out_addr, &rendered);
    ctx.cpu
        .set_data_register(DataRegister(0), if found { DOSTRUE } else { DOSFALSE });
    Ok(())
}

/// Registers `Fault`/`PrintFault` onto [`DOS_LIBRARY_BASE`], looked up
/// by name through [`DOS_LVOS`]. Called from [`crate::dispatch::
/// Runtime::new`] alongside the other `dos.library` registrations.
pub fn register_dosfault_handlers<C: Cpu + 'static>(
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
    reg!("Fault", fault_handler::<C>);
    reg!("PrintFault", print_fault_handler::<C>);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{Runtime, StartConfig};
    use crate::memory::{AddressSpace, FlatMemory};

    fn load_words(mem: &mut FlatMemory, addr: u32, words: &[u16]) {
        let mut offset = addr;
        for &w in words {
            mem.write_u16(offset, w);
            offset += 2;
        }
    }

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

    #[test]
    fn render_fault_known_code_with_header() {
        let (msg, found) = render_fault(205, b"Type");
        assert!(found);
        assert_eq!(msg, b"Type: Object not found");
    }

    #[test]
    fn render_fault_unknown_code_falls_back_to_generic_text() {
        let (msg, found) = render_fault(999999, b"");
        assert!(!found);
        assert_eq!(msg, b"Error 999999");
    }

    #[test]
    fn end_to_end_print_fault_writes_header_and_message_to_output() {
        let mut words = Vec::new();
        push_move_imm_to_d(&mut words, 1, 205); // D1 = ERROR_OBJECT_NOT_FOUND
        let header_idx = push_move_imm_to_d(&mut words, 2, 0); // D2 = header (patched)
        push_jsr(&mut words, 6, -474); // PrintFault(a6): D0 = DOSTRUE/DOSFALSE
        words.push(RTS);

        let header_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        patch_imm32(&mut words, header_idx, header_addr);

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);
        write_c_string(&mut mem, header_addr, b"TYPE");

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: header_addr + 0x40,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, DOSTRUE as i32,
            "a known error code should return DOSTRUE"
        );
        assert_eq!(out, b"TYPE: Object not found\n");
    }

    #[test]
    fn end_to_end_print_fault_zero_code_is_a_no_op() {
        let mut words = Vec::new();
        push_move_imm_to_d(&mut words, 1, 0); // D1 = 0 -- nothing should print
        push_move_imm_to_d(&mut words, 2, 0); // D2 = no header
        push_jsr(&mut words, 6, -474);
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);
        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: TRAP_TABLE_END + 0x40,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, DOSTRUE as i32);
        assert!(out.is_empty());
    }

    #[test]
    fn end_to_end_fault_fills_buffer_and_truncates_to_size() {
        let mut words = Vec::new();
        push_move_imm_to_d(&mut words, 1, 205); // D1 = code
        push_move_imm_to_d(&mut words, 2, 0); // D2 = no header
        let buf_idx = push_move_imm_to_d(&mut words, 3, 0); // D3 = buffer (patched)
        push_move_imm_to_d(&mut words, 4, 6); // D4 = size (only room for "Objec\0")
        push_jsr(&mut words, 6, -468); // Fault(a6): D0 = chars written
        words.push(RTS);

        let buf_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        patch_imm32(&mut words, buf_idx, buf_addr);

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);
        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: buf_addr + 0x40,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 5, "5 chars written before the forced NUL");
        let filled = read_c_string(rt.memory(), buf_addr);
        assert_eq!(filled, b"Objec");
    }
}
