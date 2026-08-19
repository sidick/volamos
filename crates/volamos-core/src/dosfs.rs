//! `dos.library` `IsFileSystem`: whether the handler responsible for a
//! path is a file system (i.e. backs named files/directories on a
//! volume), as opposed to a device-style handler like `CON:`/`PIPE:`
//! that this runtime doesn't model at all.
//!
//! Found missing while running the real Workbench 3.1.4 `C:/List`
//! binary: it checks this before scanning its target path.
//!
//! # Scope
//!
//! This runtime only ever backs paths with either a host-directory
//! [`crate::vfs::Vfs`] volume/assign (always a real file system) or the
//! host stdin/stdout streams (never a file system, and never named by a
//! device string a guest could pass here in the first place -- see
//! [`crate::dosfile`]'s module docs). So the real question collapses to:
//! does `name`'s device/volume prefix (or, if it has none, the current
//! directory's) resolve through the `Vfs` at all? [`Vfs::resolve`] with
//! [`ResolveMode::ParentMustExist`] answers that without requiring the
//! exact object in `name` to exist -- `IsFileSystem`'s own contract
//! ("does not need to identify a physically existing object").
//!
//! The one documented special case kept explicitly: `"*"` (the calling
//! process's own console) is never a file system, per the RKRM's own
//! `IsFileSystem34` workaround listing.

use crate::cpu::{Cpu, DataRegister};
use crate::dispatch::{DOS_LIBRARY_BASE, DispatchError, HandlerContext, LibraryTable};
use crate::guestmem::read_c_string;
use crate::lvos::dos::DOS_LVOS;
use crate::vfs::ResolveMode;

const DOSTRUE: u32 = 0xFFFF_FFFF;
const DOSFALSE: u32 = 0;

/// `IsFileSystem` (`D1` = path). `D0` = `DOSTRUE`/`DOSFALSE`. Cannot
/// fail (no `IoErr()` is set either way, per the RKRM).
fn is_file_system_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let path_addr = ctx.cpu.data_register(DataRegister(1));
    let path = read_c_string(ctx.mem, path_addr);

    let result = if path == b"*" {
        false
    } else {
        match &ctx.dos.vfs {
            Some(vfs) => {
                let name = String::from_utf8_lossy(&path);
                vfs.resolve(&name, ResolveMode::ParentMustExist).is_ok()
            }
            None => false,
        }
    };

    ctx.cpu
        .set_data_register(DataRegister(0), if result { DOSTRUE } else { DOSFALSE });
    Ok(())
}

/// Registers `IsFileSystem` onto [`DOS_LIBRARY_BASE`], looked up by name
/// through [`DOS_LVOS`]. Called from [`crate::dispatch::Runtime::new`]
/// alongside the other `dos.library` registrations.
pub fn register_dosfs_handlers<C: Cpu + 'static>(table: &mut LibraryTable<C>, mem: &mut C::Memory) {
    table
        .register_by_name(
            mem,
            DOS_LIBRARY_BASE,
            DOS_LVOS,
            "dos.library",
            "IsFileSystem",
            is_file_system_handler::<C>,
        )
        .unwrap_or_else(|e| panic!("IsFileSystem should be in DOS_LVOS: {e}"));
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
            let path = std::env::temp_dir().join(format!("volamos-dosfs-test-{tag}-{pid}-{n}"));
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

    fn run_is_file_system(path: &[u8], vfs: Option<Vfs>) -> i32 {
        let mut words = Vec::new();
        let path_idx = push_move_imm_to_d(&mut words, 1, 0);
        push_jsr(&mut words, 6, -708);
        words.push(RTS);

        let path_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        patch_imm32(&mut words, path_idx, path_addr);

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);
        write_c_string(&mut mem, path_addr, path);

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
        if let Some(v) = vfs {
            rt.set_vfs(v);
        }

        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed")
    }

    #[test]
    fn end_to_end_known_volume_is_a_file_system() {
        let tmp = TempDir::new("known");
        let code = run_is_file_system(b"SYS:", Some(vfs_over(tmp.path())));
        assert_eq!(code, DOSTRUE as i32);
    }

    #[test]
    fn end_to_end_console_star_is_never_a_file_system() {
        let tmp = TempDir::new("star");
        let code = run_is_file_system(b"*", Some(vfs_over(tmp.path())));
        assert_eq!(code, DOSFALSE as i32);
    }

    #[test]
    fn end_to_end_unknown_volume_without_vfs_is_not_a_file_system() {
        let code = run_is_file_system(b"SYS:", None);
        assert_eq!(code, DOSFALSE as i32);
    }
}
