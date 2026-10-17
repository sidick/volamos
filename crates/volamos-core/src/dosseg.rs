//! `dos.library` `LoadSeg`/`UnLoadSeg` (real `BPTR` seglist loading into
//! guest memory) and `SystemTagList`/`Execute` (Phase 3 stage 7).
//!
//! # Seglist memory layout
//!
//! A real AmigaOS "seglist" is a singly-linked chain of memory blocks, one
//! per hunk of the loaded executable. Each block is laid out, per the
//! documented AmigaOS convention, as:
//!
//! ```text
//!   addr + 0   ULONG  seg_length   total length of THIS allocation, in
//!                                  bytes (header included)
//!   addr + 4   BPTR   next_seg     BPTR of the next segment's `next_seg`
//!                                  field, or 0 for the last segment
//!   addr + 8   ...    payload      the hunk's own code/data/bss content
//! ```
//!
//! The `BPTR` a caller holds for a segment -- what [`LoadSeg`] returns in
//! `D0`, what's stored at the *previous* segment's `next_seg` field, and
//! what `UnLoadSeg`/`InternalUnLoadSeg` take as input -- points at the
//! `next_seg` field itself (`addr + 4`, i.e. `bptr_from_addr(addr + 4)`),
//! not at `addr`. This is the real AmigaOS convention (`BPTR seg =
//! LoadSeg(...)`; the *executable code* starts at `(char *)BADDR(seg) +
//! sizeof(BPTR)`, i.e. four bytes past what the BPTR addresses, which is
//! itself four bytes past the allocation's true start where `seg_length`
//! lives) -- not a simplification this runtime invented. [`SEG_HEADER_SIZE`]
//! (8) is `addr`-to-payload; the BPTR-to-payload distance is 4 (just
//! `next_seg`'s own width), matching `(char*)BADDR(seg) + 4`.
//!
//! [`build_seglist`] allocates each hunk as its own [`GuestHeap`] block
//! (via [`GuestHeap::alloc`]), writes `seg_length` as whatever
//! [`GuestHeap::size_of_live_alloc`] reports back for that block (the
//! *actual*, possibly-4-byte-rounded-up allocation size -- not the raw
//! `8 + reserved_size` request), copies in the hunk's code/data content
//! (zero-filling `BSS`, matching [`crate::loader::load`]'s own
//! zero-fill), and applies every `RELOC32` fixup against the *payload*
//! address of the target hunk (i.e. `alloc_addr + SEG_HEADER_SIZE`, not
//! the allocation's own start) -- the same "add the target's load address
//! to whatever addend is already there" semantics
//! [`crate::loader::load`] uses, just relative to a per-hunk heap
//! allocation instead of one contiguous placement. The entry point (the
//! address a real `RunCommand`/`CreateProc` would jump to) is the first
//! segment's payload address; this module doesn't itself execute
//! anything (`LoadSeg` never does, on real AmigaOS either -- it just
//! loads), so [`SegList::entry`] exists for tests/future callers to read,
//! not because any handler here consumes it.
//!
//! # State tracking: [`DosState::seglists`]
//!
//! [`crate::dosfile::DosState`] gains a `seglists: HashMap<u32, Vec<u32>>`
//! field (declared in `dosfile.rs`, per that module's own "fields here,
//! methods in the sibling module" convention already used for T11's
//! locks/`ExNext` state) mapping a live seglist's first-segment `BPTR`
//! (exactly the `D0` value [`LoadSeg`] returned, and so exactly the value
//! a guest passes back to `UnLoadSeg`) to every segment's guest-heap
//! *allocation address* (not `BPTR`) in load order. [`DosState::
//! unload_seg`] uses this to free every segment's `GuestHeap` block
//! directly, without needing to re-walk the guest-memory `next_seg` chain
//! (which would also work, since the chain is real guest-visible state,
//! but the host-side map is simpler and cheaper, and gives `UnLoadSeg` a
//! ready "is this actually a live seglist?" check for its bug-catching
//! posture below).
//!
//! # Overlay executables
//!
//! `LoadSeg`ing a [`HunkFile`] whose [`HunkFile::overlay`] is `Some`
//! (see `crate::loader`'s module docs for the on-disk format, ground-
//! truthed against a real `SLink`-linked overlay binary) needs more than
//! [`build_seglist`]:
//!
//! - The `struct OverlayHeader` at the very start of hunk 0's payload
//!   (`oh_Jump`/`oh_Magic`/`oh_FileHandle`/`oh_OVTab`/`oh_Segments`/
//!   `oh_GV`, per the AmigaDOS Manual) needs its four loader-filled
//!   fields patched with real values -- this is what makes the overlay
//!   manager (guest code shipped inside hunk 0) able to load further
//!   nodes on demand.
//! - A *global* segment table (`oh_Segments`, one `BPTR` slot per hunk
//!   across the whole tree, sized from [`loader::OverlayInfo::
//!   total_hunks`]) has to live in guest memory, since overlay nodes
//!   loaded later need to land in the *same* array the root's own hunks
//!   populated, so relocations in later nodes can resolve against
//!   already-resident ancestor hunks.
//! - The `name`d file has to stay open for the program's lifetime (the
//!   overlay manager `Seek`s and re-reads it every time it loads a node)
//!   -- unlike a plain `LoadSeg`, which never needs the bytes again once
//!   the seglist is built. [`DosState::load_seg`] opens it through the
//!   ordinary [`DosState::open`] machinery (not a bare host `File`) so
//!   the resulting `BPTR` is a real, `Seek`/`Read`-able guest
//!   `FileHandle` the manager's own `Seek()` calls resolve through
//!   normally.
//!
//! [`DosState::overlay_roots`] (declared in `dosfile.rs`, per that
//! module's own convention) tracks all of this per overlay-root load,
//! keyed by the `oh_Segments` `BPTR` -- exactly the value the overlay
//! manager passes back as `D2` on every subsequent `LoadSeg(NULL, table,
//! fh)` call, per the documented calling convention (confirmed against
//! the real manager's own compiled code: `D1 = 0`, `D2 = oh_Segments`,
//! `D3 = oh_FileHandle`). [`loadseg_handler`] detects `D1 == 0` and
//! routes to [`DosState::load_overlay_node`] instead of the ordinary
//! named-file path.
//!
//! [`DosState::load_overlay_node`] re-reads the *whole* host file (rather
//! than tracking a live cursor into the already-open `FileHandle`) each
//! time it loads a node: it only needs to know the current file position
//! (read via [`DosState::seek`] with a zero-delta `OFFSET_CURRENT`
//! `Seek`, matching what the manager itself just did before calling
//! `LoadSeg`) and [`loader::parse_overlay_node`] wants a byte slice, not
//! a stream -- simpler than threading incremental reads through, at the
//! cost of one redundant full-file read per node load. Revisit if a
//! corpus binary's overlay tree is large/deep enough for that to matter.
//!
//! **Scope cut**: `UnLoadSeg` on an overlay root only frees the root's
//! own segments today (via the ordinary [`DosState::seglists`] entry
//! [`DosState::load_seg`] also registers for it) -- it doesn't yet walk
//! [`DosState::overlay_roots`] to also free loaded node segments, the
//! `oh_Segments` array, the `oh_OVTab` copy, or close `oh_FileHandle`,
//! unlike real `UnLoadSeg`'s magic-`0xABCD`-triggered cleanup (AmigaDOS
//! Manual section 1.4.8). Not yet needed by any corpus binary (which
//! `LoadSeg`s and runs to completion without ever unloading); revisit
//! alongside real overlay-node *unloading* (a currently-loaded node being
//! displaced by a sibling), which needs the same bookkeeping.
//!
//! # `UnLoadSeg`: loud failure on an unknown seglist
//!
//! Real `UnLoadSeg` trusts its argument -- an unknown/already-freed BPTR
//! is undefined behavior. Per this runtime's bug-catching posture
//! (matching `execmem.rs`'s `FreeMem` size-mismatch check, see that
//! module's docs), [`DosState::unload_seg`] fails loudly
//! ([`DispatchError::HandlerFailed`], aborting the run) rather than
//! silently no-opping or corrupting the heap, if `bptr` isn't a key in
//! `seglists` -- except for `bptr == 0`, which is a *documented* legal
//! no-op (matching real `UnLoadSeg(0)`, and every other `0`/`NULL`
//! BPTR-taking call in this codebase, e.g. `UnLock(0)`).
//!
//! Real `UnLoadSeg`'s own Autodoc return type is effectively void (no
//! caller-visible contract on `D0`); this implementation writes AmigaOS
//! `BOOL` `TRUE` (`0xFFFFFFFF`) to `D0` on success anyway, matching this
//! codebase's own `DOSTRUE`/`DOSFALSE` convention for every other
//! success/failure dos.library call (`Close`, ...) -- a documented,
//! harmless choice since no real caller depends on `UnLoadSeg`'s return
//! value, not an attempt to match an ABI contract that doesn't exist.
//!
//! # `System()`/`Execute()` architecture: a host-side runner hook
//!
//! `System()`/`SystemTagList()`/`Execute()` need to run *another* guest
//! program to completion and report back its result -- effectively a
//! nested [`crate::dispatch::Runtime`] invocation. That's awkward to do
//! generically from inside a [`crate::dispatch::LibraryHandler`]: a
//! handler is generic over `C: Cpu` (via [`HandlerContext`]), but
//! building a *new* `Runtime<C>` needs a fresh `C` and a fresh
//! `C::Memory`, and `Cpu` has no "make me a new one" constructor
//! requirement -- baking one in would burden every other, unrelated `Cpu`
//! implementation (e.g. a future `r68k` backend, per `docs/plan.md`'s own
//! T2 note) with a concern only this one call family needs.
//!
//! So, per `docs/plan.md`'s own suggested design (option (b), "route
//! through a host-side callback"): [`DosState::system_runner`] is an
//! `Option<Box<dyn FnMut(&SystemRequest) -> i32>>` -- deliberately *not*
//! generic over `C`, since a plain closure only needs to know how to run
//! a resolved host path to completion and hand back an exit code, not
//! anything about the *calling* program's own CPU backend. A CLI (never
//! library code) installs one via [`crate::dispatch::Runtime::
//! set_system_runner`] after constructing its `Runtime`; the closure it
//! installs is free to build a brand-new `M68kCpu`/`FlatMemory`/`Runtime`
//! internally (sharing whatever `Vfs` configuration the CLI was given)
//! and actually run the nested program. `crates/volamos/src/main.rs`
//! does exactly this: `run_nested_program` loads `req.
//! resolved_program_host_path` through the ordinary
//! [`crate::loader::load`] path (a full separate guest address space --
//! *not* reusing this module's [`build_seglist`] seglist framing, since
//! that's a different in-guest-memory representation meant for `LoadSeg`
//! callers, not for actually executing a program), builds a fresh
//! `Runtime`, and runs it to completion, writing to `std::io::stdout()`
//! directly rather than threading the parent run's `out` sink through
//! (see the next section for why).
//!
//! In a library-only context (no CLI, e.g. `volamos-core`'s own unit
//! tests) no runner is ever installed, so `System`/`Execute` always take
//! the "couldn't run" path below -- this is a deliberate, documented
//! choice per `docs/plan.md`: a guest program can legitimately handle a
//! failed `System()` (many real programs check its return value), so
//! failing cleanly with an `IoErr()` rather than aborting the whole
//! [`crate::dispatch::Runtime::run`] loud-failure-style (the way
//! `UnLoadSeg` does above for a *host-side bug*, as opposed to this being
//! an entirely expected, guest-visible outcome) is the more defensible
//! choice; see [`DosState::resolve_and_run`].
//!
//! # Scope cuts (documented, not silent)
//!
//! - **Input/output redirection**: `Execute`'s `D2`/`D3` (input/output
//!   `BPTR`s) and `SystemTagList`'s tag list (`D2`, e.g. `SYS_Input`/
//!   `SYS_Output`) are read off the guest registers (so a well-behaved
//!   caller's calling convention isn't violated) but never acted on. A
//!   nested program's output goes to the same place the *parent* CLI
//!   process's own stdout goes (`std::io::stdout()`, opened fresh inside
//!   the runner closure) -- not to whatever `out` sink the *parent guest
//!   program's* `Runtime::run` call happened to be given (e.g. a `Vec<u8>`
//!   a test is capturing into), since the runner closure has no access to
//!   that mid-run borrow. For the CLI's normal case (`out` *is*
//!   `stdout()`), this is behaviourally identical to real redirection-less
//!   `System()`; the only place it's observably different is a test that
//!   captures a parent run's output into an in-memory buffer and expects
//!   a nested program's output to show up there too -- it won't. Revisit
//!   if a corpus binary needs real `SYS_Input`/`SYS_Output`/`Execute`
//!   redirection.
//! - **`System()`'s exit code vs. `Execute()`'s success flag**: real
//!   `System()` returns the invoked command's own return code (or a
//!   negative value on failure to invoke it at all); real `Execute()`
//!   returns a `BOOL` -- whether the command was *successfully invoked*,
//!   independent of what it returned. [`DosState::system`]/[`DosState::
//!   execute`] implement exactly that split on top of the same shared
//!   [`DosState::resolve_and_run`].
//!
//! # `RunCommand()`: an overlay, not a nested `Runtime` (issues #8/#156)
//!
//! `RunCommand(seg, stack, paramptr, paramlen)` is **not** implemented on
//! [`DosState::system_runner`] above, unlike `System()`/`Execute()` --
//! and deliberately so. Real `RunCommand` runs a `LoadSeg`ed seglist as a
//! subroutine *in the calling process*: same task, same CPU register
//! state (the FPU's `FPCR`/`FPSR` included -- the child starts with
//! whatever the parent last left in them, and the parent gets back
//! whatever the child left), same address space, just a fresh stack and
//! `A0`/`D0` loaded with the caller's `argptr`/`argsize` verbatim (no
//! reformatting, same convention [`crate::dispatch::StartConfig::
//! raw_command_line`] documents). A brand-new `Runtime`/`CpuCore` -- what
//! `System()`/`Execute()` correctly use, since those really do spawn
//! independent processes on real hardware -- would always hand the child
//! a *fresh* `CpuCore` (zeroed `FPCR`/`FPSR` from construction) and never
//! let anything it does propagate back, which is exactly backwards for
//! `RunCommand`.
//!
//! [`run_command_handler`] gets this for free, on the *same* `ctx.cpu`/
//! `ctx.mem` the caller is already running on, via
//! [`crate::dispatch::ContinuationStack::trampoline`] -- the same
//! guest-subroutine-callout primitive the library/device-loading plan's
//! L3 `OpenLibrary` `initFunc`/`Open` sequence uses. The seglist is
//! already sitting in guest memory, relocated and ready (every segment's
//! allocation address is in [`DosState::seglists`], populated by
//! [`DosState::load_seg`]/[`build_seglist`]) -- no re-read from disk, no
//! separate address space, nothing [`DosState::system_runner`] needs to
//! know about at all. The handler:
//!
//! 1. Resolves the entry point from `seglists[seg][0] + `[`SEG_HEADER_SIZE`]
//!    (the first segment's payload address -- exactly [`SegList::entry`]'s
//!    own formula).
//! 2. Carves a fresh stack region of `stack` bytes (clamped to
//!    [`crate::guestmem::MIN_STACK_SIZE`], matching
//!    [`crate::dispatch::Runtime::new`]'s own clamp) from the *caller's*
//!    [`GuestHeap`], saving the current task's `A7`/`tc_SPLower`/
//!    `tc_SPUpper`/`pr_Arguments`/[`DosState::cmdline`]/`cmdline_pos`/
//!    `A6` first --
//!    same bookkeeping [`crate::exectask`]'s `StackSwap` handler already
//!    does for the same reason (a stack swap entirely within one
//!    `FlatMemory`, no new `Task` needed, since `RunCommand` overlays the
//!    *same* task).
//! 3. Points the task's stack bounds and `A0`/`D0`/`A6` at the new
//!    region/the caller's own `paramptr`/`paramlen` (not a copy -- real
//!    `RunCommand` never copies the buffer either), and trampolines to
//!    the entry point.
//! 4. The continuation (run once the invoked command's own final `rts`
//!    returns, via the trampoline's stub trap) reads `D0` as the exit
//!    code, restores every saved register/field, frees the stack
//!    region, and writes the exit code to `D0` for `RunCommand`'s own
//!    caller.
//!
//! `FPCR`/`FPSR` need no explicit save/restore at all: they're plain
//! `CpuCore` fields nothing else in this codebase ever touches, so simply
//! never constructing a fresh core is sufficient -- see issue #156's own
//! repro, which is exactly this.
//!
//! An unknown `seg` (never `LoadSeg`ed, or already `UnLoadSeg`ed) fails
//! cleanly with `IoErr(`[`ERROR_OBJECT_NOT_FOUND`]`)` and `D0 = -1`, the
//! same bucket [`DosState::resolve_and_run`] uses for `System()`/
//! `Execute()` -- a guest `RunCommand` call failing to invoke anything at
//! all is an expected, guest-visible outcome, not a host-side bug.

use std::path::PathBuf;

use crate::cpu::{AddressRegister, Cpu, DataRegister};
use crate::dispatch::{DOS_LIBRARY_BASE, DispatchError, HandlerContext, LibraryTable};
use crate::dosfile::ERROR_OBJECT_NOT_FOUND;
#[cfg(test)]
use crate::dosfile::OFFSET_BEGINNING;
use crate::dosfile::{
    DosState, ERROR_FILE_NOT_OBJECT, ERROR_NO_FREE_STORE, MODE_OLDFILE, OFFSET_CURRENT,
    map_io_error, map_vfs_error,
};
use crate::exectask::{PR_ARGUMENTS_OFFSET, TC_SPLOWER, TC_SPUPPER};
use crate::guestmem::{
    GuestHeap, GuestHeapError, MIN_STACK_SIZE, addr_from_bptr, bptr_from_addr, read_c_string,
};
use crate::loader::{self, HunkFile};
use crate::lvos::dos::DOS_LVOS;
use crate::memory::AddressSpace;
use crate::vfs::ResolveMode;

/// `Close`/boolean-success convention (see [`crate::dosfile`]'s own
/// copy): AmigaOS `BOOL` true is `-1` (`0xFFFFFFFF`), not `1`.
const DOSTRUE: u32 = 0xFFFF_FFFF;
/// `Close`/boolean-failure convention.
const DOSFALSE: u32 = 0;

/// Bytes of header (`seg_length` + `next_seg`) preceding every segment's
/// payload -- see the module docs' "Seglist memory layout" section. The
/// `BPTR` a caller actually holds addresses `SEG_HEADER_SIZE - 4` bytes
/// into this (just past `seg_length`), not the allocation's own start.
///
/// `pub(crate)` (not just module-private): [`crate::execlib::
/// find_resident`] walks this same BPTR chain by this same framing, and
/// reuses this constant rather than duplicating the magic number.
pub(crate) const SEG_HEADER_SIZE: u32 = 8;
/// Byte offset of `next_seg` within a segment allocation -- also where a
/// segment's `BPTR` points (`bptr_from_addr(alloc_addr + NEXT_SEG_OFFSET)`).
/// `pub(crate)` for the same reason as [`SEG_HEADER_SIZE`].
pub(crate) const NEXT_SEG_OFFSET: u32 = 4;

/// The in-guest-memory result of [`build_seglist`]: where execution would
/// begin (the first segment's payload address -- see the module docs;
/// unused by [`LoadSeg`] itself, kept for tests/future callers), the
/// `BPTR` a caller holds (`LoadSeg`'s `D0`, `UnLoadSeg`'s `D1`), and every
/// segment's allocation address in load order (for
/// [`DosState::unload_seg`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegList {
    /// Guest address of the first instruction of the first segment.
    pub entry: u32,
    /// The `BPTR` identifying this seglist (points at the first
    /// segment's `next_seg` field).
    pub first_bptr: u32,
    /// Every segment's guest-heap allocation address, in load order
    /// (index 0 is the first/entry segment).
    pub alloc_addrs: Vec<u32>,
}

/// Allocates one [`GuestHeap`] block per hunk in `file`, writes each
/// segment's header (`seg_length`/`next_seg`) and payload (code/data
/// verbatim, `BSS` zero-filled), chains them via `next_seg`, and applies
/// every `RELOC32` fixup against the *payload* addresses -- see the
/// module docs for the exact layout and relocation semantics.
///
/// On an allocation failure partway through, every segment already
/// allocated for this call is freed before returning
/// [`GuestHeapError::OutOfMemory`], so a failed `LoadSeg` doesn't leak
/// heap space.
pub(crate) fn build_seglist(
    file: &HunkFile,
    heap: &mut GuestHeap,
    mem: &mut dyn AddressSpace,
) -> Result<SegList, GuestHeapError> {
    debug_assert!(
        !file.hunks.is_empty(),
        "loader::parse never returns a HunkFile with zero hunks (LoadError::NoHunks instead)"
    );

    // Pass 1: allocate every segment, tracking both its allocation
    // address and its payload address (alloc + SEG_HEADER_SIZE).
    let mut alloc_addrs: Vec<u32> = Vec::with_capacity(file.hunks.len());
    for hunk in &file.hunks {
        let total = SEG_HEADER_SIZE.wrapping_add(hunk.reserved_size as u32);
        match heap.alloc(&mut *mem, total) {
            Ok(addr) => alloc_addrs.push(addr),
            Err(e) => {
                // Unwind: free everything allocated so far for this
                // LoadSeg attempt so a failure doesn't leak heap space.
                for &addr in &alloc_addrs {
                    let _ = heap.free(addr);
                }
                return Err(e);
            }
        }
    }
    let payload_addrs: Vec<u32> = alloc_addrs
        .iter()
        .map(|&a| a.wrapping_add(SEG_HEADER_SIZE))
        .collect();

    // Pass 2: write each segment's header and payload content, and chain
    // next_seg to the following segment's BPTR (0 for the last one).
    for (i, hunk) in file.hunks.iter().enumerate() {
        let alloc_addr = alloc_addrs[i];
        let payload_addr = payload_addrs[i];

        let seg_length = heap
            .size_of_live_alloc(alloc_addr)
            .unwrap_or(SEG_HEADER_SIZE.wrapping_add(hunk.reserved_size as u32));
        mem.write_u32(alloc_addr, seg_length);

        let next_bptr = alloc_addrs
            .get(i + 1)
            .map(|&next_addr| bptr_from_addr(next_addr.wrapping_add(NEXT_SEG_OFFSET)))
            .unwrap_or(0);
        mem.write_u32(alloc_addr.wrapping_add(NEXT_SEG_OFFSET), next_bptr);

        match hunk.kind {
            loader::HunkKind::Code | loader::HunkKind::Data => {
                for (off, &byte) in hunk.data.iter().enumerate() {
                    mem.write_u8(payload_addr.wrapping_add(off as u32), byte);
                }
                for off in hunk.data.len()..hunk.reserved_size {
                    mem.write_u8(payload_addr.wrapping_add(off as u32), 0);
                }
            }
            loader::HunkKind::Bss => {
                for off in 0..hunk.reserved_size {
                    mem.write_u8(payload_addr.wrapping_add(off as u32), 0);
                }
            }
        }
    }

    // Pass 3: relocations, against payload addresses (not allocation
    // addresses -- the hunk's own content, and hence its relocation
    // offsets, starts at the payload, exactly matching
    // crate::loader::load's own semantics relative to its (differently
    // placed) hunk base addresses).
    for (i, hunk) in file.hunks.iter().enumerate() {
        for reloc in &hunk.relocs {
            let loc = payload_addrs[i].wrapping_add(reloc.offset);
            let target_addr = payload_addrs[reloc.target_hunk];
            let existing = mem.read_u32(loc);
            mem.write_u32(loc, existing.wrapping_add(target_addr));
        }
    }

    Ok(SegList {
        entry: payload_addrs[0],
        first_bptr: bptr_from_addr(alloc_addrs[0].wrapping_add(NEXT_SEG_OFFSET)),
        alloc_addrs,
    })
}

// --- Overlay executables (HUNK_OVERLAY) -- see the module docs. ---

/// Byte offsets of the loader-filled fields within `struct
/// OverlayHeader`, relative to the start of hunk 0's payload -- see the
/// AmigaDOS Manual's "Overlay File Format" section (ground-truthed
/// against a real overlay executable's disassembled manager, which reads
/// every one of these at exactly these offsets).
mod overlay_header {
    /// `oh_Magic`: must already hold `0x0000ABCD` in the file itself
    /// (not loader-filled, but checked as a sanity guard before writing
    /// the fields after it).
    pub(super) const MAGIC_OFFSET: u32 = 4;
    pub(super) const MAGIC_VALUE: u32 = 0x0000_ABCD;
    pub(super) const FILE_HANDLE_OFFSET: u32 = 8;
    pub(super) const OVTAB_OFFSET: u32 = 0x0c;
    pub(super) const SEGMENTS_OFFSET: u32 = 0x10;
    pub(super) const GV_OFFSET: u32 = 0x14;
}

/// Live state for one `LoadSeg`ed overlay root, keyed (in
/// [`DosState::overlay_roots`]) by the `BPTR` of its `oh_Segments` array
/// -- see the module docs.
#[derive(Debug)]
pub struct OverlayRootState {
    /// Host path to re-read from on every subsequent node load (see the
    /// module docs for why this re-reads rather than tracking a live
    /// cursor).
    host_path: PathBuf,
    /// Guest address of the `oh_Segments` array's first slot (not its
    /// `BPTR` -- the map key already carries that).
    segments_array_addr: u32,
    /// Every hunk's payload address across the whole tree, by *global*
    /// hunk index -- `None` for a not-yet-loaded overlay node's hunks.
    /// Used to resolve `RELOC32` targets in later-loaded nodes that
    /// reference already-resident ancestor hunks.
    payload_addrs: Vec<Option<u32>>,
}

/// Builds an overlay root's seglist: allocates and links the root's own
/// hunks exactly like [`build_seglist`] (reused directly), then also
/// allocates the global `oh_Segments` array and the `oh_OVTab` copy in
/// guest memory, and patches hunk 0's `OverlayHeader` fields (see the
/// module docs). `file_handle_bptr` is the already-open guest
/// `FileHandle` `BPTR` (see [`DosState::open`]) to record as
/// `oh_FileHandle`.
///
/// Returns the root [`SegList`] (exactly as [`build_seglist`] would) and
/// the [`OverlayRootState`] to register under the `oh_Segments` `BPTR`
/// (`bptr_from_addr(state.segments_array_addr)`).
fn build_overlay_root_seglist(
    file: &HunkFile,
    overlay: &loader::OverlayInfo,
    file_handle_bptr: u32,
    host_path: PathBuf,
    heap: &mut GuestHeap,
    mem: &mut dyn AddressSpace,
) -> Result<(SegList, OverlayRootState), GuestHeapError> {
    let seglist = build_seglist(file, heap, mem)?;

    let root_payload_addrs: Vec<u32> = seglist
        .alloc_addrs
        .iter()
        .map(|&a| a.wrapping_add(SEG_HEADER_SIZE))
        .collect();

    // oh_OVTab: a plain copy of the HUNK_OVERLAY payload in guest memory
    // (not a BPTR -- the manager indexes into it with byte offsets
    // directly, per the disassembly-confirmed convention).
    let ovtab_bytes = overlay.table.raw.len() as u32 * 4;
    let ovtab_addr = match heap.alloc(&mut *mem, ovtab_bytes.max(4)) {
        Ok(a) => a,
        Err(e) => {
            for &addr in &seglist.alloc_addrs {
                let _ = heap.free(addr);
            }
            return Err(e);
        }
    };
    for (i, &word) in overlay.table.raw.iter().enumerate() {
        mem.write_u32(ovtab_addr.wrapping_add(i as u32 * 4), word);
    }

    // oh_Segments: one BPTR slot per hunk in the whole tree, zeroed for
    // not-yet-loaded overlay-node hunks, filled in for the root's own.
    let segments_bytes = overlay.total_hunks as u32 * 4;
    let segments_addr = match heap.alloc(&mut *mem, segments_bytes.max(4)) {
        Ok(a) => a,
        Err(e) => {
            let _ = heap.free(ovtab_addr);
            for &addr in &seglist.alloc_addrs {
                let _ = heap.free(addr);
            }
            return Err(e);
        }
    };
    let mut payload_addrs: Vec<Option<u32>> = vec![None; overlay.total_hunks];
    for (i, &alloc_addr) in seglist.alloc_addrs.iter().enumerate() {
        let seg_bptr = bptr_from_addr(alloc_addr.wrapping_add(NEXT_SEG_OFFSET));
        mem.write_u32(segments_addr.wrapping_add(i as u32 * 4), seg_bptr);
        payload_addrs[i] = Some(root_payload_addrs[i]);
    }
    for i in seglist.alloc_addrs.len()..overlay.total_hunks {
        mem.write_u32(segments_addr.wrapping_add(i as u32 * 4), 0);
    }

    // Patch hunk 0's OverlayHeader. hunk 0 is always the root's entry
    // hunk (seglist.entry is its payload address).
    let hunk0_payload = seglist.entry;
    debug_assert_eq!(
        mem.read_u32(hunk0_payload.wrapping_add(overlay_header::MAGIC_OFFSET)),
        overlay_header::MAGIC_VALUE,
        "an overlay-root HunkFile's hunk 0 should always start with the 0x0000ABCD magic"
    );
    mem.write_u32(
        hunk0_payload.wrapping_add(overlay_header::FILE_HANDLE_OFFSET),
        file_handle_bptr,
    );
    mem.write_u32(
        hunk0_payload.wrapping_add(overlay_header::OVTAB_OFFSET),
        ovtab_addr,
    );
    let segments_bptr = bptr_from_addr(segments_addr);
    mem.write_u32(
        hunk0_payload.wrapping_add(overlay_header::SEGMENTS_OFFSET),
        segments_bptr,
    );
    // oh_GV: the BCPL global vector. Unused by C/asm managers (this
    // runtime has no BCPL global-vector support at all); left 0.
    mem.write_u32(hunk0_payload.wrapping_add(overlay_header::GV_OFFSET), 0);

    let state = OverlayRootState {
        host_path,
        segments_array_addr: segments_addr,
        payload_addrs,
    };
    Ok((seglist, state))
}

/// A resolved `System()`/`Execute()` request, handed to the host-side
/// [`DosState::system_runner`] callback -- see the module docs'
/// "`System()`/`Execute()` architecture" section.
#[derive(Debug, Clone)]
pub struct SystemRequest {
    /// The command line exactly as the guest passed it to `D1` (untouched
    /// -- e.g. useful for a runner that wants to log it).
    pub command_line: String,
    /// The first whitespace-separated token of `command_line`, resolved
    /// through the calling process's [`crate::vfs::Vfs`] to a real host
    /// path -- this is what the runner should actually execute.
    pub resolved_program_host_path: PathBuf,
    /// The remaining whitespace-separated tokens of `command_line` (i.e.
    /// everything after the program name), unparsed beyond that
    /// splitting -- no quoting/escaping support, matching this module's
    /// documented scope cuts.
    pub args: Vec<String>,
}

/// The host-side `System()`/`Execute()` runner callback's type -- what
/// [`DosState::system_runner`] holds (see the module docs' "`System()`/
/// `Execute()` architecture" section). A `type` alias so the field's
/// declaration stays readable (and clippy's `type_complexity` lint
/// satisfied) without changing the shape of the hook itself.
pub type SystemRunner = Box<dyn FnMut(&SystemRequest) -> i32>;

impl DosState {
    /// `LoadSeg(name)`: resolves `name` through `self.vfs` (must already
    /// exist, matching `Open(MODE_OLDFILE)`'s semantics -- see
    /// [`crate::dosfile`]'s own "No VFS configured" note, which applies
    /// here identically), reads it, parses it as a hunk executable, and
    /// builds a seglist for it via [`build_seglist`] -- or, if it's an
    /// overlay executable's root node, via [`build_overlay_root_seglist`]
    /// (see the module docs' "Overlay executables" section). Returns the
    /// seglist's `BPTR` on success, or an `IoErr()` code on failure:
    /// [`crate::dosfile::ERROR_OBJECT_NOT_FOUND`] (no `Vfs`, or the path
    /// doesn't resolve), a mapped host I/O error, or
    /// [`crate::dosfile::ERROR_FILE_NOT_OBJECT`] if the file exists but
    /// doesn't parse as a hunk executable.
    pub fn load_seg(
        &mut self,
        heap: &mut GuestHeap,
        mem: &mut dyn AddressSpace,
        name: &str,
    ) -> Result<u32, i32> {
        let vfs = self.vfs.as_ref().ok_or(ERROR_OBJECT_NOT_FOUND)?;
        let host_path = vfs
            .resolve(name, ResolveMode::MustExist)
            .map_err(|e| map_vfs_error(&e))?;
        let bytes = std::fs::read(&host_path).map_err(|e| map_io_error(&e))?;
        let hunk_file = loader::parse(&bytes).map_err(|_| ERROR_FILE_NOT_OBJECT)?;

        if let Some(overlay) = &hunk_file.overlay {
            // Overlay root: needs a real, Seek/Read-able guest FileHandle
            // the manager's own Seek() calls will resolve through later
            // (not just the raw bytes we already have) -- see the module
            // docs.
            let file_handle_bptr = self.open(heap, mem, name, MODE_OLDFILE)?;
            let (seglist, state) = build_overlay_root_seglist(
                &hunk_file,
                overlay,
                file_handle_bptr,
                host_path,
                heap,
                mem,
            )
            .map_err(|_| ERROR_NO_FREE_STORE)?;
            let segments_bptr = bptr_from_addr(state.segments_array_addr);
            self.overlay_roots.insert(segments_bptr, state);
            self.seglists
                .insert(seglist.first_bptr, seglist.alloc_addrs);
            return Ok(seglist.first_bptr);
        }

        let seglist = build_seglist(&hunk_file, heap, mem).map_err(|_| ERROR_NO_FREE_STORE)?;
        self.seglists
            .insert(seglist.first_bptr, seglist.alloc_addrs);
        Ok(seglist.first_bptr)
    }

    /// Loads `host_path` directly (no `Vfs` resolution) into a seglist,
    /// exactly like [`Self::load_seg`] but for the one caller that has a
    /// host path with no corresponding Amiga name to begin with:
    /// [`crate::dispatch::Runtime`]'s top-level-program launch (see
    /// [`crate::dosfile::DosState::open_host_file_direct`]'s doc, and
    /// the module docs' "Overlay executables" section for why an overlay
    /// root specifically needs a real guest `FileHandle` at all). Same
    /// error/return contract as [`Self::load_seg`].
    pub fn load_seg_from_host_path(
        &mut self,
        heap: &mut GuestHeap,
        mem: &mut dyn AddressSpace,
        host_path: &std::path::Path,
    ) -> Result<u32, i32> {
        let bytes = std::fs::read(host_path).map_err(|e| map_io_error(&e))?;
        let hunk_file = loader::parse(&bytes).map_err(|_| ERROR_FILE_NOT_OBJECT)?;

        if let Some(overlay) = &hunk_file.overlay {
            let file_handle_bptr = self.open_host_file_direct(heap, mem, host_path)?;
            let (seglist, state) = build_overlay_root_seglist(
                &hunk_file,
                overlay,
                file_handle_bptr,
                host_path.to_path_buf(),
                heap,
                mem,
            )
            .map_err(|_| ERROR_NO_FREE_STORE)?;
            let segments_bptr = bptr_from_addr(state.segments_array_addr);
            self.overlay_roots.insert(segments_bptr, state);
            self.seglists
                .insert(seglist.first_bptr, seglist.alloc_addrs);
            return Ok(seglist.first_bptr);
        }

        let seglist = build_seglist(&hunk_file, heap, mem).map_err(|_| ERROR_NO_FREE_STORE)?;
        self.seglists
            .insert(seglist.first_bptr, seglist.alloc_addrs);
        Ok(seglist.first_bptr)
    }

    /// `LoadSeg(NULL, table, fh)` -- the overlay-continuation form a
    /// resident overlay manager calls to load one more node once it's
    /// `Seek()`ed `fh` to the node's `HUNK_HEADER` file offset (see the
    /// module docs' "Overlay executables" section, and the AmigaDOS
    /// Manual's "Loading an Overlaid Node"). `table` is the `oh_Segments`
    /// `BPTR` (looked up in [`Self::overlay_roots`]); `fh` is the guest
    /// `FileHandle` `BPTR` [`Self::load_seg`] opened for this root.
    ///
    /// Allocates and links the node's own hunks (their `next_seg` chain
    /// starts fresh -- linking them after the segment table's existing
    /// entries at `ot_FirstSegment` is the *manager's* job, not
    /// `LoadSeg`'s, per the Manual), resolves their relocations (which
    /// may target already-resident ancestor hunks), and writes each new
    /// hunk's segment `BPTR` into the shared global `oh_Segments` array.
    /// Returns the first new segment's `BPTR`, or `0` with `IoErr()` set
    /// on failure ([`crate::dosfile::ERROR_OBJECT_NOT_FOUND`] for an
    /// unknown `table`/`fh`, a mapped host I/O error, or
    /// [`crate::dosfile::ERROR_FILE_NOT_OBJECT`] if the node doesn't
    /// parse, or its relocations target a hunk that isn't loaded).
    pub fn load_overlay_node(
        &mut self,
        heap: &mut GuestHeap,
        mem: &mut dyn AddressSpace,
        table_bptr: u32,
        fh_bptr: u32,
    ) -> Result<u32, i32> {
        let state = self
            .overlay_roots
            .get(&table_bptr)
            .ok_or(ERROR_OBJECT_NOT_FOUND)?;
        let host_path = state.host_path.clone();

        let fh_addr = addr_from_bptr(fh_bptr);
        // A zero-delta OFFSET_CURRENT Seek reports (without moving) the
        // position the manager itself just Seek()ed fh to.
        let position = self.seek(fh_addr, 0, OFFSET_CURRENT)?;

        let bytes = std::fs::read(&host_path).map_err(|e| map_io_error(&e))?;
        let node = loader::parse_overlay_node(&bytes, position as usize)
            .map_err(|_| ERROR_FILE_NOT_OBJECT)?;

        // Pass 1: allocate every new hunk (mirroring build_seglist's own
        // passes, just targeting global segment-table slots instead of a
        // fresh 0-indexed array).
        let mut alloc_addrs: Vec<u32> = Vec::with_capacity(node.hunks.len());
        for hunk in &node.hunks {
            let total = SEG_HEADER_SIZE.wrapping_add(hunk.reserved_size as u32);
            match heap.alloc(&mut *mem, total) {
                Ok(addr) => alloc_addrs.push(addr),
                Err(_) => {
                    for &addr in &alloc_addrs {
                        let _ = heap.free(addr);
                    }
                    return Err(ERROR_NO_FREE_STORE);
                }
            }
        }
        let payload_addrs: Vec<u32> = alloc_addrs
            .iter()
            .map(|&a| a.wrapping_add(SEG_HEADER_SIZE))
            .collect();

        // Pass 2: header + payload content, chained within this node only
        // (next_seg = 0 for the node's own last hunk -- linking into the
        // rest of the tree is the manager's job).
        for (i, hunk) in node.hunks.iter().enumerate() {
            let alloc_addr = alloc_addrs[i];
            let payload_addr = payload_addrs[i];
            let seg_length = heap
                .size_of_live_alloc(alloc_addr)
                .unwrap_or(SEG_HEADER_SIZE.wrapping_add(hunk.reserved_size as u32));
            mem.write_u32(alloc_addr, seg_length);
            let next_bptr = alloc_addrs
                .get(i + 1)
                .map(|&next_addr| bptr_from_addr(next_addr.wrapping_add(NEXT_SEG_OFFSET)))
                .unwrap_or(0);
            mem.write_u32(alloc_addr.wrapping_add(NEXT_SEG_OFFSET), next_bptr);

            match hunk.kind {
                loader::HunkKind::Code | loader::HunkKind::Data => {
                    for (off, &byte) in hunk.data.iter().enumerate() {
                        mem.write_u8(payload_addr.wrapping_add(off as u32), byte);
                    }
                    for off in hunk.data.len()..hunk.reserved_size {
                        mem.write_u8(payload_addr.wrapping_add(off as u32), 0);
                    }
                }
                loader::HunkKind::Bss => {
                    for off in 0..hunk.reserved_size {
                        mem.write_u8(payload_addr.wrapping_add(off as u32), 0);
                    }
                }
            }
        }

        // Register the new hunks' payload addresses *before* resolving
        // relocations, so a node whose hunks reference each other (as
        // well as ancestors) resolves correctly.
        let state = self
            .overlay_roots
            .get_mut(&table_bptr)
            .ok_or(ERROR_OBJECT_NOT_FOUND)?;
        for (i, &payload_addr) in payload_addrs.iter().enumerate() {
            let global_index = node.first_hunk + i;
            if global_index >= state.payload_addrs.len() {
                for &addr in &alloc_addrs {
                    let _ = heap.free(addr);
                }
                return Err(ERROR_FILE_NOT_OBJECT);
            }
            state.payload_addrs[global_index] = Some(payload_addr);
        }

        // Pass 3: relocations, which may target already-resident
        // ancestor hunks outside this node's own range.
        for (i, hunk) in node.hunks.iter().enumerate() {
            for reloc in &hunk.relocs {
                let Some(Some(target_addr)) = state.payload_addrs.get(reloc.target_hunk).copied()
                else {
                    for &addr in &alloc_addrs {
                        let _ = heap.free(addr);
                    }
                    return Err(ERROR_FILE_NOT_OBJECT);
                };
                let loc = payload_addrs[i].wrapping_add(reloc.offset);
                let existing = mem.read_u32(loc);
                mem.write_u32(loc, existing.wrapping_add(target_addr));
            }
        }

        // Write each new hunk's own segment BPTR into the shared global
        // oh_Segments array.
        let segments_array_addr = state.segments_array_addr;
        for (i, &alloc_addr) in alloc_addrs.iter().enumerate() {
            let global_index = node.first_hunk + i;
            let seg_bptr = bptr_from_addr(alloc_addr.wrapping_add(NEXT_SEG_OFFSET));
            mem.write_u32(
                segments_array_addr.wrapping_add(global_index as u32 * 4),
                seg_bptr,
            );
        }

        Ok(bptr_from_addr(alloc_addrs[0].wrapping_add(NEXT_SEG_OFFSET)))
    }

    /// `UnLoadSeg(seglist)`: frees every segment of a seglist previously
    /// returned by [`Self::load_seg`]. `bptr == 0` is a documented no-op
    /// (`Ok(())`). An unknown `bptr` is `Err` with a diagnostic message --
    /// see the module docs' "loud failure on an unknown seglist" section
    /// for why this is a bug-catching abort rather than a silent no-op.
    pub fn unload_seg(&mut self, heap: &mut GuestHeap, bptr: u32) -> Result<(), String> {
        if bptr == 0 {
            return Ok(());
        }
        let Some(addrs) = self.seglists.remove(&bptr) else {
            return Err(format!(
                "UnLoadSeg: {bptr:#010x} isn't a live seglist (already unloaded, or never \
                 returned by LoadSeg)"
            ));
        };
        for addr in addrs {
            heap.free(addr).map_err(|e| {
                format!(
                    "UnLoadSeg: heap corruption freeing segment at {addr:#010x} of seglist \
                     {bptr:#010x}: {e}"
                )
            })?;
        }
        Ok(())
    }

    /// Shared `System()`/`Execute()` resolution + invocation: splits
    /// `command_line` on whitespace (first token = program, rest =
    /// args -- no quoting support, see the module docs), resolves the
    /// program through `self.vfs`, and -- if a
    /// [`Self::system_runner`] is installed -- calls it, returning its
    /// exit code. `Err` carries the `IoErr()` code to report on any
    /// failure to get that far (empty command line, no `Vfs`, unresolved
    /// path, or no runner installed -- all folded into the same
    /// [`ERROR_OBJECT_NOT_FOUND`] "couldn't run it" bucket, per the
    /// module docs' scope-cut note on why this is a clean failure rather
    /// than a loud one).
    fn resolve_and_run(&mut self, command_line: &str) -> Result<i32, i32> {
        let mut parts = command_line.split_whitespace();
        let program = parts.next().ok_or(ERROR_OBJECT_NOT_FOUND)?;
        let args: Vec<String> = parts.map(str::to_string).collect();

        let vfs = self.vfs.as_ref().ok_or(ERROR_OBJECT_NOT_FOUND)?;
        // A bare command name is looked up the way the shell does: in the
        // current directory first, then in C:. (The CLI's Path list sits
        // between the two on real AmigaOS; this runtime has none.) SAS/C,
        // for one, runs its linker as "slink WITH ...".
        let host_path = match vfs.resolve(program, ResolveMode::MustExist) {
            Ok(path) => path,
            Err(e) if program.contains(':') => return Err(map_vfs_error(&e)),
            Err(e) => vfs
                .resolve(&format!("C:{program}"), ResolveMode::MustExist)
                .map_err(|_| map_vfs_error(&e))?,
        };

        let runner = self.system_runner.as_mut().ok_or(ERROR_OBJECT_NOT_FOUND)?;
        let request = SystemRequest {
            command_line: command_line.to_string(),
            resolved_program_host_path: host_path,
            args,
        };
        Ok(runner(&request))
    }

    /// `System()`/`SystemTagList()`: returns the invoked command's exit
    /// code, or `-1` with `IoErr()` set if it couldn't be run at all
    /// (see [`Self::resolve_and_run`]).
    pub fn system(&mut self, command_line: &str) -> i32 {
        match self.resolve_and_run(command_line) {
            Ok(code) => code,
            Err(io_err) => {
                self.set_io_err(io_err);
                -1
            }
        }
    }

    /// `Execute()`: returns whether the command was successfully
    /// *invoked* (independent of its own exit code) -- `true` maps to
    /// `D0 = DOSTRUE`, `false` to `D0 = DOSFALSE` with `IoErr()` set, in
    /// [`execute_handler`].
    pub fn execute(&mut self, command_line: &str) -> bool {
        match self.resolve_and_run(command_line) {
            Ok(_code) => true,
            Err(io_err) => {
                self.set_io_err(io_err);
                false
            }
        }
    }
}

// --- LVO handlers ---

/// `LoadSeg` (`D1` = name `CString*`, or `0` for the overlay-continuation
/// form -- see [`DosState::load_overlay_node`] and the module docs --
/// with `D2` = `oh_Segments` `BPTR`, `D3` = `oh_FileHandle` `BPTR`). `D0`
/// = seglist `BPTR`, or `0` with `IoErr()` set.
fn loadseg_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let name_ptr = ctx.cpu.data_register(DataRegister(1));
    let result = if name_ptr == 0 {
        let table_bptr = ctx.cpu.data_register(DataRegister(2));
        let fh_bptr = ctx.cpu.data_register(DataRegister(3));
        ctx.dos
            .load_overlay_node(ctx.heap, ctx.mem, table_bptr, fh_bptr)
    } else {
        let name = String::from_utf8_lossy(&read_c_string(ctx.mem, name_ptr)).into_owned();
        ctx.dos.load_seg(ctx.heap, ctx.mem, &name)
    };
    match result {
        Ok(bptr) => ctx.cpu.set_data_register(DataRegister(0), bptr),
        Err(code) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), 0);
        }
    }
    Ok(())
}

/// `UnLoadSeg` (`D1` = seglist `BPTR`). `D0` = `DOSTRUE` on success (see
/// the module docs for why this is written despite real `UnLoadSeg` not
/// documenting a meaningful return value). An unknown, non-zero seglist
/// aborts the run via [`DispatchError::HandlerFailed`] -- see the module
/// docs.
fn unloadseg_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let bptr = ctx.cpu.data_register(DataRegister(1));
    match ctx.dos.unload_seg(ctx.heap, bptr) {
        Ok(()) => {
            ctx.cpu.set_data_register(DataRegister(0), DOSTRUE);
            Ok(())
        }
        Err(message) => Err(DispatchError::HandlerFailed {
            library: "dos.library".to_string(),
            lvo: -156,
            handler_name: "UnLoadSeg".to_string(),
            message,
        }),
    }
}

/// `SystemTagList` (`D1` = command `CString*`, `D2` = `struct TagItem*`
/// tag list -- read for calling-convention fidelity, never acted on; see
/// the module docs' scope cuts). `D0` = the command's exit code, or `-1`
/// with `IoErr()` set.
fn system_tag_list_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let cmd_ptr = ctx.cpu.data_register(DataRegister(1));
    let _tags = ctx.cpu.data_register(DataRegister(2)); // unused: see module docs
    let command_line = String::from_utf8_lossy(&read_c_string(ctx.mem, cmd_ptr)).into_owned();
    let code = ctx.dos.system(&command_line);
    ctx.cpu.set_data_register(DataRegister(0), code as u32);
    Ok(())
}

/// `Execute` (`D1` = command `CString*`, `D2` = input `BPTR`, `D3` =
/// output `BPTR` -- `D2`/`D3` read but never acted on; see the module
/// docs' scope cuts). `D0` = `DOSTRUE`/`DOSFALSE` (whether the command was
/// successfully invoked, *not* its exit code -- see the module docs).
fn execute_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let cmd_ptr = ctx.cpu.data_register(DataRegister(1));
    let _input_bptr = ctx.cpu.data_register(DataRegister(2)); // unused: see module docs
    let _output_bptr = ctx.cpu.data_register(DataRegister(3)); // unused: see module docs
    let command_line = String::from_utf8_lossy(&read_c_string(ctx.mem, cmd_ptr)).into_owned();
    let ok = ctx.dos.execute(&command_line);
    ctx.cpu
        .set_data_register(DataRegister(0), if ok { DOSTRUE } else { DOSFALSE });
    Ok(())
}

/// `RunCommand` (`D1` = seglist `BPTR` from a prior `LoadSeg`, `D2` =
/// requested stack size, `D3` = param buffer, `D4` = param buffer
/// length). `D0` = the invoked command's exit code, or `-1` with
/// `IoErr()` set if `seg` isn't a live seglist -- see the module docs'
/// "`RunCommand()`: an overlay, not a nested `Runtime`" section for the
/// full mechanism (issues #8/#156).
fn run_command_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let seg = ctx.cpu.data_register(DataRegister(1));
    let requested_stack = ctx.cpu.data_register(DataRegister(2));
    let param_ptr = ctx.cpu.data_register(DataRegister(3));
    let param_len = ctx.cpu.data_register(DataRegister(4));

    let Some(alloc_addrs) = ctx.dos.seglists.get(&seg).cloned() else {
        ctx.dos.set_io_err(ERROR_OBJECT_NOT_FOUND);
        ctx.cpu.set_data_register(DataRegister(0), (-1i32) as u32);
        return Ok(());
    };
    // Same formula as `SegList::entry`/`build_seglist`: the first
    // segment's payload address, immediately past its header.
    let entry = alloc_addrs[0].wrapping_add(SEG_HEADER_SIZE);

    let stack_size = requested_stack.max(MIN_STACK_SIZE);
    let stack_base = match ctx.heap.alloc(ctx.mem, stack_size) {
        Ok(addr) => addr,
        Err(_) => {
            ctx.dos.set_io_err(ERROR_NO_FREE_STORE);
            ctx.cpu.set_data_register(DataRegister(0), (-1i32) as u32);
            return Ok(());
        }
    };
    let stack_top = stack_base.wrapping_add(stack_size);

    // Save every bit of per-task/register state this overlay is about
    // to repoint, to restore once the invoked command returns -- same
    // fields `crate::exectask`'s `StackSwap` handler saves/restores for
    // the same reason (an in-place stack swap within this one task, no
    // new `Task` struct). `A7` here still holds the return address this
    // very `RunCommand` call's own `jsr` pushed (`Runtime::run`'s
    // generic post-dispatch `rts` hasn't run yet) -- exactly the value
    // a real CPU's `SP` would hold at this point too.
    let task = ctx.current_task;
    let old_a7 = ctx.cpu.address_register(AddressRegister(7));
    let old_a6 = ctx.cpu.address_register(AddressRegister(6));
    let old_sp_lower = ctx.mem.read_u32(task + TC_SPLOWER);
    let old_sp_upper = ctx.mem.read_u32(task + TC_SPUPPER);
    let old_pr_arguments = ctx.mem.read_u32(task + PR_ARGUMENTS_OFFSET);
    let old_cmdline = ctx.dos.cmdline;
    let old_cmdline_pos = ctx.dos.cmdline_pos;

    let entry_sp = stack_top & !3;
    ctx.cpu.set_address_register(AddressRegister(7), entry_sp);
    ctx.mem.write_u32(task + TC_SPLOWER, stack_base);
    ctx.mem.write_u32(task + TC_SPUPPER, stack_top);
    // pr_Arguments (see StartConfig::entry's own setup in Runtime::new):
    // a guest startup may read this instead of (or in addition to)
    // A0/D0, so it has to describe the same buffer.
    ctx.mem.write_u32(task + PR_ARGUMENTS_OFFSET, param_ptr);
    ctx.dos.cmdline = Some((param_ptr, param_len));
    // ReadArgs's cursor into dos.cmdline -- must restart at the child's
    // own buffer's beginning, not wherever the caller's own prior
    // ReadArgs call(s) left it (this field persists across calls, see
    // crate::dosargs's module doc): without resetting it here, a
    // RunCommand'd program's own first ReadArgs call would start
    // partway (often at the very end) of *its* command line instead of
    // the start, parsing as if no arguments were given at all.
    ctx.dos.cmdline_pos = 0;
    ctx.cpu
        .set_address_register(AddressRegister(6), DOS_LIBRARY_BASE);
    // A0/D0 = argptr/argsize verbatim, exactly as RunCommand's caller
    // supplied them -- no copy, no reformatting, matching real
    // RunCommand's documented contract (same convention
    // StartConfig::raw_command_line uses for a top-level run).
    ctx.cpu.set_address_register(AddressRegister(0), param_ptr);
    ctx.cpu.set_data_register(DataRegister(0), param_len);

    if let Some(shadow) = ctx.mem.shadow_mut() {
        // Two words below entry_sp are about to be pushed by trampoline
        // (CONTINUATION_STUB_ADDR, then entry) -- track from *after*
        // those, same reasoning as StackSwap's handler: the generic
        // post-dispatch rts reads the top one before anything else gets
        // a chance to call update_stack_pointer.
        shadow.reset_stack_tracking(stack_base, stack_top, entry_sp.wrapping_sub(8));
    }

    ctx.continuations
        .trampoline(ctx.cpu, ctx.mem, entry, move |ctx| {
            let code = ctx.cpu.data_register(DataRegister(0)) as i32;

            ctx.cpu.set_address_register(AddressRegister(7), old_a7);
            ctx.cpu.set_address_register(AddressRegister(6), old_a6);
            ctx.mem.write_u32(task + TC_SPLOWER, old_sp_lower);
            ctx.mem.write_u32(task + TC_SPUPPER, old_sp_upper);
            ctx.mem
                .write_u32(task + PR_ARGUMENTS_OFFSET, old_pr_arguments);
            ctx.dos.cmdline = old_cmdline;
            ctx.dos.cmdline_pos = old_cmdline_pos;
            if let Some(shadow) = ctx.mem.shadow_mut() {
                shadow.reset_stack_tracking(old_sp_lower, old_sp_upper, old_a7);
            }
            // Heap corruption freeing a region this handler itself just
            // allocated would be a real host-side bug, not a
            // guest-visible outcome -- fail loudly rather than leak it
            // silently, matching UnLoadSeg's posture for the same class
            // of "this should be impossible" failure.
            ctx.heap
                .free(stack_base)
                .map_err(|e| DispatchError::HandlerFailed {
                    library: "dos.library".to_string(),
                    lvo: -504,
                    handler_name: "RunCommand".to_string(),
                    message: format!(
                        "heap corruption freeing the stack region at {stack_base:#010x}: {e}"
                    ),
                })?;

            ctx.cpu.set_data_register(DataRegister(0), code as u32);
            Ok(())
        });

    Ok(())
}

/// `FindSegment` (`D1` = name `CString*`, `D2` = previous match `BPTR`,
/// `D3` = system flag). `D0` = `0` (`NULL`), always -- this runtime has
/// no list of resident segments (no `AddSegment`/`Resident` support),
/// so nothing can ever be found. Sets `IoErr()` to
/// [`ERROR_OBJECT_NOT_FOUND`], matching real `FindSegment`'s own
/// "no matching segment" convention.
fn find_segment_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    ctx.dos.set_io_err(ERROR_OBJECT_NOT_FOUND);
    ctx.cpu.set_data_register(DataRegister(0), 0);
    Ok(())
}

/// Registers `LoadSeg`/`UnLoadSeg`/`SystemTagList`/`Execute` onto
/// [`DOS_LIBRARY_BASE`], looked up by name through [`DOS_LVOS`] -- same
/// registration style as `dosfile.rs`'s `register_dos_handlers`. Called
/// unconditionally from [`crate::dispatch::Runtime::new`]; these handlers
/// work (failing cleanly) even without a `Vfs`/system runner installed.
pub fn register_dosseg_handlers<C: Cpu + 'static>(
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
    reg!("LoadSeg", loadseg_handler::<C>);
    reg!("UnLoadSeg", unloadseg_handler::<C>);
    reg!("SystemTagList", system_tag_list_handler::<C>);
    reg!("Execute", execute_handler::<C>);
    reg!("RunCommand", run_command_handler::<C>);
    reg!("FindSegment", find_segment_handler::<C>);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{CpuType, M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{Runtime, StartConfig};
    use crate::memory::FlatMemory;
    use crate::vfs::{Vfs, VfsConfig};
    use std::fs;
    use std::path::{Path, PathBuf as StdPathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempDir {
        path: StdPathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let path = std::env::temp_dir().join(format!("volamos-dosseg-test-{tag}-{pid}-{n}"));
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

    /// Builds a minimal single-hunk `HUNK_HEADER` + `HUNK_CODE` +
    /// `HUNK_END` file: `moveq #0,d0 ; rts`. No relocations. Standalone
    /// (doesn't reuse `loader.rs`'s private test helper, which isn't
    /// `pub`), matching "a fixture that loads ANY small hunk executable
    /// as a segment, not tied to a specific real AmigaOS library" per
    /// `docs/plan.md`.
    fn tiny_single_hunk_file() -> Vec<u8> {
        fn u32be(v: u32) -> [u8; 4] {
            v.to_be_bytes()
        }
        let code: &[u8] = &[0x70, 0x00, 0x4E, 0x75]; // moveq #0,d0 ; rts
        let mut buf = Vec::new();
        buf.extend_from_slice(&u32be(0x3F3)); // HUNK_HEADER
        buf.extend_from_slice(&u32be(0)); // no resident names
        buf.extend_from_slice(&u32be(1)); // table_size
        buf.extend_from_slice(&u32be(0)); // first_hunk
        buf.extend_from_slice(&u32be(0)); // last_hunk
        buf.extend_from_slice(&u32be(1)); // hunk 0 size: 1 longword
        buf.extend_from_slice(&u32be(0x3E9)); // HUNK_CODE
        buf.extend_from_slice(&u32be(1));
        buf.extend_from_slice(code);
        buf.extend_from_slice(&u32be(0x3F2)); // HUNK_END
        buf
    }

    /// As [`tiny_single_hunk_file`], but with `code_words` (big-endian
    /// words, so callers can hand-assemble arbitrary instruction
    /// sequences -- e.g. real `FMOVE` opcodes for the `RunCommand`
    /// FPU-inheritance tests below) as the hunk's entire content instead
    /// of a fixed `moveq #0,d0 ; rts`.
    fn hunk_file_from_words(code_words: &[u16]) -> Vec<u8> {
        fn u32be(v: u32) -> [u8; 4] {
            v.to_be_bytes()
        }
        let mut code = Vec::with_capacity(code_words.len() * 2 + 2);
        for &w in code_words {
            code.extend_from_slice(&w.to_be_bytes());
        }
        // A hunk's size is in whole longwords; pad with a trailing NOP
        // (never reached -- it follows this code's own RTS) if the
        // caller's word count is odd.
        if code.len() % 4 != 0 {
            code.extend_from_slice(&0x4E71u16.to_be_bytes());
        }
        let longwords = (code.len() as u32) / 4;
        let mut buf = Vec::new();
        buf.extend_from_slice(&u32be(0x3F3)); // HUNK_HEADER
        buf.extend_from_slice(&u32be(0));
        buf.extend_from_slice(&u32be(1));
        buf.extend_from_slice(&u32be(0));
        buf.extend_from_slice(&u32be(0));
        buf.extend_from_slice(&u32be(longwords));
        buf.extend_from_slice(&u32be(0x3E9)); // HUNK_CODE
        buf.extend_from_slice(&u32be(longwords));
        buf.extend_from_slice(&code);
        buf.extend_from_slice(&u32be(0x3F2)); // HUNK_END
        buf
    }

    /// Builds a two-hunk (CODE referencing DATA via one `RELOC32`) file,
    /// matching `loader.rs`'s own `inter_hunk_reloc32_targets_second_hunk`
    /// test shape, for exercising cross-segment relocation + BPTR
    /// chaining through `build_seglist`.
    fn two_hunk_file_with_inter_hunk_reloc() -> Vec<u8> {
        fn u32be(v: u32) -> [u8; 4] {
            v.to_be_bytes()
        }
        let mut buf = Vec::new();
        buf.extend_from_slice(&u32be(0x3F3)); // HUNK_HEADER
        buf.extend_from_slice(&u32be(0));
        buf.extend_from_slice(&u32be(2)); // table_size
        buf.extend_from_slice(&u32be(0));
        buf.extend_from_slice(&u32be(1));
        buf.extend_from_slice(&u32be(1)); // hunk 0: 1 longword
        buf.extend_from_slice(&u32be(1)); // hunk 1: 1 longword

        // Hunk 0: CODE, one longword (addend 0), RELOC32 -> hunk 1 at
        // offset 0, HUNK_END.
        buf.extend_from_slice(&u32be(0x3E9)); // HUNK_CODE
        buf.extend_from_slice(&u32be(1));
        buf.extend_from_slice(&u32be(0)); // addend placeholder
        buf.extend_from_slice(&u32be(0x3EC)); // HUNK_RELOC32
        buf.extend_from_slice(&u32be(1)); // one offset
        buf.extend_from_slice(&u32be(1)); // target hunk 1
        buf.extend_from_slice(&u32be(0)); // offset 0 within hunk 0
        buf.extend_from_slice(&u32be(0)); // terminate reloc groups
        buf.extend_from_slice(&u32be(0x3F2)); // HUNK_END

        // Hunk 1: DATA, one longword, no relocs.
        buf.extend_from_slice(&u32be(0x3EA)); // HUNK_DATA
        buf.extend_from_slice(&u32be(1));
        buf.extend_from_slice(&u32be(0xDEAD_BEEF));
        buf.extend_from_slice(&u32be(0x3F2)); // HUNK_END

        buf
    }

    /// Builds an overlay executable: a root node (one `HUNK_CODE` hunk
    /// shaped like `struct OverlayHeader` -- `oh_Jump`/`oh_Magic` plus
    /// four zeroed loader-filled longwords) terminated by `HUNK_OVERLAY`
    /// (a minimal one-level table, `od = 1`, no ordinates/symbols), then
    /// one overlay node (global hunk 1, one `HUNK_CODE` hunk with a
    /// `RELOC32` targeting hunk 0 -- the *root*, an ancestor outside the
    /// node's own range) terminated by `HUNK_BREAK`. Returns the file
    /// bytes and the byte offset of the node's own `HUNK_HEADER` (what a
    /// real overlay manager would `Seek()` to before calling
    /// `LoadSeg(NULL, table, fh)`).
    fn overlay_root_and_one_node_file() -> (Vec<u8>, usize) {
        fn u32be(v: u32) -> [u8; 4] {
            v.to_be_bytes()
        }
        let mut buf = Vec::new();
        buf.extend_from_slice(&u32be(0x3F3)); // HUNK_HEADER
        buf.extend_from_slice(&u32be(0));
        buf.extend_from_slice(&u32be(2)); // table_size: whole tree = 2 hunks
        buf.extend_from_slice(&u32be(0)); // first_hunk
        buf.extend_from_slice(&u32be(0)); // last_hunk (root is hunk 0 only)
        buf.extend_from_slice(&u32be(6)); // hunk 0 size: 6 longwords

        buf.extend_from_slice(&u32be(0x3E9)); // HUNK_CODE
        buf.extend_from_slice(&u32be(6));
        buf.extend_from_slice(&u32be(0)); // oh_Jump (not executed by this test)
        buf.extend_from_slice(&u32be(0x0000_ABCD)); // oh_Magic
        buf.extend_from_slice(&u32be(0)); // oh_FileHandle (loader-filled)
        buf.extend_from_slice(&u32be(0)); // oh_OVTab (loader-filled)
        buf.extend_from_slice(&u32be(0)); // oh_Segments (loader-filled)
        buf.extend_from_slice(&u32be(0)); // oh_GV (loader-filled)
        buf.extend_from_slice(&u32be(0x3F2)); // HUNK_END

        buf.extend_from_slice(&u32be(0x3F5)); // HUNK_OVERLAY
        buf.extend_from_slice(&u32be(0)); // l = 0 -> table is 1 longword
        buf.extend_from_slice(&u32be(1)); // od = 1 (no ordinates/symtab)

        let node_offset = buf.len();
        buf.extend_from_slice(&u32be(0x3F3)); // HUNK_HEADER (node)
        buf.extend_from_slice(&u32be(0));
        buf.extend_from_slice(&u32be(2)); // table_size
        buf.extend_from_slice(&u32be(1)); // first_hunk
        buf.extend_from_slice(&u32be(1)); // last_hunk
        buf.extend_from_slice(&u32be(1)); // hunk 1 size: 1 longword

        buf.extend_from_slice(&u32be(0x3E9)); // HUNK_CODE
        buf.extend_from_slice(&u32be(1));
        buf.extend_from_slice(&u32be(0)); // addend placeholder
        buf.extend_from_slice(&u32be(0x3EC)); // HUNK_RELOC32
        buf.extend_from_slice(&u32be(1)); // one offset
        buf.extend_from_slice(&u32be(0)); // target hunk 0 (root, an ancestor)
        buf.extend_from_slice(&u32be(0)); // offset 0
        buf.extend_from_slice(&u32be(0)); // terminate reloc groups
        buf.extend_from_slice(&u32be(0x3F6)); // HUNK_BREAK

        (buf, node_offset)
    }

    // --- build_seglist unit tests (host-side, no CPU) ---

    #[test]
    fn build_seglist_single_hunk_layout_and_entry() {
        let bytes = tiny_single_hunk_file();
        let file = loader::parse(&bytes).unwrap();
        let mut heap = GuestHeap::new(0x1000, 0x4000);
        let mut mem = FlatMemory::new(0x4000);

        let seglist = build_seglist(&file, &mut heap, &mut mem).expect("build_seglist");
        assert_eq!(seglist.alloc_addrs.len(), 1);
        let alloc_addr = seglist.alloc_addrs[0];

        // seg_length is the actual (4-byte-aligned) allocation size.
        let seg_length = mem.read_u32(alloc_addr);
        assert_eq!(seg_length, heap.size_of_live_alloc(alloc_addr).unwrap());
        assert!(seg_length >= SEG_HEADER_SIZE + 4);

        // next_seg is 0 (single segment).
        assert_eq!(mem.read_u32(alloc_addr + NEXT_SEG_OFFSET), 0);

        // BPTR points at next_seg; payload starts 4 bytes past that.
        assert_eq!(
            seglist.first_bptr,
            bptr_from_addr(alloc_addr + NEXT_SEG_OFFSET)
        );
        let payload_addr = alloc_addr + SEG_HEADER_SIZE;
        assert_eq!(seglist.entry, payload_addr);

        // Payload content: moveq #0,d0 ; rts.
        assert_eq!(mem.read_u32(payload_addr), 0x7000_4E75);
    }

    #[test]
    fn build_seglist_multi_hunk_chains_bptr_and_relocates() {
        let bytes = two_hunk_file_with_inter_hunk_reloc();
        let file = loader::parse(&bytes).unwrap();
        let mut heap = GuestHeap::new(0x1000, 0x8000);
        let mut mem = FlatMemory::new(0x8000);

        let seglist = build_seglist(&file, &mut heap, &mut mem).expect("build_seglist");
        assert_eq!(seglist.alloc_addrs.len(), 2);
        let (seg0, seg1) = (seglist.alloc_addrs[0], seglist.alloc_addrs[1]);

        // Segment 0's next_seg BPTR resolves to segment 1's own BPTR
        // field address.
        let next_bptr = mem.read_u32(seg0 + NEXT_SEG_OFFSET);
        assert_eq!(next_bptr, bptr_from_addr(seg1 + NEXT_SEG_OFFSET));
        // Segment 1 is the end of the chain.
        assert_eq!(mem.read_u32(seg1 + NEXT_SEG_OFFSET), 0);

        // The relocated longword at hunk 0's payload should now hold
        // hunk 1's payload address (0 addend + hunk 1's payload addr).
        let payload0 = seg0 + SEG_HEADER_SIZE;
        let payload1 = seg1 + SEG_HEADER_SIZE;
        assert_eq!(mem.read_u32(payload0), payload1);
        assert_eq!(mem.read_u32(payload1), 0xDEAD_BEEF);

        assert_eq!(seglist.entry, payload0);
    }

    #[test]
    fn build_seglist_zero_fills_bss_hunk() {
        // CODE (1 longword) + BSS (4 longwords), no relocs.
        fn u32be(v: u32) -> [u8; 4] {
            v.to_be_bytes()
        }
        let mut buf = Vec::new();
        buf.extend_from_slice(&u32be(0x3F3));
        buf.extend_from_slice(&u32be(0));
        buf.extend_from_slice(&u32be(2));
        buf.extend_from_slice(&u32be(0));
        buf.extend_from_slice(&u32be(1));
        buf.extend_from_slice(&u32be(1));
        buf.extend_from_slice(&u32be(4));
        buf.extend_from_slice(&u32be(0x3E9));
        buf.extend_from_slice(&u32be(1));
        buf.extend_from_slice(&u32be(0x4E71_4E71));
        buf.extend_from_slice(&u32be(0x3F2));
        buf.extend_from_slice(&u32be(0x3EB)); // HUNK_BSS
        buf.extend_from_slice(&u32be(4));
        buf.extend_from_slice(&u32be(0x3F2));

        let file = loader::parse(&buf).unwrap();
        let mut heap = GuestHeap::new(0x1000, 0x8000);
        let mut mem = FlatMemory::new(0x8000);
        let seglist = build_seglist(&file, &mut heap, &mut mem).unwrap();

        let bss_payload = seglist.alloc_addrs[1] + SEG_HEADER_SIZE;
        for i in 0..16u32 {
            assert_eq!(mem.read_u8(bss_payload + i), 0);
        }
    }

    // --- DosState::load_seg / unload_seg unit tests ---

    #[test]
    fn load_seg_without_vfs_fails_with_object_not_found() {
        let mut heap = GuestHeap::new(0x1000, 0x4000);
        let mut mem = FlatMemory::new(0x4000);
        let mut dos = DosState::new(None);
        let err = dos
            .load_seg(&mut heap, &mut mem, "SYS:whatever")
            .unwrap_err();
        assert_eq!(err, ERROR_OBJECT_NOT_FOUND);
    }

    #[test]
    fn load_seg_missing_file_fails_with_object_not_found() {
        let tmp = TempDir::new("loadseg-missing");
        let mut heap = GuestHeap::new(0x1000, 0x4000);
        let mut mem = FlatMemory::new(0x4000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        let err = dos.load_seg(&mut heap, &mut mem, "SYS:nope").unwrap_err();
        assert_eq!(err, ERROR_OBJECT_NOT_FOUND);
    }

    #[test]
    fn load_seg_non_hunk_file_fails_with_file_not_object() {
        let tmp = TempDir::new("loadseg-bad-hunk");
        fs::write(tmp.path().join("junk.bin"), b"not a hunk file at all").unwrap();
        let mut heap = GuestHeap::new(0x1000, 0x4000);
        let mut mem = FlatMemory::new(0x4000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        let err = dos
            .load_seg(&mut heap, &mut mem, "SYS:junk.bin")
            .unwrap_err();
        assert_eq!(err, ERROR_FILE_NOT_OBJECT);
    }

    #[test]
    fn load_seg_then_unload_seg_round_trips() {
        let tmp = TempDir::new("loadseg-roundtrip");
        fs::write(tmp.path().join("prog"), tiny_single_hunk_file()).unwrap();
        let mut heap = GuestHeap::new(0x1000, 0x4000);
        let free_before = heap.free_bytes();
        let mut mem = FlatMemory::new(0x4000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));

        let bptr = dos
            .load_seg(&mut heap, &mut mem, "SYS:prog")
            .expect("valid hunk file should LoadSeg");
        assert_ne!(bptr, 0);
        assert!(heap.free_bytes() < free_before);

        dos.unload_seg(&mut heap, bptr)
            .expect("UnLoadSeg of a live seglist should succeed");
        assert_eq!(
            heap.free_bytes(),
            free_before,
            "UnLoadSeg should free everything LoadSeg allocated"
        );
    }

    // --- Overlay executables: DosState::load_seg / load_overlay_node ---

    #[test]
    fn load_seg_overlay_root_patches_header_and_loads_node_on_demand() {
        let tmp = TempDir::new("loadseg-overlay");
        let (bytes, node_offset) = overlay_root_and_one_node_file();
        fs::write(tmp.path().join("prog"), &bytes).unwrap();

        let mut heap = GuestHeap::new(0x1000, 0x8000);
        let mut mem = FlatMemory::new(0x8000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));

        let root_bptr = dos
            .load_seg(&mut heap, &mut mem, "SYS:prog")
            .expect("overlay root should LoadSeg");
        assert_ne!(root_bptr, 0);

        let alloc_addrs = dos.seglists.get(&root_bptr).unwrap().clone();
        assert_eq!(alloc_addrs.len(), 1, "root node has one hunk");
        let hunk0_payload = alloc_addrs[0] + SEG_HEADER_SIZE;

        assert_eq!(
            mem.read_u32(hunk0_payload + 4),
            0x0000_ABCD,
            "oh_Magic must be left untouched by the loader"
        );
        let fh_bptr = mem.read_u32(hunk0_payload + 8);
        let ovtab_addr = mem.read_u32(hunk0_payload + 0xc);
        let segments_bptr = mem.read_u32(hunk0_payload + 0x10);
        assert_ne!(fh_bptr, 0, "oh_FileHandle should be loader-patched");
        assert_ne!(ovtab_addr, 0, "oh_OVTab should be loader-patched");
        assert_ne!(segments_bptr, 0, "oh_Segments should be loader-patched");

        // oh_OVTab's payload is the raw HUNK_OVERLAY table verbatim: [od=1].
        assert_eq!(mem.read_u32(ovtab_addr), 1);

        let segments_addr = addr_from_bptr(segments_bptr);
        assert_ne!(
            mem.read_u32(segments_addr),
            0,
            "slot 0 (root, already loaded) should be populated"
        );
        assert_eq!(
            mem.read_u32(segments_addr + 4),
            0,
            "slot 1 (node, not yet loaded) should still be zero"
        );

        // Simulate the overlay manager: Seek fh to the node's file
        // offset, then LoadSeg(NULL, table, fh) -- the exact D1=0/D2/D3
        // convention confirmed against a real overlay manager's
        // disassembly (see the module docs).
        let fh_addr = addr_from_bptr(fh_bptr);
        dos.seek(fh_addr, node_offset as i32, OFFSET_BEGINNING)
            .expect("seek to the node's HUNK_HEADER should succeed");
        let node_bptr = dos
            .load_overlay_node(&mut heap, &mut mem, segments_bptr, fh_bptr)
            .expect("overlay node should load");
        assert_ne!(node_bptr, 0);

        assert_eq!(
            mem.read_u32(segments_addr + 4),
            node_bptr,
            "the global segment table's slot 1 should now hold the node's own BPTR"
        );

        // The node's RELOC32 (offset 0, target hunk 0) should have
        // resolved against the *root's* payload address -- a
        // cross-node relocation against an already-resident ancestor
        // hunk, the whole point of the shared global segment table.
        let node_payload = addr_from_bptr(node_bptr) + NEXT_SEG_OFFSET;
        assert_eq!(
            mem.read_u32(node_payload),
            hunk0_payload,
            "reloc against ancestor hunk 0 should resolve to its payload address"
        );
    }

    #[test]
    fn load_overlay_node_unknown_table_fails_with_object_not_found() {
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let mut mem = FlatMemory::new(0x2000);
        let mut dos = DosState::new(None);
        let err = dos
            .load_overlay_node(&mut heap, &mut mem, 0x1234, 0x5678)
            .unwrap_err();
        assert_eq!(err, ERROR_OBJECT_NOT_FOUND);
    }

    #[test]
    fn unload_seg_of_zero_is_a_no_op() {
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let mut dos = DosState::new(None);
        dos.unload_seg(&mut heap, 0)
            .expect("UnLoadSeg(0) is a no-op");
    }

    #[test]
    fn unload_seg_of_unknown_bptr_fails_loudly() {
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let mut dos = DosState::new(None);
        let err = dos.unload_seg(&mut heap, 0x1234).unwrap_err();
        assert!(err.contains("0x00001234") || err.to_lowercase().contains("seglist"));
    }

    // --- System()/Execute() unit tests ---

    #[test]
    fn system_without_runner_returns_minus_one_with_ioerr() {
        let tmp = TempDir::new("system-no-runner");
        fs::write(tmp.path().join("prog"), tiny_single_hunk_file()).unwrap();
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        let code = dos.system("SYS:prog");
        assert_eq!(code, -1);
        assert_eq!(dos.io_err(), ERROR_OBJECT_NOT_FOUND);
    }

    #[test]
    fn system_unresolvable_program_returns_minus_one() {
        let mut dos = DosState::new(None);
        let code = dos.system("SYS:nope");
        assert_eq!(code, -1);
        assert_eq!(dos.io_err(), ERROR_OBJECT_NOT_FOUND);
    }

    #[test]
    fn system_with_runner_returns_its_exit_code() {
        let tmp = TempDir::new("system-with-runner");
        fs::write(tmp.path().join("prog"), tiny_single_hunk_file()).unwrap();
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        dos.system_runner = Some(Box::new(|req: &SystemRequest| {
            assert!(req.resolved_program_host_path.ends_with("prog"));
            assert_eq!(req.args, vec!["one".to_string(), "two".to_string()]);
            42
        }));
        let code = dos.system("SYS:prog one two");
        assert_eq!(code, 42);
    }

    #[test]
    fn execute_with_runner_returns_dostrue_regardless_of_exit_code() {
        let tmp = TempDir::new("execute-with-runner");
        fs::write(tmp.path().join("prog"), tiny_single_hunk_file()).unwrap();
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        dos.system_runner = Some(Box::new(|_req: &SystemRequest| 7));
        assert!(dos.execute("SYS:prog"));
    }

    #[test]
    fn execute_without_runner_returns_false_with_ioerr() {
        let mut dos = DosState::new(None);
        assert!(!dos.execute("SYS:prog"));
        assert_eq!(dos.io_err(), ERROR_OBJECT_NOT_FOUND);
    }

    // --- End-to-end: LoadSeg/UnLoadSeg via a hand-assembled guest
    // program, matching dosfile.rs's own end-to-end test style. ---

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

    fn move_d0_to_d(n: u16) -> u16 {
        0x2000 | (n << 9)
    }

    fn jsr_disp16(an: u16) -> u16 {
        0x4EA8 | an
    }

    const RTS: u16 = 0x4E75;

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

    /// As [`runtime_with_program_and_extra`], but with a `--cpu 68020
    /// --fpu`-equivalent core (`M68kCpu::with_config`) instead of the
    /// plain, FPU-less default -- needed by the `RunCommand`
    /// FPU-inheritance tests below, since a real `FMOVE` to/from `FPCR`
    /// traps as an unimplemented F-line instruction on a core with no
    /// FPU fitted at all.
    fn runtime_with_program_and_extra_fpu(
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
            M68kCpu::with_config(CpuType::M68020, true),
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

    /// `FMOVE.L #imm32,FPCR` (`1111 0010 00 111100` + `100` control-reg
    /// select + the 32-bit immediate) -- real 68881/68882/68040/68060
    /// encoding, derived directly from the `m68k` crate's own
    /// `exec_fpu_op0` decoder (`subop` 0x4 = "FMOVE <ea>, control
    /// register", `ctrl_sel` bit 2 = FPCR, `<ea>` mode 111/reg 100 =
    /// immediate) rather than from memory, so this is guaranteed to
    /// match what the crate this runtime actually embeds will execute.
    fn fmove_imm_to_fpcr(value: u32) -> [u16; 4] {
        [0xF23C, 0x9000, (value >> 16) as u16, value as u16]
    }

    /// `FMOVE.L FPCR,Dn` -- same derivation as [`fmove_imm_to_fpcr`],
    /// `subop` 0x5 ("FMOVE control register, <ea>"), `<ea>` mode
    /// 000/reg `n` = data-register direct.
    fn fmove_fpcr_to_d(n: u16) -> [u16; 2] {
        [0xF200 | n, 0xB000]
    }

    // --- RunCommand (issues #8/#156): overlays the calling process,
    // inheriting and propagating back real CPU register state (FPCR/
    // FPSR included) -- not a nested Runtime with a fresh CpuCore. ---

    #[test]
    #[allow(clippy::vec_init_then_push)] // one `.push()` per word, each commented -- see execmem.rs's own tests for the convention
    fn run_command_of_an_unknown_seg_fails_cleanly_without_running_anything() {
        // D1 = an address that was never returned by LoadSeg -- same
        // "couldn't run it" ERROR_OBJECT_NOT_FOUND bucket System()/
        // Execute() use for an equivalent failure.
        let mut words = Vec::new();
        words.push(move_imm_to_d(1)); // D1 = bogus seg BPTR
        words.push(0x1234);
        words.push(0x5678);
        words.push(move_imm_to_d(2)); // D2 = stack size (irrelevant)
        words.push(0);
        words.push(8192);
        words.push(move_imm_to_d(3)); // D3 = paramptr
        words.push(0);
        words.push(0);
        words.push(move_imm_to_d(4)); // D4 = paramlen
        words.push(0);
        words.push(0);
        push_jsr(&mut words, 6, -504); // RunCommand
        words.push(RTS);

        let mut rt = runtime_with_program_and_extra(&words, 0, &[], None);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");

        assert_eq!(
            code, -1,
            "an unknown seg should fail cleanly, not run anything"
        );
    }

    #[test]
    fn run_command_child_inherits_the_callers_fpcr() {
        // Child: FMOVE.L FPCR,D0 ; RTS -- reports back, as its own exit
        // code, whatever FPCR it saw at startup. If RunCommand ran it on
        // a brand-new CpuCore (the pre-#156 behavior), this would always
        // read 0 regardless of what the parent set below.
        let mut child_words = Vec::new();
        child_words.extend_from_slice(&fmove_fpcr_to_d(0));
        child_words.push(RTS);
        let tmp = TempDir::new("fpu-inherit");
        fs::write(tmp.path().join("child"), hunk_file_from_words(&child_words)).unwrap();
        let name = b"SYS:child\0";

        // Confined to the exception-enable byte (bits 8-15) -- real
        // 68881/68882/68040/68060 silicon reads FPCR's bits 16-31 back
        // as 0 regardless of what's written there, so a marker value up
        // there would only prove this crate's own FPCR storage doesn't
        // mask it (which it currently doesn't, but isn't architecturally
        // guaranteed to stay that way), not that this test is checking
        // real, writable FPCR state.
        const MARKER: u32 = 0x1000;
        let mut words = Vec::new();
        words.extend_from_slice(&fmove_imm_to_fpcr(MARKER));
        let name_idx = words.len();
        words.push(move_imm_to_d(1)); // D1 = name ptr (patched below)
        words.push(0);
        words.push(0);
        push_jsr(&mut words, 6, -150); // LoadSeg(a6): D0 = BPTR
        words.push(move_d0_to_d(1)); // D1 = seglist BPTR
        words.push(move_imm_to_d(2)); // D2 = requested stack size
        words.push(0);
        words.push(8192);
        words.push(move_imm_to_d(3)); // D3 = paramptr (unused by the child)
        words.push(0);
        words.push(0);
        words.push(move_imm_to_d(4)); // D4 = paramlen
        words.push(0);
        words.push(0);
        push_jsr(&mut words, 6, -504); // RunCommand(a6): D0 = child's exit code
        words.push(RTS); // exit code = whatever RunCommand (the child) left in D0

        let name_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        patch_imm32(&mut words, name_idx, name_addr);

        let mut rt = runtime_with_program_and_extra_fpu(&words, name_addr, name, Some(tmp.path()));
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");

        assert_eq!(
            code, MARKER as i32,
            "the invoked command should inherit the caller's FPCR, not a freshly-reset one"
        );
    }

    #[test]
    fn run_command_child_writes_to_fpcr_propagate_back_to_the_caller() {
        // Child: FMOVE.L #MARKER,FPCR ; RTS -- leaves a distinctive
        // value in FPCR and exits (D0 is whatever it happened to be,
        // irrelevant to this test). If RunCommand ran it on a brand-new
        // CpuCore (the pre-#156 behavior) that's discarded on return,
        // the caller's own FPCR would never see this write at all.
        // See the sibling test above for why this stays confined to the
        // exception-enable byte (bits 8-15), the part real FPU silicon
        // actually keeps rather than masking to 0.
        const CHILD_MARKER: u32 = 0x5000;
        let mut child_words = Vec::new();
        child_words.extend_from_slice(&fmove_imm_to_fpcr(CHILD_MARKER));
        child_words.push(RTS);
        let tmp = TempDir::new("fpu-propagate");
        fs::write(tmp.path().join("child"), hunk_file_from_words(&child_words)).unwrap();
        let name = b"SYS:child\0";

        let mut words = Vec::new();
        words.extend_from_slice(&fmove_imm_to_fpcr(0x1000)); // parent's own starting FPCR
        let name_idx = words.len();
        words.push(move_imm_to_d(1));
        words.push(0);
        words.push(0);
        push_jsr(&mut words, 6, -150); // LoadSeg
        words.push(move_d0_to_d(1));
        words.push(move_imm_to_d(2));
        words.push(0);
        words.push(8192);
        words.push(move_imm_to_d(3));
        words.push(0);
        words.push(0);
        words.push(move_imm_to_d(4));
        words.push(0);
        words.push(0);
        push_jsr(&mut words, 6, -504); // RunCommand
        words.extend_from_slice(&fmove_fpcr_to_d(0)); // D0 = caller's FPCR now
        words.push(RTS);

        let name_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        patch_imm32(&mut words, name_idx, name_addr);

        let mut rt = runtime_with_program_and_extra_fpu(&words, name_addr, name, Some(tmp.path()));
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");

        assert_eq!(
            code, CHILD_MARKER as i32,
            "the invoked command's FPCR write should still be in effect once RunCommand \
             returns to its caller -- same CPU, not a discarded nested one"
        );
    }

    #[test]
    #[allow(clippy::vec_init_then_push)] // one `.push()` per word, each commented
    fn run_command_resets_readargs_cursor_for_the_child_instead_of_inheriting_the_callers() {
        // The parent calls ReadArgs on *its own* command line first,
        // advancing DosState::cmdline_pos partway (here, all the way)
        // into the parent's buffer. Without resetting that cursor for
        // the child (DosState::cmdline_pos is a single, shared field --
        // see crate::dosargs's module doc), the child's own ReadArgs
        // call would start wherever the parent's left off, typically
        // past the end of the *child's* entirely different buffer,
        // silently parsing it as empty. Caught by code review (issue
        // #156 follow-up), not by any corpus program before this.
        let tmp = TempDir::new("cmdline-pos");

        // extra's layout, all at a single fixed, known base address
        // (extra_addr, below) in the *parent's* own loaded region --
        // the child's own code can reference these addresses directly
        // as plain immediates (no relocation needed: they don't depend
        // on wherever LoadSeg ends up placing the child's own seglist).
        const TEMPLATE: &[u8] = b"ARG\0";
        const CHILD_CMDLINE: &[u8] = b"CHILD_TOKEN\n";
        const NAME: &[u8] = b"SYS:child\0";
        let template_offset = 0u32;
        let array_offset = 4u32; // must be 4-aligned; TEMPLATE is exactly 4 bytes
        let child_cmdline_offset = array_offset + 4;
        let name_offset = child_cmdline_offset + CHILD_CMDLINE.len() as u32;
        let mut extra = Vec::new();
        extra.extend_from_slice(TEMPLATE);
        extra.extend_from_slice(&[0u8; 4]); // array slot, zeroed
        extra.extend_from_slice(CHILD_CMDLINE);
        extra.extend_from_slice(NAME);

        // Child: ReadArgs("ARG", array, 0) against *its own* cmdline
        // (set by RunCommand below), then RTS -- the parent inspects
        // the array slot's content directly afterward (see below),
        // so the child's own D0/exit code doesn't matter here.
        let mut child_words = Vec::new();
        child_words.push(move_imm_to_d(1)); // D1 = template ptr (patched below)
        child_words.push(0);
        child_words.push(0);
        child_words.push(move_imm_to_d(2)); // D2 = array ptr (patched below)
        child_words.push(0);
        child_words.push(0);
        child_words.push(move_imm_to_d(3)); // D3 = rdargs (0)
        child_words.push(0);
        child_words.push(0);
        child_words.push(jsr_disp16(6));
        child_words.push((-798i32) as u16); // ReadArgs(a6)
        child_words.push(RTS);

        let mut words = Vec::new();
        // Parent's own ReadArgs call first, against its own command
        // line ("PARENT_TOKEN", below) -- advances cmdline_pos.
        let parent_template_idx = words.len();
        words.push(move_imm_to_d(1));
        words.push(0);
        words.push(0);
        let parent_array_idx = words.len();
        words.push(move_imm_to_d(2));
        words.push(0);
        words.push(0);
        words.push(move_imm_to_d(3));
        words.push(0);
        words.push(0);
        push_jsr(&mut words, 6, -798); // ReadArgs(a6) -- the parent's own

        let name_idx = words.len();
        words.push(move_imm_to_d(1)); // D1 = "SYS:child" (patched below)
        words.push(0);
        words.push(0);
        push_jsr(&mut words, 6, -150); // LoadSeg(a6): D0 = BPTR
        words.push(move_d0_to_d(1)); // D1 = seglist BPTR
        words.push(move_imm_to_d(2)); // D2 = requested stack size
        words.push(0);
        words.push(8192);
        let child_cmdline_idx = words.len();
        words.push(move_imm_to_d(3)); // D3 = child's own paramptr (patched below)
        words.push(0);
        words.push(0);
        words.push(move_imm_to_d(4)); // D4 = paramlen
        words.push(0);
        words.push(CHILD_CMDLINE.len() as u16);
        push_jsr(&mut words, 6, -504); // RunCommand(a6)
        words.push(RTS);

        let extra_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        let template_addr = extra_addr + template_offset;
        let array_addr = extra_addr + array_offset;
        let child_cmdline_addr = extra_addr + child_cmdline_offset;
        let name_addr = extra_addr + name_offset;
        patch_imm32(&mut words, parent_template_idx, template_addr);
        patch_imm32(&mut words, parent_array_idx, array_addr);
        patch_imm32(&mut words, name_idx, name_addr);
        patch_imm32(&mut words, child_cmdline_idx, child_cmdline_addr);
        patch_imm32(&mut child_words, 0, template_addr);
        patch_imm32(&mut child_words, 3, array_addr);

        fs::write(tmp.path().join("child"), hunk_file_from_words(&child_words)).unwrap();

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);
        for (i, &b) in extra.iter().enumerate() {
            mem.write_u8(extra_addr + i as u32, b);
        }
        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: extra_addr + extra.len() as u32,
                args: vec!["PARENT_TOKEN".to_string()],
                ..StartConfig::default()
            },
        );
        rt.set_vfs(vfs_over(tmp.path()));
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_ne!(
            code, 0,
            "RunCommand should have run the child to completion"
        );

        let matched_ptr = rt.memory().read_u32(array_addr);
        assert_ne!(
            matched_ptr, 0,
            "the child's own ReadArgs call should have matched something"
        );
        assert_eq!(
            read_c_string(rt.memory(), matched_ptr),
            b"CHILD_TOKEN",
            "the child's ReadArgs should parse the child's own command line from its start, \
             not wherever the parent's prior ReadArgs call left DosState::cmdline_pos"
        );
    }

    #[test]
    fn end_to_end_loadseg_returns_nonzero_bptr_then_unloadseg_succeeds() {
        let tmp = TempDir::new("e2e-loadseg");
        fs::write(tmp.path().join("prog"), tiny_single_hunk_file()).unwrap();
        let name = b"SYS:prog\0";

        let mut words = Vec::new();
        let name_idx = words.len();
        words.push(move_imm_to_d(1)); // D1 = name (patched below)
        words.push(0);
        words.push(0);
        push_jsr(&mut words, 6, -150); // LoadSeg(a6): D0 = BPTR or 0
        words.push(move_d0_to_d(1)); // D1 = seglist BPTR (survives to UnLoadSeg)
        push_jsr(&mut words, 6, -156); // UnLoadSeg(a6): D0 = DOSTRUE
        words.push(RTS); // exit code = UnLoadSeg's D0

        let name_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        patch_imm32(&mut words, name_idx, name_addr);

        let mut rt = runtime_with_program_and_extra(&words, name_addr, name, Some(tmp.path()));

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code as u32, DOSTRUE,
            "UnLoadSeg's D0 (DOSTRUE) should be the exit code"
        );
    }

    #[test]
    fn end_to_end_loadseg_missing_file_returns_zero_and_sets_ioerr() {
        let tmp = TempDir::new("e2e-loadseg-missing");
        let name = b"SYS:nope\0";

        let mut words = Vec::new();
        let name_idx = words.len();
        words.push(move_imm_to_d(1));
        words.push(0);
        words.push(0);
        push_jsr(&mut words, 6, -150); // LoadSeg(a6): D0 = 0
        push_jsr(&mut words, 6, -132); // IoErr(a6): D0 = current IoErr()
        words.push(RTS);

        let name_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        patch_imm32(&mut words, name_idx, name_addr);

        let mut rt = runtime_with_program_and_extra(&words, name_addr, name, Some(tmp.path()));

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, ERROR_OBJECT_NOT_FOUND);
    }

    #[test]
    fn end_to_end_find_segment_never_finds_anything() {
        let mut words = Vec::new();
        let name_idx = words.len();
        words.push(move_imm_to_d(1)); // D1 = name (patched)
        words.push(0);
        words.push(0);
        words.push(move_imm_to_d(2)); // D2 = 0 (no previous match)
        words.push(0);
        words.push(0);
        words.push(move_imm_to_d(3)); // D3 = 0 (system flag)
        words.push(0);
        words.push(0);
        push_jsr(&mut words, 6, -780); // FindSegment(a6): D0 = 0
        push_jsr(&mut words, 6, -132); // IoErr(a6): D0 = current IoErr()
        words.push(RTS);

        let name = b"anything\0";
        let name_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        patch_imm32(&mut words, name_idx, name_addr);

        let mut rt = runtime_with_program_and_extra(&words, name_addr, name, None);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, ERROR_OBJECT_NOT_FOUND);
    }
}
