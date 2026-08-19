//! `dos.library` `MatchFirst`/`MatchNext`/`MatchEnd`: the `AnchorPath`-
//! based directory-tree scanner built on top of `crate::dospattern`'s
//! matching engine. Every real `C:` command that resolves a wildcard
//! filename argument (`List`, `Copy`, `Delete`, `Dir`, `Type`, ...) --
//! including the *non-wildcard* case, where real `MatchFirst` just
//! `Lock()`s the object directly -- goes through these three calls.
//!
//! # Scope
//!
//! - **Single wildcard component only.** A pattern is split at its
//!   *last* `/` or `:` into a literal directory prefix and a final
//!   component; only that final component may contain wildcards
//!   (`SYS:C/#?`, `RAM:foo#?`, `#?.info` all work; `SYS:*/foo`, a
//!   wildcard in a *non-final* component, does not -- no known real
//!   `C:` command corpus target needs that). If the final component has
//!   no wildcard at all, the whole pattern is `Lock()`ed directly,
//!   matching real `MatchFirst`'s own documented behavior for that case
//!   exactly (down to reporting [`crate::dosfile::ERROR_OBJECT_NOT_FOUND`]
//!   on failure, per the autodoc).
//! - **Recursive descent (`APF_DODIR`) is fully supported**, and reapplies
//!   the *same* single-component pattern at every directory level --
//!   this is exactly what the NDK's own `ScanDirectories()` worked
//!   example does, and covers the realistic "scan a whole tree for
//!   `#?.info`" use case completely, without needing multi-component
//!   pattern support at all.
//! - **No hard/soft-link distinction**: a directory entry counts as
//!   "a directory" purely from `std::fs::Metadata::is_dir`, matching
//!   this runtime's existing `Examine`/`ExNext` simplification
//!   (`crate::doslock`). `APF_FollowHLinks` is accepted but has no
//!   effect (there's nothing it would change).
//! - **`ap_BreakBits`/`ap_FoundBreak` (`Ctrl-C` abort mid-scan) is not
//!   implemented** -- a scan never aborts early; `ap_FoundBreak` is
//!   never written.
//!
//! # `AnchorPath`/`AChain` layout
//!
//! Unlike `ReadArgs`'s opaque `RDArgs` anchor, real guest code reads
//! `AnchorPath` fields directly (`ap_Info.fib_FileName`, `ap_Flags`,
//! `ap_Current->an_Lock`, optionally `ap_Buf`) per the NDK's own worked
//! example, so this runtime lays out real, byte-accurate `struct
//! AnchorPath`/`struct AChain` (NDK `dos/dosasl.h`) in guest memory --
//! `AnchorPath` is caller-allocated (this runtime never allocates it),
//! `AChain` nodes are allocated on the guest heap, one per directory
//! level currently being scanned (`AnchorMatchState::levels`, a stack:
//! push on `APF_DODIR` descent, pop (with `APF_DIDDIR` signaled) on
//! exhaustion).
//!
//! ```text
//! struct AnchorPath {                    struct AChain {
//!     AChain *ap_Base;        +0             AChain *an_Child;    +0
//!     AChain *ap_Last;        +4             AChain *an_Parent;   +4
//!     LONG    ap_BreakBits;   +8             BPTR    an_Lock;     +8
//!     LONG    ap_FoundBreak;  +12            FileInfoBlock an_Info; +12 (260)
//!     BYTE    ap_Flags;       +16            BYTE    an_Flags;    +272
//!     BYTE    ap_Reserved;    +17            TEXT    an_String[1]; +273
//!     WORD    ap_Strlen;      +18        };                       (273+)
//!     FileInfoBlock ap_Info;  +20 (260)
//!     TEXT    ap_Buf[1];      +280
//! };                          (280+, caller-allocated tail)
//! ```
//!
//! # `MatchNext` state machine
//!
//! One call either (a) returns the next filtered-and-sorted entry at the
//! current (top-of-stack) directory level, advancing its cursor; (b)
//! first, if the *previous* return was a directory and the caller has
//! since set `APF_DODIR`, pushes a new level for it (clearing
//! `APF_DODIR`, setting `APF_DirChanged`) and then does (a) against the
//! new level -- immediately cascading to (c) if it's empty; or (c), on
//! exhausting the current level with a parent still on the stack, pops
//! it (unlocking its directory lock), reverts `ap_Last` to the parent's
//! `AChain`, sets `APF_DIDDIR`, and returns success *without* advancing
//! the parent's cursor -- exactly matching the NDK's own documented
//! "reverts back to the parent directory, continuing the scan there [on
//! the next call]" behavior, including `ap_Info` still showing the
//! just-exited directory's own info on the `APF_DIDDIR` return.
//! Exhausting the outermost level returns
//! [`crate::doslock::ERROR_NO_MORE_ENTRIES`].

use std::path::{Path, PathBuf};

use crate::cpu::{Cpu, DataRegister};
use crate::dispatch::{DOS_LIBRARY_BASE, DispatchError, HandlerContext, LibraryTable};
use crate::dosfile::{DosState, ERROR_OBJECT_NOT_FOUND, map_io_error};
use crate::doslock::{ERROR_NO_MORE_ENTRIES, FIB_SIZE, SHARED_LOCK, fill_fib, own_display_name};
use crate::dospattern::{self, Node};
use crate::guestmem::{GuestHeap, addr_from_bptr, read_c_string};
use crate::lvos::dos::DOS_LVOS;
use crate::memory::AddressSpace;

/// A `ap_Strlen`/`ap_Buf` overflow, or an internal buffer overflow --
/// NDK `dos/dosasl.h`.
pub const ERROR_BUFFER_OVERFLOW: i32 = 303;

// --- struct AnchorPath field offsets ---
const AP_BASE_OFFSET: u32 = 0;
const AP_LAST_OFFSET: u32 = 4;
const AP_FLAGS_OFFSET: u32 = 16;
const AP_STRLEN_OFFSET: u32 = 18;
const AP_INFO_OFFSET: u32 = 20;
const AP_BUF_OFFSET: u32 = AP_INFO_OFFSET + FIB_SIZE; // 280

// --- struct AChain field offsets ---
const AN_PARENT_OFFSET: u32 = 4;
const AN_LOCK_OFFSET: u32 = 8;
const AN_INFO_OFFSET: u32 = 12;
/// `sizeof(struct AChain)` up to (not including) the variable-length
/// `an_String` tail -- this runtime doesn't populate `an_String` (it's
/// documented as internal-only), so nothing is allocated for it.
const ACHAIN_SIZE: u32 = AN_INFO_OFFSET + FIB_SIZE + 1; // an_Flags is 1 byte

const APF_ITSWILD: u8 = 2;
const APF_DODIR: u8 = 4;
const APF_DIDDIR: u8 = 8;
const APF_DIR_CHANGED: u8 = 64;

/// One directory currently being scanned -- one per level of `APF_DODIR`
/// recursion, kept as a stack in [`AnchorMatchState::levels`].
struct ScanLevel {
    /// Guest address of this level's `struct AChain` (whose `an_Lock`
    /// field already holds this directory's lock `BPTR`).
    achain_addr: u32,
    /// This directory's lock's own guest address, for
    /// `DosState::unlock` on pop/`MatchEnd`.
    dir_lock_addr: u32,
    /// The Amiga path that reached this directory, always ending in `:`
    /// or `/`, so a child entry's full path is just this plus its name.
    dir_amiga_path: String,
    dir_host_path: PathBuf,
    /// Entries already filtered by the pattern and sorted, with a flag
    /// for whether each is itself a directory (eligible for further
    /// `APF_DODIR` descent).
    entries: Vec<(String, bool)>,
    /// Index into `entries` the next `MatchNext` call returns.
    cursor: usize,
    /// This directory's *own* name (i.e. the entry that was matched to
    /// trigger descending into it) -- restored into `ap_Info` when this
    /// level is popped (the `APF_DIDDIR` event), since by then the
    /// entries *inside* it have long since overwritten `ap_Info` with
    /// their own data. Matches the NDK's own documented `APF_DIDDIR`
    /// example (`Printf("leaving %s", ac->ap_Info.fib_FileName)`), which
    /// only makes sense if `ap_Info` shows the directory's own name at
    /// that point.
    self_name: String,
}

/// Live `MatchFirst`/`MatchNext` state for one `AnchorPath`, keyed by
/// its guest address in [`DosState::anchor_states`].
pub(crate) struct AnchorMatchState {
    /// `None` for a non-wildcard match (a single `Lock()`ed object, no
    /// `APF_DODIR` recursion supported -- see the module docs' "Scope"
    /// section); `Some` for a wildcard scan, reapplied at every
    /// recursively-entered directory level.
    pattern: Option<Node>,
    levels: Vec<ScanLevel>,
    /// The most recently returned entry's name and whether it's a
    /// directory, consulted by the *next* `MatchNext` call to decide
    /// whether an `APF_DODIR`-triggered descent applies. `None`
    /// immediately after an `APF_DIDDIR` return (entering a directory
    /// right after leaving one isn't a sensible operation to check for).
    pending_entry: Option<(String, bool)>,
}

fn join_amiga(dir: &str, name: &str) -> String {
    if dir.is_empty() || dir.ends_with(':') || dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

fn set_flag_bit(mem: &mut dyn AddressSpace, ap_addr: u32, bit: u8, set: bool) {
    let flags = mem.read_u8(ap_addr + AP_FLAGS_OFFSET);
    mem.write_u8(
        ap_addr + AP_FLAGS_OFFSET,
        if set { flags | bit } else { flags & !bit },
    );
}

fn alloc_achain(heap: &mut GuestHeap, mem: &mut dyn AddressSpace) -> Result<u32, i32> {
    let addr = heap
        .alloc(ACHAIN_SIZE)
        .map_err(|_| crate::dosfile::ERROR_NO_FREE_STORE)?;
    for i in 0..ACHAIN_SIZE {
        mem.write_u8(addr + i, 0);
    }
    Ok(addr)
}

/// Fills `ap_Info` (in the `AnchorPath` at `ap_addr`) and `an_Info` (in
/// the `AChain` at `achain_addr`) for a matched entry, and `ap_Buf`
/// (truncated to `ap_Strlen`, per the documented convention) with its
/// full path, if `ap_Strlen` is non-zero. Returns
/// [`ERROR_BUFFER_OVERFLOW`] (still having filled everything else) if
/// the path didn't fit.
fn write_match_result(
    mem: &mut dyn AddressSpace,
    ap_addr: u32,
    achain_addr: u32,
    name: &str,
    is_dir: bool,
    size: u32,
    full_amiga_path: &str,
) -> Result<(), i32> {
    fill_fib(mem, ap_addr + AP_INFO_OFFSET, name, is_dir, size);
    fill_fib(mem, achain_addr + AN_INFO_OFFSET, name, is_dir, size);

    let strlen = mem.read_u16(ap_addr + AP_STRLEN_OFFSET) as usize;
    if strlen == 0 {
        return Ok(());
    }
    let bytes = full_amiga_path.as_bytes();
    let overflowed = bytes.len() + 1 > strlen;
    let n = bytes.len().min(strlen.saturating_sub(1));
    let buf_addr = ap_addr + AP_BUF_OFFSET;
    for (i, &b) in bytes[..n].iter().enumerate() {
        mem.write_u8(buf_addr + i as u32, b);
    }
    if !overflowed {
        mem.write_u8(buf_addr + n as u32, 0);
    }
    if overflowed {
        Err(ERROR_BUFFER_OVERFLOW)
    } else {
        Ok(())
    }
}

fn list_and_filter(host_dir: &Path, pattern: &Node) -> Result<Vec<(String, bool)>, i32> {
    let mut out = Vec::new();
    for dirent in std::fs::read_dir(host_dir).map_err(|e| map_io_error(&e))? {
        let dirent = dirent.map_err(|e| map_io_error(&e))?;
        let name = dirent.file_name().to_string_lossy().into_owned();
        if dospattern::full_match(pattern, name.as_bytes(), true) {
            let is_dir = dirent.file_type().map(|t| t.is_dir()).unwrap_or(false);
            out.push((name, is_dir));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Locks `amiga_path` (a directory) and returns `(bptr, addr,
/// host_path)`. A thin wrapper around [`DosState::lock`] just to bundle
/// the address-derivation and host-path lookup every call site here
/// needs.
fn lock_dir(
    heap: &mut GuestHeap,
    mem: &mut dyn AddressSpace,
    dos: &mut DosState,
    amiga_path: &str,
) -> Result<(u32, u32, PathBuf), i32> {
    let bptr = dos
        .lock(heap, mem, amiga_path, SHARED_LOCK)
        .map_err(|_| ERROR_OBJECT_NOT_FOUND)?;
    let addr = addr_from_bptr(bptr);
    let host_path = dos.locks.get(&addr).expect("just locked").host_path.clone();
    Ok((bptr, addr, host_path))
}

fn match_first(
    heap: &mut GuestHeap,
    mem: &mut dyn AddressSpace,
    dos: &mut DosState,
    pat_bytes: &[u8],
    ap_addr: u32,
) -> Result<(), i32> {
    let pat_str = String::from_utf8_lossy(pat_bytes).into_owned();
    let split_idx = pat_str.rfind([':', '/']);
    let (dir_part, name_part) = match split_idx {
        Some(i) => (pat_str[..=i].to_string(), &pat_str[i + 1..]),
        None => {
            let cwd = dos
                .vfs
                .as_ref()
                .map(|v| v.cwd().to_string())
                .unwrap_or_default();
            (format!("{cwd}/"), pat_str.as_str())
        }
    };
    let (pattern_node, has_wild) =
        dospattern::parse(name_part.as_bytes()).map_err(|_| crate::dosargs::ERROR_BAD_TEMPLATE)?;

    if !has_wild {
        let bptr = dos
            .lock(heap, mem, &pat_str, SHARED_LOCK)
            .map_err(|_| ERROR_OBJECT_NOT_FOUND)?;
        let addr = addr_from_bptr(bptr);
        let entry = dos.locks.get(&addr).expect("just locked").clone();
        let is_dir = entry.host_path.is_dir();
        let size = if is_dir {
            0
        } else {
            std::fs::metadata(&entry.host_path)
                .map(|m| m.len() as u32)
                .unwrap_or(0)
        };
        let display_name = own_display_name(&entry.amiga_path);
        let achain_addr = alloc_achain(heap, mem)?;
        mem.write_u32(achain_addr + AN_LOCK_OFFSET, bptr);
        write_match_result(
            mem,
            ap_addr,
            achain_addr,
            &display_name,
            is_dir,
            size,
            &entry.amiga_path,
        )?;
        mem.write_u32(ap_addr + AP_BASE_OFFSET, achain_addr);
        mem.write_u32(ap_addr + AP_LAST_OFFSET, achain_addr);

        let level = ScanLevel {
            achain_addr,
            dir_lock_addr: addr,
            dir_amiga_path: entry.amiga_path.clone(),
            dir_host_path: entry.host_path,
            self_name: display_name.clone(),
            entries: vec![(display_name, is_dir)],
            cursor: 1,
        };
        dos.anchor_states.insert(
            ap_addr,
            AnchorMatchState {
                pattern: None,
                levels: vec![level],
                pending_entry: None,
            },
        );
        return Ok(());
    }

    let (bptr, addr, host_path) = lock_dir(heap, mem, dos, &dir_part)?;
    let entries = match list_and_filter(&host_path, &pattern_node) {
        Ok(e) => e,
        Err(code) => {
            dos.unlock(heap, addr);
            return Err(code);
        }
    };
    if entries.is_empty() {
        dos.unlock(heap, addr);
        return Err(ERROR_NO_MORE_ENTRIES);
    }

    let achain_addr = alloc_achain(heap, mem)?;
    mem.write_u32(achain_addr + AN_LOCK_OFFSET, bptr);
    let (name0, is_dir0) = entries[0].clone();
    let full_path0 = join_amiga(&dir_part, &name0);
    let size0 = if is_dir0 {
        0
    } else {
        std::fs::metadata(host_path.join(&name0))
            .map(|m| m.len() as u32)
            .unwrap_or(0)
    };
    write_match_result(
        mem,
        ap_addr,
        achain_addr,
        &name0,
        is_dir0,
        size0,
        &full_path0,
    )?;
    mem.write_u32(ap_addr + AP_BASE_OFFSET, achain_addr);
    mem.write_u32(ap_addr + AP_LAST_OFFSET, achain_addr);
    set_flag_bit(mem, ap_addr, APF_ITSWILD, true);

    let self_name = own_display_name(&dir_part);
    let level = ScanLevel {
        achain_addr,
        dir_lock_addr: addr,
        dir_amiga_path: dir_part,
        dir_host_path: host_path,
        self_name,
        entries,
        cursor: 1,
    };
    dos.anchor_states.insert(
        ap_addr,
        AnchorMatchState {
            pattern: Some(pattern_node),
            levels: vec![level],
            pending_entry: Some((name0, is_dir0)),
        },
    );
    Ok(())
}

fn match_next(
    heap: &mut GuestHeap,
    mem: &mut dyn AddressSpace,
    dos: &mut DosState,
    ap_addr: u32,
) -> Result<(), i32> {
    let Some(mut state) = dos.anchor_states.remove(&ap_addr) else {
        return Err(ERROR_NO_MORE_ENTRIES);
    };
    let result = match_next_inner(heap, mem, dos, ap_addr, &mut state);
    dos.anchor_states.insert(ap_addr, state);
    result
}

fn match_next_inner(
    heap: &mut GuestHeap,
    mem: &mut dyn AddressSpace,
    dos: &mut DosState,
    ap_addr: u32,
    state: &mut AnchorMatchState,
) -> Result<(), i32> {
    // Step 1: a directory-entering descent, if the previous match was a
    // directory and the caller has since set APF_DODIR.
    if let Some(pattern) = &state.pattern
        && let Some((name, true)) = state.pending_entry.take()
    {
        let flags = mem.read_u8(ap_addr + AP_FLAGS_OFFSET);
        if flags & APF_DODIR != 0 {
            let top = state.levels.last().expect("non-empty by construction");
            let child_amiga = join_amiga(&top.dir_amiga_path, &format!("{name}/"));
            let old_top_achain = top.achain_addr;

            let (bptr, addr, host_path) = lock_dir(heap, mem, dos, &child_amiga)?;
            let entries = list_and_filter(&host_path, pattern).unwrap_or_default();

            let achain_addr = alloc_achain(heap, mem)?;
            mem.write_u32(achain_addr + AN_LOCK_OFFSET, bptr);
            mem.write_u32(achain_addr + AN_PARENT_OFFSET, old_top_achain);
            mem.write_u32(ap_addr + AP_LAST_OFFSET, achain_addr);
            set_flag_bit(mem, ap_addr, APF_DODIR, false);
            set_flag_bit(mem, ap_addr, APF_DIR_CHANGED, true);

            state.levels.push(ScanLevel {
                achain_addr,
                dir_lock_addr: addr,
                dir_amiga_path: child_amiga,
                dir_host_path: host_path,
                self_name: name,
                entries,
                cursor: 0,
            });
        }
    }

    // Step 2: return the next entry at the current level, or (if it's
    // exhausted) cascade-pop one level and signal APF_DIDDIR -- either
    // way this settles the call; a level that pops empty into another
    // empty level is resolved by the *next* MatchNext call, not this
    // one, matching the NDK's own documented one-event-per-call
    // behavior.
    let level = state.levels.last_mut().expect("non-empty by construction");
    if let Some((name, is_dir)) = level.entries.get(level.cursor).cloned() {
        level.cursor += 1;
        let full_path = join_amiga(&level.dir_amiga_path, &name);
        let host_path = level.dir_host_path.join(&name);
        let achain_addr = level.achain_addr;
        let size = if is_dir {
            0
        } else {
            std::fs::metadata(&host_path)
                .map(|m| m.len() as u32)
                .unwrap_or(0)
        };
        write_match_result(mem, ap_addr, achain_addr, &name, is_dir, size, &full_path)?;
        if state.pattern.is_some() {
            state.pending_entry = Some((name, is_dir));
        }
        return Ok(());
    }

    if state.levels.len() == 1 {
        return Err(ERROR_NO_MORE_ENTRIES);
    }
    let finished = state.levels.pop().expect("len() > 1 just checked");
    dos.unlock(heap, finished.dir_lock_addr);
    let parent = state.levels.last().expect("len() > 1 just checked");
    let parent_achain = parent.achain_addr;
    let full_path = join_amiga(&parent.dir_amiga_path, &finished.self_name);
    mem.write_u32(ap_addr + AP_LAST_OFFSET, parent_achain);
    // Restore ap_Info to the just-exited directory's own descriptor
    // (see ScanLevel::self_name's doc) before signaling APF_DIDDIR.
    write_match_result(
        mem,
        ap_addr,
        parent_achain,
        &finished.self_name,
        true,
        0,
        &full_path,
    )?;
    set_flag_bit(mem, ap_addr, APF_DIDDIR, true);
    state.pending_entry = None;
    Ok(())
}

fn match_end(heap: &mut GuestHeap, dos: &mut DosState, ap_addr: u32) {
    if let Some(state) = dos.anchor_states.remove(&ap_addr) {
        for level in state.levels {
            dos.unlock(heap, level.dir_lock_addr);
        }
    }
}

/// `MatchFirst` (`D1` = pattern `CString*`, `D2` = `AnchorPath*`,
/// caller-allocated and initialized per the module docs). `D0` =
/// `0` on success (an `IoErr()`-style error code otherwise -- `D0`
/// itself carries the error, unlike most `dos.library` calls).
fn match_first_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let pat_ptr = ctx.cpu.data_register(DataRegister(1));
    let ap_addr = ctx.cpu.data_register(DataRegister(2));
    let pat_bytes = read_c_string(ctx.mem, pat_ptr);
    let result = match_first(ctx.heap, ctx.mem, ctx.dos, &pat_bytes, ap_addr);
    ctx.cpu.set_data_register(
        DataRegister(0),
        match result {
            Ok(()) => 0,
            Err(code) => code as u32,
        },
    );
    Ok(())
}

/// `MatchNext` (`D1` = `AnchorPath*`). `D0` = `0` on success (error code
/// otherwise), same convention as `MatchFirst`.
fn match_next_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let ap_addr = ctx.cpu.data_register(DataRegister(1));
    let result = match_next(ctx.heap, ctx.mem, ctx.dos, ap_addr);
    ctx.cpu.set_data_register(
        DataRegister(0),
        match result {
            Ok(()) => 0,
            Err(code) => code as u32,
        },
    );
    Ok(())
}

/// `MatchEnd` (`D1` = `AnchorPath*`). No return value.
fn match_end_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let ap_addr = ctx.cpu.data_register(DataRegister(1));
    match_end(ctx.heap, ctx.dos, ap_addr);
    Ok(())
}

/// Registers `MatchFirst`/`MatchNext`/`MatchEnd` onto [`DOS_LIBRARY_BASE`],
/// looked up by name through [`DOS_LVOS`]. Called from
/// [`crate::dispatch::Runtime::new`] alongside the other `dos.library`
/// registrations.
pub fn register_dosanchor_handlers<C: Cpu + 'static>(
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
    reg!("MatchFirst", match_first_handler::<C>);
    reg!("MatchNext", match_next_handler::<C>);
    reg!("MatchEnd", match_end_handler::<C>);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::FlatMemory;
    use crate::vfs::{Vfs, VfsConfig};
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let path = std::env::temp_dir().join(format!("volamos-dosanchor-test-{tag}-{pid}-{n}"));
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

    /// Allocates and zeroes an `AnchorPath` with room for a `strlen`-byte
    /// `ap_Buf` tail, setting `ap_Strlen` accordingly.
    fn alloc_ap(heap: &mut GuestHeap, mem: &mut FlatMemory, strlen: u16) -> u32 {
        let size = AP_BUF_OFFSET + u32::from(strlen);
        let addr = heap.alloc(size).unwrap();
        for i in 0..size {
            mem.write_u8(addr + i, 0);
        }
        mem.write_u16(addr + AP_STRLEN_OFFSET, strlen);
        addr
    }

    fn fib_name(mem: &FlatMemory, ap_addr: u32) -> Vec<u8> {
        read_c_string(mem, ap_addr + AP_INFO_OFFSET + 8) // FIB_FILENAME_OFFSET
    }

    fn flags(mem: &FlatMemory, ap_addr: u32) -> u8 {
        mem.read_u8(ap_addr + AP_FLAGS_OFFSET)
    }

    #[test]
    fn non_wildcard_match_locks_the_object_directly() {
        let tmp = TempDir::new("nonwild");
        fs::write(tmp.path().join("hello.txt"), b"hi").unwrap();
        let (mut heap, mut mem, mut dos) = setup(tmp.path());
        let ap = alloc_ap(&mut heap, &mut mem, 0);

        match_first(&mut heap, &mut mem, &mut dos, b"SYS:hello.txt", ap).expect("match");
        assert_eq!(fib_name(&mem, ap), b"hello.txt");
        assert_eq!(
            mem.read_u32(ap + AP_BASE_OFFSET),
            mem.read_u32(ap + AP_LAST_OFFSET)
        );
        assert_eq!(flags(&mem, ap) & APF_ITSWILD, 0, "no wildcard was used");

        let err = match_next(&mut heap, &mut mem, &mut dos, ap).unwrap_err();
        assert_eq!(err, ERROR_NO_MORE_ENTRIES);

        match_end(&mut heap, &mut dos, ap);
        assert!(dos.locks.is_empty());
    }

    #[test]
    fn non_wildcard_missing_object_is_object_not_found() {
        let tmp = TempDir::new("nonwild-missing");
        let (mut heap, mut mem, mut dos) = setup(tmp.path());
        let ap = alloc_ap(&mut heap, &mut mem, 0);
        let err = match_first(&mut heap, &mut mem, &mut dos, b"SYS:missing.txt", ap).unwrap_err();
        assert_eq!(err, ERROR_OBJECT_NOT_FOUND);
    }

    #[test]
    fn wildcard_match_iterates_sorted_filtered_entries() {
        let tmp = TempDir::new("wildcard");
        fs::create_dir(tmp.path().join("work")).unwrap();
        fs::write(tmp.path().join("work/b.txt"), b"b").unwrap();
        fs::write(tmp.path().join("work/a.txt"), b"a").unwrap();
        fs::write(tmp.path().join("work/c.txt"), b"c").unwrap();
        fs::write(tmp.path().join("work/skip.dat"), b"x").unwrap();
        let (mut heap, mut mem, mut dos) = setup(tmp.path());
        let ap = alloc_ap(&mut heap, &mut mem, 0);

        match_first(&mut heap, &mut mem, &mut dos, b"SYS:work/#?.txt", ap).expect("match");
        assert_eq!(fib_name(&mem, ap), b"a.txt");
        assert_ne!(flags(&mem, ap) & APF_ITSWILD, 0, "pattern had a wildcard");

        match_next(&mut heap, &mut mem, &mut dos, ap).expect("second");
        assert_eq!(fib_name(&mem, ap), b"b.txt");

        match_next(&mut heap, &mut mem, &mut dos, ap).expect("third");
        assert_eq!(fib_name(&mem, ap), b"c.txt");

        let err = match_next(&mut heap, &mut mem, &mut dos, ap).unwrap_err();
        assert_eq!(err, ERROR_NO_MORE_ENTRIES);

        match_end(&mut heap, &mut dos, ap);
        assert!(dos.locks.is_empty());
    }

    #[test]
    fn wildcard_no_match_is_no_more_entries() {
        let tmp = TempDir::new("wildcard-empty");
        fs::create_dir(tmp.path().join("work")).unwrap();
        fs::write(tmp.path().join("work/x.dat"), b"x").unwrap();
        let (mut heap, mut mem, mut dos) = setup(tmp.path());
        let ap = alloc_ap(&mut heap, &mut mem, 0);
        let err = match_first(&mut heap, &mut mem, &mut dos, b"SYS:work/#?.txt", ap).unwrap_err();
        assert_eq!(err, ERROR_NO_MORE_ENTRIES);
        // MatchFirst's own failure path must not leave a dangling lock.
        assert!(dos.locks.is_empty());
    }

    #[test]
    fn ap_buf_is_filled_with_the_full_path_when_strlen_is_set() {
        let tmp = TempDir::new("apbuf");
        fs::write(tmp.path().join("hello.txt"), b"hi").unwrap();
        let (mut heap, mut mem, mut dos) = setup(tmp.path());
        let ap = alloc_ap(&mut heap, &mut mem, 64);

        match_first(&mut heap, &mut mem, &mut dos, b"SYS:hello.txt", ap).expect("match");
        let path = read_c_string(&mem, ap + AP_BUF_OFFSET);
        assert_eq!(path, b"SYS:hello.txt");
    }

    #[test]
    fn ap_buf_overflow_is_reported_but_still_truncated() {
        let tmp = TempDir::new("apbuf-overflow");
        fs::write(tmp.path().join("hello.txt"), b"hi").unwrap();
        let (mut heap, mut mem, mut dos) = setup(tmp.path());
        let ap = alloc_ap(&mut heap, &mut mem, 4); // "SYS:hello.txt" doesn't fit in 4

        let err = match_first(&mut heap, &mut mem, &mut dos, b"SYS:hello.txt", ap).unwrap_err();
        assert_eq!(err, ERROR_BUFFER_OVERFLOW);
        // The FIB itself should still have been filled in correctly.
        assert_eq!(fib_name(&mem, ap), b"hello.txt");
    }

    #[test]
    fn recursive_dodir_descent_reapplies_pattern_and_signals_diddir() {
        let tmp = TempDir::new("recursive");
        fs::create_dir_all(tmp.path().join("root/sub")).unwrap();
        fs::write(tmp.path().join("root/top.txt"), b"top").unwrap();
        fs::write(tmp.path().join("root/sub/inner.txt"), b"inner").unwrap();
        let (mut heap, mut mem, mut dos) = setup(tmp.path());
        let ap = alloc_ap(&mut heap, &mut mem, 0);

        // "sub" < "top.txt" byte-wise, so the directory comes first.
        match_first(&mut heap, &mut mem, &mut dos, b"SYS:root/#?", ap).expect("match");
        assert_eq!(fib_name(&mem, ap), b"sub");

        // Ask to descend into it.
        set_flag_bit(&mut mem, ap, APF_DODIR, true);
        match_next(&mut heap, &mut mem, &mut dos, ap).expect("descend");
        assert_eq!(fib_name(&mem, ap), b"inner.txt", "should scan inside sub/");
        assert_eq!(
            flags(&mem, ap) & APF_DODIR,
            0,
            "APF_DODIR is cleared on entry"
        );

        // sub/ has only one matching entry, so the next call pops back
        // out, restoring ap_Info to "sub" itself and signaling DIDDIR.
        match_next(&mut heap, &mut mem, &mut dos, ap).expect("leave sub");
        assert_ne!(flags(&mem, ap) & APF_DIDDIR, 0);
        assert_eq!(fib_name(&mem, ap), b"sub", "ap_Info shows the exited dir");

        // Caller clears APF_DIDDIR and continues; next entry is top.txt.
        set_flag_bit(&mut mem, ap, APF_DIDDIR, false);
        match_next(&mut heap, &mut mem, &mut dos, ap).expect("top.txt");
        assert_eq!(fib_name(&mem, ap), b"top.txt");

        let err = match_next(&mut heap, &mut mem, &mut dos, ap).unwrap_err();
        assert_eq!(err, ERROR_NO_MORE_ENTRIES);

        match_end(&mut heap, &mut dos, ap);
        assert!(dos.locks.is_empty(), "MatchEnd should unlock every level");
    }

    // --- End-to-end: real A-line trap dispatch ---

    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{Runtime, StartConfig};
    use crate::guestmem::write_c_string;

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
    fn load_words(mem: &mut FlatMemory, addr: u32, words: &[u16]) {
        let mut offset = addr;
        for &w in words {
            mem.write_u16(offset, w);
            offset += 2;
        }
    }

    #[test]
    fn end_to_end_match_first_via_trap_dispatch() {
        let tmp = TempDir::new("e2e");
        fs::write(tmp.path().join("hello.txt"), b"hi").unwrap();

        // D1 = pattern, D2 = AnchorPath*; jsr MatchFirst(a6); D0 (== the
        // exit code) should be 0 (success).
        let mut words = Vec::new();
        let pat_idx = push_move_imm_to_d(&mut words, 1, 0);
        let ap_idx = push_move_imm_to_d(&mut words, 2, 0);
        push_jsr(&mut words, 6, -822); // MatchFirst(a6)
        words.push(RTS);

        let pat = b"SYS:hello.txt";
        let pat_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        let ap_addr = (pat_addr + pat.len() as u32 + 1 + 3) & !3;
        patch_imm32(&mut words, pat_idx, pat_addr);
        patch_imm32(&mut words, ap_idx, ap_addr);

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);
        write_c_string(&mut mem, pat_addr, pat);
        for i in 0..AP_BUF_OFFSET {
            mem.write_u8(ap_addr + i, 0);
        }

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: ap_addr + AP_BUF_OFFSET,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        rt.set_vfs(vfs_over(tmp.path()));
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 0, "MatchFirst should report success");
        assert_eq!(
            read_c_string(rt.memory(), ap_addr + AP_INFO_OFFSET + 8),
            b"hello.txt"
        );
    }
}
