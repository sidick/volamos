//! `dos.library` `SetFileDate`: sets a file system object's
//! modification date.
//!
//! Found missing while running the real Workbench 3.1.4 `C:/SetDate`
//! binary (the Shell command wrapping this call). Closes a gap flagged
//! but not implemented back when `.uaem` sidecars were first added
//! (see [`crate::dosmeta`]'s module docs' "Not done" note): `date` is
//! the only field a sidecar stores that had a reader (`crate::doslock`'s
//! `fill_fib`) but no writer until now.
//!
//! Written to the target's `.uaem` sidecar ([`crate::dosmeta`]), merged
//! onto whatever was already recorded there (so a prior
//! `SetProtection`/`SetComment` survives) -- `crate::doslock`'s
//! `fill_fib` reads that back for `fib_Date` on a later
//! `Examine`/`ExNext`.

use crate::cpu::{Cpu, DataRegister};
use crate::dispatch::{DOS_LIBRARY_BASE, DispatchError, HandlerContext, LibraryTable};
use crate::dosfile::map_io_error;
use crate::dosmeta;
use crate::guestmem::read_c_string;
use crate::lvos::dos::DOS_LVOS;
use crate::memory::AddressSpace;
use crate::vfs::ResolveMode;

const DOSTRUE: u32 = 0xFFFF_FFFF;
const DOSFALSE: u32 = 0;

const DS_DAYS_OFFSET: u32 = 0;
const DS_MINUTE_OFFSET: u32 = 4;
const DS_TICK_OFFSET: u32 = 8;

/// `SetFileDate` (`D1` = name `CString*`, `D2` = `struct DateStamp*`).
/// `D0` = `DOSTRUE`/`DOSFALSE` (+ `IoErr()` set on failure).
fn set_file_date_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let name_ptr = ctx.cpu.data_register(DataRegister(1));
    let date_ptr = ctx.cpu.data_register(DataRegister(2));
    let name = String::from_utf8_lossy(&read_c_string(ctx.mem, name_ptr)).into_owned();
    let days = ctx.mem.read_u32(date_ptr.wrapping_add(DS_DAYS_OFFSET)) as i32;
    let minute = ctx.mem.read_u32(date_ptr.wrapping_add(DS_MINUTE_OFFSET)) as i32;
    let tick = ctx.mem.read_u32(date_ptr.wrapping_add(DS_TICK_OFFSET)) as i32;

    let result = (|| -> Result<(), i32> {
        let vfs = ctx
            .dos
            .vfs
            .as_ref()
            .ok_or(crate::dosfile::ERROR_OBJECT_NOT_FOUND)?;
        let host_path = vfs
            .resolve(&name, ResolveMode::MustExist)
            .map_err(|e| crate::dosfile::map_vfs_error(&e))?;
        // Touch the target so a missing object fails cleanly
        // (SetFileDate has no other host-side effect to fail on
        // otherwise -- same convention as crate::dosnote's SetComment).
        std::fs::metadata(&host_path).map_err(|e| map_io_error(&e))?;

        let mut sidecar_meta = dosmeta::read_sidecar(&host_path).unwrap_or_default();
        sidecar_meta.date = (days, minute, tick);
        dosmeta::write_sidecar(&host_path, &sidecar_meta).map_err(|e| map_io_error(&e))
    })();

    match result {
        Ok(()) => ctx.cpu.set_data_register(DataRegister(0), DOSTRUE),
        Err(code) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), DOSFALSE);
        }
    }
    Ok(())
}

/// Registers `SetFileDate` onto [`DOS_LIBRARY_BASE`], looked up by
/// name through [`DOS_LVOS`]. Called from [`crate::dispatch::
/// Runtime::new`] alongside the other `dos.library` registrations.
pub fn register_dossetfiledate_handlers<C: Cpu + 'static>(
    table: &mut LibraryTable<C>,
    mem: &mut C::Memory,
) {
    table
        .register_by_name(
            mem,
            DOS_LIBRARY_BASE,
            DOS_LVOS,
            "dos.library",
            "SetFileDate",
            set_file_date_handler::<C>,
        )
        .unwrap_or_else(|e| panic!("SetFileDate should be in DOS_LVOS: {e}"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{Runtime, StartConfig};
    use crate::guestmem::write_c_string;
    use crate::memory::{AddressSpace, FlatMemory};
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
            let path =
                std::env::temp_dir().join(format!("volamos-dossetfiledate-test-{tag}-{pid}-{n}"));
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

    fn run_set_file_date(name: &[u8], days: i32, minute: i32, tick: i32, vfs_root: &Path) -> i32 {
        let mut words = Vec::new();
        let name_idx = push_move_imm_to_d(&mut words, 1, 0);
        let date_idx = push_move_imm_to_d(&mut words, 2, 0);
        push_jsr(&mut words, 6, -396); // SetFileDate(a6)
        words.push(RTS);

        let name_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        let date_addr = (name_addr + name.len() as u32 + 3) & !3;
        patch_imm32(&mut words, name_idx, name_addr);
        patch_imm32(&mut words, date_idx, date_addr);

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);
        write_c_string(&mut mem, name_addr, name);
        mem.write_u32(date_addr, days as u32);
        mem.write_u32(date_addr + 4, minute as u32);
        mem.write_u32(date_addr + 8, tick as u32);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: date_addr + 0x100,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        rt.set_vfs(vfs_over(vfs_root));

        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed")
    }

    #[test]
    fn end_to_end_set_file_date_writes_a_sidecar() {
        let tmp = TempDir::new("basic");
        let path = tmp.path().join("f.txt");
        fs::write(&path, b"hi").unwrap();

        let code = run_set_file_date(b"SYS:f.txt\0", 6282, 720, 0, tmp.path());
        assert_eq!(code, DOSTRUE as i32);

        let meta = crate::dosmeta::read_sidecar(&path).expect("sidecar should exist");
        assert_eq!(meta.date, (6282, 720, 0));
    }

    #[test]
    fn set_file_date_preserves_an_existing_sidecar_comment() {
        let tmp = TempDir::new("merge");
        let path = tmp.path().join("f.txt");
        fs::write(&path, b"hi").unwrap();
        crate::dosmeta::write_sidecar(
            &path,
            &crate::dosmeta::Meta {
                prot: 0,
                date: (0, 0, 0),
                comment: Some(b"existing comment".to_vec()),
            },
        )
        .unwrap();

        let code = run_set_file_date(b"SYS:f.txt\0", 100, 200, 300, tmp.path());
        assert_eq!(code, DOSTRUE as i32);

        let meta = crate::dosmeta::read_sidecar(&path).unwrap();
        assert_eq!(meta.date, (100, 200, 300));
        assert_eq!(meta.comment.as_deref(), Some(&b"existing comment"[..]));
    }

    #[test]
    fn end_to_end_set_file_date_missing_object_fails() {
        let tmp = TempDir::new("missing");
        let code = run_set_file_date(b"SYS:nope.txt\0", 0, 0, 0, tmp.path());
        assert_eq!(code, DOSFALSE as i32);
    }
}
