//! `dos.library` pure path-string functions: `AddPart`/`FilePart`/
//! `PathPart`. Unlike `Open`/`Lock`/`Examine` et al, these never touch a
//! `Vfs` or a handler -- they operate purely on the bytes of the path
//! strings passed in, per the RKRM (`paths-and-filenames.md`): "This
//! function does not interact with file systems and does not check
//! whether the paths passed in correspond to accessible objects."
//!
//! Found missing while running the real Workbench 3.1.4 `C:/List`
//! binary: it calls `AddPart` to build each listed entry's full path
//! before it ever prints a directory entry.

use crate::cpu::{Cpu, DataRegister};
use crate::dispatch::{DOS_LIBRARY_BASE, DispatchError, HandlerContext, LibraryTable};
use crate::dospattern::ERROR_LINE_TOO_LONG;
use crate::guestmem::read_c_string;
use crate::lvos::dos::DOS_LVOS;
use crate::memory::AddressSpace;

const DOSTRUE: u32 = 0xFFFF_FFFF;
const DOSFALSE: u32 = 0;

/// Core of `AddPart`: appends `filename` to `dirname` per the real
/// algorithm (matching the RKRM's documented rules, and the classic
/// AmigaDOS `AddPart()` implementation):
///
/// - a leading `:` in `filename` truncates `dirname` to just its
///   device-name prefix (up to and including the `:`), discarding
///   everything else -- "the colon identifies the root of the volume";
/// - each further leading `/` in `filename` pops one trailing path
///   component off `dirname` (or, once only `device:` remains, is a
///   no-op) -- "a leading slash the parent directory which removes the
///   trailing component";
/// - the (now slash/colon-stripped) remainder of `filename` is appended,
///   inserting a `/` separator first unless `dirname` already ends in
///   `/` or `:` (or is empty).
///
/// Returns the joined path, or `None` if it wouldn't fit in `cap` bytes
/// (including the `NUL` terminator) -- the caller maps that to
/// [`ERROR_LINE_TOO_LONG`].
fn add_part(dirname: &[u8], filename: &[u8], cap: usize) -> Option<Vec<u8>> {
    let mut dir = dirname.to_vec();
    let mut rest = filename;

    if let Some((b':', tail)) = rest.split_first() {
        match dir.iter().position(|&b| b == b':') {
            Some(colon) => dir.truncate(colon + 1),
            None => dir.clear(),
        }
        rest = tail;
    } else {
        while let Some((b'/', tail)) = rest.split_first() {
            match dir.iter().rposition(|&b| b == b'/' || b == b':') {
                Some(sep) if dir[sep] == b'/' => dir.truncate(sep),
                Some(sep) => dir.truncate(sep + 1), // stop at "device:"
                None => dir.clear(),
            }
            rest = tail;
        }
        if !dir.is_empty() && *dir.last().unwrap() != b'/' && *dir.last().unwrap() != b':' {
            dir.push(b'/');
        }
    }

    dir.extend_from_slice(rest);
    if dir.len() + 1 > cap {
        return None;
    }
    Some(dir)
}

/// Core of `FilePart`: the byte offset (from `path`'s start) of the last
/// path component. If `path` ends in two or more `/`, that's the offset
/// of the final `/` itself (per the RKRM: "a pointer to '/' in case the
/// input path terminates with at least two slashes").
fn file_part_offset(path: &[u8]) -> usize {
    if path.len() >= 2 && path[path.len() - 1] == b'/' && path[path.len() - 2] == b'/' {
        return path.len() - 1;
    }
    let trimmed = if path.last() == Some(&b'/') {
        &path[..path.len() - 1]
    } else {
        path
    };
    match trimmed.iter().rposition(|&b| b == b'/' || b == b':') {
        Some(sep) => sep + 1,
        None => 0,
    }
}

/// Core of `PathPart`: the byte offset of the end of the next-to-last
/// component -- i.e. where a `NUL` would need to go to leave just the
/// directory containing the last component. Identical to
/// [`file_part_offset`] except it doesn't include the separator itself,
/// and a single-component path returns `path.len()` (points at the
/// input's own end, since there's no "directory containing it" to cut
/// at).
fn path_part_offset(path: &[u8]) -> usize {
    let off = file_part_offset(path);
    if off == 0 { path.len() } else { off - 1 }
}

/// `AddPart` (`D1` = `dirname` buffer, `D2` = `filename`, `D3` =
/// `dirname`'s capacity). `D0` = `DOSTRUE` on success (buffer
/// overwritten in place), `DOSFALSE` (+ `IoErr()` = [`ERROR_LINE_TOO_LONG`])
/// if it wouldn't fit.
fn add_part_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let dir_addr = ctx.cpu.data_register(DataRegister(1));
    let file_addr = ctx.cpu.data_register(DataRegister(2));
    let cap = ctx.cpu.data_register(DataRegister(3)) as usize;

    let dirname = read_c_string(ctx.mem, dir_addr);
    let filename = read_c_string(ctx.mem, file_addr);

    match add_part(&dirname, &filename, cap) {
        Some(joined) => {
            let mut a = dir_addr;
            for b in &joined {
                ctx.mem.write_u8(a, *b);
                a = a.wrapping_add(1);
            }
            ctx.mem.write_u8(a, 0);
            ctx.cpu.set_data_register(DataRegister(0), DOSTRUE);
        }
        None => {
            ctx.dos.set_io_err(ERROR_LINE_TOO_LONG);
            ctx.cpu.set_data_register(DataRegister(0), DOSFALSE);
        }
    }
    Ok(())
}

/// `FilePart` (`D1` = path). `D0` = pointer into the same string, at its
/// last component. Cannot fail.
fn file_part_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let path_addr = ctx.cpu.data_register(DataRegister(1));
    let path = read_c_string(ctx.mem, path_addr);
    let off = file_part_offset(&path);
    ctx.cpu
        .set_data_register(DataRegister(0), path_addr.wrapping_add(off as u32));
    Ok(())
}

/// `PathPart` (`D1` = path). `D0` = pointer into the same string, at the
/// end of the next-to-last component. Cannot fail.
fn path_part_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let path_addr = ctx.cpu.data_register(DataRegister(1));
    let path = read_c_string(ctx.mem, path_addr);
    let off = path_part_offset(&path);
    ctx.cpu
        .set_data_register(DataRegister(0), path_addr.wrapping_add(off as u32));
    Ok(())
}

/// Registers `AddPart`/`FilePart`/`PathPart` onto [`DOS_LIBRARY_BASE`],
/// looked up by name through [`DOS_LVOS`]. Called from
/// [`crate::dispatch::Runtime::new`] alongside the other `dos.library`
/// registrations.
pub fn register_dospath_handlers<C: Cpu + 'static>(
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
    reg!("AddPart", add_part_handler::<C>);
    reg!("FilePart", file_part_handler::<C>);
    reg!("PathPart", path_part_handler::<C>);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{Runtime, StartConfig};
    use crate::guestmem::write_c_string;
    use crate::memory::FlatMemory;

    // --- add_part: unit-level ---

    #[test]
    fn add_part_appends_with_a_slash_separator() {
        assert_eq!(
            add_part(b"SYS:work", b"foo.txt", 64).unwrap(),
            b"SYS:work/foo.txt"
        );
    }

    #[test]
    fn add_part_no_double_slash_after_a_trailing_slash() {
        assert_eq!(
            add_part(b"SYS:work/", b"foo.txt", 64).unwrap(),
            b"SYS:work/foo.txt"
        );
    }

    #[test]
    fn add_part_no_slash_needed_after_a_bare_device_colon() {
        assert_eq!(add_part(b"SYS:", b"foo.txt", 64).unwrap(), b"SYS:foo.txt");
    }

    #[test]
    fn add_part_empty_dirname_just_becomes_filename() {
        assert_eq!(add_part(b"", b"foo.txt", 64).unwrap(), b"foo.txt");
    }

    #[test]
    fn add_part_leading_colon_resets_to_device_root() {
        assert_eq!(
            add_part(b"SYS:work/sub", b":other", 64).unwrap(),
            b"SYS:other"
        );
    }

    #[test]
    fn add_part_leading_slash_pops_one_trailing_component() {
        assert_eq!(
            add_part(b"SYS:work/sub", b"/foo.txt", 64).unwrap(),
            b"SYS:work/foo.txt"
        );
    }

    #[test]
    fn add_part_two_leading_slashes_pop_two_components() {
        assert_eq!(
            add_part(b"SYS:work/sub/deep", b"//foo.txt", 64).unwrap(),
            b"SYS:work/foo.txt"
        );
    }

    #[test]
    fn add_part_leading_slash_past_device_root_is_a_no_op_pop() {
        assert_eq!(
            add_part(b"SYS:work", b"/foo.txt", 64).unwrap(),
            b"SYS:foo.txt"
        );
    }

    #[test]
    fn add_part_too_long_reports_none() {
        assert!(add_part(b"SYS:work", b"foo.txt", 10).is_none());
    }

    // --- file_part / path_part: unit-level ---

    #[test]
    fn file_part_offset_finds_the_last_component() {
        assert_eq!(file_part_offset(b"SYS:work/foo.txt"), 9);
        assert_eq!(file_part_offset(b"foo.txt"), 0);
        assert_eq!(file_part_offset(b"SYS:"), 4);
    }

    #[test]
    fn file_part_offset_double_trailing_slash_points_at_the_slash() {
        let path = b"SYS:work//";
        assert_eq!(file_part_offset(path), path.len() - 1);
    }

    #[test]
    fn path_part_offset_points_just_before_the_last_components_separator() {
        assert_eq!(path_part_offset(b"SYS:work/foo.txt"), 8); // just after "work"
        assert_eq!(path_part_offset(b"foo.txt"), 7); // single component -> end of string
    }

    // --- End-to-end: real A-line trap dispatch ---

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
    fn end_to_end_add_part_joins_in_place_via_trap_dispatch() {
        let mut words = Vec::new();
        let dir_idx = push_move_imm_to_d(&mut words, 1, 0); // D1 = dirname buffer (patched)
        let file_idx = push_move_imm_to_d(&mut words, 2, 0); // D2 = filename (patched)
        push_move_imm_to_d(&mut words, 3, 64); // D3 = capacity
        push_jsr(&mut words, 6, -882); // AddPart(a6): D0 = DOSTRUE/DOSFALSE
        words.push(RTS);

        let dir_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        let file_addr = dir_addr + 64; // room for the in-place-grown buffer
        patch_imm32(&mut words, dir_idx, dir_addr);
        patch_imm32(&mut words, file_idx, file_addr);

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);
        write_c_string(&mut mem, dir_addr, b"SYS:work");
        write_c_string(&mut mem, file_addr, b"foo.txt");

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: file_addr + 0x40,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, DOSTRUE as i32);
        assert_eq!(read_c_string(rt.memory(), dir_addr), b"SYS:work/foo.txt");
    }

    #[test]
    fn end_to_end_file_part_returns_pointer_into_the_same_string() {
        let mut words = Vec::new();
        let path_idx = push_move_imm_to_d(&mut words, 1, 0); // D1 = path (patched)
        push_jsr(&mut words, 6, -870); // FilePart(a6): D0 = pointer
        words.push(RTS);

        let path_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        patch_imm32(&mut words, path_idx, path_addr);

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);
        write_c_string(&mut mem, path_addr, b"SYS:work/foo.txt");

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: path_addr + 0x40,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, (path_addr + 9) as i32);
        assert_eq!(read_c_string(rt.memory(), code as u32), b"foo.txt");
    }
}
