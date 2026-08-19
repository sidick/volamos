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

use std::path::PathBuf;

use crate::cpu::{Cpu, DataRegister};
use crate::dispatch::{DOS_LIBRARY_BASE, DispatchError, HandlerContext, LibraryTable};
use crate::dosfile::ERROR_OBJECT_NOT_FOUND;
use crate::dosfile::{
    DosState, ERROR_FILE_NOT_OBJECT, ERROR_NO_FREE_STORE, map_io_error, map_vfs_error,
};
use crate::guestmem::{GuestHeap, GuestHeapError, bptr_from_addr, read_c_string};
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
const SEG_HEADER_SIZE: u32 = 8;
/// Byte offset of `next_seg` within a segment allocation -- also where a
/// segment's `BPTR` points (`bptr_from_addr(alloc_addr + NEXT_SEG_OFFSET)`).
const NEXT_SEG_OFFSET: u32 = 4;

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
        match heap.alloc(total) {
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
    /// `RunCommand`'s explicit `stack` argument, threaded through so the
    /// nested run gets the *real* requested stack size instead of
    /// silently falling back to the parent's own `--stack`/default --
    /// `None` for `System()`/`Execute()`, which have no such argument at
    /// all (a runner should fall back to whatever it already uses for
    /// those in that case).
    pub stack_size_override: Option<u32>,
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
    /// builds a seglist for it via [`build_seglist`]. Returns the
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
        let seglist = build_seglist(&hunk_file, heap, mem).map_err(|_| ERROR_NO_FREE_STORE)?;
        self.seglists
            .insert(seglist.first_bptr, seglist.alloc_addrs);
        self.seglist_host_paths
            .insert(seglist.first_bptr, host_path);
        Ok(seglist.first_bptr)
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
        self.seglist_host_paths.remove(&bptr);
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
        let host_path = vfs
            .resolve(program, ResolveMode::MustExist)
            .map_err(|e| map_vfs_error(&e))?;

        let runner = self.system_runner.as_mut().ok_or(ERROR_OBJECT_NOT_FOUND)?;
        let request = SystemRequest {
            command_line: command_line.to_string(),
            resolved_program_host_path: host_path,
            args,
            stack_size_override: None,
        };
        Ok(runner(&request))
    }

    /// `RunCommand(seg, stack, paramptr, paramlen)`: re-runs the program
    /// a prior [`Self::load_seg`] call loaded, via the same
    /// [`Self::system_runner`] nested-execution path `System()`/
    /// `Execute()` use -- see [`crate::dosfile::DosState::
    /// seglist_host_paths`]'s doc for why this re-runs from the
    /// remembered host path rather than executing the already-loaded
    /// seglist bytes in place, and the faithfulness trade-off that
    /// implies. `paramptr`/`paramlen` (the raw command-tail buffer, not
    /// necessarily NUL-terminated) is decoded as UTF-8 (lossily) and
    /// split on whitespace into `args`, matching `System()`/`Execute()`'s
    /// own documented no-quoting scope cut -- real `RunCommand` callers
    /// pass exactly this kind of plain, space-separated argument buffer
    /// in the overwhelming majority of real-world use. Returns the
    /// invoked command's exit code, or `-1` with `IoErr()` set if it
    /// couldn't be run at all (unknown `seg`, or no runner installed --
    /// same [`ERROR_OBJECT_NOT_FOUND`] "couldn't run it" bucket
    /// [`Self::resolve_and_run`] uses).
    pub fn run_command(&mut self, seg: u32, args: Vec<String>, stack_size: u32) -> i32 {
        match self.resolve_and_run_command(seg, args, stack_size) {
            Ok(code) => code,
            Err(io_err) => {
                self.set_io_err(io_err);
                -1
            }
        }
    }

    fn resolve_and_run_command(
        &mut self,
        seg: u32,
        args: Vec<String>,
        stack_size: u32,
    ) -> Result<i32, i32> {
        let host_path = self
            .seglist_host_paths
            .get(&seg)
            .cloned()
            .ok_or(ERROR_OBJECT_NOT_FOUND)?;
        let runner = self.system_runner.as_mut().ok_or(ERROR_OBJECT_NOT_FOUND)?;
        let request = SystemRequest {
            command_line: args.join(" "),
            resolved_program_host_path: host_path,
            args,
            stack_size_override: Some(stack_size),
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

/// `LoadSeg` (`D1` = name `CString*`). `D0` = seglist `BPTR`, or `0` with
/// `IoErr()` set.
fn loadseg_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let name_ptr = ctx.cpu.data_register(DataRegister(1));
    let name = String::from_utf8_lossy(&read_c_string(ctx.mem, name_ptr)).into_owned();
    match ctx.dos.load_seg(ctx.heap, ctx.mem, &name) {
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
/// `IoErr()` set if it couldn't be run at all -- see
/// [`DosState::run_command`]'s doc.
fn run_command_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let seg = ctx.cpu.data_register(DataRegister(1));
    let stack_size = ctx.cpu.data_register(DataRegister(2));
    let param_ptr = ctx.cpu.data_register(DataRegister(3));
    let param_len = ctx.cpu.data_register(DataRegister(4));

    let mut param_bytes = Vec::with_capacity(param_len as usize);
    for i in 0..param_len {
        param_bytes.push(ctx.mem.read_u8(param_ptr.wrapping_add(i)));
    }
    let param_str = String::from_utf8_lossy(&param_bytes);
    let args: Vec<String> = param_str.split_whitespace().map(str::to_string).collect();

    let code = ctx.dos.run_command(seg, args, stack_size);
    ctx.cpu.set_data_register(DataRegister(0), code as u32);
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
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
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
