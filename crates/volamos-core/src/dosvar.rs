//! `dos.library` shell variables: `SetVar`/`GetVar`/`DeleteVar`.
//!
//! # Scope: local (`LV_VAR`) variables only
//!
//! Real AmigaOS shell variables come in two flavors: *local* (per-
//! process, kept in memory, linked off `pr_LocalVars`) and *global*
//! (files under the `ENV:` assign, so they're visible to every process
//! and can be made to survive a reboot via `GVF_SAVE_VAR`/`ENVARC:`).
//! This module implements only local, `LV_VAR`-type variables (see
//! [`DosState::local_vars`]) -- no `ENV:`-backed storage, and no
//! `LV_ALIAS` support (`SetVar`'s `flags` low byte selecting `LV_ALIAS`
//! is treated as an unsupported request, not a crash). `GVF_GLOBAL_ONLY`
//! therefore always fails cleanly here (`ERROR_OBJECT_NOT_FOUND` on
//! read/delete, `DOSFALSE` + the same code on write) rather than
//! reading/writing real files -- most real `C:` commands that touch
//! variables at all (like `Version`, whose own `SetVar` call is what
//! motivated adding this module) use plain local variables, not global
//! ones, so this covers the common case without needing a fake `ENV:`
//! filesystem.
//!
//! Variable names are matched case-insensitively (upper-cased before
//! use as the [`DosState::local_vars`] key), matching the real
//! behavior. `SetVar`'s `size == -1` convention (buffer is a `NUL`-
//! terminated string; the runtime measures it) is implemented; ordinary
//! non-printable/binary variable content and `GetVar`'s `GVF_BINARY_VAR`/
//! `GVF_DONT_NULL_TERM` flags are implemented per the documented
//! semantics (content is truncated at the first `NUL`/newline unless
//! `GVF_BINARY_VAR` is set).

use crate::cpu::{Cpu, DataRegister};
use crate::dispatch::{DOS_LIBRARY_BASE, DispatchError, HandlerContext, LibraryTable};
use crate::dosargs::ERROR_BAD_NUMBER;
use crate::dosfile::{DosState, ERROR_OBJECT_NOT_FOUND};
use crate::guestmem::read_c_string;
use crate::lvos::dos::DOS_LVOS;
use crate::memory::AddressSpace;

const LV_ALIAS: u32 = 1;
const GVF_GLOBAL_ONLY: u32 = 0x0100;
const GVF_BINARY_VAR: u32 = 0x0400;
const GVF_DONT_NULL_TERM: u32 = 0x0800;

const DOSTRUE: u32 = 0xFFFF_FFFF;
const DOSFALSE: u32 = 0;

fn var_key(mem: &dyn AddressSpace, name_ptr: u32) -> String {
    String::from_utf8_lossy(&read_c_string(mem, name_ptr)).to_ascii_uppercase()
}

/// `SetVar` (`D1` = name, `D2` = buffer (`0` deletes), `D3` = size
/// (`-1` = buffer is a `NUL`-terminated string), `D4` = flags). `D0` =
/// `DOSTRUE`/`DOSFALSE` (+ `IoErr()` set on failure).
fn set_var_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let name_ptr = ctx.cpu.data_register(DataRegister(1));
    let buffer_ptr = ctx.cpu.data_register(DataRegister(2));
    let size = ctx.cpu.data_register(DataRegister(3)) as i32;
    let flags = ctx.cpu.data_register(DataRegister(4));

    let result = set_var(ctx.mem, ctx.dos, name_ptr, buffer_ptr, size, flags);
    let value = match result {
        Ok(()) => DOSTRUE,
        Err(code) => {
            ctx.dos.set_io_err(code);
            DOSFALSE
        }
    };
    ctx.cpu.set_data_register(DataRegister(0), value);
    Ok(())
}

fn set_var(
    mem: &dyn AddressSpace,
    dos: &mut DosState,
    name_ptr: u32,
    buffer_ptr: u32,
    size: i32,
    flags: u32,
) -> Result<(), i32> {
    if flags & 0xFF == LV_ALIAS {
        return Err(ERROR_OBJECT_NOT_FOUND);
    }
    let key = var_key(mem, name_ptr);

    if buffer_ptr == 0 {
        return if dos.local_vars.remove(&key).is_some() {
            Ok(())
        } else {
            Err(ERROR_OBJECT_NOT_FOUND)
        };
    }

    if flags & GVF_GLOBAL_ONLY != 0 {
        return Err(ERROR_OBJECT_NOT_FOUND);
    }

    let content = if size == -1 {
        read_c_string(mem, buffer_ptr)
    } else {
        (0..size.max(0) as u32)
            .map(|i| mem.read_u8(buffer_ptr.wrapping_add(i)))
            .collect()
    };
    dos.local_vars.insert(key, content);
    Ok(())
}

/// `GetVar` (`D1` = name, `D2` = buffer, `D3` = buffer capacity in
/// bytes (including room for a `NUL`), `D4` = flags). `D0` = bytes
/// copied (excluding any terminator), or `-1` (+ `IoErr()` set).
fn get_var_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let name_ptr = ctx.cpu.data_register(DataRegister(1));
    let buffer_ptr = ctx.cpu.data_register(DataRegister(2));
    let size = ctx.cpu.data_register(DataRegister(3)) as i32;
    let flags = ctx.cpu.data_register(DataRegister(4));

    match get_var(ctx.mem, ctx.dos, name_ptr, buffer_ptr, size, flags) {
        Ok(len) => ctx.cpu.set_data_register(DataRegister(0), len as u32),
        Err(code) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
        }
    }
    Ok(())
}

fn get_var(
    mem: &mut dyn AddressSpace,
    dos: &mut DosState,
    name_ptr: u32,
    buffer_ptr: u32,
    size: i32,
    flags: u32,
) -> Result<i32, i32> {
    if size <= 0 {
        return Err(ERROR_BAD_NUMBER);
    }
    if flags & GVF_GLOBAL_ONLY != 0 {
        // No ENV:-backed storage -- see the module docs.
        return Err(ERROR_OBJECT_NOT_FOUND);
    }

    let key = var_key(mem, name_ptr);
    let Some(value) = dos.local_vars.get(&key) else {
        return Err(ERROR_OBJECT_NOT_FOUND);
    };

    let binary = flags & GVF_BINARY_VAR != 0;
    let mut content: &[u8] = value;
    if !binary {
        let cut = content
            .iter()
            .position(|&b| b == 0 || b == b'\n')
            .unwrap_or(content.len());
        content = &content[..cut];
    }

    let null_term = !(binary && flags & GVF_DONT_NULL_TERM != 0);
    let capacity = size as usize;
    let max_content = if null_term {
        capacity.saturating_sub(1)
    } else {
        capacity
    };
    let copy_len = content.len().min(max_content);

    let mut addr = buffer_ptr;
    for &b in &content[..copy_len] {
        mem.write_u8(addr, b);
        addr = addr.wrapping_add(1);
    }
    if null_term {
        mem.write_u8(addr, 0);
    }

    Ok(copy_len as i32)
}

/// `DeleteVar` (`D1` = name, `D2` = flags). `D0` = `DOSTRUE`/`DOSFALSE`
/// (+ `IoErr()` set on failure). Equivalent to `SetVar(name, NULL, 0,
/// flags)`.
fn delete_var_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let name_ptr = ctx.cpu.data_register(DataRegister(1));
    let flags = ctx.cpu.data_register(DataRegister(2));

    let result = set_var(ctx.mem, ctx.dos, name_ptr, 0, 0, flags);
    let value = match result {
        Ok(()) => DOSTRUE,
        Err(code) => {
            ctx.dos.set_io_err(code);
            DOSFALSE
        }
    };
    ctx.cpu.set_data_register(DataRegister(0), value);
    Ok(())
}

/// Registers `SetVar`/`GetVar`/`DeleteVar` onto [`DOS_LIBRARY_BASE`],
/// looked up by name through [`DOS_LVOS`]. Called from
/// [`crate::dispatch::Runtime::new`] alongside the other `dos.library`
/// registrations.
pub fn register_dosvar_handlers<C: Cpu + 'static>(
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
    reg!("SetVar", set_var_handler::<C>);
    reg!("GetVar", get_var_handler::<C>);
    reg!("DeleteVar", delete_var_handler::<C>);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guestmem::write_c_string;
    use crate::memory::FlatMemory;

    fn setup() -> (FlatMemory, DosState) {
        (FlatMemory::new(0x1000), DosState::new(None))
    }

    #[test]
    fn set_then_get_round_trips() {
        let (mut mem, mut dos) = setup();
        write_c_string(&mut mem, 0x100, b"MYVAR");
        write_c_string(&mut mem, 0x200, b"hello");
        set_var(&mem, &mut dos, 0x100, 0x200, -1, 0).expect("set should succeed");

        let mut buf = [0u8; 32];
        for (i, b) in buf.iter_mut().enumerate() {
            mem.write_u8(0x300 + i as u32, *b);
            *b = 0;
        }
        let len = get_var(&mut mem, &mut dos, 0x100, 0x300, 32, 0).expect("get should succeed");
        assert_eq!(len, 5);
        assert_eq!(read_c_string(&mem, 0x300), b"hello");
    }

    #[test]
    fn name_matching_is_case_insensitive() {
        let (mut mem, mut dos) = setup();
        write_c_string(&mut mem, 0x100, b"MyVar");
        write_c_string(&mut mem, 0x200, b"x");
        set_var(&mem, &mut dos, 0x100, 0x200, -1, 0).expect("set should succeed");

        write_c_string(&mut mem, 0x110, b"myvar");
        let len = get_var(&mut mem, &mut dos, 0x110, 0x300, 8, 0).expect("get should succeed");
        assert_eq!(len, 1);
    }

    #[test]
    fn get_missing_var_is_object_not_found() {
        let (mut mem, mut dos) = setup();
        write_c_string(&mut mem, 0x100, b"NOPE");
        let err = get_var(&mut mem, &mut dos, 0x100, 0x300, 8, 0).unwrap_err();
        assert_eq!(err, ERROR_OBJECT_NOT_FOUND);
    }

    #[test]
    fn get_truncates_at_buffer_capacity() {
        let (mut mem, mut dos) = setup();
        write_c_string(&mut mem, 0x100, b"LONGVAR");
        write_c_string(&mut mem, 0x200, b"0123456789");
        set_var(&mem, &mut dos, 0x100, 0x200, -1, 0).expect("set should succeed");

        let len = get_var(&mut mem, &mut dos, 0x100, 0x300, 4, 0).expect("get should succeed");
        assert_eq!(len, 3, "capacity 4 leaves room for 3 chars + NUL");
        assert_eq!(read_c_string(&mem, 0x300), b"012");
    }

    #[test]
    fn set_null_buffer_deletes() {
        let (mut mem, mut dos) = setup();
        write_c_string(&mut mem, 0x100, b"TEMP");
        write_c_string(&mut mem, 0x200, b"x");
        set_var(&mem, &mut dos, 0x100, 0x200, -1, 0).expect("set should succeed");
        set_var(&mem, &mut dos, 0x100, 0, 0, 0).expect("delete should succeed");
        let err = get_var(&mut mem, &mut dos, 0x100, 0x300, 8, 0).unwrap_err();
        assert_eq!(err, ERROR_OBJECT_NOT_FOUND);
    }

    #[test]
    fn delete_missing_var_is_an_error() {
        let (mut mem, mut dos) = setup();
        write_c_string(&mut mem, 0x100, b"NOPE");
        let err = set_var(&mem, &mut dos, 0x100, 0, 0, 0).unwrap_err();
        assert_eq!(err, ERROR_OBJECT_NOT_FOUND);
    }

    #[test]
    fn global_only_is_not_supported() {
        let (mut mem, mut dos) = setup();
        write_c_string(&mut mem, 0x100, b"X");
        write_c_string(&mut mem, 0x200, b"y");
        let err = set_var(&mem, &mut dos, 0x100, 0x200, -1, GVF_GLOBAL_ONLY).unwrap_err();
        assert_eq!(err, ERROR_OBJECT_NOT_FOUND);
        let err = get_var(&mut mem, &mut dos, 0x100, 0x300, 8, GVF_GLOBAL_ONLY).unwrap_err();
        assert_eq!(err, ERROR_OBJECT_NOT_FOUND);
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
    fn end_to_end_set_var_then_get_var_via_trap_dispatch() {
        let mut words = Vec::new();
        let name_idx = push_move_imm_to_d(&mut words, 1, 0);
        let val_idx = push_move_imm_to_d(&mut words, 2, 0);
        push_move_imm_to_d(&mut words, 3, 0xFFFF_FFFF); // size = -1
        push_move_imm_to_d(&mut words, 4, 0); // flags = 0
        push_jsr(&mut words, 6, -900); // SetVar(a6)

        let name2_idx = push_move_imm_to_d(&mut words, 1, 0);
        let out_idx = push_move_imm_to_d(&mut words, 2, 0);
        push_move_imm_to_d(&mut words, 3, 16); // buffer capacity
        push_move_imm_to_d(&mut words, 4, 0); // flags = 0
        push_jsr(&mut words, 6, -906); // GetVar(a6)
        words.push(RTS);

        let name = b"GREETING";
        let val = b"hi";
        let name_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        let val_addr = name_addr + name.len() as u32 + 1;
        let out_addr = (val_addr + val.len() as u32 + 1 + 3) & !3;
        patch_imm32(&mut words, name_idx, name_addr);
        patch_imm32(&mut words, val_idx, val_addr);
        patch_imm32(&mut words, name2_idx, name_addr);
        patch_imm32(&mut words, out_idx, out_addr);

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);
        write_c_string(&mut mem, name_addr, name);
        write_c_string(&mut mem, val_addr, val);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: out_addr + 16,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 2, "GetVar should report 2 characters copied");
        assert_eq!(read_c_string(rt.memory(), out_addr), b"hi");
    }
}
