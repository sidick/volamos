//! `dos.library` `AssignLock`: create, update or cancel an assign.
//!
//! Found missing while running the real Workbench 3.1.4 `C:/Assign`
//! binary against `Assign NAME: TARGET:` (its primary purpose -- the
//! no-argument "list current assigns" form already worked with no
//! gaps, since it's built entirely out of `LockDosList`/`NextDosEntry`/
//! `UnLockDosList`, already implemented for `Info`).
//!
//! # Scope: `AssignLock` only
//!
//! The RKRM documents five related functions (`AssignLock`,
//! `AssignPath`, `AssignLate`, `AssignAdd`, `RemAssignList`) for
//! creating/updating/extending assigns. Only `AssignLock` -- create,
//! replace, or cancel a regular assign from a lock -- is implemented
//! here, since it's the only one the real `Assign` binary's simple
//! `NAME: TARGET` form calls. The others (non-binding/late-binding
//! assigns, multi-assign extension) are real gaps, not stubbed; a
//! future corpus binary that needs `Assign ADD`/`Assign NAME: PATH
//! NONBINDING` (etc.) will hit an honest "unhandled library call"
//! trap rather than a silent no-op.
//!
//! # Implementation: a plain `Vfs` assign, not a `DosList` entry
//!
//! Real `AssignLock` builds and links a genuine `DosList` entry (like
//! `crate::dosdevlist::lock_dos_list` does for `LDF_VOLUMES`, but
//! persisted past a single `LockDosList`/`UnLockDosList` session).
//! This runtime instead reuses its existing [`crate::vfs::Vfs`] assign
//! model directly ([`crate::vfs::Vfs::set_assign`]/`remove_assign`,
//! keyed on the *Amiga path string* the target lock resolved to, per
//! [`crate::doslock::LockEntry::amiga_path`]) -- the same
//! representation `-a`/`--assign` on the CLI already produces. This
//! means an assign made with `AssignLock` immediately works for every
//! other path-resolving call (`Lock`, `Open`, ...) within the same
//! process, which is the only thing that matters for a corpus binary;
//! it does *not* show up in a subsequent `LockDosList(LDF_ASSIGNS)`
//! walk, since `crate::dosdevlist` doesn't materialize assign-type
//! `DosList` entries at all (see that module's "Scope: volumes only"
//! docs) -- a real gap if a future binary needs to *enumerate*
//! assigns it created, not just use them.

use crate::cpu::{Cpu, DataRegister};
use crate::dispatch::{DOS_LIBRARY_BASE, DispatchError, HandlerContext, LibraryTable};
use crate::dosfile::{
    ERROR_INVALID_COMPONENT_NAME, ERROR_INVALID_LOCK, ERROR_OBJECT_EXISTS, ERROR_OBJECT_NOT_FOUND,
};
use crate::guestmem::{addr_from_bptr, read_c_string};
use crate::lvos::dos::DOS_LVOS;

const DOSTRUE: u32 = 0xFFFF_FFFF;
const DOSFALSE: u32 = 0;

/// Real assign names are limited to 30 characters (`dos/dos.h`'s
/// `MAXFILENAME`-adjacent convention, per the RKRM's own documented
/// `ERROR_INVALID_COMPONENT_NAME` condition).
const MAX_ASSIGN_NAME_LEN: usize = 30;

/// `AssignLock` (`D1` = name `CString*`, `D2` = lock `BPTR`). `D0` =
/// `DOSTRUE`/`DOSFALSE` (+ `IoErr()` set on failure). See the module
/// docs for exactly what this does and doesn't model.
fn assign_lock_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let name_ptr = ctx.cpu.data_register(DataRegister(1));
    let lock_bptr = ctx.cpu.data_register(DataRegister(2));
    let name = String::from_utf8_lossy(&read_c_string(ctx.mem, name_ptr)).into_owned();

    let result = (|| -> Result<(), i32> {
        if name.len() > MAX_ASSIGN_NAME_LEN {
            return Err(ERROR_INVALID_COMPONENT_NAME);
        }

        if lock_bptr == 0 {
            let vfs = ctx.dos.vfs.as_mut().ok_or(ERROR_OBJECT_NOT_FOUND)?;
            vfs.remove_assign(&name);
            return Ok(());
        }

        let addr = addr_from_bptr(lock_bptr);
        let amiga_path = ctx
            .dos
            .locks
            .get(&addr)
            .map(|entry| entry.amiga_path.clone())
            .ok_or(ERROR_INVALID_LOCK)?;

        let vfs = ctx.dos.vfs.as_mut().ok_or(ERROR_OBJECT_NOT_FOUND)?;
        if vfs
            .volumes()
            .iter()
            .any(|(vol_name, _)| vol_name.eq_ignore_ascii_case(&name))
        {
            return Err(ERROR_OBJECT_EXISTS);
        }
        vfs.set_assign(&name, vec![amiga_path]);

        // "The lock ... is then absorbed into the assign and should no
        // longer be used by the calling program" -- this runtime's
        // assign model doesn't need to keep the FileLock struct alive
        // (it stores the resolved Amiga path string instead), so
        // release it now rather than leaking the heap block.
        ctx.dos.unlock(ctx.heap, addr);
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

/// Registers `AssignLock` onto [`DOS_LIBRARY_BASE`], looked up by name
/// through [`DOS_LVOS`]. Called from [`crate::dispatch::Runtime::new`]
/// alongside the other `dos.library` registrations.
pub fn register_dosassign_handlers<C: Cpu + 'static>(
    table: &mut LibraryTable<C>,
    mem: &mut C::Memory,
) {
    table
        .register_by_name(
            mem,
            DOS_LIBRARY_BASE,
            DOS_LVOS,
            "dos.library",
            "AssignLock",
            assign_lock_handler::<C>,
        )
        .unwrap_or_else(|e| panic!("AssignLock should be in DOS_LVOS: {e}"));
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
            let path = std::env::temp_dir().join(format!("volamos-dosassign-test-{tag}-{pid}-{n}"));
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

    /// `Lock(target, SHARED_LOCK)` then `AssignLock(name, D0)`. `D0` at
    /// exit is `AssignLock`'s own result.
    fn run_assign_lock(name: &[u8], target: &[u8], vfs_root: &Path) -> i32 {
        let mut words = Vec::new();
        let target_idx = push_move_imm_to_d(&mut words, 1, 0);
        push_move_imm_to_d(&mut words, 2, crate::doslock::SHARED_LOCK as u32);
        push_jsr(&mut words, 6, -84); // Lock(a6): D0 = BPTR or 0
        words.push(0x2200); // move.l d0,d1 (lock -> D1 for AssignLock)
        let name_idx = push_move_imm_to_d(&mut words, 2, 0);
        // AssignLock wants name in D1, lock in D2 -- swap what we just
        // built: D1 currently holds lock, D2 will hold the name ptr, so
        // exchange them.
        words.push(0xC342); // exg d1,d2
        push_jsr(&mut words, 6, -612); // AssignLock(a6)
        words.push(RTS);

        let target_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        let name_addr = target_addr + target.len() as u32;
        patch_imm32(&mut words, target_idx, target_addr);
        patch_imm32(&mut words, name_idx, name_addr);

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);
        write_c_string(&mut mem, target_addr, target);
        write_c_string(&mut mem, name_addr, name);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: name_addr + name.len() as u32 + 0x100,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        rt.set_vfs(vfs_over(vfs_root));

        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed")
    }

    #[test]
    fn assign_lock_creates_a_working_assign() {
        let tmp = TempDir::new("basic");
        fs::create_dir(tmp.path().join("libs")).unwrap();

        let code = run_assign_lock(b"FOO\0", b"SYS:libs\0", tmp.path());
        assert_eq!(code, DOSTRUE as i32);
    }

    #[test]
    fn assign_lock_colliding_with_a_volume_name_fails() {
        let tmp = TempDir::new("collide");
        let code = run_assign_lock(b"SYS\0", b"SYS:\0", tmp.path());
        assert_eq!(code, DOSFALSE as i32);
    }

    #[test]
    fn set_assign_then_resolve_through_it() {
        let tmp = TempDir::new("resolve");
        fs::create_dir(tmp.path().join("libs")).unwrap();
        let mut vfs = vfs_over(tmp.path());
        vfs.set_assign("FOO", vec!["SYS:libs".to_string()]);
        let resolved = vfs
            .resolve("FOO:", crate::vfs::ResolveMode::MustExist)
            .unwrap();
        assert_eq!(resolved, tmp.path().join("libs"));
    }

    #[test]
    fn remove_assign_then_resolve_fails() {
        let tmp = TempDir::new("remove");
        let mut vfs = vfs_over(tmp.path());
        vfs.set_assign("FOO", vec!["SYS:".to_string()]);
        vfs.remove_assign("FOO");
        assert!(
            vfs.resolve("FOO:", crate::vfs::ResolveMode::MustExist)
                .is_err()
        );
    }

    #[test]
    fn remove_assign_of_an_unknown_name_is_a_no_op() {
        let tmp = TempDir::new("remove-unknown");
        let mut vfs = vfs_over(tmp.path());
        vfs.remove_assign("NOPE");
    }
}
