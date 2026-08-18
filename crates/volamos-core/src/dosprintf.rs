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

use crate::cpu::{Cpu, DataRegister};
use crate::dispatch::{DOS_LIBRARY_BASE, DispatchError, HandlerContext, LibraryTable};
use crate::dosfile::map_io_error;
use crate::execfmt::render_format;
use crate::guestmem::{addr_from_bptr, read_c_string};
use crate::lvos::dos::DOS_LVOS;

const RESULT_ERROR: u32 = 0xFFFF_FFFF;

/// `VPrintf` (`D1` = format string, `D2` = data stream). `D0` = number
/// of characters written, or `-1` (+ `IoErr()` set).
fn vprintf_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let fmt_ptr = ctx.cpu.data_register(DataRegister(1));
    let data_ptr = ctx.cpu.data_register(DataRegister(2));

    let fmt = read_c_string(ctx.mem, fmt_ptr);
    let (rendered, _) = render_format(ctx.mem, &fmt, data_ptr);

    match ctx.out.write_all(&rendered) {
        Ok(()) => ctx
            .cpu
            .set_data_register(DataRegister(0), rendered.len() as u32),
        Err(e) => {
            ctx.dos.set_io_err(map_io_error(&e));
            ctx.cpu.set_data_register(DataRegister(0), RESULT_ERROR);
        }
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

    if ctx.dos.is_output_default(addr) {
        match ctx.out.write_all(&rendered) {
            Ok(()) => ctx
                .cpu
                .set_data_register(DataRegister(0), rendered.len() as u32),
            Err(e) => {
                ctx.dos.set_io_err(map_io_error(&e));
                ctx.cpu.set_data_register(DataRegister(0), RESULT_ERROR);
            }
        }
        return Ok(());
    }

    match ctx.dos.write(addr, &rendered) {
        Ok(n) => ctx.cpu.set_data_register(DataRegister(0), n as u32),
        Err(code) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), RESULT_ERROR);
        }
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
}
