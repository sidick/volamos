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
use crate::dosbuf::{Written, write_bytes};
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

    let out_addr = match crate::dosfile::current_output_addr(ctx) {
        Ok(addr) => addr,
        Err(io_code) => {
            ctx.dos.set_io_err(io_code);
            ctx.cpu.set_data_register(DataRegister(0), DOSFALSE);
            return Ok(());
        }
    };
    if matches!(
        write_bytes(ctx, out_addr, &rendered, "PrintFault")?,
        Written::Switched
    ) {
        return Ok(());
    }
    ctx.cpu
        .set_data_register(DataRegister(0), if found { DOSTRUE } else { DOSFALSE });
    Ok(())
}

/// The error codes real `ErrorReport` has a requester for -- and, per
/// its own documented bug, the only ones it sets `IoErr()` from.
///
/// `dos/dos.h`'s `ERROR_DISK_NOT_VALIDATED` (213),
/// `ERROR_DISK_WRITE_PROTECTED` (214), `ERROR_DEVICE_NOT_MOUNTED`
/// (218), `ERROR_DISK_FULL` (221), `ERROR_NOT_A_DOS_DISK` (225),
/// `ERROR_NO_DISK` (226), plus `dos/dosextens.h`'s `ABORT_DISK_ERROR`
/// (296) and `ABORT_BUSY` (288).
const ERROR_REPORT_CODES: [i32; 8] = [213, 214, 218, 221, 225, 226, 288, 296];

/// `ErrorReport` (`D1` = code, `D2` = type, `D3` = arg1, `D4` =
/// device). `D0` = `DOSTRUE` for "cancel", `DOSFALSE` for "retry".
///
/// Always answers `DOSTRUE`. The autodoc makes that the right answer
/// for a runtime with no requesters: it returns `DOS_TRUE` "if the
/// user selects CANCEL or if the attempt to put up the requester
/// fails, or if the process `pr_WindowPtr` is -1", and `DOS_TRUE`
/// immediately for a code it doesn't understand. volamos has no
/// Intuition screen to put a Retry/Cancel requester on, so every one
/// of those conditions holds; answering `DOSFALSE` instead would tell
/// the caller to *retry* an operation that cannot start working, which
/// is how a program ends up in a loop rather than reporting an error.
///
/// `IoErr()` is set to `code` only for [`ERROR_REPORT_CODES`], which
/// is not an oversight but the documented behaviour of every version
/// volamos targets: "In V36-V47.26 it would only set `IoErr()` to code
/// for the error codes listed in the INPUTS section. For all other
/// codes the `IoErr()` status prior to calling `ErrorReport()` would
/// persist." A caller that needs the general case is documented to
/// call `SetIoErr(code)` itself first, and one that was written
/// against a real 3.x machine will be relying on exactly this.
///
/// Found needed by clib2's `lstat()` (issue #288), which calls this
/// for any lock failure other than "not found" or "is a soft link" --
/// as an AmigaDOS program should -- so an unimplemented `ErrorReport`
/// ended the run outright.
fn error_report_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let code = ctx.cpu.data_register(DataRegister(1)) as i32;
    if ERROR_REPORT_CODES.contains(&code) {
        ctx.dos.set_io_err(code);
    }
    *ctx.call_detail = Some(format!(
        "ErrorReport code {code} -> cancel (no requester to put up)"
    ));
    ctx.cpu.set_data_register(DataRegister(0), DOSTRUE);
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
    reg!("ErrorReport", error_report_handler::<C>);
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

    /// Runs `SetIoErr(prior); ErrorReport(code, REPORT_LOCK, 0, 0)` and
    /// returns `(D0, IoErr())`.
    fn run_error_report(code: u32, prior: u32) -> (i32, i32) {
        let mut words = Vec::new();
        push_move_imm_to_d(&mut words, 1, prior);
        push_jsr(&mut words, 6, -462); // SetIoErr(a6)
        push_move_imm_to_d(&mut words, 1, code);
        push_move_imm_to_d(&mut words, 2, 0); // REPORT_LOCK
        push_move_imm_to_d(&mut words, 3, 0); // arg1
        push_move_imm_to_d(&mut words, 4, 0); // device
        push_jsr(&mut words, 6, -480); // ErrorReport(a6)
        let d0_cell_idx = words.len();
        words.push(0x23C0); // MOVE.L D0,(xxx).L
        words.extend_from_slice(&[0, 0]);
        push_jsr(&mut words, 6, -132); // IoErr(a6)
        let err_cell_idx = words.len();
        words.push(0x23C0);
        words.extend_from_slice(&[0, 0]);
        words.push(RTS);

        let d0_cell = (TRAP_TABLE_END + (words.len() as u32) * 2 + 3) & !3;
        let err_cell = d0_cell + 4;
        words[d0_cell_idx + 1] = (d0_cell >> 16) as u16;
        words[d0_cell_idx + 2] = d0_cell as u16;
        words[err_cell_idx + 1] = (err_cell >> 16) as u16;
        words[err_cell_idx + 2] = err_cell as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);
        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: err_cell + 64,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed");
        (
            rt.memory().read_u32(d0_cell) as i32,
            rt.memory().read_u32(err_cell) as i32,
        )
    }

    #[test]
    fn error_report_answers_cancel_with_no_requester_to_put_up() {
        // Issue #288: unimplemented, so clib2's lstat() -- which calls
        // this for any lock failure other than "not found" or "is a
        // soft link", as an AmigaDOS program should -- ended the run.
        //
        // DOSTRUE is "cancel". The autodoc gives three separate reasons
        // it is the right answer here ("if the attempt to put up the
        // requester fails, or if the process pr_WindowPtr is -1", and
        // immediately "if the code is not understood"), and all of them
        // hold for a runtime with no Intuition screen. DOSFALSE would
        // mean *retry*, which is how a caller ends up looping on an
        // operation that cannot start working.
        let (code, io_err) = run_error_report(221, 0); // ERROR_DISK_FULL
        assert_eq!(code, DOSTRUE as i32, "cancel");
        assert_eq!(
            io_err, 221,
            "a code with a requester sets IoErr to it (autodoc: \"supposed \
             to set IoErr() to code before returning\")"
        );
    }

    #[test]
    fn error_report_leaves_io_err_alone_for_a_code_it_has_no_requester_for() {
        // Not an oversight -- the documented behaviour of every version
        // volamos targets: "In V36-V47.26 it would only set IoErr() to
        // code for the error codes listed in the INPUTS section. For
        // all other codes the IoErr() status prior to calling
        // ErrorReport() would persist." A program written against a
        // real 3.x machine relies on exactly this, and the autodoc's
        // own workaround is to SetIoErr(code) first.
        let (code, io_err) = run_error_report(205, 42); // ERROR_OBJECT_NOT_FOUND
        assert_eq!(code, DOSTRUE as i32);
        assert_eq!(io_err, 42, "the prior IoErr persists");
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
