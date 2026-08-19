//! `dos.library` locks (`Lock`/`UnLock`/`DupLock`/`ParentDir`/
//! `CurrentDir`) and directory traversal (`Examine`/`ExNext`).
//!
//! # Lock model
//!
//! A lock is, on the guest side, a 20-byte `struct FileLock` allocated on
//! the guest heap (real AmigaOS layout: `fl_Link` `BPTR` at offset 0,
//! `fl_Key` `LONG` at offset 4, `fl_Access` `LONG` at offset 8, `fl_Task`
//! at offset 12, `fl_Volume` at offset 16 -- the last two are always
//! zeroed here, since nothing in this runtime inspects them and this
//! isn't a real multi-process AmigaOS). `Lock()` writes the requested
//! access mode into `fl_Access` and a debug id into `fl_Key`, purely so a
//! guest- or host-side debugger inspecting the struct sees something
//! meaningful there -- exactly [`crate::dosfile`]'s `FileHandle`/
//! `fh_Arg1` pattern, reused verbatim.
//!
//! The struct's guest *address* keys a host-side registry entry
//! ([`LockEntry`], in [`crate::dosfile::DosState::locks`]) holding the
//! resolved host path, the *normalized Amiga path* that reached it (see
//! below), and the requested access mode. There's no real shared/
//! exclusive enforcement -- this runtime is single-process -- so `Lock`
//! always succeeds regardless of what other locks exist on the same
//! object; the access mode is recorded for debuggability only.
//!
//! # `CurrentDir` and Amiga-path reconstruction
//!
//! A lock's host path alone can't drive `CurrentDir`: [`crate::vfs::Vfs`]'s
//! current directory is an *Amiga* path string, and a host [`std::path::
//! PathBuf`] can't be turned back into one in general (assigns and
//! auto-assign mean several Amiga paths -- or none -- can map to the same
//! host directory). So every lock also remembers the normalized Amiga
//! path that produced it, via [`crate::vfs::Vfs::resolve_with_amiga_path`]
//! (a small T11 addition to `vfs.rs`: it's [`crate::vfs::Vfs::resolve`]
//! plus reconstructing `leading_name:comp/comp/...` from the same
//! component-matching walk, so the case matches what's actually on disk).
//!
//! `CurrentDir(lock)` looks up `lock`'s `amiga_path`, calls
//! [`crate::vfs::Vfs::set_cwd`] with it, and returns the *previous*
//! current-dir lock's `BPTR` (`0` if the process was still on its
//! initial, no-lock, current directory). `CurrentDir(0)` restores that
//! initial current directory: [`crate::dosfile::DosState::initial_cwd`]
//! caches `Vfs::cwd()` the first time any `CurrentDir` call runs (before
//! it's ever changed), specifically so `0` has something to restore to.
//!
//! # `ParentDir` and the volume-root clamp
//!
//! `ParentDir` pops the last component off a lock's normalized Amiga
//! path and re-resolves. When there's no component left to pop (the lock
//! is already a volume root), real `ParentDir` returns `0` in `D0` *with
//! `IoErr()` left at `0`* -- not an error, just "no parent" -- and this
//! implementation matches that rather than setting an error code.
//!
//! # `Examine`/`ExNext`
//!
//! `Examine` fills a guest `struct FileInfoBlock` (`fib`, 260 bytes,
//! allocated by the *guest program*, not by us -- see [`fill_fib`] for
//! the exact offsets) from a lock's own target (file or directory; for a
//! directory this includes a volume root, whose `fib_FileName` is the
//! volume name). If the lock is a directory, `Examine` also
//! (re)initializes ExNext iterator state for it: a `Vec` of the
//! directory's entry names, sorted byte-wise for deterministic (parity-
//! run-stable) enumeration order, plus a cursor -- exactly the real
//! "`Examine` then repeated `ExNext` until failure" protocol. `ExNext`
//! advances that cursor, filling `fib` from each entry in turn; when
//! exhausted it returns `DOSFALSE` with `IoErr()` =
//! [`ERROR_NO_MORE_ENTRIES`]. `Examine`/`ExNext` on a *file* lock: `Examine`
//! still fills `fib` (from the file itself), but `ExNext` fails with
//! [`crate::dosfile::ERROR_OBJECT_WRONG_TYPE`] -- there's nothing to
//! enumerate.
//!
//! `fib_Date` is filled with a fixed `DateStamp` (`ds_Days`/`ds_Minute`/
//! `ds_Tick` all `0`, i.e. the AmigaOS epoch) rather than any host mtime.
//! Phase 4's parity harness freezes/normalizes virtual time entirely
//! (see `docs/plan.md`), so real timestamps would just be a source of
//! non-determinism between runs and runners this side of that harness;
//! a fixed epoch is the simplest thing that's already parity-ready.

use std::path::PathBuf;

use crate::cpu::{Cpu, DataRegister};
use crate::dispatch::{DOS_LIBRARY_BASE, DispatchError, HandlerContext, LibraryTable};
use crate::dosfile::{
    DosState, ERROR_INVALID_LOCK, ERROR_OBJECT_WRONG_TYPE, map_io_error, map_vfs_error,
};
use crate::guestmem::{GuestHeap, addr_from_bptr, bptr_from_addr, read_c_string, write_c_string};
use crate::lvos::dos::DOS_LVOS;
use crate::memory::AddressSpace;
use crate::vfs::ResolveMode;

// --- AmigaOS constants this module owns ---

/// `Lock`'s `accessMode` argument: shared access (the common case; most
/// callers use this).
pub const SHARED_LOCK: i32 = -2;
/// `Lock`'s `accessMode` argument: exclusive access.
pub const EXCLUSIVE_LOCK: i32 = -1;

/// `SameLock`'s return codes (`dos/dos.h`).
pub const LOCK_DIFFERENT: i32 = -1;
pub const LOCK_SAME: i32 = 0;
pub const LOCK_SAME_VOLUME: i32 = 1;
/// Alias for [`SHARED_LOCK`] used by some callers/headers.
pub const ACCESS_READ: i32 = SHARED_LOCK;
/// Alias for [`EXCLUSIVE_LOCK`] used by some callers/headers.
pub const ACCESS_WRITE: i32 = EXCLUSIVE_LOCK;

/// `ExNext`'s "no more entries" `IoErr()` code.
pub const ERROR_NO_MORE_ENTRIES: i32 = 232;

/// `Close`/boolean-success convention: AmigaOS `BOOL` true is `-1`.
const DOSTRUE: u32 = 0xFFFF_FFFF;
/// `Close`/boolean-failure convention.
const DOSFALSE: u32 = 0;

/// `sizeof(struct FileLock)`: 20 bytes (`fl_Link`, `fl_Key`, `fl_Access`,
/// `fl_Task`, `fl_Volume`, each a 4-byte field). See the module docs'
/// "Lock model" section.
const FILE_LOCK_SIZE: u32 = 20;
/// Byte offset of `fl_Key` within `struct FileLock`.
const FL_KEY_OFFSET: u32 = 4;
/// Byte offset of `fl_Access` within `struct FileLock`.
const FL_ACCESS_OFFSET: u32 = 8;

/// `sizeof(struct FileInfoBlock)`: 260 bytes. Allocated by the *guest
/// program*, never by this runtime -- see [`fill_fib`].
pub(crate) const FIB_SIZE: u32 = 260;
const FIB_DISKKEY_OFFSET: u32 = 0;
const FIB_DIRENTRYTYPE_OFFSET: u32 = 4;
const FIB_FILENAME_OFFSET: u32 = 8;
/// `fib_FileName` is a `NUL`-terminated `TEXT[108]` (NDK `dos/dos.h`) --
/// 107 usable characters plus the terminator.
const FIB_FILENAME_MAX_CHARS: usize = 107;
const FIB_PROTECTION_OFFSET: u32 = 116;
const FIB_ENTRYTYPE_OFFSET: u32 = 120;
const FIB_SIZE_OFFSET: u32 = 124;
const FIB_NUMBLOCKS_OFFSET: u32 = 128;
const FIB_DATE_OFFSET: u32 = 132;
const FIB_COMMENT_OFFSET: u32 = 144;

/// `fib_DirEntryType`/`fib_EntryType` for a directory (positive; the
/// exact positive value is only ever compared `> 0` by real callers, `2`
/// matches what real AmigaOS filesystems commonly report for a
/// subdirectory).
const ENTRY_TYPE_DIR: i32 = 2;
/// `fib_DirEntryType`/`fib_EntryType` for a plain file.
const ENTRY_TYPE_FILE: i32 = -3;

/// Bytes per block used to compute `fib_NumBlocks` from a file's size
/// (`ceil(size / BLOCK_SIZE)`); AmigaOS filesystems commonly use 512-byte
/// blocks, and nothing about this runtime depends on it matching a real
/// filesystem's actual block size -- it's cosmetic.
const BLOCK_SIZE: u32 = 512;

/// One `Lock`'s host-side registry entry.
#[derive(Debug, Clone)]
pub struct LockEntry {
    /// The resolved host path.
    pub host_path: PathBuf,
    /// The normalized Amiga path that reached it (see the module docs'
    /// "`CurrentDir` and Amiga-path reconstruction" section).
    pub amiga_path: String,
    /// The access mode requested at `Lock` time ([`SHARED_LOCK`] or
    /// [`EXCLUSIVE_LOCK`]) -- recorded for debuggability only, since this
    /// runtime doesn't enforce real exclusivity (single process).
    pub access: i32,
}

/// Per-lock `ExNext` iterator state, created by `Examine` on a directory
/// lock.
#[derive(Debug, Clone)]
pub struct ExNextState {
    /// Directory entry names, sorted byte-wise for deterministic
    /// enumeration order.
    entries: Vec<String>,
    /// Index of the next entry `ExNext` will return.
    next_index: usize,
}

impl DosState {
    /// Allocates a fresh `struct FileLock` on the guest heap for
    /// `(host_path, amiga_path)` at `access`, registers it in
    /// `self.locks`, and returns its `BPTR`.
    fn new_lock(
        &mut self,
        heap: &mut GuestHeap,
        mem: &mut dyn AddressSpace,
        host_path: PathBuf,
        amiga_path: String,
        access: i32,
    ) -> Result<u32, i32> {
        self.next_lock_id = self.next_lock_id.wrapping_add(1);
        let key = self.next_lock_id;
        let addr = alloc_lock_struct(heap, mem, key, access)
            .map_err(|_| crate::dosfile::ERROR_NO_FREE_STORE)?;
        self.locks.insert(
            addr,
            LockEntry {
                host_path,
                amiga_path,
                access,
            },
        );
        Ok(bptr_from_addr(addr))
    }

    /// `Lock(name, access_mode)`: resolves `name` (must already exist --
    /// works on files and directories alike) and creates a lock on it.
    pub fn lock(
        &mut self,
        heap: &mut GuestHeap,
        mem: &mut dyn AddressSpace,
        name: &str,
        access_mode: i32,
    ) -> Result<u32, i32> {
        let vfs = self
            .vfs
            .as_ref()
            .ok_or(crate::dosfile::ERROR_OBJECT_NOT_FOUND)?;
        let resolved = vfs
            .resolve_with_amiga_path(name, ResolveMode::MustExist)
            .map_err(|e| map_vfs_error(&e))?;
        self.new_lock(
            heap,
            mem,
            resolved.host_path,
            resolved.amiga_path,
            access_mode,
        )
    }

    /// `CreateDir(name)`: creates a new, empty directory -- its parent
    /// must already exist; this doesn't create intermediate directories
    /// -- and returns an [`EXCLUSIVE_LOCK`] on it. Fails with
    /// [`crate::dosfile::ERROR_OBJECT_EXISTS`] if a file or directory of
    /// that name already exists.
    pub fn create_dir(
        &mut self,
        heap: &mut GuestHeap,
        mem: &mut dyn AddressSpace,
        name: &str,
    ) -> Result<u32, i32> {
        let vfs = self
            .vfs
            .as_ref()
            .ok_or(crate::dosfile::ERROR_OBJECT_NOT_FOUND)?;
        let resolved = vfs
            .resolve_with_amiga_path(name, ResolveMode::ParentMustExist)
            .map_err(|e| map_vfs_error(&e))?;
        if resolved.host_path.exists() {
            return Err(crate::dosfile::ERROR_OBJECT_EXISTS);
        }
        std::fs::create_dir(&resolved.host_path).map_err(|e| map_io_error(&e))?;
        self.new_lock(
            heap,
            mem,
            resolved.host_path,
            resolved.amiga_path,
            EXCLUSIVE_LOCK,
        )
    }

    /// `SameLock(lock1, lock2)`: [`LOCK_SAME`]/[`LOCK_SAME_VOLUME`]/
    /// [`LOCK_DIFFERENT`]. Identical `BPTR`s (including two `ZERO`
    /// locks) are always [`LOCK_SAME`]; otherwise this compares the
    /// locks' resolved, canonicalized host paths for [`LOCK_SAME`], and
    /// falls back to comparing their Amiga volume names for
    /// [`LOCK_SAME_VOLUME`]. An unknown (already-freed, or never a
    /// lock) address that isn't `ZERO` -- or a `ZERO` lock compared
    /// against a non-`ZERO` one -- is [`LOCK_DIFFERENT`], matching the
    /// RKRM's own "does not identify the ZERO lock as identical with a
    /// lock on the root of the boot volume" caveat.
    pub fn same_lock(&self, addr1: u32, addr2: u32) -> i32 {
        if addr1 == addr2 {
            return LOCK_SAME;
        }
        let (Some(e1), Some(e2)) = (self.locks.get(&addr1), self.locks.get(&addr2)) else {
            return LOCK_DIFFERENT;
        };
        let c1 = e1
            .host_path
            .canonicalize()
            .unwrap_or_else(|_| e1.host_path.clone());
        let c2 = e2
            .host_path
            .canonicalize()
            .unwrap_or_else(|_| e2.host_path.clone());
        if c1 == c2 {
            return LOCK_SAME;
        }
        let vol1 = e1.amiga_path.split(':').next().unwrap_or("");
        let vol2 = e2.amiga_path.split(':').next().unwrap_or("");
        if vol1.eq_ignore_ascii_case(vol2) {
            LOCK_SAME_VOLUME
        } else {
            LOCK_DIFFERENT
        }
    }

    /// `UnLock(lock)`: frees the guest struct, the registry entry, and
    /// any `ExNext` iterator state for it. `addr == 0` (i.e. `UnLock(0)`)
    /// is a legal no-op, matching real `UnLock(BPTR NULL)`. Unknown
    /// (already-freed, or never a lock) addresses are also a silent
    /// no-op -- real `UnLock` returns nothing (`void`), so there's no
    /// `IoErr()`-reporting convention to follow here either way.
    pub fn unlock(&mut self, heap: &mut GuestHeap, addr: u32) {
        if addr == 0 {
            return;
        }
        self.exnext.remove(&addr);
        if self.locks.remove(&addr).is_some() {
            let _ = heap.free(addr);
        }
        if self.current_dir_lock == Some(addr) {
            self.current_dir_lock = None;
        }
    }

    /// `DupLock(lock)`: a new [`SHARED_LOCK`] on the same path.
    /// `DupLock(0) == 0` (also a legal no-op / not-an-error case).
    /// Fails with [`ERROR_INVALID_LOCK`] if `addr` isn't a currently-open
    /// lock.
    pub fn dup_lock(
        &mut self,
        heap: &mut GuestHeap,
        mem: &mut dyn AddressSpace,
        addr: u32,
    ) -> Result<u32, i32> {
        if addr == 0 {
            return Ok(0);
        }
        let entry = self.locks.get(&addr).ok_or(ERROR_INVALID_LOCK)?.clone();
        self.new_lock(heap, mem, entry.host_path, entry.amiga_path, SHARED_LOCK)
    }

    /// `NameFromLock(lock)`: the absolute Amiga path that produced
    /// `lock` (`lock == 0` resolves to the literal string `"SYS:"`,
    /// matching the RKRM's own documented quirk for the `ZERO` lock --
    /// see the module docs). Fails with [`ERROR_INVALID_LOCK`] if
    /// `addr != 0` isn't a currently-open lock.
    pub fn name_from_lock(&self, addr: u32) -> Result<String, i32> {
        if addr == 0 {
            return Ok("SYS:".to_string());
        }
        Ok(self
            .locks
            .get(&addr)
            .ok_or(ERROR_INVALID_LOCK)?
            .amiga_path
            .clone())
    }

    /// `ParentDir(lock)`: a [`SHARED_LOCK`] on the parent directory of
    /// `lock`'s target (`lock == 0` means the current directory). Returns
    /// `Ok(0)` (not an error -- `IoErr()` stays `0`) when `lock` is
    /// already at a volume root, matching real `ParentDir`'s "no parent"
    /// convention. Fails with [`ERROR_INVALID_LOCK`] if `addr != 0` isn't
    /// a currently-open lock.
    pub fn parent_dir(
        &mut self,
        heap: &mut GuestHeap,
        mem: &mut dyn AddressSpace,
        addr: u32,
    ) -> Result<u32, i32> {
        let vfs = self
            .vfs
            .as_ref()
            .ok_or(crate::dosfile::ERROR_OBJECT_NOT_FOUND)?;
        let amiga_path = if addr == 0 {
            vfs.cwd().to_string()
        } else {
            self.locks
                .get(&addr)
                .ok_or(ERROR_INVALID_LOCK)?
                .amiga_path
                .clone()
        };

        let Some((vol, rest)) = amiga_path.split_once(':') else {
            return Err(crate::dosfile::ERROR_INVALID_COMPONENT_NAME);
        };
        let mut comps: Vec<&str> = rest.split('/').filter(|c| !c.is_empty()).collect();
        if comps.is_empty() {
            // Already at a volume root: "no parent", not an error.
            return Ok(0);
        }
        comps.pop();
        let parent_path = format!("{vol}:{}", comps.join("/"));

        let vfs = self.vfs.as_ref().expect("checked above");
        let resolved = vfs
            .resolve_with_amiga_path(&parent_path, ResolveMode::MustExist)
            .map_err(|e| map_vfs_error(&e))?;
        self.new_lock(
            heap,
            mem,
            resolved.host_path,
            resolved.amiga_path,
            SHARED_LOCK,
        )
    }

    /// `CurrentDir(lock)`: sets the `Vfs` current directory to `lock`'s
    /// path (`lock == 0` restores the initial current directory -- see
    /// the module docs), returning the *previous* current-dir lock's
    /// `BPTR` (`0` if there wasn't one yet). Fails with
    /// [`ERROR_INVALID_LOCK`] if `addr != 0` isn't a currently-open lock,
    /// or if it's a lock on a file rather than a directory
    /// ([`ERROR_OBJECT_WRONG_TYPE`]).
    pub fn current_dir(&mut self, addr: u32) -> Result<u32, i32> {
        if self.vfs.is_none() {
            return Err(crate::dosfile::ERROR_OBJECT_NOT_FOUND);
        }
        if self.initial_cwd.is_none() {
            self.initial_cwd = Some(self.vfs.as_ref().expect("checked above").cwd().to_string());
        }

        let new_path = if addr == 0 {
            self.initial_cwd.clone().expect("just set above")
        } else {
            let entry = self.locks.get(&addr).ok_or(ERROR_INVALID_LOCK)?;
            if !entry.host_path.is_dir() {
                return Err(ERROR_OBJECT_WRONG_TYPE);
            }
            entry.amiga_path.clone()
        };

        let vfs = self.vfs.as_mut().expect("checked above");
        vfs.set_cwd(&new_path).map_err(|e| map_vfs_error(&e))?;

        let old = self.current_dir_lock.map(bptr_from_addr).unwrap_or(0);
        self.current_dir_lock = if addr == 0 { None } else { Some(addr) };
        Ok(old)
    }

    /// `Examine(lock, fib)`: fills `fib` (a guest `struct FileInfoBlock`
    /// at `fib_addr`) from `addr`'s own target. If it's a directory, also
    /// (re)initializes `ExNext` iterator state for it. Returns `Ok(())`
    /// on success, or an `IoErr()` code (`ERROR_INVALID_LOCK` for an
    /// unknown lock; a mapped [`std::io::Error`] if `read_dir`/`metadata`
    /// fails on the host).
    pub fn examine(
        &mut self,
        mem: &mut dyn AddressSpace,
        addr: u32,
        fib_addr: u32,
    ) -> Result<(), i32> {
        let entry = self.locks.get(&addr).ok_or(ERROR_INVALID_LOCK)?;
        let host_path = entry.host_path.clone();
        let display_name = own_display_name(&entry.amiga_path);

        let meta = std::fs::metadata(&host_path).map_err(|e| map_io_error(&e))?;
        let is_dir = meta.is_dir();
        let size = if is_dir { 0 } else { meta.len() as u32 };
        fill_fib(mem, fib_addr, &display_name, is_dir, size);

        if is_dir {
            let mut names = Vec::new();
            for dirent in std::fs::read_dir(&host_path).map_err(|e| map_io_error(&e))? {
                let dirent = dirent.map_err(|e| map_io_error(&e))?;
                names.push(dirent.file_name().to_string_lossy().into_owned());
            }
            names.sort();
            self.exnext.insert(
                addr,
                ExNextState {
                    entries: names,
                    next_index: 0,
                },
            );
        } else {
            self.exnext.remove(&addr);
        }
        Ok(())
    }

    /// `ExNext(lock, fib)`: fills `fib` from the next directory entry (in
    /// sorted order, established by the preceding `Examine`), or fails
    /// with [`ERROR_NO_MORE_ENTRIES`] once exhausted (or if `Examine`
    /// was never called on this lock -- there's nothing to iterate).
    /// Fails with [`ERROR_OBJECT_WRONG_TYPE`] if `addr`'s lock is on a
    /// file rather than a directory, and [`ERROR_INVALID_LOCK`] if it
    /// isn't a currently-open lock at all.
    pub fn ex_next(
        &mut self,
        mem: &mut dyn AddressSpace,
        addr: u32,
        fib_addr: u32,
    ) -> Result<(), i32> {
        let entry = self.locks.get(&addr).ok_or(ERROR_INVALID_LOCK)?;
        if !entry.host_path.is_dir() {
            return Err(ERROR_OBJECT_WRONG_TYPE);
        }
        let host_path = entry.host_path.clone();

        let Some(state) = self.exnext.get_mut(&addr) else {
            return Err(ERROR_NO_MORE_ENTRIES);
        };
        let Some(name) = state.entries.get(state.next_index).cloned() else {
            return Err(ERROR_NO_MORE_ENTRIES);
        };
        state.next_index += 1;

        let entry_path = host_path.join(&name);
        let meta = std::fs::metadata(&entry_path).map_err(|e| map_io_error(&e))?;
        let is_dir = meta.is_dir();
        let size = if is_dir { 0 } else { meta.len() as u32 };
        fill_fib(mem, fib_addr, &name, is_dir, size);
        Ok(())
    }
}

/// The "own name" of a lock's target, for `Examine`: the last component
/// of its normalized Amiga path, or (for a volume root, which has no
/// components) the volume name itself.
pub(crate) fn own_display_name(amiga_path: &str) -> String {
    let (vol, rest) = amiga_path.split_once(':').unwrap_or((amiga_path, ""));
    match rest.rsplit('/').find(|c| !c.is_empty()) {
        Some(last) => last.to_string(),
        None => vol.to_string(),
    }
}

/// Allocates a zeroed `sizeof(struct FileLock)`-byte block on `heap` and
/// fills `fl_Key`/`fl_Access` (see the module docs).
fn alloc_lock_struct(
    heap: &mut GuestHeap,
    mem: &mut dyn AddressSpace,
    key: u32,
    access: i32,
) -> Result<u32, crate::guestmem::GuestHeapError> {
    let addr = heap.alloc(FILE_LOCK_SIZE)?;
    for i in 0..FILE_LOCK_SIZE {
        mem.write_u8(addr.wrapping_add(i), 0);
    }
    mem.write_u32(addr.wrapping_add(FL_KEY_OFFSET), key);
    mem.write_u32(addr.wrapping_add(FL_ACCESS_OFFSET), access as u32);
    Ok(addr)
}

/// Fills a guest `struct FileInfoBlock` at `fib_addr` for an entry named
/// `name` (a file if `is_dir` is false, in which case `size` is its byte
/// length; `size` is ignored -- written as `0` -- for a directory). See
/// the module docs for the field-by-field layout and the fixed
/// `DateStamp`/empty-comment choices.
pub(crate) fn fill_fib(
    mem: &mut dyn AddressSpace,
    fib_addr: u32,
    name: &str,
    is_dir: bool,
    size: u32,
) {
    let entry_type = if is_dir {
        ENTRY_TYPE_DIR
    } else {
        ENTRY_TYPE_FILE
    };
    let num_blocks = size.div_ceil(BLOCK_SIZE);

    mem.write_u32(fib_addr + FIB_DISKKEY_OFFSET, 0);
    mem.write_u32(fib_addr + FIB_DIRENTRYTYPE_OFFSET, entry_type as u32);

    let truncated_name: &str = {
        let mut end = name.len().min(FIB_FILENAME_MAX_CHARS);
        while end > 0 && !name.is_char_boundary(end) {
            end -= 1;
        }
        &name[..end]
    };
    write_c_string(
        mem,
        fib_addr + FIB_FILENAME_OFFSET,
        truncated_name.as_bytes(),
    );

    mem.write_u32(fib_addr + FIB_PROTECTION_OFFSET, 0);
    mem.write_u32(fib_addr + FIB_ENTRYTYPE_OFFSET, entry_type as u32);
    mem.write_u32(fib_addr + FIB_SIZE_OFFSET, size);
    mem.write_u32(fib_addr + FIB_NUMBLOCKS_OFFSET, num_blocks);
    // fib_Date: fixed DateStamp (ds_Days, ds_Minute, ds_Tick), all 0 --
    // see the module docs' "Examine/ExNext" section.
    mem.write_u32(fib_addr + FIB_DATE_OFFSET, 0);
    mem.write_u32(fib_addr + FIB_DATE_OFFSET + 4, 0);
    mem.write_u32(fib_addr + FIB_DATE_OFFSET + 8, 0);
    // fib_Comment: empty NUL-terminated string (TEXT fib_Comment[80],
    // NDK dos/dos.h -- not a BSTR, same fix as fib_FileName above).
    write_c_string(mem, fib_addr + FIB_COMMENT_OFFSET, b"");
}

// --- LVO handlers ---

/// `Lock` (`D1` = name `CString*`, `D2` = access mode). `D0` = `BPTR` or
/// `0` (+`IoErr()` set).
fn lock_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let name_ptr = ctx.cpu.data_register(DataRegister(1));
    let name = String::from_utf8_lossy(&read_c_string(ctx.mem, name_ptr)).into_owned();
    let access_mode = ctx.cpu.data_register(DataRegister(2)) as i32;
    match ctx.dos.lock(ctx.heap, ctx.mem, &name, access_mode) {
        Ok(bptr) => ctx.cpu.set_data_register(DataRegister(0), bptr),
        Err(code) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), 0);
        }
    }
    Ok(())
}

/// `CreateDir` (`D1` = name `CString*`). `D0` = `BPTR` (an
/// [`EXCLUSIVE_LOCK`] on the new directory) or `0` (+`IoErr()` set).
fn create_dir_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let name_ptr = ctx.cpu.data_register(DataRegister(1));
    let name = String::from_utf8_lossy(&read_c_string(ctx.mem, name_ptr)).into_owned();
    match ctx.dos.create_dir(ctx.heap, ctx.mem, &name) {
        Ok(bptr) => ctx.cpu.set_data_register(DataRegister(0), bptr),
        Err(code) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), 0);
        }
    }
    Ok(())
}

/// `SameLock` (`D1`/`D2` = `BPTR`s). `D0` = `LOCK_SAME`/
/// `LOCK_SAME_VOLUME`/`LOCK_DIFFERENT`.
fn same_lock_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let bptr1 = ctx.cpu.data_register(DataRegister(1));
    let bptr2 = ctx.cpu.data_register(DataRegister(2));
    let result = ctx
        .dos
        .same_lock(addr_from_bptr(bptr1), addr_from_bptr(bptr2));
    ctx.cpu.set_data_register(DataRegister(0), result as u32);
    Ok(())
}

/// `UnLock` (`D1` = `BPTR`). No return value (real `UnLock` is `void`).
fn unlock_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let bptr = ctx.cpu.data_register(DataRegister(1));
    let addr = addr_from_bptr(bptr);
    ctx.dos.unlock(ctx.heap, addr);
    Ok(())
}

/// `DupLock` (`D1` = `BPTR`). `D0` = new `BPTR` or `0` (+`IoErr()` set on
/// failure; `DupLock(0)` is `0` with no error).
fn dup_lock_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let bptr = ctx.cpu.data_register(DataRegister(1));
    let addr = addr_from_bptr(bptr);
    match ctx.dos.dup_lock(ctx.heap, ctx.mem, addr) {
        Ok(new_bptr) => ctx.cpu.set_data_register(DataRegister(0), new_bptr),
        Err(code) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), 0);
        }
    }
    Ok(())
}

/// `ParentDir` (`D1` = `BPTR`). `D0` = new `BPTR`, or `0` (with `IoErr()`
/// left untouched -- "no parent" isn't an error -- unless `D1` itself was
/// an invalid lock, which does set `IoErr()`).
fn parent_dir_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let bptr = ctx.cpu.data_register(DataRegister(1));
    let addr = addr_from_bptr(bptr);
    match ctx.dos.parent_dir(ctx.heap, ctx.mem, addr) {
        Ok(new_bptr) => ctx.cpu.set_data_register(DataRegister(0), new_bptr),
        Err(code) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), 0);
        }
    }
    Ok(())
}

/// `CurrentDir` (`D1` = `BPTR`). `D0` = the *previous* current-dir lock's
/// `BPTR` (`0` if there wasn't one), or `0` with `IoErr()` set on
/// failure.
fn current_dir_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let bptr = ctx.cpu.data_register(DataRegister(1));
    let addr = addr_from_bptr(bptr);
    match ctx.dos.current_dir(addr) {
        Ok(old_bptr) => ctx.cpu.set_data_register(DataRegister(0), old_bptr),
        Err(code) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), 0);
        }
    }
    Ok(())
}

/// `NameFromLock` (`D1` = `BPTR` lock, `D2` = buffer, `D3` = buffer
/// capacity). `D0` = `DOSTRUE`/`DOSFALSE` (`DOSFALSE` + `IoErr()` =
/// [`crate::dospattern::ERROR_LINE_TOO_LONG`] if the path doesn't fit).
fn name_from_lock_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let bptr = ctx.cpu.data_register(DataRegister(1));
    let buf_addr = ctx.cpu.data_register(DataRegister(2));
    let cap = ctx.cpu.data_register(DataRegister(3)) as usize;
    let addr = addr_from_bptr(bptr);

    match ctx.dos.name_from_lock(addr) {
        Ok(path) if path.len() < cap => {
            write_c_string(ctx.mem, buf_addr, path.as_bytes());
            ctx.cpu.set_data_register(DataRegister(0), DOSTRUE);
        }
        Ok(path) => {
            let mut truncated = path.into_bytes();
            truncated.truncate(cap.saturating_sub(1));
            write_c_string(ctx.mem, buf_addr, &truncated);
            ctx.dos.set_io_err(crate::dospattern::ERROR_LINE_TOO_LONG);
            ctx.cpu.set_data_register(DataRegister(0), DOSFALSE);
        }
        Err(code) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), DOSFALSE);
        }
    }
    Ok(())
}

/// `Examine` (`D1` = `BPTR` lock, `D2` = `struct FileInfoBlock*`). `D0` =
/// `DOSTRUE`/`DOSFALSE`.
fn examine_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let bptr = ctx.cpu.data_register(DataRegister(1));
    let fib_addr = ctx.cpu.data_register(DataRegister(2));
    let addr = addr_from_bptr(bptr);
    match ctx.dos.examine(ctx.mem, addr, fib_addr) {
        Ok(()) => ctx.cpu.set_data_register(DataRegister(0), DOSTRUE),
        Err(code) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), DOSFALSE);
        }
    }
    Ok(())
}

/// `ExNext` (`D1` = `BPTR` lock, `D2` = `struct FileInfoBlock*`). `D0` =
/// `DOSTRUE`/`DOSFALSE`.
fn ex_next_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let bptr = ctx.cpu.data_register(DataRegister(1));
    let fib_addr = ctx.cpu.data_register(DataRegister(2));
    let addr = addr_from_bptr(bptr);
    match ctx.dos.ex_next(ctx.mem, addr, fib_addr) {
        Ok(()) => ctx.cpu.set_data_register(DataRegister(0), DOSTRUE),
        Err(code) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), DOSFALSE);
        }
    }
    Ok(())
}

/// Registers every T11 lock/`Examine`/`ExNext` handler onto
/// [`DOS_LIBRARY_BASE`], looked up by name through [`DOS_LVOS`]. Called
/// from [`crate::dispatch::Runtime::new`] right alongside
/// [`crate::dosfile::register_dos_handlers`] -- like that function, these
/// handlers work (failing cleanly with `IoErr()` set) even without a
/// `Vfs` installed.
pub fn register_lock_handlers<C: Cpu + 'static>(table: &mut LibraryTable<C>, mem: &mut C::Memory) {
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
    reg!("Lock", lock_handler::<C>);
    reg!("CreateDir", create_dir_handler::<C>);
    reg!("SameLock", same_lock_handler::<C>);
    reg!("UnLock", unlock_handler::<C>);
    reg!("DupLock", dup_lock_handler::<C>);
    reg!("Examine", examine_handler::<C>);
    reg!("ExNext", ex_next_handler::<C>);
    reg!("CurrentDir", current_dir_handler::<C>);
    reg!("ParentDir", parent_dir_handler::<C>);
    reg!("NameFromLock", name_from_lock_handler::<C>);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{Runtime, StartConfig};
    use crate::dosfile::{ERROR_OBJECT_NOT_FOUND, MODE_NEWFILE};
    use crate::memory::FlatMemory;
    use crate::vfs::{Vfs, VfsConfig};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A unique temp directory, cleaned up on drop.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let path = std::env::temp_dir().join(format!("volamos-doslock-test-{tag}-{pid}-{n}"));
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

    // --- DosState unit tests ---

    #[test]
    fn lock_existing_file_succeeds() {
        let tmp = TempDir::new("lockfile");
        fs::write(tmp.path().join("f.txt"), b"hi").unwrap();
        let mut heap = GuestHeap::new(0x1000, 0x4000);
        let mut mem = FlatMemory::new(0x4000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        let bptr = dos
            .lock(&mut heap, &mut mem, "SYS:f.txt", SHARED_LOCK)
            .expect("lock should succeed");
        assert_ne!(bptr, 0);
    }

    #[test]
    fn lock_existing_dir_succeeds() {
        let tmp = TempDir::new("lockdir");
        fs::create_dir(tmp.path().join("sub")).unwrap();
        let mut heap = GuestHeap::new(0x1000, 0x4000);
        let mut mem = FlatMemory::new(0x4000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        let bptr = dos
            .lock(&mut heap, &mut mem, "SYS:sub", SHARED_LOCK)
            .expect("lock should succeed");
        assert_ne!(bptr, 0);
    }

    #[test]
    fn lock_missing_fails_with_object_not_found() {
        let tmp = TempDir::new("lockmissing");
        let mut heap = GuestHeap::new(0x1000, 0x4000);
        let mut mem = FlatMemory::new(0x4000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        let err = dos
            .lock(&mut heap, &mut mem, "SYS:nope.txt", SHARED_LOCK)
            .unwrap_err();
        assert_eq!(err, ERROR_OBJECT_NOT_FOUND);
    }

    #[test]
    fn unlock_frees_the_lock() {
        let tmp = TempDir::new("unlock");
        fs::write(tmp.path().join("f.txt"), b"hi").unwrap();
        let mut heap = GuestHeap::new(0x1000, 0x4000);
        let mut mem = FlatMemory::new(0x4000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        let bptr = dos
            .lock(&mut heap, &mut mem, "SYS:f.txt", SHARED_LOCK)
            .unwrap();
        let addr = addr_from_bptr(bptr);
        assert!(dos.locks.contains_key(&addr));
        dos.unlock(&mut heap, addr);
        assert!(!dos.locks.contains_key(&addr));
    }

    #[test]
    fn unlock_zero_is_a_no_op() {
        let mut heap = GuestHeap::new(0x1000, 0x4000);
        let mut dos = DosState::new(None);
        dos.unlock(&mut heap, 0); // shouldn't panic
    }

    #[test]
    fn dup_lock_works() {
        let tmp = TempDir::new("duplock");
        fs::write(tmp.path().join("f.txt"), b"hi").unwrap();
        let mut heap = GuestHeap::new(0x1000, 0x4000);
        let mut mem = FlatMemory::new(0x4000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        let bptr = dos
            .lock(&mut heap, &mut mem, "SYS:f.txt", EXCLUSIVE_LOCK)
            .unwrap();
        let addr = addr_from_bptr(bptr);
        let dup_bptr = dos.dup_lock(&mut heap, &mut mem, addr).unwrap();
        assert_ne!(dup_bptr, 0);
        assert_ne!(dup_bptr, bptr);
        let dup_addr = addr_from_bptr(dup_bptr);
        assert_eq!(
            dos.locks.get(&dup_addr).unwrap().host_path,
            dos.locks.get(&addr).unwrap().host_path
        );
        assert_eq!(dos.locks.get(&dup_addr).unwrap().access, SHARED_LOCK);
    }

    #[test]
    fn dup_lock_zero_is_zero() {
        let mut heap = GuestHeap::new(0x1000, 0x4000);
        let mut mem = FlatMemory::new(0x4000);
        let mut dos = DosState::new(None);
        assert_eq!(dos.dup_lock(&mut heap, &mut mem, 0).unwrap(), 0);
    }

    #[test]
    fn create_dir_makes_a_new_directory_and_locks_it_exclusively() {
        let tmp = TempDir::new("createdir");
        let mut heap = GuestHeap::new(0x1000, 0x4000);
        let mut mem = FlatMemory::new(0x4000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        let bptr = dos.create_dir(&mut heap, &mut mem, "SYS:newdir").unwrap();
        assert!(tmp.path().join("newdir").is_dir());
        let addr = addr_from_bptr(bptr);
        assert_eq!(dos.locks.get(&addr).unwrap().access, EXCLUSIVE_LOCK);
    }

    #[test]
    fn create_dir_existing_object_is_object_exists() {
        let tmp = TempDir::new("createdir-exists");
        fs::create_dir(tmp.path().join("already")).unwrap();
        let mut heap = GuestHeap::new(0x1000, 0x4000);
        let mut mem = FlatMemory::new(0x4000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        let err = dos
            .create_dir(&mut heap, &mut mem, "SYS:already")
            .unwrap_err();
        assert_eq!(err, crate::dosfile::ERROR_OBJECT_EXISTS);
    }

    #[test]
    fn create_dir_missing_parent_fails() {
        let tmp = TempDir::new("createdir-noparent");
        let mut heap = GuestHeap::new(0x1000, 0x4000);
        let mut mem = FlatMemory::new(0x4000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        assert!(
            dos.create_dir(&mut heap, &mut mem, "SYS:nope/newdir")
                .is_err()
        );
    }

    #[test]
    fn same_lock_identical_bptrs_is_lock_same() {
        let dos = DosState::new(None);
        assert_eq!(dos.same_lock(0, 0), LOCK_SAME);
        assert_eq!(dos.same_lock(0x100, 0x100), LOCK_SAME);
    }

    #[test]
    fn same_lock_two_locks_on_the_same_object_is_lock_same() {
        let tmp = TempDir::new("samelock-same");
        fs::write(tmp.path().join("f.txt"), b"hi").unwrap();
        let mut heap = GuestHeap::new(0x1000, 0x4000);
        let mut mem = FlatMemory::new(0x4000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        let b1 = dos
            .lock(&mut heap, &mut mem, "SYS:f.txt", SHARED_LOCK)
            .unwrap();
        let b2 = dos
            .lock(&mut heap, &mut mem, "SYS:f.txt", SHARED_LOCK)
            .unwrap();
        assert_eq!(
            dos.same_lock(addr_from_bptr(b1), addr_from_bptr(b2)),
            LOCK_SAME
        );
    }

    #[test]
    fn same_lock_two_locks_on_the_same_volume_is_lock_same_volume() {
        let tmp = TempDir::new("samelock-volume");
        fs::write(tmp.path().join("a.txt"), b"a").unwrap();
        fs::write(tmp.path().join("b.txt"), b"b").unwrap();
        let mut heap = GuestHeap::new(0x1000, 0x4000);
        let mut mem = FlatMemory::new(0x4000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        let b1 = dos
            .lock(&mut heap, &mut mem, "SYS:a.txt", SHARED_LOCK)
            .unwrap();
        let b2 = dos
            .lock(&mut heap, &mut mem, "SYS:b.txt", SHARED_LOCK)
            .unwrap();
        assert_eq!(
            dos.same_lock(addr_from_bptr(b1), addr_from_bptr(b2)),
            LOCK_SAME_VOLUME
        );
    }

    #[test]
    fn same_lock_unknown_addresses_are_lock_different() {
        let dos = DosState::new(None);
        assert_eq!(dos.same_lock(0x1111, 0x2222), LOCK_DIFFERENT);
    }

    #[test]
    fn name_from_lock_zero_is_sys_colon() {
        let dos = DosState::new(None);
        assert_eq!(dos.name_from_lock(0).unwrap(), "SYS:");
    }

    #[test]
    fn name_from_lock_returns_the_locks_absolute_amiga_path() {
        let tmp = TempDir::new("namefromlock");
        fs::create_dir(tmp.path().join("work")).unwrap();
        let mut heap = GuestHeap::new(0x1000, 0x4000);
        let mut mem = FlatMemory::new(0x4000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        let bptr = dos.lock(&mut heap, &mut mem, "work", SHARED_LOCK).unwrap();
        let addr = addr_from_bptr(bptr);
        assert_eq!(dos.name_from_lock(addr).unwrap(), "SYS:work");
    }

    #[test]
    fn name_from_lock_invalid_lock_is_an_error() {
        let dos = DosState::new(None);
        let err = dos.name_from_lock(0x1234).unwrap_err();
        assert_eq!(err, ERROR_INVALID_LOCK);
    }

    #[test]
    fn examine_and_exnext_enumerate_directory_sorted() {
        let tmp = TempDir::new("examinedir");
        fs::create_dir(tmp.path().join("work")).unwrap();
        fs::write(tmp.path().join("work/b.txt"), b"bb").unwrap();
        fs::write(tmp.path().join("work/a.txt"), b"a").unwrap();
        fs::create_dir(tmp.path().join("work/c_dir")).unwrap();

        let mut heap = GuestHeap::new(0x1000, 0x8000);
        let mut mem = FlatMemory::new(0x8000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        let bptr = dos
            .lock(&mut heap, &mut mem, "SYS:work", SHARED_LOCK)
            .unwrap();
        let addr = addr_from_bptr(bptr);

        let fib_addr = 0x4000;
        dos.examine(&mut mem, addr, fib_addr).expect("examine dir");
        assert_eq!(
            mem.read_u32(fib_addr + FIB_DIRENTRYTYPE_OFFSET) as i32,
            ENTRY_TYPE_DIR
        );
        assert_eq!(read_c_string(&mem, fib_addr + FIB_FILENAME_OFFSET), b"work");

        // Sorted order: a.txt, b.txt, c_dir.
        dos.ex_next(&mut mem, addr, fib_addr).expect("first entry");
        assert_eq!(
            read_c_string(&mem, fib_addr + FIB_FILENAME_OFFSET),
            b"a.txt"
        );
        assert_eq!(mem.read_u32(fib_addr + FIB_SIZE_OFFSET), 1);
        assert_eq!(
            mem.read_u32(fib_addr + FIB_DIRENTRYTYPE_OFFSET) as i32,
            ENTRY_TYPE_FILE
        );

        dos.ex_next(&mut mem, addr, fib_addr).expect("second entry");
        assert_eq!(
            read_c_string(&mem, fib_addr + FIB_FILENAME_OFFSET),
            b"b.txt"
        );
        assert_eq!(mem.read_u32(fib_addr + FIB_SIZE_OFFSET), 2);

        dos.ex_next(&mut mem, addr, fib_addr).expect("third entry");
        assert_eq!(
            read_c_string(&mem, fib_addr + FIB_FILENAME_OFFSET),
            b"c_dir"
        );
        assert_eq!(
            mem.read_u32(fib_addr + FIB_DIRENTRYTYPE_OFFSET) as i32,
            ENTRY_TYPE_DIR
        );

        let err = dos.ex_next(&mut mem, addr, fib_addr).unwrap_err();
        assert_eq!(err, ERROR_NO_MORE_ENTRIES);
    }

    #[test]
    fn examine_on_file_gives_negative_type_and_size() {
        let tmp = TempDir::new("examinefile");
        fs::write(tmp.path().join("f.txt"), b"hello").unwrap();
        let mut heap = GuestHeap::new(0x1000, 0x4000);
        let mut mem = FlatMemory::new(0x4000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        let bptr = dos
            .lock(&mut heap, &mut mem, "SYS:f.txt", SHARED_LOCK)
            .unwrap();
        let addr = addr_from_bptr(bptr);

        let fib_addr = 0x2000;
        dos.examine(&mut mem, addr, fib_addr).expect("examine file");
        assert_eq!(
            mem.read_u32(fib_addr + FIB_DIRENTRYTYPE_OFFSET) as i32,
            ENTRY_TYPE_FILE
        );
        assert_eq!(mem.read_u32(fib_addr + FIB_SIZE_OFFSET), 5);
        assert_eq!(
            read_c_string(&mem, fib_addr + FIB_FILENAME_OFFSET),
            b"f.txt"
        );

        let err = dos.ex_next(&mut mem, addr, fib_addr).unwrap_err();
        assert_eq!(err, ERROR_OBJECT_WRONG_TYPE);
    }

    #[test]
    fn parent_dir_walks_up_and_stops_at_root() {
        let tmp = TempDir::new("parentdir");
        fs::create_dir_all(tmp.path().join("a/b")).unwrap();
        let mut heap = GuestHeap::new(0x1000, 0x8000);
        let mut mem = FlatMemory::new(0x8000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));

        let bptr = dos
            .lock(&mut heap, &mut mem, "SYS:a/b", SHARED_LOCK)
            .unwrap();
        let addr = addr_from_bptr(bptr);

        let parent_bptr = dos.parent_dir(&mut heap, &mut mem, addr).unwrap();
        assert_ne!(parent_bptr, 0);
        let parent_addr = addr_from_bptr(parent_bptr);
        assert_eq!(dos.locks.get(&parent_addr).unwrap().amiga_path, "SYS:a");

        let root_bptr = dos.parent_dir(&mut heap, &mut mem, parent_addr).unwrap();
        assert_ne!(root_bptr, 0);
        let root_addr = addr_from_bptr(root_bptr);
        assert_eq!(dos.locks.get(&root_addr).unwrap().amiga_path, "SYS:");

        // At the volume root: no parent, 0 with no error.
        let none_bptr = dos.parent_dir(&mut heap, &mut mem, root_addr).unwrap();
        assert_eq!(none_bptr, 0);
    }

    #[test]
    fn current_dir_changes_relative_resolution_and_returns_previous() {
        let tmp = TempDir::new("currentdir");
        fs::create_dir(tmp.path().join("work")).unwrap();
        fs::write(tmp.path().join("work/inner.txt"), b"x").unwrap();
        let mut heap = GuestHeap::new(0x1000, 0x8000);
        let mut mem = FlatMemory::new(0x8000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));

        // Before any CurrentDir call, relative resolution is against the
        // initial cwd (SYS:), so a bare relative name doesn't find
        // work/inner.txt.
        assert!(
            dos.lock(&mut heap, &mut mem, "inner.txt", SHARED_LOCK)
                .is_err()
        );

        let work_bptr = dos
            .lock(&mut heap, &mut mem, "SYS:work", SHARED_LOCK)
            .unwrap();
        let work_addr = addr_from_bptr(work_bptr);

        let old_bptr = dos
            .current_dir(work_addr)
            .expect("CurrentDir should succeed");
        assert_eq!(old_bptr, 0, "no previous current-dir lock yet");

        // Now relative resolution should find it.
        let inner_bptr = dos
            .lock(&mut heap, &mut mem, "inner.txt", SHARED_LOCK)
            .expect("relative lock should now resolve under SYS:work");
        assert_ne!(inner_bptr, 0);

        // Switching again returns the previous current-dir lock's BPTR.
        let root_bptr = dos.lock(&mut heap, &mut mem, "SYS:", SHARED_LOCK).unwrap();
        let root_addr = addr_from_bptr(root_bptr);
        let prev = dos.current_dir(root_addr).unwrap();
        assert_eq!(prev, work_bptr);

        // CurrentDir(0) restores the initial cwd.
        dos.current_dir(0).unwrap();
        assert!(
            dos.lock(&mut heap, &mut mem, "inner.txt", SHARED_LOCK)
                .is_err()
        );
    }

    // --- End-to-end tests through Runtime, mirroring dosfile.rs's style ---

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

    fn runtime_with_program_and_extra(
        words: &[u16],
        extra_addr: u32,
        extra: &[u8],
        vfs_root: Option<&Path>,
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
        if let Some(root) = vfs_root {
            rt.set_vfs(vfs_over(root));
        }
        rt
    }

    #[test]
    fn end_to_end_lock_missing_returns_zero_and_ioerr() {
        let tmp = TempDir::new("e2e-lockmissing");
        let name = b"SYS:nope.txt\0";

        let mut words = Vec::new();
        let name_idx = words.len();
        words.push(move_imm_to_d(1));
        words.push(0);
        words.push(0);
        push_move_imm_to_d(&mut words, 2, SHARED_LOCK as u32);
        push_jsr(&mut words, 6, -84); // Lock(a6): D0 = BPTR or 0
        push_jsr(&mut words, 6, -132); // IoErr(a6)
        words.push(RTS);

        let name_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        patch_imm32(&mut words, name_idx, name_addr);

        let mut rt = runtime_with_program_and_extra(&words, name_addr, name, Some(tmp.path()));
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, ERROR_OBJECT_NOT_FOUND);
    }

    #[test]
    fn end_to_end_lock_existing_file_then_unlock() {
        let tmp = TempDir::new("e2e-lockfile");
        fs::write(tmp.path().join("f.txt"), b"hi").unwrap();
        let name = b"SYS:f.txt\0";

        let mut words = Vec::new();
        let name_idx = words.len();
        words.push(move_imm_to_d(1));
        words.push(0);
        words.push(0);
        push_move_imm_to_d(&mut words, 2, SHARED_LOCK as u32);
        push_jsr(&mut words, 6, -84); // Lock(a6): D0 = BPTR
        words.push(0x2001); // move.l d0,d1 (save BPTR for UnLock)
        push_jsr(&mut words, 6, -90); // UnLock(a6)
        words.push(0x7001); // moveq #1,d0 (exit code 1: reached the end)
        words.push(RTS);

        let name_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        patch_imm32(&mut words, name_idx, name_addr);

        let mut rt = runtime_with_program_and_extra(&words, name_addr, name, Some(tmp.path()));
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 1);
    }

    #[test]
    fn end_to_end_open_after_current_dir_uses_new_cwd() {
        let tmp = TempDir::new("e2e-currentdir");
        fs::create_dir(tmp.path().join("work")).unwrap();
        fs::write(tmp.path().join("work/inner.txt"), b"present").unwrap();

        // Lock("SYS:work"), CurrentDir(that lock), then Open("inner.txt",
        // MODE_OLDFILE) should now find it via the relative path.
        let dir_name = b"SYS:work\0";
        let file_name = b"inner.txt\0";

        let mut words = Vec::new();
        let dir_name_idx = words.len();
        words.push(move_imm_to_d(1));
        words.push(0);
        words.push(0);
        push_move_imm_to_d(&mut words, 2, SHARED_LOCK as u32);
        push_jsr(&mut words, 6, -84); // Lock(a6): D0 = BPTR of SYS:work
        words.push(0x2200); // move.l d0,d1
        push_jsr(&mut words, 6, -126); // CurrentDir(a6): D0 = previous (0)

        let file_name_idx = words.len();
        words.push(move_imm_to_d(1));
        words.push(0);
        words.push(0);
        push_move_imm_to_d(&mut words, 2, MODE_NEWFILE as u32); // exists, but any mode works
        push_jsr(&mut words, 6, -30); // Open(a6): D0 = BPTR or 0
        words.push(RTS);

        let dir_name_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        let file_name_addr = dir_name_addr + dir_name.len() as u32;
        patch_imm32(&mut words, dir_name_idx, dir_name_addr);
        patch_imm32(&mut words, file_name_idx, file_name_addr);

        let mut extra = dir_name.to_vec();
        extra.extend_from_slice(file_name);
        let mut rt =
            runtime_with_program_and_extra(&words, dir_name_addr, &extra, Some(tmp.path()));
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_ne!(code, 0, "Open of the relative path should now succeed");
    }

    #[test]
    fn end_to_end_name_from_lock_writes_the_absolute_path() {
        let tmp = TempDir::new("e2e-namefromlock");
        fs::create_dir(tmp.path().join("work")).unwrap();
        let dir_name = b"SYS:work\0";

        let mut words = Vec::new();
        let dir_name_idx = words.len();
        words.push(move_imm_to_d(1));
        words.push(0);
        words.push(0);
        push_move_imm_to_d(&mut words, 2, SHARED_LOCK as u32);
        push_jsr(&mut words, 6, -84); // Lock(a6): D0 = BPTR
        words.push(0x2200); // move.l d0,d1 (lock for NameFromLock)
        let buf_idx = push_move_imm_to_d(&mut words, 2, 0); // D2 = buffer (patched)
        push_move_imm_to_d(&mut words, 3, 64); // D3 = capacity
        push_jsr(&mut words, 6, -402); // NameFromLock(a6): D0 = DOSTRUE/DOSFALSE
        words.push(RTS);

        let dir_name_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        let buf_addr = dir_name_addr + dir_name.len() as u32;
        patch_imm32(&mut words, dir_name_idx, dir_name_addr);
        patch_imm32(&mut words, buf_idx, buf_addr);

        let mut rt =
            runtime_with_program_and_extra(&words, dir_name_addr, dir_name, Some(tmp.path()));
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, DOSTRUE as i32);
        assert_eq!(read_c_string(rt.memory(), buf_addr), b"SYS:work");
    }

    #[test]
    fn end_to_end_create_dir_then_same_lock_via_trap_dispatch() {
        let tmp = TempDir::new("e2e-createdir-samelock");
        let name = b"SYS:newdir\0";

        // CreateDir(name) -> D0 = lock #1, saved in D3. Lock(name,
        // SHARED_LOCK) -> D0 = lock #2 on the same (now-existing)
        // directory, moved to D1. D3 -> D2. SameLock(D1, D2) -> D0.
        let mut words = Vec::new();
        let name_idx = words.len();
        words.push(move_imm_to_d(1)); // D1 = name (patched)
        words.push(0);
        words.push(0);
        push_jsr(&mut words, 6, -120); // CreateDir(a6): D0 = lock #1
        words.push(0x2600); // move.l d0,d3 (save lock #1)

        let name_idx2 = words.len();
        words.push(move_imm_to_d(1)); // D1 = name again (patched)
        words.push(0);
        words.push(0);
        push_move_imm_to_d(&mut words, 2, SHARED_LOCK as u32); // D2 = access mode
        push_jsr(&mut words, 6, -84); // Lock(a6): D0 = lock #2
        words.push(0x2200); // move.l d0,d1 (D1 = lock #2)
        words.push(0x2403); // move.l d3,d2 (D2 = lock #1)
        push_jsr(&mut words, 6, -420); // SameLock(a6): D0 = LOCK_SAME
        words.push(RTS);

        let name_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        patch_imm32(&mut words, name_idx, name_addr);
        patch_imm32(&mut words, name_idx2, name_addr);

        let mut rt = runtime_with_program_and_extra(&words, name_addr, name, Some(tmp.path()));
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, LOCK_SAME);
        assert!(tmp.path().join("newdir").is_dir());
    }
}
