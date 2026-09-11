//! `dos.library` `ExAll`/`ExAllEnd`: the batched directory scanner
//! `readdir()`-style callers use instead of `Examine`/`ExNext`.
//!
//! # Why this matters
//!
//! libnix's `readdir()` (and so most gcc-built programs that scan a
//! directory) is built directly on `ExAll()` -- with only the LVO table
//! entry and no handler, every directory looked empty to those
//! programs. Same motivation as amitools PR
//! <https://github.com/AmigaPorts/amitools/pull/8>, which added the
//! equivalent to vamos.
//!
//! # Call shape
//!
//! `ExAll(lock, buffer, size, type, control)` fills `buffer` with a
//! chain of `struct ExAllData` entries (linked through `ed_Next`, each
//! longword-aligned) for the directory `lock` is on, and returns a
//! "more to come" flag: non-zero means call again with the same
//! arguments, `0` means the scan ended -- successfully if `IoErr()` is
//! [`crate::doslock::ERROR_NO_MORE_ENTRIES`], otherwise with that
//! error. `control` is a `struct ExAllControl` that *must* come from
//! `AllocDosObject(DOS_EXALLCONTROL)` (see [`crate::dosfile`]'s
//! `AllocDosObject`); each call writes `eac_Entries` (how many entries
//! this call produced -- possibly `0` even mid-scan, when a pattern
//! filters everything in range out) and `eac_LastKey` (the resume
//! cookie, `0` before the first call and opaque to the caller after
//! that).
//!
//! `type` selects how much of `struct ExAllData` each entry carries:
//! `ED_NAME` (1) .. `ED_OWNER` (7), each level including all the
//! previous ones' fields ([`ED_DATA_SIZES`]). An out-of-range `type`
//! fails with [`crate::dosargs::ERROR_BAD_NUMBER`], matching the
//! autodoc's "sanity-checked" promise.
//!
//! # Scan state: `eac_LastKey` as an index
//!
//! Real filesystems store a handler-private disk key in `eac_LastKey`;
//! the only contract is that the caller zeroes it before the first
//! call and doesn't touch it between calls. Here it's the index of the
//! next entry to examine in the directory's *sorted* listing
//! ([`crate::doslock::sorted_dir_entries`] -- the same deterministic,
//! `.uaem`-sidecar-filtered listing `Examine`/`ExNext` iterate), which
//! is re-taken on every call rather than cached host-side: continuation
//! needs no registry entry to leak if a scan is abandoned, and the
//! sorted order makes the resumed listing stable. An entry that doesn't
//! fit in `buffer` is *not* consumed -- `eac_LastKey` stays put so the
//! next call re-reads it -- and a buffer too small for even one entry
//! fails with [`crate::dosfile::ERROR_NO_FREE_STORE`] rather than
//! looping forever.
//!
//! # `eac_MatchString` and `eac_MatchFunc`
//!
//! `eac_MatchString`, when non-`NULL`, points to a *tokenized* pattern
//! (the autodoc requires `ParsePatternNoCase`) and filters entries by
//! name, case-insensitively -- decoded with [`crate::dospattern`]'s
//! own `decode_from_mem`/`full_match`, i.e. exactly what
//! `MatchPatternNoCase` would do. A corrupt/undecodable buffer matches
//! nothing (same fail-closed posture as `MatchPattern` itself).
//! Entries a pattern rejects still advance `eac_LastKey` -- they're
//! consumed, just not stored. `eac_MatchFunc` (a guest `struct Hook`
//! called per entry) is not supported and is ignored: invoking guest
//! callbacks re-entrantly from inside a host handler is the same
//! reentrant-guest-code gap tracked for overlay support (issue #8),
//! and no corpus binary has needed it.
//!
//! # Entry contents
//!
//! Entries are built directly from the host listing plus each entry's
//! `.uaem` sidecar ([`crate::dosmeta`]) for protection/date/comment --
//! no scratch `FileInfoBlock` round trip -- with the same values
//! `Examine`/`ExNext` would report ([`crate::doslock`]'s `fill_fib`
//! defaults: sidecar values if present, else protection `0`, the
//! AmigaOS epoch, empty comment). `ed_OwnerUID`/`ed_OwnerGID` are
//! always `0` -- this runtime has no notion of multiuser ownership
//! (neither does `fill_fib`, whose `FileInfoBlock` owner fields stay
//! zeroed the same way).
//!
//! `ExAllEnd` terminates a scan early. With the scan state living
//! entirely in `eac_LastKey` there's nothing host-side to free, so it
//! just resets `eac_Entries`/`eac_LastKey` to `0`, leaving the control
//! block ready for a fresh scan -- same observable behavior as vamos's
//! implementation.

use crate::cpu::{Cpu, DataRegister};
use crate::dispatch::{DOS_LIBRARY_BASE, DispatchError, HandlerContext, LibraryTable};
use crate::dosargs::ERROR_BAD_NUMBER;
use crate::dosfile::{
    DosState, ERROR_INVALID_LOCK, ERROR_NO_FREE_STORE, ERROR_OBJECT_WRONG_TYPE, map_io_error,
};
use crate::doslock::{ENTRY_TYPE_DIR, ENTRY_TYPE_FILE, ERROR_NO_MORE_ENTRIES, sorted_dir_entries};
use crate::dosmeta;
use crate::dospattern;
use crate::guestmem::{addr_from_bptr, write_c_string};
use crate::lvos::dos::DOS_LVOS;
use crate::memory::AddressSpace;

// --- AmigaOS constants this module owns (dos/exall.h) ---

/// `ExAll`'s `type` argument: entries carry `ed_Name` only.
pub const ED_NAME: u32 = 1;
/// ... plus `ed_Type`.
pub const ED_TYPE: u32 = 2;
/// ... plus `ed_Size`.
pub const ED_SIZE: u32 = 3;
/// ... plus `ed_Prot`.
pub const ED_PROTECTION: u32 = 4;
/// ... plus `ed_Days`/`ed_Mins`/`ed_Ticks`.
pub const ED_DATE: u32 = 5;
/// ... plus `ed_Comment`.
pub const ED_COMMENT: u32 = 6;
/// ... plus `ed_OwnerUID`/`ed_OwnerGID`.
pub const ED_OWNER: u32 = 7;

/// Bytes of `struct ExAllData` a given `type` stores per entry (index
/// = the `ED_*` value; `ed_Next`+`ed_Name` = 8, each further level
/// adding its own fields' sizes, with `ed_OwnerUID`/`ed_OwnerGID` two
/// `UWORD`s in one longword). The entry's name (and comment, from
/// [`ED_COMMENT`] up) strings follow this fixed part.
const ED_DATA_SIZES: [u32; 8] = [0, 8, 12, 16, 20, 32, 36, 40];

// struct ExAllData field offsets.
const ED_NEXT_OFFSET: u32 = 0;
const ED_NAME_OFFSET: u32 = 4;
const ED_TYPE_OFFSET: u32 = 8;
const ED_SIZE_OFFSET: u32 = 12;
const ED_PROT_OFFSET: u32 = 16;
const ED_DAYS_OFFSET: u32 = 20;
const ED_MINS_OFFSET: u32 = 24;
const ED_TICKS_OFFSET: u32 = 28;
const ED_COMMENT_OFFSET: u32 = 32;
const ED_OWNERUID_OFFSET: u32 = 36;
const ED_OWNERGID_OFFSET: u32 = 38;

// struct ExAllControl field offsets.
const EAC_ENTRIES_OFFSET: u32 = 0;
const EAC_LASTKEY_OFFSET: u32 = 4;
const EAC_MATCHSTRING_OFFSET: u32 = 8;
#[allow(dead_code)] // documents the ignored eac_MatchFunc slot -- see the module docs
const EAC_MATCHFUNC_OFFSET: u32 = 12;
/// `sizeof(struct ExAllControl)`: the four longword fields above. Used
/// by [`crate::dosfile`]'s `AllocDosObject(DOS_EXALLCONTROL)`.
pub(crate) const EXALLCONTROL_SIZE: u32 = 16;

/// AmigaOS `BOOL` true/false, same convention as [`crate::doslock`].
const DOSTRUE: u32 = 0xFFFF_FFFF;
const DOSFALSE: u32 = 0;

impl DosState {
    /// `ExAll(lock, buffer, size, type, control)`: fills `buffer` with
    /// a chain of `ExAllData` entries starting at the listing index in
    /// `control`'s `eac_LastKey`, writing `eac_Entries`/`eac_LastKey`
    /// back. `Ok(true)` = more entries to come (`D0` = `DOSTRUE`),
    /// `Ok(false)` = the scan completed (`D0` = `0`, `IoErr()` =
    /// [`ERROR_NO_MORE_ENTRIES`] -- set by the handler), `Err(code)` =
    /// a real failure (`eac_Entries` already zeroed). See the module
    /// docs for the full contract.
    pub fn ex_all(
        &mut self,
        mem: &mut dyn AddressSpace,
        lock_addr: u32,
        buffer: u32,
        size: u32,
        data_type: u32,
        control: u32,
    ) -> Result<bool, i32> {
        // eac_Entries is (re)written every call; zero it first so every
        // early-error path leaves it consistent with "no entries".
        mem.write_u32(control + EAC_ENTRIES_OFFSET, 0);

        if !(ED_NAME..=ED_OWNER).contains(&data_type) {
            return Err(ERROR_BAD_NUMBER);
        }
        let entry = self.locks.get(&lock_addr).ok_or(ERROR_INVALID_LOCK)?;
        let host_path = entry.host_path.clone();
        if !host_path.is_dir() {
            return Err(ERROR_OBJECT_WRONG_TYPE);
        }

        let match_ptr = mem.read_u32(control + EAC_MATCHSTRING_OFFSET);
        let pattern = if match_ptr != 0 {
            // A corrupt/undecodable tokenized buffer decodes to None;
            // full_match against a None pattern below matches nothing
            // (fail closed, like MatchPattern itself).
            let mut addr = match_ptr;
            Some(dospattern::decode_from_mem(mem, &mut addr))
        } else {
            None
        };

        let names = sorted_dir_entries(&host_path)?;
        let fixed_size = ED_DATA_SIZES[data_type as usize];

        let mut index = mem.read_u32(control + EAC_LASTKEY_OFFSET) as usize;
        let mut entries: u32 = 0;
        // u64 so a hostile buffer/size pair can't wrap the end bound.
        let mut pos = buffer as u64;
        let end = buffer as u64 + size as u64;
        let mut prev_entry: Option<u32> = None;
        let mut more = false;

        while let Some(name) = names.get(index) {
            if let Some(node) = &pattern {
                let matched = node
                    .as_ref()
                    .is_some_and(|n| dospattern::full_match(n, name.as_bytes(), true));
                if !matched {
                    // Consumed (the resume cookie moves past it), just
                    // not stored.
                    index += 1;
                    continue;
                }
            }
            let entry_path = host_path.join(name);
            let meta = std::fs::metadata(&entry_path).map_err(|e| map_io_error(&e))?;
            let sidecar = dosmeta::read_sidecar(&entry_path).unwrap_or_default();
            let comment = sidecar.comment.as_deref().unwrap_or(b"");

            let mut need = fixed_size + name.len() as u32 + 1;
            if data_type >= ED_COMMENT {
                need += comment.len() as u32 + 1;
            }
            need = (need + 3) & !3; // next entry longword-aligned
            if pos + u64::from(need) > end {
                if entries == 0 {
                    // Not even one entry fits: a real error, and
                    // eac_LastKey stays untouched.
                    return Err(ERROR_NO_FREE_STORE);
                }
                // Doesn't fit this call: leave it un-consumed so the
                // next call re-reads it.
                more = true;
                break;
            }

            let ead = pos as u32;
            let is_dir = meta.is_dir();
            let size_val = if is_dir { 0 } else { meta.len() as u32 };
            let mut str_addr = ead + fixed_size;
            mem.write_u32(ead + ED_NEXT_OFFSET, 0);
            mem.write_u32(ead + ED_NAME_OFFSET, str_addr);
            write_c_string(mem, str_addr, name.as_bytes());
            str_addr += name.len() as u32 + 1;
            if data_type >= ED_TYPE {
                let entry_type = if is_dir {
                    ENTRY_TYPE_DIR
                } else {
                    ENTRY_TYPE_FILE
                };
                mem.write_u32(ead + ED_TYPE_OFFSET, entry_type as u32);
            }
            if data_type >= ED_SIZE {
                mem.write_u32(ead + ED_SIZE_OFFSET, size_val);
            }
            if data_type >= ED_PROTECTION {
                mem.write_u32(ead + ED_PROT_OFFSET, sidecar.prot);
            }
            if data_type >= ED_DATE {
                mem.write_u32(ead + ED_DAYS_OFFSET, sidecar.date.0 as u32);
                mem.write_u32(ead + ED_MINS_OFFSET, sidecar.date.1 as u32);
                mem.write_u32(ead + ED_TICKS_OFFSET, sidecar.date.2 as u32);
            }
            if data_type >= ED_COMMENT {
                mem.write_u32(ead + ED_COMMENT_OFFSET, str_addr);
                write_c_string(mem, str_addr, comment);
            }
            if data_type >= ED_OWNER {
                mem.write_u16(ead + ED_OWNERUID_OFFSET, 0);
                mem.write_u16(ead + ED_OWNERGID_OFFSET, 0);
            }
            if let Some(prev) = prev_entry {
                mem.write_u32(prev + ED_NEXT_OFFSET, ead);
            }
            prev_entry = Some(ead);
            pos += u64::from(need);
            entries += 1;
            index += 1;
        }

        mem.write_u32(control + EAC_ENTRIES_OFFSET, entries);
        mem.write_u32(control + EAC_LASTKEY_OFFSET, index as u32);
        Ok(more)
    }

    /// `ExAllEnd(lock, buffer, size, type, control)`: terminates a scan
    /// early. No host-side state exists to free (see the module docs),
    /// so this just resets `eac_Entries`/`eac_LastKey`, leaving the
    /// control block ready for a fresh scan.
    pub fn ex_all_end(&mut self, mem: &mut dyn AddressSpace, control: u32) {
        mem.write_u32(control + EAC_ENTRIES_OFFSET, 0);
        mem.write_u32(control + EAC_LASTKEY_OFFSET, 0);
    }
}

// --- LVO handlers ---

/// `ExAll` (`D1` = `BPTR` lock, `D2` = buffer, `D3` = buffer size,
/// `D4` = `ED_*` type, `D5` = `struct ExAllControl*`). `D0` =
/// non-zero if more entries are coming, `0` when the scan ends --
/// with `IoErr()` = [`ERROR_NO_MORE_ENTRIES`] on normal completion,
/// or the real error code otherwise.
fn ex_all_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let lock_addr = addr_from_bptr(ctx.cpu.data_register(DataRegister(1)));
    let buffer = ctx.cpu.data_register(DataRegister(2));
    let size = ctx.cpu.data_register(DataRegister(3));
    let data_type = ctx.cpu.data_register(DataRegister(4));
    let control = ctx.cpu.data_register(DataRegister(5));
    match ctx
        .dos
        .ex_all(ctx.mem, lock_addr, buffer, size, data_type, control)
    {
        Ok(true) => {
            ctx.dos.set_io_err(0);
            ctx.cpu.set_data_register(DataRegister(0), DOSTRUE);
        }
        Ok(false) => {
            ctx.dos.set_io_err(ERROR_NO_MORE_ENTRIES);
            ctx.cpu.set_data_register(DataRegister(0), DOSFALSE);
        }
        Err(code) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), DOSFALSE);
        }
    }
    Ok(())
}

/// `ExAllEnd` (same registers as `ExAll`; only `D5`'s control block is
/// consulted). No return value (real `ExAllEnd` is `void`).
fn ex_all_end_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let control = ctx.cpu.data_register(DataRegister(5));
    ctx.dos.ex_all_end(ctx.mem, control);
    Ok(())
}

/// Registers `ExAll`/`ExAllEnd` onto [`DOS_LIBRARY_BASE`], looked up by
/// name through [`DOS_LVOS`]. Called from
/// [`crate::dispatch::Runtime::new`] alongside the other `dos.library`
/// registrations; same no-`Vfs`-required posture (fails cleanly with
/// `IoErr()` set) as [`crate::doslock::register_lock_handlers`].
pub fn register_dosexall_handlers<C: Cpu + 'static>(
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
    reg!("ExAll", ex_all_handler::<C>);
    reg!("ExAllEnd", ex_all_end_handler::<C>);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{Runtime, StartConfig};
    use crate::doslock::SHARED_LOCK;
    use crate::guestmem::{GuestHeap, read_c_string};
    use crate::memory::FlatMemory;
    use crate::vfs::{Vfs, VfsConfig};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A unique temp directory, cleaned up on drop (same pattern as
    /// `doslock.rs`'s tests).
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let path = std::env::temp_dir().join(format!("volamos-dosexall-test-{tag}-{pid}-{n}"));
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
            ..Default::default()
        })
        .expect("build vfs")
    }

    /// Guest addresses the DosState-level tests place things at (all
    /// well clear of the test heap's [0x1000, 0x2000) range).
    const CONTROL_ADDR: u32 = 0x3000;
    const PATTERN_ADDR: u32 = 0x3800;
    const BUFFER_ADDR: u32 = 0x4000;

    /// A `(DosState, heap, mem)` triple with a directory lock on
    /// `amiga_path`, ready for `ex_all` calls.
    fn setup(root: &Path, amiga_path: &str) -> (DosState, GuestHeap, FlatMemory, u32) {
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let mut mem = FlatMemory::new(0x8000);
        let mut dos = DosState::new(Some(vfs_over(root)));
        let bptr = dos
            .lock(&mut heap, &mut mem, amiga_path, SHARED_LOCK)
            .expect("lock should succeed");
        let addr = addr_from_bptr(bptr);
        (dos, heap, mem, addr)
    }

    /// Reads the `ed_Name` strings off a chain of entries starting at
    /// `ead`, following `ed_Next` until `NULL`.
    fn chain_names(mem: &FlatMemory, mut ead: u32, expected_count: u32) -> Vec<Vec<u8>> {
        let mut names = Vec::new();
        for _ in 0..expected_count {
            assert_ne!(ead, 0, "chain shorter than eac_Entries claims");
            names.push(read_c_string(mem, mem.read_u32(ead + ED_NAME_OFFSET)));
            ead = mem.read_u32(ead + ED_NEXT_OFFSET);
        }
        assert_eq!(ead, 0, "last entry's ed_Next must be NULL");
        names
    }

    #[test]
    fn ed_name_scan_lists_all_entries_sorted_in_one_call() {
        let tmp = TempDir::new("edname");
        fs::write(tmp.path().join("b.txt"), b"abc").unwrap();
        fs::write(tmp.path().join("a.txt"), b"hello").unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        let (mut dos, _heap, mut mem, lock) = setup(tmp.path(), "SYS:");

        let more = dos
            .ex_all(&mut mem, lock, BUFFER_ADDR, 0x400, ED_NAME, CONTROL_ADDR)
            .expect("ex_all should succeed");
        assert!(!more, "everything fits: the scan is complete");
        assert_eq!(mem.read_u32(CONTROL_ADDR + EAC_ENTRIES_OFFSET), 3);
        assert_eq!(
            chain_names(&mem, BUFFER_ADDR, 3),
            vec![b"a.txt".to_vec(), b"b.txt".to_vec(), b"sub".to_vec()]
        );
    }

    #[test]
    fn ed_size_scan_fills_type_and_size() {
        let tmp = TempDir::new("edsize");
        fs::write(tmp.path().join("f.txt"), b"hello").unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        let (mut dos, _heap, mut mem, lock) = setup(tmp.path(), "SYS:");

        let more = dos
            .ex_all(&mut mem, lock, BUFFER_ADDR, 0x400, ED_SIZE, CONTROL_ADDR)
            .expect("ex_all should succeed");
        assert!(!more);
        // Sorted order: f.txt then sub.
        let file_ead = BUFFER_ADDR;
        assert_eq!(
            read_c_string(&mem, mem.read_u32(file_ead + ED_NAME_OFFSET)),
            b"f.txt"
        );
        assert_eq!(
            mem.read_u32(file_ead + ED_TYPE_OFFSET) as i32,
            ENTRY_TYPE_FILE
        );
        assert_eq!(mem.read_u32(file_ead + ED_SIZE_OFFSET), 5);
        let dir_ead = mem.read_u32(file_ead + ED_NEXT_OFFSET);
        assert_eq!(
            read_c_string(&mem, mem.read_u32(dir_ead + ED_NAME_OFFSET)),
            b"sub"
        );
        assert_eq!(
            mem.read_u32(dir_ead + ED_TYPE_OFFSET) as i32,
            ENTRY_TYPE_DIR
        );
        assert_eq!(mem.read_u32(dir_ead + ED_SIZE_OFFSET), 0);
    }

    #[test]
    fn small_buffer_continues_across_calls_via_eac_lastkey() {
        let tmp = TempDir::new("continue");
        for name in ["a.txt", "b.txt", "c.txt"] {
            fs::write(tmp.path().join(name), b"x").unwrap();
        }
        let (mut dos, _heap, mut mem, lock) = setup(tmp.path(), "SYS:");

        // ED_SIZE fixed part 16 + "a.txt\0" = 22 -> 24 aligned; a
        // 32-byte buffer holds exactly one entry per call.
        let mut calls = 0;
        let mut names = Vec::new();
        loop {
            let more = dos
                .ex_all(&mut mem, lock, BUFFER_ADDR, 32, ED_SIZE, CONTROL_ADDR)
                .expect("ex_all should succeed");
            calls += 1;
            let entries = mem.read_u32(CONTROL_ADDR + EAC_ENTRIES_OFFSET);
            names.extend(chain_names(&mem, BUFFER_ADDR, entries));
            if !more {
                break;
            }
            assert!(calls < 10, "scan must terminate");
        }
        assert!(calls >= 2, "the scan should have needed a continuation");
        assert_eq!(
            names,
            vec![b"a.txt".to_vec(), b"b.txt".to_vec(), b"c.txt".to_vec()]
        );
    }

    #[test]
    fn buffer_too_small_for_one_entry_is_no_free_store() {
        let tmp = TempDir::new("toosmall");
        fs::write(tmp.path().join("a.txt"), b"x").unwrap();
        let (mut dos, _heap, mut mem, lock) = setup(tmp.path(), "SYS:");

        let err = dos
            .ex_all(&mut mem, lock, BUFFER_ADDR, 8, ED_SIZE, CONTROL_ADDR)
            .unwrap_err();
        assert_eq!(err, ERROR_NO_FREE_STORE);
        assert_eq!(mem.read_u32(CONTROL_ADDR + EAC_ENTRIES_OFFSET), 0);
        assert_eq!(
            mem.read_u32(CONTROL_ADDR + EAC_LASTKEY_OFFSET),
            0,
            "the failed entry must stay un-consumed"
        );
    }

    #[test]
    fn out_of_range_type_is_bad_number() {
        let tmp = TempDir::new("badtype");
        let (mut dos, _heap, mut mem, lock) = setup(tmp.path(), "SYS:");
        for bad in [0, ED_OWNER + 1] {
            let err = dos
                .ex_all(&mut mem, lock, BUFFER_ADDR, 0x400, bad, CONTROL_ADDR)
                .unwrap_err();
            assert_eq!(err, ERROR_BAD_NUMBER);
        }
    }

    #[test]
    fn file_lock_is_object_wrong_type() {
        let tmp = TempDir::new("filelock");
        fs::write(tmp.path().join("f.txt"), b"x").unwrap();
        let (mut dos, _heap, mut mem, lock) = setup(tmp.path(), "SYS:f.txt");
        let err = dos
            .ex_all(&mut mem, lock, BUFFER_ADDR, 0x400, ED_NAME, CONTROL_ADDR)
            .unwrap_err();
        assert_eq!(err, ERROR_OBJECT_WRONG_TYPE);
    }

    #[test]
    fn unknown_lock_is_invalid_lock() {
        let tmp = TempDir::new("nolock");
        let (mut dos, _heap, mut mem, _lock) = setup(tmp.path(), "SYS:");
        let err = dos
            .ex_all(&mut mem, 0xDEAD, BUFFER_ADDR, 0x400, ED_NAME, CONTROL_ADDR)
            .unwrap_err();
        assert_eq!(err, ERROR_INVALID_LOCK);
    }

    #[test]
    fn eac_match_string_filters_names_case_insensitively() {
        let tmp = TempDir::new("matchstring");
        fs::write(tmp.path().join("a.txt"), b"x").unwrap();
        fs::write(tmp.path().join("b.doc"), b"x").unwrap();
        fs::write(tmp.path().join("C.TXT"), b"x").unwrap();
        let (mut dos, _heap, mut mem, lock) = setup(tmp.path(), "SYS:");

        // Write a tokenized "#?.txt" where eac_MatchString points, the
        // same encoding ParsePatternNoCase would produce.
        let (node, _) = dospattern::parse(b"#?.txt").expect("parse pattern");
        let mut bytes = Vec::new();
        dospattern::encode(&node, &mut bytes);
        bytes.push(0);
        for (i, &b) in bytes.iter().enumerate() {
            mem.write_u8(PATTERN_ADDR + i as u32, b);
        }
        mem.write_u32(CONTROL_ADDR + EAC_MATCHSTRING_OFFSET, PATTERN_ADDR);

        let more = dos
            .ex_all(&mut mem, lock, BUFFER_ADDR, 0x400, ED_NAME, CONTROL_ADDR)
            .expect("ex_all should succeed");
        assert!(!more);
        let entries = mem.read_u32(CONTROL_ADDR + EAC_ENTRIES_OFFSET);
        assert_eq!(
            chain_names(&mem, BUFFER_ADDR, entries),
            vec![b"C.TXT".to_vec(), b"a.txt".to_vec()],
            "b.doc filtered out; matching is case-insensitive"
        );
    }

    #[test]
    fn ed_comment_reads_protection_date_and_comment_from_the_uaem_sidecar() {
        let tmp = TempDir::new("sidecar");
        fs::write(tmp.path().join("f.txt"), b"hello").unwrap();
        dosmeta::write_sidecar(
            &tmp.path().join("f.txt"),
            &dosmeta::Meta {
                prot: 0x11,
                date: (1000, 720, 50),
                comment: Some(b"a note".to_vec()),
            },
        )
        .unwrap();
        let (mut dos, _heap, mut mem, lock) = setup(tmp.path(), "SYS:");

        let more = dos
            .ex_all(&mut mem, lock, BUFFER_ADDR, 0x400, ED_COMMENT, CONTROL_ADDR)
            .expect("ex_all should succeed");
        assert!(!more);
        let ead = BUFFER_ADDR;
        assert_eq!(mem.read_u32(ead + ED_PROT_OFFSET), 0x11);
        assert_eq!(mem.read_u32(ead + ED_DAYS_OFFSET), 1000);
        assert_eq!(mem.read_u32(ead + ED_MINS_OFFSET), 720);
        assert_eq!(mem.read_u32(ead + ED_TICKS_OFFSET), 50);
        assert_eq!(
            read_c_string(&mem, mem.read_u32(ead + ED_COMMENT_OFFSET)),
            b"a note"
        );
    }

    #[test]
    fn uaem_sidecars_never_appear_as_entries() {
        let tmp = TempDir::new("sidecar-hidden");
        fs::write(tmp.path().join("f.txt"), b"hello").unwrap();
        dosmeta::write_sidecar(
            &tmp.path().join("f.txt"),
            &dosmeta::Meta {
                prot: 0,
                date: (0, 0, 0),
                comment: Some(b"x".to_vec()),
            },
        )
        .unwrap();
        let (mut dos, _heap, mut mem, lock) = setup(tmp.path(), "SYS:");

        dos.ex_all(&mut mem, lock, BUFFER_ADDR, 0x400, ED_NAME, CONTROL_ADDR)
            .expect("ex_all should succeed");
        assert_eq!(mem.read_u32(CONTROL_ADDR + EAC_ENTRIES_OFFSET), 1);
        assert_eq!(chain_names(&mem, BUFFER_ADDR, 1), vec![b"f.txt".to_vec()]);
    }

    #[test]
    fn ex_all_end_resets_the_control_block() {
        let tmp = TempDir::new("exallend");
        for name in ["a.txt", "b.txt"] {
            fs::write(tmp.path().join(name), b"x").unwrap();
        }
        let (mut dos, _heap, mut mem, lock) = setup(tmp.path(), "SYS:");

        // One small-buffer call leaves the scan mid-way ...
        let more = dos
            .ex_all(&mut mem, lock, BUFFER_ADDR, 32, ED_SIZE, CONTROL_ADDR)
            .expect("ex_all should succeed");
        assert!(more);
        assert_ne!(mem.read_u32(CONTROL_ADDR + EAC_LASTKEY_OFFSET), 0);

        // ... ExAllEnd abandons it, ready for a fresh scan.
        dos.ex_all_end(&mut mem, CONTROL_ADDR);
        assert_eq!(mem.read_u32(CONTROL_ADDR + EAC_ENTRIES_OFFSET), 0);
        assert_eq!(mem.read_u32(CONTROL_ADDR + EAC_LASTKEY_OFFSET), 0);
    }

    // --- End-to-end tests through Runtime, mirroring doslock.rs's style ---

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
    fn end_to_end_ex_all_via_alloc_dos_object_and_trap_dispatch() {
        let tmp = TempDir::new("e2e-exall");
        fs::write(tmp.path().join("a.txt"), b"hello").unwrap();
        fs::write(tmp.path().join("b.txt"), b"hi").unwrap();
        let name = b"SYS:\0\0\0\0"; // padded so the buffer after it is aligned

        // Lock("SYS:"), AllocDosObject(DOS_EXALLCONTROL, NULL) for the
        // control block, one big-buffer ExAll (completes in one call),
        // then IoErr() as the exit code: ERROR_NO_MORE_ENTRIES.
        let mut words = Vec::new();
        let name_idx = words.len();
        words.push(move_imm_to_d(1)); // D1 = "SYS:" (patched)
        words.push(0);
        words.push(0);
        push_move_imm_to_d(&mut words, 2, SHARED_LOCK as u32);
        push_jsr(&mut words, 6, -84); // Lock(a6): D0 = BPTR
        words.push(0x2600); // move.l d0,d3 (save the lock)

        words.push(0x7201); // moveq #1,d1 (DOS_EXALLCONTROL)
        words.push(0x7400); // moveq #0,d2 (no tags)
        push_jsr(&mut words, 6, -228); // AllocDosObject(a6): D0 = eac
        words.push(0x2A00); // move.l d0,d5 (D5 = control for ExAll)

        words.push(0x2203); // move.l d3,d1 (D1 = the lock)
        let buf_idx = push_move_imm_to_d(&mut words, 2, 0); // D2 = buffer (patched)
        push_move_imm_to_d(&mut words, 3, 0x100); // D3 = buffer size
        push_move_imm_to_d(&mut words, 4, ED_SIZE); // D4 = type
        push_jsr(&mut words, 6, -432); // ExAll(a6): D0 = 0 (scan done)
        push_jsr(&mut words, 6, -132); // IoErr(a6)
        words.push(RTS);

        let name_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        let buf_addr = name_addr + name.len() as u32;
        patch_imm32(&mut words, name_idx, name_addr);
        patch_imm32(&mut words, buf_idx, buf_addr);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &words);
        for (i, &b) in name.iter().enumerate() {
            mem.write_u8(name_addr + i as u32, b);
        }
        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: entry + 0x400,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        rt.set_vfs(vfs_over(tmp.path()));
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, ERROR_NO_MORE_ENTRIES, "a completed scan's IoErr()");
        // The buffer really holds the chained entries.
        let mem = rt.memory();
        assert_eq!(
            read_c_string(mem, mem.read_u32(buf_addr + ED_NAME_OFFSET)),
            b"a.txt"
        );
        assert_eq!(mem.read_u32(buf_addr + ED_SIZE_OFFSET), 5);
        let second = mem.read_u32(buf_addr + ED_NEXT_OFFSET);
        assert_ne!(second, 0);
        assert_eq!(
            read_c_string(mem, mem.read_u32(second + ED_NAME_OFFSET)),
            b"b.txt"
        );
        assert_eq!(mem.read_u32(second + ED_NEXT_OFFSET), 0);
    }
}
