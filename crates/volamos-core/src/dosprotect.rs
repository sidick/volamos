//! `dos.library` `SetProtection`: sets a file system object's
//! protection bits.
//!
//! Found missing while running the real Workbench 3.1.4 `C:/Copy`
//! binary: it calls this after copying a file, to replicate the
//! source's protection bits onto the new copy.
//!
//! # Two effects: real host writability, plus the full mask via a
//! `.uaem` sidecar
//!
//! `mask` (per `dos/dos.h`) is an 8-bit field: `FIBB_DELETE`(0)/
//! `FIBB_EXECUTE`(1)/`FIBB_WRITE`(2)/`FIBB_READ`(3)/`FIBB_ARCHIVE`(4)/
//! `FIBB_PURE`(5)/`FIBB_SCRIPT`(6)/`FIBB_HOLD`(7), all "active when
//! clear" for the first four (DEWR) per the RKRM. Of these, only
//! `FIBB_WRITE` maps onto anything the host file system actually
//! enforces (`std::fs::Permissions::set_readonly`) -- `DELETE`/
//! `EXECUTE`/`READ` and the rest have no meaningful host-level
//! equivalent this runtime can act on, so that's the only bit this
//! handler ever changes the host file's *real* permissions for. The
//! *full* mask, though, is also written to the target's `.uaem` sidecar
//! ([`crate::dosmeta`]), merged onto whatever was already recorded there
//! (so a prior `SetComment`'s comment survives) -- `crate::doslock`'s
//! `fill_fib` reads that back for `fib_Protection` on a later
//! `Examine`/`ExNext`, so (unlike this module's own earlier scope note)
//! `SetProtection` *does* now round-trip fully, just via the sidecar
//! rather than a real host filesystem attribute for the seven bits the
//! host can't represent.

use crate::cpu::{Cpu, DataRegister};
use crate::dispatch::{DOS_LIBRARY_BASE, DispatchError, HandlerContext, LibraryTable};
use crate::dosfile::map_io_error;
use crate::dosmeta;
use crate::guestmem::read_c_string;
use crate::lvos::dos::DOS_LVOS;
use crate::vfs::ResolveMode;
use std::fs;

const DOSTRUE: u32 = 0xFFFF_FFFF;
const DOSFALSE: u32 = 0;

/// `FIBB_WRITE`'s bit position (per `dos/dos.h`); set means "the file
/// system refuses to modify the file" (write-protected).
const FIBF_WRITE: u32 = 1 << 2;

/// `SetProtection` (`D1` = name `CString*`, `D2` = protection mask).
/// `D0` = `DOSTRUE`/`DOSFALSE` (+ `IoErr()` set on failure).
fn set_protection_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let name_ptr = ctx.cpu.data_register(DataRegister(1));
    let mask = ctx.cpu.data_register(DataRegister(2));
    let name = String::from_utf8_lossy(&read_c_string(ctx.mem, name_ptr)).into_owned();

    let result = (|| -> Result<(), i32> {
        let vfs = ctx
            .dos
            .vfs
            .as_ref()
            .ok_or(crate::dosfile::ERROR_OBJECT_NOT_FOUND)?;
        let host_path = vfs
            .resolve(&name, ResolveMode::MustExist)
            .map_err(|e| crate::dosfile::map_vfs_error(&e))?;
        let mut perms = fs::metadata(&host_path)
            .map_err(|e| map_io_error(&e))?
            .permissions();
        perms.set_readonly(mask & FIBF_WRITE != 0);
        fs::set_permissions(&host_path, perms).map_err(|e| map_io_error(&e))?;

        let mut sidecar_meta = dosmeta::read_sidecar(&host_path).unwrap_or_default();
        sidecar_meta.prot = mask;
        // Best-effort: a sidecar write failure shouldn't fail the whole
        // call when the real host permission change (the primary,
        // always-real effect) already succeeded above.
        let _ = dosmeta::write_sidecar(&host_path, &sidecar_meta);
        Ok(())
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

/// Registers `SetProtection` onto [`DOS_LIBRARY_BASE`], looked up by
/// name through [`DOS_LVOS`]. Called from [`crate::dispatch::
/// Runtime::new`] alongside the other `dos.library` registrations.
pub fn register_dosprotect_handlers<C: Cpu + 'static>(
    table: &mut LibraryTable<C>,
    mem: &mut C::Memory,
) {
    table
        .register_by_name(
            mem,
            DOS_LIBRARY_BASE,
            DOS_LVOS,
            "dos.library",
            "SetProtection",
            set_protection_handler::<C>,
        )
        .unwrap_or_else(|e| panic!("SetProtection should be in DOS_LVOS: {e}"));
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
                std::env::temp_dir().join(format!("volamos-dosprotect-test-{tag}-{pid}-{n}"));
            fs::create_dir_all(&path).expect("create temp dir");
            TempDir { path }
        }
        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            // Restore writability so removal doesn't fail on a
            // write-protected temp file left over from a test.
            if let Ok(entries) = fs::read_dir(&self.path) {
                for entry in entries.flatten() {
                    if let Ok(meta) = entry.metadata() {
                        let mut perms = meta.permissions();
                        #[allow(clippy::permissions_set_readonly_false)]
                        perms.set_readonly(false);
                        let _ = fs::set_permissions(entry.path(), perms);
                    }
                }
            }
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

    fn run_set_protection(name: &[u8], mask: u32, vfs_root: &Path) -> i32 {
        let mut words = Vec::new();
        let name_idx = push_move_imm_to_d(&mut words, 1, 0);
        push_move_imm_to_d(&mut words, 2, mask);
        push_jsr(&mut words, 6, -186);
        words.push(RTS);

        let name_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        patch_imm32(&mut words, name_idx, name_addr);

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);
        write_c_string(&mut mem, name_addr, name);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: name_addr + 0x40,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        rt.set_vfs(vfs_over(vfs_root));

        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed")
    }

    #[test]
    fn end_to_end_set_protection_write_bit_makes_the_host_file_readonly() {
        let tmp = TempDir::new("write-protect");
        fs::write(tmp.path().join("f.txt"), b"hi").unwrap();

        let code = run_set_protection(b"SYS:f.txt\0", FIBF_WRITE, tmp.path());
        assert_eq!(code, DOSTRUE as i32);

        let meta = fs::metadata(tmp.path().join("f.txt")).unwrap();
        assert!(meta.permissions().readonly());
    }

    #[test]
    fn end_to_end_set_protection_clear_write_bit_makes_the_host_file_writable() {
        let tmp = TempDir::new("un-protect");
        let path = tmp.path().join("f.txt");
        fs::write(&path, b"hi").unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&path, perms).unwrap();

        let code = run_set_protection(b"SYS:f.txt\0", 0, tmp.path());
        assert_eq!(code, DOSTRUE as i32);

        let meta = fs::metadata(&path).unwrap();
        assert!(!meta.permissions().readonly());
    }

    #[test]
    fn set_protection_writes_the_full_mask_to_a_uaem_sidecar() {
        let tmp = TempDir::new("sidecar");
        let path = tmp.path().join("f.txt");
        fs::write(&path, b"hi").unwrap();

        // 0x11 = FIBB_ARCHIVE | FIBB_DELETE, per crate::dosmeta's own
        // real-captured-example test.
        let code = run_set_protection(b"SYS:f.txt\0", 0x11, tmp.path());
        assert_eq!(code, DOSTRUE as i32);

        let meta = crate::dosmeta::read_sidecar(&path).expect("sidecar should exist");
        assert_eq!(meta.prot, 0x11);
    }

    #[test]
    fn set_protection_preserves_an_existing_sidecar_comment() {
        let tmp = TempDir::new("sidecar-merge");
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

        let code = run_set_protection(b"SYS:f.txt\0", FIBF_WRITE, tmp.path());
        assert_eq!(code, DOSTRUE as i32);

        let meta = crate::dosmeta::read_sidecar(&path).expect("sidecar should exist");
        assert_eq!(meta.prot, FIBF_WRITE);
        assert_eq!(meta.comment.as_deref(), Some(&b"existing comment"[..]));
    }

    #[test]
    fn end_to_end_set_protection_missing_file_fails() {
        let tmp = TempDir::new("missing");
        let code = run_set_protection(b"SYS:nope.txt\0", FIBF_WRITE, tmp.path());
        assert_eq!(code, DOSFALSE as i32);
    }
}
