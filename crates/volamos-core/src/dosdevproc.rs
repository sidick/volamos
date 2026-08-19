//! `dos.library` `GetDeviceProc`/`FreeDeviceProc`: resolves a path down
//! to the handler responsible for it.
//!
//! Found missing while running the real Workbench 3.1.4 `C:/Delete`
//! binary: it calls `GetDeviceProc` (after `Lock`+`CurrentDir` on the
//! target's containing directory) before deleting anything.
//!
//! # Scope: no real handler processes or packets
//!
//! Real `GetDeviceProc` returns a `struct DevProc` whose `dvp_Port` is
//! the real `MsgPort` of the handler/file-system process responsible
//! for the path -- callers that go on to build and send a `DosPacket`
//! to it expect a live process answering on the other end. This
//! runtime has no such processes (see `crate::execlist`'s module docs:
//! its message-port primitives are single-threaded scaffolding, not a
//! real multi-task packet round-trip), so `dvp_Port` is always `0`
//! (`NULL`) here. What this runtime *can* do faithfully is the locking
//! half: `dvp_Lock`, a [`crate::doslock::SHARED_LOCK`] on the target's
//! containing directory -- confirmed as `GetDeviceProc`'s actual role
//! for e.g. `CreateDir()` internally, per the RKRM's own packet
//! documentation ("The lock in `dp_Arg1` is the directory to which the
//! path in `dp_Arg2` is relative. The `CreateDir()` function obtains it
//! from `GetDeviceProc()`"). A corpus binary that only uses the
//! returned lock (as opposed to sending it raw packets over
//! `dvp_Port`) works correctly against this; one that needs the packet
//! round-trip will hit the next unhandled call at `dvp_Port` usage
//! instead of crashing silently on a `NULL`-dereference here.
//!
//! Multi-assign iteration (`GetDeviceProc` called again with a
//! previous, non-`NULL` result to advance to the next directory of a
//! multi-assign) isn't supported -- this runtime's assigns are always
//! single-directory (see `crate::vfs`'s module docs) -- so a non-`NULL`
//! `prevDevProc` always ends the iteration (`NULL`, after releasing
//! `prevDevProc`'s resources), matching real `GetDeviceProc`'s own
//! "no more matches" contract.

use crate::cpu::{Cpu, DataRegister};
use crate::dispatch::{DOS_LIBRARY_BASE, DispatchError, HandlerContext, LibraryTable};
use crate::dosfile::DosState;
use crate::doslock::SHARED_LOCK;
use crate::guestmem::{GuestHeap, addr_from_bptr, read_c_string};
use crate::lvos::dos::DOS_LVOS;
use crate::memory::AddressSpace;

const DVP_PORT_OFFSET: u32 = 0;
const DVP_LOCK_OFFSET: u32 = 4;
const DVP_FLAGS_OFFSET: u32 = 8;
const DVP_DEVNODE_OFFSET: u32 = 12;
const DEVPROC_SIZE: u32 = 16;

/// Releases a `DevProc` at `addr` (its `dvp_Lock`, then the struct
/// itself). Used by both `FreeDeviceProc` and `GetDeviceProc`'s own
/// "no more matches" path (which must free the caller's previous
/// `DevProc`, per the RKRM: "In case of failure, this function ...
/// releases all resources, including the `DevProc` structure provided
/// as second argument").
fn free_device_proc(
    heap: &mut GuestHeap,
    mem: &mut dyn AddressSpace,
    dos: &mut DosState,
    addr: u32,
) {
    if addr == 0 {
        return;
    }
    let lock_bptr = mem.read_u32(addr + DVP_LOCK_OFFSET);
    dos.unlock(heap, addr_from_bptr(lock_bptr));
    let _ = heap.free(addr);
}

/// Core of `GetDeviceProc`: `name`'s containing directory, locked
/// ([`SHARED_LOCK`]) and wrapped in a heap-allocated `DevProc`. Returns
/// `Ok(0)` (not `Err`) for the "no more matches" `prev != 0` case, per
/// the module docs.
fn get_device_proc(
    heap: &mut GuestHeap,
    mem: &mut dyn AddressSpace,
    dos: &mut DosState,
    name: &str,
    prev: u32,
) -> Result<u32, i32> {
    if prev != 0 {
        free_device_proc(heap, mem, dos, prev);
        return Ok(0);
    }

    let dir_part = match name.rfind([':', '/']) {
        Some(i) => name[..=i].to_string(),
        None => {
            let cwd = dos
                .vfs
                .as_ref()
                .map(|v| v.cwd().to_string())
                .unwrap_or_default();
            format!("{cwd}/")
        }
    };

    let lock_bptr = dos.lock(heap, mem, &dir_part, SHARED_LOCK)?;
    match heap.alloc(DEVPROC_SIZE) {
        Ok(addr) => {
            mem.write_u32(addr + DVP_PORT_OFFSET, 0);
            mem.write_u32(addr + DVP_LOCK_OFFSET, lock_bptr);
            mem.write_u32(addr + DVP_FLAGS_OFFSET, 0);
            mem.write_u32(addr + DVP_DEVNODE_OFFSET, 0);
            Ok(addr)
        }
        Err(_) => {
            dos.unlock(heap, addr_from_bptr(lock_bptr));
            Err(crate::dosfile::ERROR_NO_FREE_STORE)
        }
    }
}

/// `GetDeviceProc` (`D1` = name `CString*`, `D2` = previous
/// `DevProc*`). `D0` = a new `DevProc*`, or `0` if `name` doesn't
/// resolve (or a previous `DevProc` was passed in -- see the module
/// docs' "no multi-assign iteration" note).
fn get_device_proc_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let name_ptr = ctx.cpu.data_register(DataRegister(1));
    let prev = ctx.cpu.data_register(DataRegister(2));
    let name = String::from_utf8_lossy(&read_c_string(ctx.mem, name_ptr)).into_owned();

    match get_device_proc(ctx.heap, ctx.mem, ctx.dos, &name, prev) {
        Ok(addr) => ctx.cpu.set_data_register(DataRegister(0), addr),
        Err(code) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), 0);
        }
    }
    Ok(())
}

/// `FreeDeviceProc` (`D1` = `DevProc*`). No return value.
fn free_device_proc_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let addr = ctx.cpu.data_register(DataRegister(1));
    free_device_proc(ctx.heap, ctx.mem, ctx.dos, addr);
    Ok(())
}

/// Registers `GetDeviceProc`/`FreeDeviceProc` onto [`DOS_LIBRARY_BASE`],
/// looked up by name through [`DOS_LVOS`]. Called from
/// [`crate::dispatch::Runtime::new`] alongside the other `dos.library`
/// registrations.
pub fn register_dosdevproc_handlers<C: Cpu + 'static>(
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
    reg!("GetDeviceProc", get_device_proc_handler::<C>);
    reg!("FreeDeviceProc", free_device_proc_handler::<C>);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{Runtime, StartConfig};
    use crate::guestmem::write_c_string;
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
            let path =
                std::env::temp_dir().join(format!("volamos-dosdevproc-test-{tag}-{pid}-{n}"));
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

    fn setup(root: &Path) -> (GuestHeap, FlatMemory, DosState) {
        let heap = GuestHeap::new(0x1000, 0x40000);
        let mem = FlatMemory::new(0x40000);
        let dos = DosState::new(Some(vfs_over(root)));
        (heap, mem, dos)
    }

    // --- get_device_proc / free_device_proc: unit-level ---

    #[test]
    fn get_device_proc_locks_the_containing_directory() {
        let tmp = TempDir::new("unit-basic");
        fs::create_dir(tmp.path().join("work")).unwrap();
        fs::write(tmp.path().join("work/f.txt"), b"hi").unwrap();
        let (mut heap, mut mem, mut dos) = setup(tmp.path());

        let addr = get_device_proc(&mut heap, &mut mem, &mut dos, "SYS:work/f.txt", 0).unwrap();
        assert_ne!(addr, 0);
        assert_eq!(
            mem.read_u32(addr + DVP_PORT_OFFSET),
            0,
            "no real handler process"
        );
        let lock_bptr = mem.read_u32(addr + DVP_LOCK_OFFSET);
        assert_ne!(lock_bptr, 0);
        let lock_addr = addr_from_bptr(lock_bptr);
        assert_eq!(
            dos.locks.get(&lock_addr).unwrap().host_path,
            tmp.path().join("work")
        );
    }

    #[test]
    fn get_device_proc_missing_directory_fails() {
        let tmp = TempDir::new("unit-missing");
        let (mut heap, mut mem, mut dos) = setup(tmp.path());
        assert!(get_device_proc(&mut heap, &mut mem, &mut dos, "SYS:nope/f.txt", 0).is_err());
    }

    #[test]
    fn get_device_proc_with_a_previous_devproc_ends_iteration_and_frees_it() {
        let tmp = TempDir::new("unit-iterate");
        fs::write(tmp.path().join("f.txt"), b"hi").unwrap();
        let (mut heap, mut mem, mut dos) = setup(tmp.path());

        let first = get_device_proc(&mut heap, &mut mem, &mut dos, "SYS:f.txt", 0).unwrap();
        let lock_bptr = mem.read_u32(first + DVP_LOCK_OFFSET);
        let lock_addr = addr_from_bptr(lock_bptr);
        assert!(dos.locks.contains_key(&lock_addr));

        let second = get_device_proc(&mut heap, &mut mem, &mut dos, "SYS:f.txt", first).unwrap();
        assert_eq!(second, 0, "no multi-assign iteration -> end of matches");
        assert!(
            !dos.locks.contains_key(&lock_addr),
            "the previous DevProc's lock should have been released"
        );
    }

    #[test]
    fn free_device_proc_releases_the_lock_and_the_struct() {
        let tmp = TempDir::new("unit-free");
        fs::write(tmp.path().join("f.txt"), b"hi").unwrap();
        let (mut heap, mut mem, mut dos) = setup(tmp.path());
        let free_before = heap.free_bytes();

        let addr = get_device_proc(&mut heap, &mut mem, &mut dos, "SYS:f.txt", 0).unwrap();
        assert!(heap.free_bytes() < free_before);
        let lock_bptr = mem.read_u32(addr + DVP_LOCK_OFFSET);
        let lock_addr = addr_from_bptr(lock_bptr);
        assert!(dos.locks.contains_key(&lock_addr));

        free_device_proc(&mut heap, &mut mem, &mut dos, addr);
        assert!(!dos.locks.contains_key(&lock_addr));
        assert_eq!(heap.free_bytes(), free_before);
    }

    #[test]
    fn free_device_proc_zero_is_a_no_op() {
        let tmp = TempDir::new("unit-free-zero");
        let (mut heap, mut mem, mut dos) = setup(tmp.path());
        free_device_proc(&mut heap, &mut mem, &mut dos, 0); // should not panic
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
    fn end_to_end_get_device_proc_returns_a_lock_on_the_containing_dir() {
        let tmp = TempDir::new("getdeviceproc");
        fs::create_dir(tmp.path().join("work")).unwrap();
        fs::write(tmp.path().join("work/f.txt"), b"hi").unwrap();
        let name = b"SYS:work/f.txt\0";

        let mut words = Vec::new();
        let name_idx = push_move_imm_to_d(&mut words, 1, 0);
        push_move_imm_to_d(&mut words, 2, 0); // D2 = NULL (first call)
        push_jsr(&mut words, 6, -642); // GetDeviceProc(a6): D0 = DevProc*
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
        rt.set_vfs(vfs_over(tmp.path()));

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_ne!(code, 0, "should return a non-NULL DevProc*");

        let addr = code as u32;
        assert_eq!(rt.memory().read_u32(addr + DVP_PORT_OFFSET), 0);
        let lock_bptr = rt.memory().read_u32(addr + DVP_LOCK_OFFSET);
        assert_ne!(lock_bptr, 0, "dvp_Lock should be a real lock");
    }

    #[test]
    fn end_to_end_get_device_proc_missing_path_returns_null() {
        let tmp = TempDir::new("getdeviceproc-missing");

        let mut words = Vec::new();
        let name_idx = push_move_imm_to_d(&mut words, 1, 0);
        push_move_imm_to_d(&mut words, 2, 0);
        push_jsr(&mut words, 6, -642);
        words.push(RTS);

        let name = b"SYS:nope/f.txt\0";
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
        rt.set_vfs(vfs_over(tmp.path()));

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 0);
    }

    #[test]
    fn end_to_end_get_device_proc_then_free_unlocks_and_frees() {
        let tmp = TempDir::new("getdeviceproc-free");
        fs::write(tmp.path().join("f.txt"), b"hi").unwrap();
        let name = b"SYS:f.txt\0";

        let mut words = Vec::new();
        let name_idx = push_move_imm_to_d(&mut words, 1, 0);
        push_move_imm_to_d(&mut words, 2, 0);
        push_jsr(&mut words, 6, -642); // GetDeviceProc(a6): D0 = DevProc*
        words.push(0x2200); // move.l d0,d1 (save for FreeDeviceProc)
        push_jsr(&mut words, 6, -648); // FreeDeviceProc(a6)
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
        rt.set_vfs(vfs_over(tmp.path()));

        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed");
        // No direct accessor for post-run DosState from this harness;
        // this test's purpose is to confirm the trap-dispatch wiring for
        // both calls works end-to-end without panicking -- the actual
        // unlock-and-free effect is covered by the unit-level
        // `free_device_proc_releases_the_lock_and_the_struct` above.
    }
}
