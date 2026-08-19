//! `dos.library` `LockDosList`/`NextDosEntry`/`UnLockDosList`: read-only
//! iteration of the device list.
//!
//! Found missing while running the real Workbench 3.1.4 `C:/Info`
//! binary, which walks the device list to report on every mounted
//! volume.
//!
//! # Scope: volumes only
//!
//! This runtime models Amiga volumes ([`crate::vfs::VfsConfig::volumes`])
//! but has no separate representation of handler devices or assigns as
//! distinct `DosList` entries (assigns resolve internally within
//! [`crate::vfs`] without ever producing an addressable object). So only
//! `LDF_VOLUMES` entries are ever materialized here; a `flags` value
//! that also (or only) requests `LDF_DEVICES`/`LDF_ASSIGNS` simply never
//! yields any of those -- matching a real device list on a system with
//! no other handlers/assigns mounted, rather than crashing or lying
//! about entries this runtime can't back.
//!
//! # Locking model
//!
//! Real `LockDosList` blocks until the list is available and returns an
//! opaque handle; `UnLockDosList` takes no handle back, only the same
//! `flags` used to lock (see the RKRM: the real implementation tracks
//! the lock itself, not per-caller state). This runtime is
//! single-threaded and never blocks, so there is at most one
//! outstanding "session" at a time:
//! [`crate::dosfile::DosState::dos_list_active`] holds every heap
//! address `LockDosList` allocated (the header node, each `DosList`
//! entry, and their `dol_Name` buffers), freed in one shot by
//! `UnLockDosList`. A second `LockDosList` before the first is unlocked
//! leaks the first session's allocations rather than blocking or
//! erroring -- acceptable for the well-behaved lock/iterate/unlock
//! pattern every real caller (including `Info`) uses, and not worth
//! modeling proper reentrant/blocking semantics for.
//!
//! # `struct DosList` layout
//!
//! ```text
//! offset  0  dol_Next      BPTR  (next DosList, or 0)
//! offset  4  dol_Type      LONG  (DLT_VOLUME, ...)
//! offset  8  dol_Task      APTR  (a fixed non-NULL sentinel, never a
//!                                 real dereferenceable MsgPort -- see
//!                                 crate::dosfile::DEFAULT_FILE_SYS_TASK)
//! offset 12  dol_Lock      BPTR  (always 0 here)
//! offset 16  dol_misc      union, 24 bytes -- dol_volume variant used:
//!            +0  dol_VolumeDate  struct DateStamp (3 LONGs: Days/Mins/Ticks)
//!            +12 dol_LockList    BPTR (always 0 here)
//!            +16 dol_DiskType    LONG (ID_DOS_DISK)
//! offset 40  dol_Name      BSTR  (length-prefixed, also NUL-terminated
//!                                 as a courtesy, per the RKRM)
//! -- total 44 bytes
//! ```
//! Byte offsets confirmed against the AmiBlitz3 `dosextens.ab3` include
//! (the same external reference this session already used for the
//! `LOCK_SAME`-family constants), since the RKRM's own prose doesn't
//! give literal struct offsets.

use crate::cpu::{Cpu, DataRegister};
use crate::dispatch::{DOS_LIBRARY_BASE, DispatchError, HandlerContext, LibraryTable};
use crate::dosfile::DosState;
use crate::guestmem::{GuestHeap, addr_from_bptr, bptr_from_addr, write_bstr};
use crate::lvos::dos::DOS_LVOS;
use crate::memory::AddressSpace;

pub const DLT_DEVICE: i32 = 0;
pub const DLT_DIRECTORY: i32 = 1;
pub const DLT_VOLUME: i32 = 2;
pub const DLT_LATE: i32 = 3;
pub const DLT_NONBINDING: i32 = 4;
pub const DLT_PRIVATE: i32 = -1;

pub const LDF_READ: u32 = 1 << 0;
pub const LDF_WRITE: u32 = 1 << 1;
pub const LDF_DEVICES: u32 = 1 << 2;
pub const LDF_VOLUMES: u32 = 1 << 3;
pub const LDF_ASSIGNS: u32 = 1 << 4;

const ID_DOS_DISK: u32 = 0x444F_5300;

const DOL_NEXT_OFFSET: u32 = 0;
const DOL_TYPE_OFFSET: u32 = 4;
const DOL_TASK_OFFSET: u32 = 8;
const DOL_DISKTYPE_OFFSET: u32 = 32;
const DOL_NAME_OFFSET: u32 = 40;
const DOSLIST_SIZE: u32 = 44;

fn zero_doslist(mem: &mut dyn AddressSpace, addr: u32) {
    for i in 0..DOSLIST_SIZE {
        mem.write_u8(addr.wrapping_add(i), 0);
    }
}

/// Returns a stable, process-lifetime, non-dereferenceable synthetic
/// `dol_Task` id for `volume_name`, allocated the first time it's
/// asked for and reused after that (so repeated `LockDosList` sessions
/// hand back the *same* id for the same volume, matching how a real
/// volume's handler task doesn't change across the process's lifetime).
/// Distinct from [`crate::dosfile::DEFAULT_FILE_SYS_TASK`] (which is a
/// single shared sentinel for `GetFileSysTask`) since `crate::dospkt`'s
/// `DoPkt` needs to tell *which* volume a packet's `port` identifies.
fn task_id_for_volume(dos: &mut DosState, volume_name: &str) -> u32 {
    let key = volume_name.to_ascii_uppercase();
    let next_id =
        crate::dosfile::DEFAULT_FILE_SYS_TASK + (dos.volume_task_ids.len() as u32 + 1) * 4;
    *dos.volume_task_ids.entry(key).or_insert(next_id)
}

/// The volume name (as configured, uppercased) that `task_id` was
/// allocated for by [`task_id_for_volume`], or `None` if `task_id`
/// isn't a known volume's task id. Used by `crate::dospkt`'s `DoPkt`.
pub(crate) fn volume_for_task_id(dos: &DosState, task_id: u32) -> Option<String> {
    dos.volume_task_ids
        .iter()
        .find(|&(_, &id)| id == task_id)
        .map(|(name, _)| name.clone())
}

/// Core of `LockDosList`: builds a header node plus one `DosList` entry
/// per configured volume (if `flags & LDF_VOLUMES`), chained via
/// `dol_Next`, and records every allocated address on `dos.dos_list_active`
/// for `unlock_dos_list` to free. Returns the header's address (never
/// fails -- see the module docs' locking model).
fn lock_dos_list(
    heap: &mut GuestHeap,
    mem: &mut dyn AddressSpace,
    dos: &mut DosState,
    flags: u32,
) -> u32 {
    let mut allocated = Vec::new();
    let header = heap
        .alloc(DOSLIST_SIZE)
        .expect("LockDosList: heap exhausted allocating header");
    zero_doslist(mem, header);
    // DLT_PRIVATE, not a real entry type: this node is a scan starting
    // point only, and must never itself match a NextDosEntry/
    // FindDosEntry type filter (DLT_DEVICE is 0, i.e. what a zeroed
    // dol_Type would otherwise read as).
    mem.write_u32(header + DOL_TYPE_OFFSET, DLT_PRIVATE as u32);
    allocated.push(header);

    let mut entries = Vec::new();
    if flags & LDF_VOLUMES != 0 {
        let volumes: Vec<String> = dos
            .vfs
            .as_ref()
            .map(|vfs| vfs.volumes().iter().map(|(name, _)| name.clone()).collect())
            .unwrap_or_default();
        for name in volumes {
            let addr = heap
                .alloc(DOSLIST_SIZE)
                .expect("LockDosList: heap exhausted allocating a DosList entry");
            zero_doslist(mem, addr);
            mem.write_u32(addr + DOL_TYPE_OFFSET, DLT_VOLUME as u32);
            // Non-NULL, matching a real live volume -- see
            // crate::dosfile::DEFAULT_FILE_SYS_TASK's doc comment for
            // why callers like Info() check this for NULL rather than
            // dereferencing it, and task_id_for_volume's for why each
            // volume gets a distinct id rather than sharing one.
            mem.write_u32(addr + DOL_TASK_OFFSET, task_id_for_volume(dos, &name));
            mem.write_u32(addr + DOL_DISKTYPE_OFFSET, ID_DOS_DISK);

            let name_bytes = name.as_bytes();
            // +1 length byte, +1 NUL courtesy byte (see module docs).
            let name_addr = heap
                .alloc(name_bytes.len() as u32 + 2)
                .expect("LockDosList: heap exhausted allocating dol_Name");
            let written = write_bstr(mem, name_addr, name_bytes);
            mem.write_u8(name_addr + 1 + written as u32, 0);
            mem.write_u32(addr + DOL_NAME_OFFSET, bptr_from_addr(name_addr));

            allocated.push(name_addr);
            allocated.push(addr);
            entries.push(addr);
        }
    }

    let mut prev = header;
    for &entry in &entries {
        mem.write_u32(prev + DOL_NEXT_OFFSET, bptr_from_addr(entry));
        prev = entry;
    }

    dos.dos_list_active = allocated;
    header
}

/// Core of `NextDosEntry`: walks forward from `dlist` (a header or entry
/// address), returning the first entry whose `dol_Type` matches `flags`,
/// or `0` at the end of the list.
fn next_dos_entry(mem: &dyn AddressSpace, dlist: u32, flags: u32) -> u32 {
    let mut next_bptr = if dlist == 0 {
        0
    } else {
        mem.read_u32(dlist + DOL_NEXT_OFFSET)
    };

    while next_bptr != 0 {
        let addr = addr_from_bptr(next_bptr);
        let dol_type = mem.read_u32(addr + DOL_TYPE_OFFSET) as i32;
        let matches = match dol_type {
            DLT_VOLUME => flags & LDF_VOLUMES != 0,
            DLT_DEVICE => flags & LDF_DEVICES != 0,
            DLT_DIRECTORY | DLT_LATE | DLT_NONBINDING => flags & LDF_ASSIGNS != 0,
            _ => false,
        };
        if matches {
            return addr;
        }
        next_bptr = mem.read_u32(addr + DOL_NEXT_OFFSET);
    }
    0
}

/// Core of `FindDosEntry`: scans starting at (and *including*) `dlist`
/// itself, returning the first entry whose `dol_Type` matches `flags`
/// and, if `name` is `Some`, whose `dol_Name` case-insensitively equals
/// it -- `None` matches any name of a matching type. `0` at the end of
/// the list.
fn find_dos_entry(mem: &dyn AddressSpace, dlist: u32, name: Option<&str>, flags: u32) -> u32 {
    let mut cur = dlist;
    while cur != 0 {
        let dol_type = mem.read_u32(cur + DOL_TYPE_OFFSET) as i32;
        let type_matches = match dol_type {
            DLT_VOLUME => flags & LDF_VOLUMES != 0,
            DLT_DEVICE => flags & LDF_DEVICES != 0,
            DLT_DIRECTORY | DLT_LATE | DLT_NONBINDING => flags & LDF_ASSIGNS != 0,
            _ => false,
        };
        if type_matches {
            let name_matches = match name {
                None => true,
                Some(wanted) => {
                    let name_bptr = mem.read_u32(cur + DOL_NAME_OFFSET);
                    let entry_name = crate::guestmem::read_bstr(mem, addr_from_bptr(name_bptr));
                    entry_name.eq_ignore_ascii_case(wanted.as_bytes())
                }
            };
            if name_matches {
                return cur;
            }
        }
        cur = addr_from_bptr(mem.read_u32(cur + DOL_NEXT_OFFSET));
    }
    0
}

/// Core of `UnLockDosList`: frees every heap allocation the active
/// session made (see the module docs' locking model).
fn unlock_dos_list(heap: &mut GuestHeap, dos: &mut DosState) {
    for addr in std::mem::take(&mut dos.dos_list_active) {
        let _ = heap.free(addr);
    }
}

/// `LockDosList` (`D1` = flags). `D0` = an opaque `struct DosList *`
/// handle, valid for `NextDosEntry`.
fn lock_dos_list_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let flags = ctx.cpu.data_register(DataRegister(1));
    let header = lock_dos_list(ctx.heap, ctx.mem, ctx.dos, flags);
    ctx.cpu.set_data_register(DataRegister(0), header);
    Ok(())
}

/// `NextDosEntry` (`D1` = previous `DosList*`/handle, `D2` = flags).
/// `D0` = the next entry whose `dol_Type` matches `flags`, or `0` at the
/// end of the list.
fn next_dos_entry_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let dlist = ctx.cpu.data_register(DataRegister(1));
    let flags = ctx.cpu.data_register(DataRegister(2));
    let result = next_dos_entry(ctx.mem, dlist, flags);
    ctx.cpu.set_data_register(DataRegister(0), result);
    Ok(())
}

/// `FindDosEntry` (`D1` = `DosList*`/handle, `D2` = name `CString*` or
/// `0`, `D3` = flags). `D0` = the matching entry, or `0`.
fn find_dos_entry_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let dlist = ctx.cpu.data_register(DataRegister(1));
    let name_ptr = ctx.cpu.data_register(DataRegister(2));
    let flags = ctx.cpu.data_register(DataRegister(3));
    let name = if name_ptr == 0 {
        None
    } else {
        Some(
            String::from_utf8_lossy(&crate::guestmem::read_c_string(ctx.mem, name_ptr))
                .into_owned(),
        )
    };
    let result = find_dos_entry(ctx.mem, dlist, name.as_deref(), flags);
    ctx.cpu.set_data_register(DataRegister(0), result);
    Ok(())
}

/// `UnLockDosList` (`D1` = flags, unused -- see the module docs' locking
/// model). No return value. Frees every heap allocation the active
/// `LockDosList` session made.
fn unlock_dos_list_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    unlock_dos_list(ctx.heap, ctx.dos);
    Ok(())
}

/// Registers `LockDosList`/`NextDosEntry`/`UnLockDosList` onto
/// [`DOS_LIBRARY_BASE`], looked up by name through [`DOS_LVOS`]. Called
/// from [`crate::dispatch::Runtime::new`] alongside the other
/// `dos.library` registrations.
pub fn register_dosdevlist_handlers<C: Cpu + 'static>(
    table: &mut LibraryTable<C>,
    mem: &mut C::Memory,
) {
    table
        .register_by_name(
            mem,
            DOS_LIBRARY_BASE,
            DOS_LVOS,
            "dos.library",
            "LockDosList",
            lock_dos_list_handler::<C>,
        )
        .unwrap_or_else(|e| panic!("LockDosList should be in DOS_LVOS: {e}"));
    table
        .register_by_name(
            mem,
            DOS_LIBRARY_BASE,
            DOS_LVOS,
            "dos.library",
            "NextDosEntry",
            next_dos_entry_handler::<C>,
        )
        .unwrap_or_else(|e| panic!("NextDosEntry should be in DOS_LVOS: {e}"));
    table
        .register_by_name(
            mem,
            DOS_LIBRARY_BASE,
            DOS_LVOS,
            "dos.library",
            "FindDosEntry",
            find_dos_entry_handler::<C>,
        )
        .unwrap_or_else(|e| panic!("FindDosEntry should be in DOS_LVOS: {e}"));
    table
        .register_by_name(
            mem,
            DOS_LIBRARY_BASE,
            DOS_LVOS,
            "dos.library",
            "UnLockDosList",
            unlock_dos_list_handler::<C>,
        )
        .unwrap_or_else(|e| panic!("UnLockDosList should be in DOS_LVOS: {e}"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dosfile::DosState;
    use crate::guestmem::{GuestHeap, read_bstr};
    use crate::memory::{AddressSpace, FlatMemory};
    use crate::vfs::{Vfs, VfsConfig};
    use std::path::PathBuf;

    fn setup(volumes: &[&str]) -> (GuestHeap, FlatMemory, DosState) {
        let heap = GuestHeap::new(0x1000, 0x1_0000);
        let mem = FlatMemory::new(0x2_0000);
        let vfs = Vfs::new(VfsConfig {
            volumes: volumes
                .iter()
                .map(|v| (v.to_string(), PathBuf::from("/tmp")))
                .collect(),
            assigns: vec![],
            auto_assign_root: None,
            cwd: format!("{}:", volumes[0]),
        })
        .expect("build vfs");
        (heap, mem, DosState::new(Some(vfs)))
    }

    #[test]
    fn lock_dos_list_builds_one_entry_per_volume_with_bstr_names() {
        let (mut heap, mut mem, mut dos) = setup(&["SYS", "WORK"]);

        let header = lock_dos_list(&mut heap, &mut mem, &mut dos, LDF_VOLUMES);
        assert_ne!(header, 0);

        let first = next_dos_entry(&mem, header, LDF_VOLUMES);
        assert_ne!(first, 0);
        assert_eq!(mem.read_u32(first + DOL_TYPE_OFFSET), DLT_VOLUME as u32);
        let name_bptr = mem.read_u32(first + DOL_NAME_OFFSET);
        let name_addr = addr_from_bptr(name_bptr);
        let name = read_bstr(&mem, name_addr);
        assert_eq!(name, b"SYS");
        // NUL courtesy byte right after the BSTR data (per the RKRM).
        assert_eq!(mem.read_u8(name_addr + 1 + name.len() as u32), 0);

        let second = next_dos_entry(&mem, first, LDF_VOLUMES);
        assert_ne!(second, 0);
        let second_name = read_bstr(&mem, addr_from_bptr(mem.read_u32(second + DOL_NAME_OFFSET)));
        assert_eq!(second_name, b"WORK");

        assert_eq!(next_dos_entry(&mem, second, LDF_VOLUMES), 0, "end of list");
    }

    #[test]
    fn next_dos_entry_filters_by_type_flags() {
        let (mut heap, mut mem, mut dos) = setup(&["SYS"]);
        let header = lock_dos_list(&mut heap, &mut mem, &mut dos, LDF_VOLUMES);
        // Real volumes exist, but asking for devices only should yield
        // nothing -- see the module docs' "volumes only" scope note.
        assert_eq!(next_dos_entry(&mem, header, LDF_DEVICES), 0);
        assert_ne!(next_dos_entry(&mem, header, LDF_VOLUMES), 0);
    }

    #[test]
    fn lock_dos_list_with_no_volume_flag_yields_an_empty_list() {
        let (mut heap, mut mem, mut dos) = setup(&["SYS"]);
        let header = lock_dos_list(&mut heap, &mut mem, &mut dos, LDF_DEVICES | LDF_ASSIGNS);
        assert_eq!(
            next_dos_entry(&mem, header, LDF_VOLUMES | LDF_DEVICES | LDF_ASSIGNS),
            0
        );
    }

    #[test]
    fn find_dos_entry_matches_by_name_case_insensitively() {
        let (mut heap, mut mem, mut dos) = setup(&["SYS", "WORK"]);
        let header = lock_dos_list(&mut heap, &mut mem, &mut dos, LDF_VOLUMES);

        let found = find_dos_entry(&mem, header, Some("work"), LDF_VOLUMES);
        assert_ne!(found, 0);
        let name = read_bstr(&mem, addr_from_bptr(mem.read_u32(found + DOL_NAME_OFFSET)));
        assert_eq!(name, b"WORK");
    }

    #[test]
    fn find_dos_entry_with_no_name_matches_the_first_of_the_type() {
        let (mut heap, mut mem, mut dos) = setup(&["SYS", "WORK"]);
        let header = lock_dos_list(&mut heap, &mut mem, &mut dos, LDF_VOLUMES);
        let found = find_dos_entry(&mem, header, None, LDF_VOLUMES);
        assert_eq!(found, next_dos_entry(&mem, header, LDF_VOLUMES));
    }

    #[test]
    fn find_dos_entry_never_matches_the_private_header_node() {
        let (mut heap, mut mem, mut dos) = setup(&["SYS"]);
        let header = lock_dos_list(&mut heap, &mut mem, &mut dos, LDF_DEVICES);
        // Header's dol_Type is DLT_PRIVATE, not DLT_DEVICE -- must not
        // spuriously match a DEVICES-flagged, name-less search.
        assert_eq!(find_dos_entry(&mem, header, None, LDF_DEVICES), 0);
    }

    #[test]
    fn find_dos_entry_missing_name_returns_zero() {
        let (mut heap, mut mem, mut dos) = setup(&["SYS"]);
        let header = lock_dos_list(&mut heap, &mut mem, &mut dos, LDF_VOLUMES);
        assert_eq!(find_dos_entry(&mem, header, Some("NOPE"), LDF_VOLUMES), 0);
    }

    #[test]
    fn unlock_dos_list_frees_the_active_session() {
        let (mut heap, mut mem, mut dos) = setup(&["SYS", "WORK"]);
        let before_free = heap.total_free();

        let _ = lock_dos_list(&mut heap, &mut mem, &mut dos, LDF_VOLUMES);
        assert!(!dos.dos_list_active.is_empty());
        assert!(heap.total_free() < before_free);

        unlock_dos_list(&mut heap, &mut dos);
        assert!(dos.dos_list_active.is_empty());
        assert_eq!(
            heap.total_free(),
            before_free,
            "every session allocation should be freed"
        );
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
    const MOVE_L_D0_D1: u16 = 0x2200;

    fn push_move_imm_to_d(words: &mut Vec<u16>, dn: u16, imm: u32) {
        words.push(move_imm_to_d(dn));
        words.push((imm >> 16) as u16);
        words.push(imm as u16);
    }
    fn push_jsr(words: &mut Vec<u16>, an: u16, disp: i32) {
        words.push(jsr_disp16(an));
        words.push(disp as u16);
    }
    fn load_words(mem: &mut FlatMemory, addr: u32, words: &[u16]) {
        let mut offset = addr;
        for &w in words {
            mem.write_u16(offset, w);
            offset += 2;
        }
    }

    #[test]
    fn end_to_end_lock_next_dos_entry_walks_a_real_volume() {
        // LockDosList(LDF_VOLUMES) -> D0 = header
        // D1 = D0 (header); D2 = LDF_VOLUMES
        // NextDosEntry(header, LDF_VOLUMES) -> D0 = first entry addr
        let mut words = Vec::new();
        push_move_imm_to_d(&mut words, 1, LDF_VOLUMES);
        push_jsr(&mut words, 6, -654); // LockDosList(a6)
        words.push(MOVE_L_D0_D1); // D1 = header
        push_move_imm_to_d(&mut words, 2, LDF_VOLUMES);
        push_jsr(&mut words, 6, -690); // NextDosEntry(a6)
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: TRAP_TABLE_END + (words.len() as u32) * 2 + 0x100,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let vfs = Vfs::new(VfsConfig {
            volumes: vec![("SYS".to_string(), PathBuf::from("/tmp"))],
            assigns: vec![],
            auto_assign_root: None,
            cwd: "SYS:".to_string(),
        })
        .expect("build vfs");
        rt.set_vfs(vfs);

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_ne!(code, 0, "should have found the SYS: entry");

        let entry_addr = code as u32;
        assert_eq!(
            rt.memory().read_u32(entry_addr + DOL_TYPE_OFFSET),
            DLT_VOLUME as u32
        );
        let name_bptr = rt.memory().read_u32(entry_addr + DOL_NAME_OFFSET);
        assert_eq!(read_bstr(rt.memory(), addr_from_bptr(name_bptr)), b"SYS");
    }

    #[test]
    fn end_to_end_find_dos_entry_locates_a_volume_by_name() {
        // LockDosList(LDF_VOLUMES) -> D0 = header
        // D1 = D0 (header); D2 = name ptr; D3 = LDF_VOLUMES
        // FindDosEntry(header, "WORK", LDF_VOLUMES) -> D0 = entry addr
        let mut words = Vec::new();
        push_move_imm_to_d(&mut words, 1, LDF_VOLUMES);
        push_jsr(&mut words, 6, -654); // LockDosList(a6)
        words.push(MOVE_L_D0_D1); // D1 = header
        let name_idx = words.len();
        push_move_imm_to_d(&mut words, 2, 0); // D2 = name ptr (patched)
        push_move_imm_to_d(&mut words, 3, LDF_VOLUMES);
        push_jsr(&mut words, 6, -684); // FindDosEntry(a6)
        words.push(RTS);

        let name = b"WORK\0";
        let name_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        words[name_idx + 1] = (name_addr >> 16) as u16;
        words[name_idx + 2] = name_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);
        crate::guestmem::write_c_string(&mut mem, name_addr, name);

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
        let vfs = Vfs::new(VfsConfig {
            volumes: vec![
                ("SYS".to_string(), PathBuf::from("/tmp")),
                ("WORK".to_string(), PathBuf::from("/tmp")),
            ],
            assigns: vec![],
            auto_assign_root: None,
            cwd: "SYS:".to_string(),
        })
        .expect("build vfs");
        rt.set_vfs(vfs);

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_ne!(code, 0, "should have found the WORK: entry");

        let entry_addr = code as u32;
        let name_bptr = rt.memory().read_u32(entry_addr + DOL_NAME_OFFSET);
        assert_eq!(read_bstr(rt.memory(), addr_from_bptr(name_bptr)), b"WORK");
    }
}
