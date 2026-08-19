//! `dos.library` `SetComment`: sets a file system object's comment.
//!
//! Found missing while running the real Workbench 3.1.4 `C:/Filenote`
//! binary (the Shell command wrapping this call).
//!
//! Written to the target's `.uaem` sidecar ([`crate::dosmeta`]), merged
//! onto whatever was already recorded there (so a prior
//! `SetProtection`'s mask survives) -- `crate::doslock`'s `fill_fib`
//! reads that back for `fib_Comment` on a later `Examine`/`ExNext`.

use crate::cpu::{Cpu, DataRegister};
use crate::dispatch::{DOS_LIBRARY_BASE, DispatchError, HandlerContext, LibraryTable};
use crate::dosfile::map_io_error;
use crate::dosmeta;
use crate::guestmem::read_c_string;
use crate::lvos::dos::DOS_LVOS;
use crate::vfs::ResolveMode;

const DOSTRUE: u32 = 0xFFFF_FFFF;
const DOSFALSE: u32 = 0;

/// `SetComment` (`D1` = name `CString*`, `D2` = comment `CString*`).
/// `D0` = `DOSTRUE`/`DOSFALSE` (+ `IoErr()` set on failure).
fn set_comment_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let name_ptr = ctx.cpu.data_register(DataRegister(1));
    let comment_ptr = ctx.cpu.data_register(DataRegister(2));
    let name = String::from_utf8_lossy(&read_c_string(ctx.mem, name_ptr)).into_owned();
    let comment = read_c_string(ctx.mem, comment_ptr);

    let result = (|| -> Result<(), i32> {
        let vfs = ctx
            .dos
            .vfs
            .as_ref()
            .ok_or(crate::dosfile::ERROR_OBJECT_NOT_FOUND)?;
        let host_path = vfs
            .resolve(&name, ResolveMode::MustExist)
            .map_err(|e| crate::dosfile::map_vfs_error(&e))?;
        // Touch the target so a missing object fails cleanly (SetComment
        // has no other host-side effect to fail on otherwise).
        std::fs::metadata(&host_path).map_err(|e| map_io_error(&e))?;

        let mut sidecar_meta = dosmeta::read_sidecar(&host_path).unwrap_or_default();
        sidecar_meta.comment = if comment.is_empty() {
            None
        } else {
            Some(comment)
        };
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

/// Registers `SetComment` onto [`DOS_LIBRARY_BASE`], looked up by name
/// through [`DOS_LVOS`]. Called from [`crate::dispatch::Runtime::new`]
/// alongside the other `dos.library` registrations.
pub fn register_dosnote_handlers<C: Cpu + 'static>(
    table: &mut LibraryTable<C>,
    mem: &mut C::Memory,
) {
    table
        .register_by_name(
            mem,
            DOS_LIBRARY_BASE,
            DOS_LVOS,
            "dos.library",
            "SetComment",
            set_comment_handler::<C>,
        )
        .unwrap_or_else(|e| panic!("SetComment should be in DOS_LVOS: {e}"));
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
            let path = std::env::temp_dir().join(format!("volamos-dosnote-test-{tag}-{pid}-{n}"));
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

    fn run_set_comment(name: &[u8], comment: &[u8], vfs_root: &Path) -> i32 {
        let mut words = Vec::new();
        let name_idx = push_move_imm_to_d(&mut words, 1, 0);
        let comment_idx = push_move_imm_to_d(&mut words, 2, 0);
        push_jsr(&mut words, 6, -180);
        words.push(RTS);

        let name_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        let comment_addr = name_addr + name.len() as u32;
        patch_imm32(&mut words, name_idx, name_addr);
        patch_imm32(&mut words, comment_idx, comment_addr);

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);
        write_c_string(&mut mem, name_addr, name);
        write_c_string(&mut mem, comment_addr, comment);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: comment_addr + 0x100,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        rt.set_vfs(vfs_over(vfs_root));

        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed")
    }

    #[test]
    fn end_to_end_set_comment_writes_a_sidecar() {
        let tmp = TempDir::new("basic");
        let path = tmp.path().join("f.txt");
        fs::write(&path, b"hi").unwrap();

        let code = run_set_comment(b"SYS:f.txt\0", b"hello world", tmp.path());
        assert_eq!(code, DOSTRUE as i32);

        let meta = crate::dosmeta::read_sidecar(&path).expect("sidecar should exist");
        assert_eq!(meta.comment.as_deref(), Some(&b"hello world"[..]));
    }

    #[test]
    fn set_comment_preserves_an_existing_sidecar_protection() {
        let tmp = TempDir::new("merge");
        let path = tmp.path().join("f.txt");
        fs::write(&path, b"hi").unwrap();
        crate::dosmeta::write_sidecar(
            &path,
            &crate::dosmeta::Meta {
                prot: 0x11,
                date: (0, 0, 0),
                comment: None,
            },
        )
        .unwrap();

        let code = run_set_comment(b"SYS:f.txt\0", b"a comment", tmp.path());
        assert_eq!(code, DOSTRUE as i32);

        let meta = crate::dosmeta::read_sidecar(&path).unwrap();
        assert_eq!(meta.prot, 0x11);
        assert_eq!(meta.comment.as_deref(), Some(&b"a comment"[..]));
    }

    #[test]
    fn end_to_end_set_comment_missing_object_fails() {
        let tmp = TempDir::new("missing");
        let code = run_set_comment(b"SYS:nope.txt\0", b"comment", tmp.path());
        assert_eq!(code, DOSFALSE as i32);
    }
}
