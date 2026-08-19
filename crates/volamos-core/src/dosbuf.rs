//! `dos.library` buffered I/O: `FGetC`/`FPutC`/`UnGetC`/`FRead`/
//! `FWrite`/`FGets`/`FPuts`/`WriteChars`/`Flush`/`SetVBuf`.
//!
//! # Scope: no real internal buffer
//!
//! Real AmigaOS gives every `FileHandle` a `SetVBuf`-configurable
//! internal buffer purely as a *performance* optimization (batching
//! small reads/writes into fewer handler round-trips) -- per the RKRM,
//! `SetVBuf`/buffer size/mode never change what bytes end up where, only
//! how many host-level I/O calls it takes to get them there. Since this
//! runtime's own [`crate::dosfile::DosState::read`]/[`crate::dosfile::
//! DosState::write`] already reach the host file immediately on every
//! call (there is no intermediate buffering layer to bypass or flush),
//! every function here is implemented directly on top of those, one
//! host call at a time, with **no behavioral difference** from a real,
//! buffered implementation as observed by a guest program -- the same
//! bytes go to and come from the same places in the same order. Given
//! that, `SetVBuf` is a no-op (always reports success) and `Flush` is a
//! no-op (always reports `DOSTRUE`, matching the real function's own
//! "currently always `DOSTRUE`" documented return anyway) -- there is
//! nothing to flush.
//!
//! The one piece of real, observable state this scope still requires is
//! `UnGetC`'s one-byte pushback (`DosState::ungetc_buf`/`last_getc`):
//! `FGetC` consults it before ever touching the host file, so a
//! `FGetC`/`UnGetC`/`FGetC` sequence round-trips correctly.
//!
//! `SelectInput`/`SelectOutput` (redirecting a *process's* default
//! `Input()`/`Output()` handle) and interactive-file-specific behavior
//! (`IsInteractive`, the BCPL command-line-parameters-in-the-input-
//! buffer quirk `files.md` documents for `Input()`) are out of scope --
//! this runtime's `Input()` is backed by real host stdin already (see
//! `crate::dosfile`'s module docs), not a command-line buffer, so that
//! particular real-AmigaOS quirk doesn't apply here in the first place.

use crate::cpu::{Cpu, DataRegister};
use crate::dispatch::{DOS_LIBRARY_BASE, DispatchError, HandlerContext, LibraryTable};
use crate::dosfile::map_io_error;
use crate::guestmem::{addr_from_bptr, read_c_string};
use crate::lvos::dos::DOS_LVOS;
use crate::memory::AddressSpace;

/// `FGetC`/`FPutC`/`FPuts`/`FGets`/`UnGetC`'s "end of stream or error"
/// sentinel -- `dos/stdio.h`.
const ENDSTREAMCH: i32 = -1;

/// `Flush`'s always-`DOSTRUE` return value.
const DOSTRUE: u32 = 0xFFFF_FFFF;

/// Core of `FGetC`: consults [`crate::dosfile::DosState::ungetc_buf`]
/// first, otherwise reads one real byte. Records the result in
/// `last_getc` either way, for a subsequent `UnGetC(fh, -1)`.
fn fgetc(ctx: &mut HandlerContext<'_, impl Cpu>, addr: u32) -> i32 {
    let value = if let Some(pushed) = ctx.dos.ungetc_buf.remove(&addr) {
        pushed
    } else {
        match ctx.dos.read(addr, 1) {
            Ok(bytes) if !bytes.is_empty() => i32::from(bytes[0]),
            Ok(_) => {
                ctx.dos.set_io_err(0); // clean EOF, not an error
                ENDSTREAMCH
            }
            Err(code) => {
                ctx.dos.set_io_err(code);
                ENDSTREAMCH
            }
        }
    };
    ctx.dos.last_getc.insert(addr, value);
    value
}

/// Core of `FPutC`/`FPuts`/`FWrite`/`WriteChars` (and, via `crate::
/// dosprintf`/`crate::dosfile`'s `PutStr`, of any other call that
/// writes to an already-resolved `FileHandle*` address): writes `data`
/// to `addr`, routing through [`HandlerContext::out`] for the `Output()`
/// default handle exactly like [`crate::dosfile`]'s own `Write` handler
/// does. Returns the number of bytes actually written, or an `IoErr()`
/// code.
pub(crate) fn write_bytes(
    ctx: &mut HandlerContext<'_, impl Cpu>,
    addr: u32,
    data: &[u8],
) -> Result<usize, i32> {
    if ctx.dos.is_output_default(addr) {
        return ctx
            .out
            .write_all(data)
            .map(|()| data.len())
            .map_err(|e| map_io_error(&e));
    }
    ctx.dos.write(addr, data)
}

/// `FGetC` (`D1` = `BPTR`). `D0` = the character read (`0..=255`), or
/// `ENDSTREAMCH` on EOF/error.
fn fgetc_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let bptr = ctx.cpu.data_register(DataRegister(1));
    let addr = addr_from_bptr(bptr);
    let value = fgetc(ctx, addr);
    ctx.cpu.set_data_register(DataRegister(0), value as u32);
    Ok(())
}

/// `FPutC` (`D1` = `BPTR`, `D2` = character). `D0` = the character
/// written, or `ENDSTREAMCH` on error.
fn fputc_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let bptr = ctx.cpu.data_register(DataRegister(1));
    let ch = ctx.cpu.data_register(DataRegister(2)) as u8;
    let addr = addr_from_bptr(bptr);
    let result = match write_bytes(ctx, addr, &[ch]) {
        Ok(_) => i32::from(ch),
        Err(code) => {
            ctx.dos.set_io_err(code);
            ENDSTREAMCH
        }
    };
    ctx.cpu.set_data_register(DataRegister(0), result as u32);
    Ok(())
}

/// `UnGetC` (`D1` = `BPTR`, `D2` = character, or `-1` for "whatever was
/// last read"). `D0` = non-zero on success, `0` if a byte was already
/// pending (at most one pushback is supported between reads, matching
/// the real function's own documented limit).
fn ungetc_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let bptr = ctx.cpu.data_register(DataRegister(1));
    let ch = ctx.cpu.data_register(DataRegister(2)) as i32;
    let addr = addr_from_bptr(bptr);

    let ok = if ctx.dos.ungetc_buf.contains_key(&addr) {
        false
    } else {
        let value = if ch == ENDSTREAMCH {
            ctx.dos.last_getc.get(&addr).copied().unwrap_or(ENDSTREAMCH)
        } else {
            ch
        };
        ctx.dos.ungetc_buf.insert(addr, value);
        true
    };
    ctx.cpu.set_data_register(DataRegister(0), u32::from(ok));
    Ok(())
}

/// `FRead` (`D1` = `BPTR`, `D2` = buffer, `D3` = record size, `D4` =
/// record count). `D0` = number of *complete* records read.
fn fread_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let bptr = ctx.cpu.data_register(DataRegister(1));
    let buf_addr = ctx.cpu.data_register(DataRegister(2));
    let block_len = ctx.cpu.data_register(DataRegister(3));
    let block_count = ctx.cpu.data_register(DataRegister(4));
    let addr = addr_from_bptr(bptr);

    let total_wanted = (block_len as u64) * (block_count as u64);
    let mut got = 0u64;
    while got < total_wanted {
        match ctx.dos.read(addr, 1) {
            Ok(bytes) if !bytes.is_empty() => {
                ctx.mem
                    .write_u8(buf_addr.wrapping_add(got as u32), bytes[0]);
                got += 1;
            }
            Ok(_) => break, // EOF
            Err(code) => {
                ctx.dos.set_io_err(code);
                break;
            }
        }
    }
    let complete_records = if block_len == 0 {
        0
    } else {
        (got / u64::from(block_len)) as u32
    };
    ctx.cpu.set_data_register(DataRegister(0), complete_records);
    Ok(())
}

/// `FWrite` (`D1` = `BPTR`, `D2` = buffer, `D3` = record size, `D4` =
/// record count). `D0` = number of *complete* records written.
fn fwrite_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let bptr = ctx.cpu.data_register(DataRegister(1));
    let buf_addr = ctx.cpu.data_register(DataRegister(2));
    let block_len = ctx.cpu.data_register(DataRegister(3));
    let block_count = ctx.cpu.data_register(DataRegister(4));
    let addr = addr_from_bptr(bptr);

    let total = (block_len as u64) * (block_count as u64);
    let mut buf = Vec::with_capacity(total as usize);
    for i in 0..total {
        buf.push(ctx.mem.read_u8(buf_addr.wrapping_add(i as u32)));
    }
    let written = match write_bytes(ctx, addr, &buf) {
        Ok(n) => n as u64,
        Err(code) => {
            ctx.dos.set_io_err(code);
            0
        }
    };
    let complete_records = if block_len == 0 {
        0
    } else {
        (written / u64::from(block_len)) as u32
    };
    ctx.cpu.set_data_register(DataRegister(0), complete_records);
    Ok(())
}

/// `FGets` (`D1` = `BPTR`, `D2` = buffer, `D3` = buffer capacity). `D0`
/// = `buf` on success, `0` on EOF/error (with nothing copied).
fn fgets_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let bptr = ctx.cpu.data_register(DataRegister(1));
    let buf_addr = ctx.cpu.data_register(DataRegister(2));
    let cap = ctx.cpu.data_register(DataRegister(3)) as usize;
    let addr = addr_from_bptr(bptr);

    if cap == 0 {
        ctx.cpu.set_data_register(DataRegister(0), 0);
        return Ok(());
    }

    let mut n = 0usize;
    let mut saw_any = false;
    while n < cap - 1 {
        let c = fgetc(ctx, addr);
        if c == ENDSTREAMCH {
            break;
        }
        saw_any = true;
        ctx.mem.write_u8(buf_addr.wrapping_add(n as u32), c as u8);
        n += 1;
        if c == b'\n' as i32 {
            break;
        }
    }
    ctx.mem.write_u8(buf_addr.wrapping_add(n as u32), 0);

    let result = if saw_any { buf_addr } else { 0 };
    ctx.cpu.set_data_register(DataRegister(0), result);
    Ok(())
}

/// `FPuts` (`D1` = `BPTR`, `D2` = `CString*`). `D0` = `0` on success,
/// `ENDSTREAMCH` on error.
fn fputs_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let bptr = ctx.cpu.data_register(DataRegister(1));
    let str_ptr = ctx.cpu.data_register(DataRegister(2));
    let addr = addr_from_bptr(bptr);
    let bytes = read_c_string(ctx.mem, str_ptr);
    let result = match write_bytes(ctx, addr, &bytes) {
        Ok(_) => 0,
        Err(code) => {
            ctx.dos.set_io_err(code);
            ENDSTREAMCH
        }
    };
    ctx.cpu.set_data_register(DataRegister(0), result as u32);
    Ok(())
}

/// `WriteChars` (`D1` = buffer, `D2` = length). `D0` = bytes written.
/// Equivalent to `FWrite(Output(),buf,1,buflen)`.
fn write_chars_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let buf_addr = ctx.cpu.data_register(DataRegister(1));
    let len = ctx.cpu.data_register(DataRegister(2));
    let out_addr = match ctx.dos.output_addr(ctx.heap, ctx.mem) {
        Ok(addr) => addr,
        Err(code) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), 0);
            return Ok(());
        }
    };
    let mut buf = Vec::with_capacity(len as usize);
    for i in 0..len {
        buf.push(ctx.mem.read_u8(buf_addr.wrapping_add(i)));
    }
    let written = match write_bytes(ctx, out_addr, &buf) {
        Ok(n) => n as u32,
        Err(code) => {
            ctx.dos.set_io_err(code);
            0
        }
    };
    ctx.cpu.set_data_register(DataRegister(0), written);
    Ok(())
}

/// `Flush` (`D1` = `BPTR`). `D0` = `DOSTRUE`, always -- see the module
/// docs' "no real internal buffer" section.
fn flush_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    ctx.cpu.set_data_register(DataRegister(0), DOSTRUE);
    Ok(())
}

/// `SetVBuf` (`D1` = `BPTR`, `D2` = buffer, `D3` = mode, `D4` = size).
/// `D0` = `0` (success), always -- see the module docs.
fn set_vbuf_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    ctx.cpu.set_data_register(DataRegister(0), 0);
    Ok(())
}

/// Registers `FGetC`/`FPutC`/`UnGetC`/`FRead`/`FWrite`/`FGets`/`FPuts`/
/// `WriteChars`/`Flush`/`SetVBuf` onto [`DOS_LIBRARY_BASE`], looked up
/// by name through [`DOS_LVOS`]. Called from [`crate::dispatch::
/// Runtime::new`] alongside the other `dos.library` registrations.
pub fn register_dosbuf_handlers<C: Cpu + 'static>(
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
    reg!("FGetC", fgetc_handler::<C>);
    reg!("FPutC", fputc_handler::<C>);
    reg!("UnGetC", ungetc_handler::<C>);
    reg!("FRead", fread_handler::<C>);
    reg!("FWrite", fwrite_handler::<C>);
    reg!("FGets", fgets_handler::<C>);
    reg!("FPuts", fputs_handler::<C>);
    reg!("WriteChars", write_chars_handler::<C>);
    reg!("Flush", flush_handler::<C>);
    reg!("SetVBuf", set_vbuf_handler::<C>);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{Runtime, StartConfig};
    use crate::dosfile::{MODE_NEWFILE, MODE_OLDFILE};
    use crate::memory::FlatMemory;
    use crate::vfs::{Vfs, VfsConfig};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let path = std::env::temp_dir().join(format!("volamos-dosbuf-test-{tag}-{pid}-{n}"));
            fs::create_dir_all(&path).expect("create temp dir");
            TempDir { path }
        }
        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn vfs_over(root: &Path) -> Vfs {
        Vfs::new(VfsConfig {
            volumes: vec![("SYS".to_string(), root.to_path_buf())],
            assigns: vec![],
            auto_assign_root: None,
            cwd: "SYS:".to_string(),
        })
        .expect("build vfs")
    }

    // --- End-to-end: real A-line trap dispatch, matching dosfile.rs's
    // and dosanchor.rs's own test style.

    fn load_words(mem: &mut FlatMemory, addr: u32, words: &[u16]) {
        let mut offset = addr;
        for &w in words {
            mem.write_u16(offset, w);
            offset += 2;
        }
    }

    /// `move.l #imm32,Dn`.
    fn move_imm_to_d(n: u16) -> u16 {
        0x203C | (n << 9)
    }

    /// `move.l D0,Dn`.
    fn move_d0_to_d(n: u16) -> u16 {
        0x2000 | (n << 9)
    }

    /// `jsr <disp16>(An)`.
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

    /// Builds a `Runtime` around `words` placed at the program's entry
    /// point (A6 pre-seeded to `DOS_LIBRARY_BASE`), with `extra` (e.g. a
    /// C-string name, or scratch bytes for a read/write buffer) written
    /// starting at `extra_addr`, and a `Vfs` rooted at `vfs_root`
    /// installed before returning.
    fn runtime_with_program_and_extra(
        words: &[u16],
        extra_addr: u32,
        extra: &[u8],
        vfs_root: &Path,
    ) -> Runtime<M68kCpu> {
        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, words);
        for (i, &b) in extra.iter().enumerate() {
            mem.write_u8(extra_addr + i as u32, b);
        }
        let load_end = entry + 0x400;
        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        rt.set_vfs(vfs_over(vfs_root));
        rt
    }

    /// `D1 = Open(name, MODE_OLDFILE/MODE_NEWFILE); D1 = D0` (the file
    /// handle survives later calls, which only ever touch D0).
    fn push_open_into_d1(words: &mut Vec<u16>, name_idx: usize, mode: i32) -> usize {
        // The caller has already pushed `move.l #imm,D1` for the name at
        // `name_idx`; this just appends the mode + call + save-to-D1.
        push_move_imm_to_d(words, 2, mode as u32);
        push_jsr(words, 6, -30); // Open(a6): D0 = BPTR or 0
        words.push(move_d0_to_d(1)); // D1 = handle
        name_idx
    }

    #[test]
    fn end_to_end_fgetc_returns_first_byte_of_file() {
        let tmp = TempDir::new("fgetc-first");
        fs::write(tmp.path().join("f.txt"), b"hi").unwrap();
        let name = b"SYS:f.txt\0";

        let mut words = Vec::new();
        let name_idx = words.len();
        words.push(move_imm_to_d(1));
        words.push(0);
        words.push(0);
        push_open_into_d1(&mut words, name_idx, MODE_OLDFILE);
        push_jsr(&mut words, 6, -306); // FGetC(a6): D0 = char
        words.push(RTS);

        let name_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        patch_imm32(&mut words, name_idx, name_addr);

        let mut rt = runtime_with_program_and_extra(&words, name_addr, name, tmp.path());
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, i32::from(b'h'));
    }

    #[test]
    fn end_to_end_fgetc_returns_endstreamch_at_eof() {
        let tmp = TempDir::new("fgetc-eof");
        fs::write(tmp.path().join("f.txt"), b"hi").unwrap();
        let name = b"SYS:f.txt\0";

        let mut words = Vec::new();
        let name_idx = words.len();
        words.push(move_imm_to_d(1));
        words.push(0);
        words.push(0);
        push_open_into_d1(&mut words, name_idx, MODE_OLDFILE);
        push_jsr(&mut words, 6, -306); // 'h'
        push_jsr(&mut words, 6, -306); // 'i'
        push_jsr(&mut words, 6, -306); // EOF
        words.push(RTS);

        let name_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        patch_imm32(&mut words, name_idx, name_addr);

        let mut rt = runtime_with_program_and_extra(&words, name_addr, name, tmp.path());
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, ENDSTREAMCH);
    }

    #[test]
    fn end_to_end_fputc_writes_to_output_default() {
        let tmp = TempDir::new("fputc");

        let mut words = Vec::new();
        push_jsr(&mut words, 6, -60); // Output(a6): D0 = BPTR
        words.push(move_d0_to_d(1)); // D1 = Output() handle
        push_move_imm_to_d(&mut words, 2, u32::from(b'X'));
        push_jsr(&mut words, 6, -312); // FPutC(a6): D0 = char written
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);
        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: TRAP_TABLE_END + 0x400,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        rt.set_vfs(vfs_over(tmp.path()));

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, i32::from(b'X'));
        assert_eq!(out, b"X");
    }

    #[test]
    fn end_to_end_ungetc_pushback_then_fgetc_repeats_the_byte() {
        let tmp = TempDir::new("ungetc");
        fs::write(tmp.path().join("f.txt"), b"hi").unwrap();
        let name = b"SYS:f.txt\0";

        let mut words = Vec::new();
        let name_idx = words.len();
        words.push(move_imm_to_d(1));
        words.push(0);
        words.push(0);
        push_open_into_d1(&mut words, name_idx, MODE_OLDFILE);
        push_jsr(&mut words, 6, -306); // FGetC: D0 = 'h', D1 unchanged
        push_move_imm_to_d(&mut words, 2, 0xFFFF_FFFF); // D2 = -1 ("last read")
        push_jsr(&mut words, 6, -318); // UnGetC(a6): D0 = ok flag (discarded)
        push_jsr(&mut words, 6, -306); // FGetC again: should replay 'h'
        words.push(RTS);

        let name_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        patch_imm32(&mut words, name_idx, name_addr);

        let mut rt = runtime_with_program_and_extra(&words, name_addr, name, tmp.path());
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, i32::from(b'h'));
    }

    #[test]
    fn end_to_end_fgets_reads_one_line_into_the_buffer() {
        let tmp = TempDir::new("fgets");
        fs::write(tmp.path().join("f.txt"), b"line1\nline2\n").unwrap();
        let name = b"SYS:f.txt\0";

        let mut words = Vec::new();
        let name_idx = words.len();
        words.push(move_imm_to_d(1));
        words.push(0);
        words.push(0);
        push_open_into_d1(&mut words, name_idx, MODE_OLDFILE);
        let buf_idx = push_move_imm_to_d(&mut words, 2, 0); // D2 = buf addr (patched)
        push_move_imm_to_d(&mut words, 3, 16); // D3 = capacity
        push_jsr(&mut words, 6, -336); // FGets(a6): D0 = buf addr, or 0
        words.push(RTS);

        let name_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        let buf_addr = name_addr + name.len() as u32;
        patch_imm32(&mut words, name_idx, name_addr);
        patch_imm32(&mut words, buf_idx, buf_addr);

        let mut rt = runtime_with_program_and_extra(&words, name_addr, name, tmp.path());
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, buf_addr as i32,
            "FGets should return the buffer address"
        );

        let line = read_c_string(rt.memory(), buf_addr);
        assert_eq!(line, b"line1\n");
    }

    #[test]
    fn end_to_end_fwrite_writes_complete_records() {
        let tmp = TempDir::new("fwrite");
        let name = b"SYS:blocks.dat\0";
        let payload = b"abcdef"; // 6 bytes = 3 records of 2 bytes each

        let mut words = Vec::new();
        let name_idx = words.len();
        words.push(move_imm_to_d(1));
        words.push(0);
        words.push(0);
        push_open_into_d1(&mut words, name_idx, MODE_NEWFILE);
        let buf_idx = push_move_imm_to_d(&mut words, 2, 0); // D2 = write buf (patched)
        push_move_imm_to_d(&mut words, 3, 2); // D3 = record size
        push_move_imm_to_d(&mut words, 4, 3); // D4 = record count
        push_jsr(&mut words, 6, -330); // FWrite(a6): D0 = records written
        words.push(RTS);

        let name_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        let buf_addr = name_addr + name.len() as u32;
        patch_imm32(&mut words, name_idx, name_addr);
        patch_imm32(&mut words, buf_idx, buf_addr);

        let mut extra = name.to_vec();
        extra.extend_from_slice(payload);
        let mut rt = runtime_with_program_and_extra(&words, name_addr, &extra, tmp.path());
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 3, "3 complete 2-byte records from a 6-byte buffer");
        assert_eq!(fs::read(tmp.path().join("blocks.dat")).unwrap(), payload);
    }

    #[test]
    fn end_to_end_fread_reads_only_complete_records() {
        let tmp = TempDir::new("fread");
        fs::write(tmp.path().join("f.dat"), b"abcdefg").unwrap(); // 7 bytes
        let name = b"SYS:f.dat\0";

        let mut words = Vec::new();
        let name_idx = words.len();
        words.push(move_imm_to_d(1));
        words.push(0);
        words.push(0);
        push_open_into_d1(&mut words, name_idx, MODE_OLDFILE);
        let buf_idx = push_move_imm_to_d(&mut words, 2, 0); // D2 = read buf (patched)
        push_move_imm_to_d(&mut words, 3, 3); // D3 = record size
        push_move_imm_to_d(&mut words, 4, 2); // D4 = record count (wants 6 of 7 bytes)
        push_jsr(&mut words, 6, -324); // FRead(a6): D0 = records read
        words.push(RTS);

        let name_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        let buf_addr = name_addr + name.len() as u32;
        patch_imm32(&mut words, name_idx, name_addr);
        patch_imm32(&mut words, buf_idx, buf_addr);

        let mut rt = runtime_with_program_and_extra(&words, name_addr, name, tmp.path());
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 2, "2 complete 3-byte records available from 7 bytes");

        let read_back: Vec<u8> = (0..6).map(|i| rt.memory().read_u8(buf_addr + i)).collect();
        assert_eq!(read_back, b"abcdef");
    }

    #[test]
    fn end_to_end_flush_and_setvbuf_are_no_ops_returning_success() {
        let tmp = TempDir::new("flush-setvbuf");
        fs::write(tmp.path().join("f.txt"), b"x").unwrap();
        let name = b"SYS:f.txt\0";

        let mut words = Vec::new();
        let name_idx = words.len();
        words.push(move_imm_to_d(1));
        words.push(0);
        words.push(0);
        push_open_into_d1(&mut words, name_idx, MODE_OLDFILE);
        push_jsr(&mut words, 6, -360); // Flush(a6): D0 = DOSTRUE
        words.push(move_d0_to_d(2)); // D2 = Flush's result, saved
        push_move_imm_to_d(&mut words, 3, 0); // D3 = buffer (unused)
        push_move_imm_to_d(&mut words, 4, 0); // D4 = mode (unused)
        // SetVBuf takes D1=fh, D2=buf, D3=mode, D4=size; reuse D1 as fh.
        words.push(move_imm_to_d(2)); // clobber D2 with 0 for the buf arg
        words.push(0);
        words.push(0);
        push_jsr(&mut words, 6, -366); // SetVBuf(a6): D0 = 0
        words.push(RTS);

        let name_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        patch_imm32(&mut words, name_idx, name_addr);

        let mut rt = runtime_with_program_and_extra(&words, name_addr, name, tmp.path());
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 0, "SetVBuf always reports success (D0 = 0)");
    }
}
