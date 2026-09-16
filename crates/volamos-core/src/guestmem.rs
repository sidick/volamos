//! Guest memory layout, a host-side heap allocator over guest address
//! space, and BPTR/BSTR/C-string helpers.
//!
//! # Memory layout
//!
//! The guest is a single flat [`crate::memory::FlatMemory`] region (1 MiB
//! as configured by `crates/volamos/src/main.rs`'s `GUEST_MEMORY_SIZE`).
//! Phase 1 fixed two ends of it: the reserved trap-table/jump-table
//! region at the bottom (`[`crate::backend::TRAP_TABLE_BASE`,
//! `crate::backend::TRAP_TABLE_END`)`, with the loaded program starting
//! at [`crate::backend::TRAP_TABLE_END`]), and the initial stack pointer
//! at the very top of memory.
//!
//! This module fixes the rest of the layout:
//!
//! - [`DEFAULT_STACK_SIZE`]: a 64 KiB region at the top of guest memory is
//!   reserved for the stack by default (growing downward from the top,
//!   as `Runtime::new` already sets `A7`). 64 KiB is generous for the
//!   kind of small CLI programs this runtime targets. Phase 3 stage 6
//!   makes this configurable: [`crate::dispatch::StartConfig::stack_size`]
//!   overrides it (clamped to at least [`MIN_STACK_SIZE`]), threaded from
//!   the CLI's `--stack` flag.
//! - The heap occupies the space between the end of the loaded program
//!   and the base of the stack region (`stack base = memory length -
//!   STACK_SIZE`, 4-byte aligned).
//!
//! Since T12, heap start is derived directly from where the loaded
//! program actually ends: [`crate::dispatch::Runtime::new`] takes a
//! [`crate::dispatch::StartConfig`] whose `load_end` field (typically
//! [`crate::loader::LoadResult::end`]) becomes the heap's start address,
//! so it never overlaps the program image. [`crate::dispatch::Runtime::
//! set_heap`] remains available to install a different heap outright
//! (e.g. for tests).
//!
//! # Sanitizer mode: redzones and a free quarantine (issue #65)
//!
//! [`GuestHeap`] can optionally run in a heap-sanitizer style mode aimed
//! at catching guest heap-corruption bugs (buffer overruns, use-after-
//! free) rather than merely surviving them. Both features are strictly
//! opt-in and default to off, so with sanitizing disabled `GuestHeap`
//! behaves byte-for-byte as it always has -- this matters because
//! `execmem.rs`'s `FreeMem` handler enforces an exact-size contract
//! against [`GuestHeap::size_of_live_alloc`], so nothing here may change
//! what that reports for a plain, non-sanitized allocation.
//!
//! - **Redzones** ([`GuestHeap::with_redzone_size`] /
//!   [`GuestHeap::set_redzone_size`]): when enabled with a nonzero size,
//!   every [`GuestHeap::alloc`] carves `redzone + user bytes + redzone`
//!   out of the free list instead of just the user bytes, but still
//!   returns the address of the *user* block (just past the leading
//!   redzone) and still reports the plain user size from
//!   `size_of_live_alloc`. The redzone bytes themselves are invisible to
//!   every existing caller; a later step (outside this module) poisons
//!   them in a shadow map using [`GuestHeap::extent_of_live_alloc`], so
//!   any guest access that spills past the requested size lands in
//!   poisoned territory instead of quietly overwriting a neighboring
//!   allocation. Enabling redzones necessarily reduces
//!   [`GuestHeap::total_free`]/[`GuestHeap::largest_free`] (and hence
//!   `AvailMem`) versus the same heap with sanitizing off, because the
//!   redzone bytes are genuinely unavailable for allocation -- that's the
//!   whole point, not a bug.
//! - **Free quarantine** ([`GuestHeap::with_quarantine_budget`] /
//!   [`GuestHeap::set_quarantine_budget`]): when enabled with a nonzero
//!   byte budget, [`GuestHeap::free`] does not immediately return a
//!   block to the free list. It goes onto a FIFO queue instead, and only
//!   once the queue's total size exceeds the budget do the oldest
//!   entries get drained back into the free list (coalescing exactly as
//!   an immediate free would). While a block sits in quarantine its
//!   address cannot be handed out again, which is what makes use-after-
//!   free detectable instead of silently "usually working" because nothing
//!   reused the address yet. Quarantined bytes count neither as free nor
//!   as live -- they are, again, genuinely unavailable, so
//!   `total_free`/`largest_free` correctly exclude them.
//! - **Recently-freed history** ([`GuestHeap::recently_freed_info`]): a
//!   small bounded ring of the most recent frees (address, user size, and
//!   a monotonically increasing serial number -- deliberately not a
//!   wall-clock timestamp, since all that's needed is a stable ordering
//!   for diagnostics) is kept regardless of whether quarantine is
//!   enabled, so a handler that catches a bad access can report "this
//!   address was a live allocation that was freed" even after the block
//!   has long since been coalesced back into the free list.
//! - **Extent queries** ([`GuestHeap::extent_of_live_alloc`],
//!   [`AllocExtent`]): expose the exact byte ranges of a live
//!   allocation's leading redzone, requested user bytes, alignment
//!   slack, and trailing redzone, unambiguously and without requiring a
//!   caller to duplicate this module's rounding/redzone arithmetic.

use crate::memory::AddressSpace;
use std::collections::VecDeque;

/// Default size in bytes of the guest stack region, reserved at the top
/// of guest memory, used when [`crate::dispatch::StartConfig::stack_size`]
/// isn't overridden. Generous for the small CLI-style programs this
/// runtime targets; the `--stack` CLI flag (Phase 3 stage 6) lets a
/// caller raise it for programs that recurse or allocate large stack
/// frames.
pub const DEFAULT_STACK_SIZE: u32 = 64 * 1024;

/// The smallest stack size [`crate::dispatch::Runtime::new`] will honor,
/// mirroring real AmigaOS's own minimum task stack size (`AmigaDOS`'s
/// `Run`/`RunCommand` and `CreateNewProc` both refuse less than this).
/// A [`crate::dispatch::StartConfig::stack_size`] below this is clamped
/// up to it rather than rejected outright -- see
/// [`crate::dispatch::Runtime::new`]'s doc for why a clamp (not an
/// error) was chosen.
pub const MIN_STACK_SIZE: u32 = 4096;

/// Errors [`GuestHeap`] operations can report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuestHeapError {
    /// The heap has no contiguous free block big enough to satisfy an
    /// `alloc` request.
    OutOfMemory { requested: u32, available: u32 },
    /// [`GuestHeap::free`] was called with an address that isn't the
    /// start of a currently-live allocation (already freed, or never
    /// allocated by this heap).
    DoubleOrInvalidFree { addr: u32 },
}

impl std::fmt::Display for GuestHeapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GuestHeapError::OutOfMemory {
                requested,
                available,
            } => write!(
                f,
                "guest heap out of memory: requested {requested} bytes, {available} available"
            ),
            GuestHeapError::DoubleOrInvalidFree { addr } => write!(
                f,
                "guest heap: free of unknown address {addr:#010x} (double free or invalid pointer)"
            ),
        }
    }
}

impl std::error::Error for GuestHeapError {}

/// One free block: `[start, start + size)`, half-open.
#[derive(Debug, Clone, Copy)]
struct FreeBlock {
    start: u32,
    size: u32,
}

/// Default redzone size in bytes used when a caller enables redzones
/// without specifying a size (e.g. a future `--sanitize` CLI flag with no
/// explicit tuning). Large enough to catch the overruns real bugs tend
/// to produce (a few bytes to a small struct's worth), small enough that
/// a heap-sanitized run doesn't balloon the guest's address space needs.
/// Redzones are off (size `0`) unless a caller explicitly opts in --
/// this constant only supplies the *value* to opt in with.
pub const DEFAULT_REDZONE_SIZE: u32 = 32;

/// Default free-quarantine budget in bytes used when a caller enables
/// quarantining without specifying a budget. Large enough that a modest
/// churn of small CLI-program allocations survives in quarantine for a
/// while (making use-after-free reliably visible rather than a
/// probabilistic near-miss), small enough not to starve a 16 MiB-class
/// guest heap of usable free space. Quarantining is off (budget `0`)
/// unless a caller explicitly opts in.
pub const DEFAULT_QUARANTINE_BUDGET: u32 = 64 * 1024;

/// The pattern `--dirty-heap` fills non-`MEMF_CLEAR` allocations with
/// (issue #80). `0xA5` repeated is the conventional debug poison, and it
/// is a deliberate choice over `0x00` or `0xFF` for two reasons: it is
/// unmistakable in a memory dump, and `0xA5A5A5A5` is an **odd**
/// address, so a guest that reads it out of an uninitialized field and
/// then dereferences it as a pointer takes an address error on a 68000
/// immediately rather than quietly reading somewhere plausible. A bug
/// that announces itself beats one that limps on.
pub const DIRTY_HEAP_FILL_BYTE: u8 = 0xA5;

/// A bound on how many recently-freed allocations [`GuestHeap`] remembers
/// for [`GuestHeap::recently_freed_info`], independent of whether the
/// free quarantine itself is enabled. This is a plain ring buffer over a
/// `VecDeque`, not a `HashMap`, because the only query is "was this
/// address freed, and if so what was its most recent free," which a
/// linear scan over a few hundred entries answers plenty fast for a
/// diagnostics path -- it is never on the hot alloc/free path itself.
const RECENTLY_FREED_HISTORY_CAPACITY: usize = 256;

/// Bookkeeping for one currently-live allocation: enough to answer
/// `size_of_live_alloc` (the plain user size, unaffected by redzones) and
/// to reconstruct the full extent (including any redzones and alignment
/// slack) via [`GuestHeap::extent_of_live_alloc`], and to know exactly
/// which underlying free-list block to release on `free` (which, with
/// redzones enabled, is larger than just the user bytes).
#[derive(Debug, Clone, Copy)]
struct LiveAlloc {
    /// The 4-byte-aligned user size -- exactly what `alloc`/
    /// `alloc_with_requested`'s `size` parameter rounds up to, and
    /// exactly what `size_of_live_alloc` reports. Does not include
    /// redzone bytes.
    user_size: u32,
    /// The caller's true requested size before 4-byte rounding, as
    /// passed to `alloc_with_requested` (or, for a plain `alloc` call,
    /// the same `size` argument -- see its doc). `user_size - requested`
    /// is the alignment slack, which sits between the user's actual data
    /// and the trailing redzone (if any).
    requested: u32,
    /// Start of the whole free-list block this allocation consumed,
    /// including any leading redzone. Equal to the returned user address
    /// when redzones are disabled.
    block_start: u32,
    /// End (exclusive) of the whole free-list block this allocation
    /// consumed, including any trailing redzone.
    block_end: u32,
}

/// One block sitting in the free quarantine, awaiting release back to
/// the free list. Only the raw block range matters here -- the user-
/// facing bookkeeping (size, requested, address) has already been moved
/// into the recently-freed history by the time a block is quarantined.
#[derive(Debug, Clone, Copy)]
struct QuarantinedBlock {
    block_start: u32,
    block_end: u32,
}

/// One entry in [`GuestHeap`]'s recently-freed history, returned by
/// [`GuestHeap::recently_freed_info`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FreedAllocInfo {
    /// The address the allocation was returned at (and freed from).
    pub addr: u32,
    /// The plain user size it was allocated with (same value
    /// `size_of_live_alloc` would have reported while it was live).
    pub size: u32,
    /// A monotonically increasing counter, incremented once per `free`
    /// call, identifying this free's position in the sequence of frees
    /// this heap has processed. Deliberately not a wall-clock timestamp
    /// -- diagnostics only need a stable relative ordering ("which of
    /// two frees happened first"), and a counter gives that exactly and
    /// deterministically, which matters for reproducible test fixtures
    /// and for replaying a guest run.
    pub serial: u64,
}

/// The exact byte layout of one live allocation, as tracked internally
/// by [`GuestHeap`]. Returned by [`GuestHeap::extent_of_live_alloc`] so a
/// caller (e.g. a shadow-memory poisoner) can derive every sub-range of
/// interest without reimplementing this module's rounding/redzone
/// arithmetic:
///
/// - Leading redzone: `[block_start, user_start)`.
/// - The caller's actual requested bytes: `[user_start, user_start +
///   requested_size)`.
/// - Alignment slack (the gap between what was requested and the
///   4-byte-rounded `user_size` that was actually reserved):
///   `[user_start + requested_size, user_start + user_size)`.
/// - Trailing redzone: `[user_start + user_size, block_end)`.
///
/// All four ranges are well-defined (possibly empty) regardless of
/// whether redzones are enabled: with redzones off, `block_start ==
/// user_start` and `block_end == user_start + user_size`, so both
/// redzone ranges are empty and only the requested/slack split remains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllocExtent {
    /// The address `alloc`/`alloc_with_requested` returned.
    pub user_start: u32,
    /// The 4-byte-aligned user size (what `size_of_live_alloc` reports).
    pub user_size: u32,
    /// The caller's true requested size before 4-byte rounding.
    pub requested_size: u32,
    /// Start of the whole underlying free-list block, including any
    /// leading redzone.
    pub block_start: u32,
    /// End (exclusive) of the whole underlying free-list block,
    /// including any trailing redzone.
    pub block_end: u32,
}

/// A simple host-side bump/free-list allocator over a reserved range of
/// guest address space.
///
/// This is host-side bookkeeping only: it tracks which sub-ranges of
/// `[start, end)` are free or allocated, and hands back guest addresses,
/// but it never itself reads or writes guest memory (handlers write into
/// memory at the returned address themselves). It does not implement
/// real AmigaOS `MemHeader`/`MemChunk` fidelity -- that's Phase 3; this
/// is just enough to give handlers guest-visible scratch structures
/// (`FileHandle`, `FileInfoBlock`, string buffers, ...).
///
/// All returned addresses are 4-byte aligned. Freeing is by exact start
/// address (matching how `FreeMem`-style APIs are actually called, where
/// the caller passes back exactly the pointer `AllocMem` gave it);
/// adjacent free blocks are coalesced on free to keep fragmentation down,
/// though correctness (never handing out overlapping memory, never
/// losing track of freed space) matters more here than allocator
/// sophistication.
#[derive(Debug, Clone)]
pub struct GuestHeap {
    /// Free blocks, kept sorted by `start` and coalesced so no two are
    /// adjacent or overlapping.
    free: Vec<FreeBlock>,
    /// Live allocations: user address -> bookkeeping, so `free(addr)`
    /// knows how big a block to return (and, with redzones enabled, the
    /// whole underlying block including redzones) and can detect unknown
    /// addresses.
    live: std::collections::HashMap<u32, LiveAlloc>,
    /// Redzone size in bytes placed both before and after every
    /// allocation's user bytes. `0` (the default) disables redzones
    /// entirely, making `alloc`/`free` behave exactly as they did before
    /// this feature existed. Always kept 4-byte aligned (see
    /// `set_redzone_size`) so the returned user address stays 4-byte
    /// aligned as documented.
    redzone_size: u32,
    /// Total bytes a quarantined block may occupy before the oldest
    /// entries get drained back to the free list. `0` (the default)
    /// disables quarantining, making `free` return blocks to the free
    /// list immediately as it did before this feature existed.
    quarantine_budget: u32,
    /// Blocks freed while quarantining is enabled, oldest first. Drained
    /// from the front once `quarantine_bytes` exceeds `quarantine_budget`.
    quarantine: VecDeque<QuarantinedBlock>,
    /// Sum of `block_end - block_start` for every entry currently in
    /// `quarantine` -- kept as a running total rather than recomputed
    /// each `free` so draining stays O(1) amortized instead of O(n) per
    /// call.
    quarantine_bytes: u32,
    /// Bounded history of recent frees, oldest first, capped at
    /// `RECENTLY_FREED_HISTORY_CAPACITY` entries. Populated on every
    /// `free` regardless of whether quarantining is enabled, since it's
    /// pure diagnostics and doesn't affect allocator behavior.
    recently_freed: VecDeque<FreedAllocInfo>,
    /// Monotonically increasing counter, incremented once per `free`
    /// call, used to stamp `FreedAllocInfo::serial`. Deliberately a
    /// plain counter rather than a wall-clock timestamp -- see
    /// `FreedAllocInfo::serial`'s doc.
    next_free_serial: u64,
    /// The byte every non-`MEMF_CLEAR` allocation's user range should be
    /// filled with, or `None` (the default) to leave it alone -- issue
    /// #80's `--dirty-heap`.
    ///
    /// This is **policy only**. `GuestHeap` deliberately never reads or
    /// writes guest memory itself (see this type's own doc), so it
    /// stores the pattern but never applies it; `crate::execmem`'s alloc
    /// handlers, which already hold both the memory and the
    /// `MEMF_CLEAR` requirement bits, do the filling. Keeping the flag
    /// here rather than threading a bool through `HandlerContext` is
    /// what makes it reachable from all three of `AllocMem`/`AllocVec`/
    /// `AllocPooled` without a new plumbing parameter.
    dirty_fill: Option<u8>,
}

impl GuestHeap {
    /// Creates a heap managing `[start, end)`. `start` and `end` are
    /// rounded to keep the managed region 4-byte aligned (`start` up,
    /// `end` down); if that leaves nothing to manage (`end <= start`),
    /// the heap is created empty (every `alloc` call returns
    /// `OutOfMemory`).
    ///
    /// Redzones and the free quarantine both default to off (sizes `0`)
    /// -- use [`GuestHeap::with_redzone_size`]/[`GuestHeap::
    /// with_quarantine_budget`] (or the `set_*` equivalents after
    /// construction) to opt in. With both left at their defaults this
    /// type's behavior is unchanged from before those features existed.
    pub fn new(start: u32, end: u32) -> Self {
        let start = align_up(start);
        let end = end & !3;
        let free = if end > start {
            vec![FreeBlock {
                start,
                size: end - start,
            }]
        } else {
            Vec::new()
        };
        Self {
            free,
            live: std::collections::HashMap::new(),
            redzone_size: 0,
            quarantine_budget: 0,
            quarantine: VecDeque::new(),
            quarantine_bytes: 0,
            recently_freed: VecDeque::new(),
            next_free_serial: 0,
            dirty_fill: None,
        }
    }

    /// Sets the byte that `crate::execmem`'s alloc handlers should fill
    /// every non-`MEMF_CLEAR` allocation's user range with, or `None` to
    /// leave fresh allocations as they are (the default). See
    /// [`GuestHeap::dirty_fill`]'s field doc for why this heap stores
    /// the policy but never applies it, and
    /// [`DIRTY_HEAP_FILL_BYTE`] for the conventional value.
    pub fn set_dirty_fill(&mut self, fill: Option<u8>) {
        self.dirty_fill = fill;
    }

    /// The configured `--dirty-heap` fill byte, if any.
    pub fn dirty_fill(&self) -> Option<u8> {
        self.dirty_fill
    }

    /// Builder-style: enables redzones at `redzone_size` bytes (rounded
    /// up to a multiple of 4, so the user address `alloc` returns -- just
    /// past the leading redzone -- stays 4-byte aligned as documented).
    /// Chain off [`GuestHeap::new`]; see the module docs' sanitizer-mode
    /// section for what this changes.
    pub fn with_redzone_size(mut self, redzone_size: u32) -> Self {
        self.set_redzone_size(redzone_size);
        self
    }

    /// Builder-style: enables the free quarantine with a `budget_bytes`
    /// byte budget. Chain off [`GuestHeap::new`]; see the module docs'
    /// sanitizer-mode section for what this changes.
    pub fn with_quarantine_budget(mut self, budget_bytes: u32) -> Self {
        self.set_quarantine_budget(budget_bytes);
        self
    }

    /// Sets the redzone size (bytes placed before and after every
    /// allocation's user bytes), rounded up to a multiple of 4. Pass `0`
    /// to disable redzones. Only affects allocations made *after* this
    /// call -- allocations already live keep whatever redzone (or lack
    /// of one) they were created with, since their block extents are
    /// already fixed.
    pub fn set_redzone_size(&mut self, redzone_size: u32) {
        self.redzone_size = align_up(redzone_size);
    }

    /// The current redzone size in bytes (`0` if disabled).
    pub fn redzone_size(&self) -> u32 {
        self.redzone_size
    }

    /// Sets the free quarantine's byte budget. Pass `0` to disable
    /// quarantining -- any blocks already sitting in the quarantine are
    /// immediately drained back to the free list, since a budget of `0`
    /// (like any exceeded budget) means nothing may remain queued.
    pub fn set_quarantine_budget(&mut self, budget_bytes: u32) {
        self.quarantine_budget = budget_bytes;
        self.drain_quarantine_over_budget();
    }

    /// The current free-quarantine byte budget (`0` if disabled).
    pub fn quarantine_budget(&self) -> u32 {
        self.quarantine_budget
    }

    /// Allocates `size` bytes, returning the 4-byte-aligned guest address
    /// of the start of the block, or [`GuestHeapError::OutOfMemory`] if
    /// no free block is large enough. `size` is rounded up to a multiple
    /// of 4 so every allocation's end (and hence the next allocation's
    /// start) stays aligned; a request for `0` bytes still consumes a
    /// (minimal, 0-sized) accounted block so a subsequent `free` on it is
    /// well-defined.
    pub fn alloc(&mut self, size: u32) -> Result<u32, GuestHeapError> {
        self.alloc_with_requested(size, size)
    }

    /// Like [`GuestHeap::alloc`], but additionally records the caller's
    /// true pre-rounding `requested` size, so a later
    /// [`GuestHeap::extent_of_live_alloc`] call can identify the
    /// alignment-slack range unambiguously (the gap between `requested`
    /// and the 4-byte-rounded `size`).
    ///
    /// `alloc`'s own signature is left untouched (existing callers --
    /// `execmem.rs` in particular -- already pass an already-rounded
    /// size and have no separate "true requested size" to give); this
    /// method exists purely so a caller that *does* have the original
    /// pre-rounding size on hand (e.g. a future direct-from-guest `size`
    /// argument, before `execmem.rs`'s own `MEMBLOCKSIZE` rounding) can
    /// supply it without a breaking signature change. `alloc(size)` is
    /// exactly `alloc_with_requested(size, size)`, i.e. "no distinct
    /// requested size, treat the two rounding steps this module already
    /// does as the only slack there is."
    pub fn alloc_with_requested(
        &mut self,
        size: u32,
        requested: u32,
    ) -> Result<u32, GuestHeapError> {
        let aligned_size = align_up(size);

        // Clamp `requested` to what was actually reserved. A caller that
        // passes a `requested` larger than `size` would otherwise leave
        // `AllocExtent` describing an alignment-slack range that runs
        // backwards (`user_start + requested_size` past `user_start +
        // user_size`), and a shadow-map poisoner deriving the slack
        // length as `user_size - requested_size` would underflow. Since
        // the guest can never legitimately touch more than was reserved,
        // clamping is both safe and the only interpretation that keeps
        // every range in `AllocExtent`'s doc well-ordered.
        let requested = requested.min(aligned_size);

        // Redzones (if enabled) sit both before and after the user
        // bytes, so a satisfying free block must hold `redzone + user +
        // redzone`. Compute this with checked arithmetic: a huge `size`
        // plus two redzones must fail cleanly as `OutOfMemory` rather
        // than wrapping around u32 and appearing to fit a tiny block.
        let Some(total_needed) = self
            .redzone_size
            .checked_mul(2)
            .and_then(|redzones| redzones.checked_add(aligned_size))
        else {
            let available = self.free.iter().map(|b| b.size).max().unwrap_or(0);
            return Err(GuestHeapError::OutOfMemory {
                requested: size,
                available,
            });
        };

        // First-fit: good enough for the sizes/allocation counts this
        // runtime deals with, and simple to keep correct. Note this
        // looks for a block big enough for the *whole* block including
        // redzones -- a block that would only fit the user bytes alone
        // correctly fails here rather than handing out an unguarded
        // allocation.
        let Some(idx) = self.free.iter().position(|b| b.size >= total_needed) else {
            let available = self.free.iter().map(|b| b.size).max().unwrap_or(0);
            return Err(GuestHeapError::OutOfMemory {
                requested: size,
                available,
            });
        };

        let block = self.free[idx];
        let block_start = block.start;
        let block_end = block_start + total_needed;
        let user_start = block_start + self.redzone_size;
        if block.size == total_needed {
            self.free.remove(idx);
        } else {
            self.free[idx] = FreeBlock {
                start: block_end,
                size: block.size - total_needed,
            };
        }

        self.live.insert(
            user_start,
            LiveAlloc {
                user_size: aligned_size,
                requested,
                block_start,
                block_end,
            },
        );
        Ok(user_start)
    }

    /// Frees a block previously returned by [`GuestHeap::alloc`].
    ///
    /// Returns [`GuestHeapError::DoubleOrInvalidFree`] if `addr` isn't
    /// the start address of a currently-live allocation (already freed,
    /// or never allocated by this heap) rather than silently corrupting
    /// the free list or aborting -- callers (dos.library handlers) can
    /// turn that into a guest-visible error instead of UB.
    ///
    /// With the free quarantine disabled (the default), the underlying
    /// block -- including any redzones -- is returned to the free list
    /// immediately, coalescing with adjacent free blocks exactly as
    /// before this feature existed. With quarantining enabled, the block
    /// instead goes onto the back of a FIFO queue and is *not* yet
    /// reusable by a subsequent `alloc`; only once the queue's total
    /// size exceeds the configured budget does releasing begin, oldest
    /// blocks first. Either way, `addr` is immediately removed from the
    /// live set, so a second `free(addr)` call correctly reports
    /// `DoubleOrInvalidFree` whether or not the block has actually made
    /// it back to the free list yet.
    pub fn free(&mut self, addr: u32) -> Result<(), GuestHeapError> {
        let Some(live) = self.live.remove(&addr) else {
            return Err(GuestHeapError::DoubleOrInvalidFree { addr });
        };

        self.record_recently_freed(addr, live.user_size);

        if self.quarantine_budget > 0 {
            self.quarantine.push_back(QuarantinedBlock {
                block_start: live.block_start,
                block_end: live.block_end,
            });
            self.quarantine_bytes += live.block_end - live.block_start;
            self.drain_quarantine_over_budget();
        } else {
            self.release_block(live.block_start, live.block_end);
        }
        Ok(())
    }

    /// Releases `[start, end)` back to the free list, coalescing with
    /// adjacent free blocks. This is exactly the coalescing logic
    /// `free` always used, factored out so both an immediate free and a
    /// quarantine drain can share it.
    fn release_block(&mut self, start: u32, end: u32) {
        let mut block = FreeBlock {
            start,
            size: end - start,
        };

        // Keep the free list sorted by start so adjacency checks are a
        // simple neighbor comparison.
        let insert_at = self
            .free
            .iter()
            .position(|b| b.start > block.start)
            .unwrap_or(self.free.len());

        // Merge with the block to the left, if adjacent.
        let merge_left = insert_at > 0 && {
            let left = self.free[insert_at - 1];
            left.start + left.size == block.start
        };
        let left_idx = if merge_left {
            let left = self.free.remove(insert_at - 1);
            block = FreeBlock {
                start: left.start,
                size: left.size + block.size,
            };
            insert_at - 1
        } else {
            insert_at
        };

        // Merge with the block to the right, if adjacent (indices may
        // have shifted by one if we removed a left neighbor above).
        if left_idx < self.free.len() {
            let right = self.free[left_idx];
            if block.start + block.size == right.start {
                block = FreeBlock {
                    start: block.start,
                    size: block.size + right.size,
                };
                self.free.remove(left_idx);
            }
        }

        self.free.insert(left_idx, block);
    }

    /// Drains the free quarantine from the front (oldest first) until
    /// its total size is at or under the current budget, releasing each
    /// drained block back to the free list. Called after every
    /// quarantined `free` and whenever the budget itself shrinks (via
    /// `set_quarantine_budget`), so a budget of `0` correctly drains
    /// everything rather than leaving stale entries queued forever.
    fn drain_quarantine_over_budget(&mut self) {
        while self.quarantine_bytes > self.quarantine_budget {
            let Some(block) = self.quarantine.pop_front() else {
                break;
            };
            self.quarantine_bytes -= block.block_end - block.block_start;
            self.release_block(block.block_start, block.block_end);
        }
    }

    /// Appends a freed allocation to the bounded recently-freed history,
    /// dropping the oldest entry if that would exceed
    /// `RECENTLY_FREED_HISTORY_CAPACITY`.
    fn record_recently_freed(&mut self, addr: u32, size: u32) {
        let serial = self.next_free_serial;
        self.next_free_serial += 1;
        self.recently_freed
            .push_back(FreedAllocInfo { addr, size, serial });
        while self.recently_freed.len() > RECENTLY_FREED_HISTORY_CAPACITY {
            self.recently_freed.pop_front();
        }
    }

    /// Looks up whether `addr` was the start of a recently-freed
    /// allocation, for diagnostics (e.g. a use-after-free report saying
    /// "this address was freed N frees ago"). Only covers a bounded
    /// history (`RECENTLY_FREED_HISTORY_CAPACITY` entries) -- a `None`
    /// here does not prove `addr` was never allocated, only that it
    /// isn't in the recent window. If `addr` was freed more than once
    /// (allocated, freed, reallocated, freed again), the most recent
    /// free is returned.
    pub fn recently_freed_info(&self, addr: u32) -> Option<FreedAllocInfo> {
        self.recently_freed
            .iter()
            .rev()
            .find(|r| r.addr == addr)
            .copied()
    }

    /// Returns the exact byte layout of the live allocation starting at
    /// `addr`, or `None` if `addr` isn't a currently-live allocation.
    /// See [`AllocExtent`]'s doc for how to derive the redzone and
    /// alignment-slack ranges from the returned extent.
    pub fn extent_of_live_alloc(&self, addr: u32) -> Option<AllocExtent> {
        self.live.get(&addr).map(|live| AllocExtent {
            user_start: addr,
            user_size: live.user_size,
            requested_size: live.requested,
            block_start: live.block_start,
            block_end: live.block_end,
        })
    }

    /// Total free bytes remaining across all free blocks (not
    /// necessarily allocatable as one contiguous chunk). Mostly useful
    /// for tests/diagnostics.
    ///
    /// Deliberately excludes redzone bytes belonging to live allocations
    /// and blocks currently sitting in the free quarantine -- both are
    /// genuinely unavailable for a subsequent `alloc` right now, so
    /// counting them as "free" would be a lie an `AvailMem`-driven guest
    /// program could act on (e.g. deciding it has enough room when it
    /// doesn't). This means enabling redzones and/or the free quarantine
    /// reduces what this (and therefore `AvailMem`, via
    /// [`GuestHeap::total_free`]) reports versus the same heap with
    /// sanitizing off -- that's correct and expected, not a regression.
    pub fn free_bytes(&self) -> u32 {
        self.free.iter().map(|b| b.size).sum()
    }

    /// Total free bytes remaining across all free blocks -- an alias for
    /// [`GuestHeap::free_bytes`] under the name `exec.library`'s
    /// `AvailMem` (see `crate::execmem`) uses for its default (non-
    /// `MEMF_LARGEST`) query, so that module doesn't need to know
    /// `free_bytes` predates it.
    pub fn total_free(&self) -> u32 {
        self.free_bytes()
    }

    /// The size in bytes of the single largest free block, or `0` if the
    /// heap has no free space at all. Backs `AvailMem`'s `MEMF_LARGEST`
    /// query (`crate::execmem`): the largest block a single subsequent
    /// `alloc` could satisfy, as opposed to [`GuestHeap::total_free`]'s
    /// sum across every (possibly non-contiguous) free block.
    ///
    /// Like [`GuestHeap::free_bytes`], this excludes redzone bytes and
    /// quarantined blocks -- see that method's doc for why.
    pub fn largest_free(&self) -> u32 {
        self.free.iter().map(|b| b.size).max().unwrap_or(0)
    }

    /// The size of the live allocation starting at `addr`, if any --
    /// i.e. exactly what a prior [`GuestHeap::alloc`] call returned. Used
    /// by `crate::execmem`'s `FreeMem`/`FreeVec` handlers to validate the
    /// size the guest claims it's freeing against what was actually
    /// allocated, without needing a parallel host-side size-tracking map.
    ///
    /// Always the plain user size, never including redzone bytes --
    /// redzones must be completely invisible to this query, since
    /// `execmem.rs`'s `FreeMem` handler errors out loudly if this
    /// doesn't exactly match the (rounded) size the guest passes to
    /// `FreeMem`, and the guest never knows redzones exist.
    pub fn size_of_live_alloc(&self, addr: u32) -> Option<u32> {
        self.live.get(&addr).map(|live| live.user_size)
    }
}

/// Rounds `value` up to the nearest multiple of 4, saturating at `u32::MAX`
/// rather than wrapping to 0.
fn align_up(value: u32) -> u32 {
    value.checked_add(3).map(|v| v & !3).unwrap_or(!3)
}

/// Reads a NUL-terminated string starting at `addr` out of guest memory.
/// The terminator is not included in the returned bytes.
pub fn read_c_string(mem: &dyn AddressSpace, addr: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut a = addr;
    loop {
        let b = mem.read_u8(a);
        if b == 0 {
            break;
        }
        bytes.push(b);
        a = a.wrapping_add(1);
    }
    bytes
}

/// Writes `bytes` followed by a NUL terminator starting at `addr`.
/// Returns the total number of bytes written (`bytes.len() + 1`).
pub fn write_c_string(mem: &mut dyn AddressSpace, addr: u32, bytes: &[u8]) -> u32 {
    let mut a = addr;
    for &b in bytes {
        mem.write_u8(a, b);
        a = a.wrapping_add(1);
    }
    mem.write_u8(a, 0);
    bytes.len() as u32 + 1
}

/// Converts a byte address to a BPTR (a "byte pointer" shifted down to a
/// longword count, per AmigaOS convention: `bptr = addr >> 2`).
///
/// # Panics
///
/// Panics (in debug builds, via the shift) only if used incorrectly is
/// not possible here -- this is a plain shift, valid for any `addr`,
/// though callers should note the low 2 bits of `addr` are lost (BPTRs
/// can only address 4-byte-aligned locations).
pub const fn bptr_from_addr(addr: u32) -> u32 {
    addr >> 2
}

/// Converts a BPTR back to a byte address (`addr = bptr << 2`).
pub const fn addr_from_bptr(bptr: u32) -> u32 {
    bptr << 2
}

/// Reads a BSTR (a length-prefixed, *not* NUL-terminated Amiga string) at
/// byte address `addr`: one length byte (0-255) followed by that many
/// data bytes. `addr` is a byte address (already converted from a BPTR
/// via [`addr_from_bptr`] if the caller had one); the returned `Vec`
/// does not include the length byte.
pub fn read_bstr(mem: &dyn AddressSpace, addr: u32) -> Vec<u8> {
    let len = mem.read_u8(addr) as u32;
    let mut bytes = Vec::with_capacity(len as usize);
    for i in 0..len {
        bytes.push(mem.read_u8(addr.wrapping_add(1 + i)));
    }
    bytes
}

/// Writes `bytes` as a BSTR at byte address `addr`: a length byte
/// followed by the data. BSTR lengths are a single byte, so `bytes`
/// longer than 255 is truncated to the first 255 bytes (rather than
/// erroring) -- this matches how the real AmigaOS convention has no
/// representation for longer BSTRs at all, so silently truncating (and
/// telling the caller how many bytes actually got written) is more
/// useful to a handler than a hard error would be here.
///
/// Returns the number of *data* bytes written (i.e. `min(bytes.len(),
/// 255)`), not counting the length-prefix byte.
pub fn write_bstr(mem: &mut dyn AddressSpace, addr: u32, bytes: &[u8]) -> u8 {
    let len = bytes.len().min(255) as u8;
    mem.write_u8(addr, len);
    for (i, &b) in bytes[..len as usize].iter().enumerate() {
        mem.write_u8(addr.wrapping_add(1 + i as u32), b);
    }
    len
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::FlatMemory;

    #[test]
    fn alloc_returns_4_byte_aligned_addresses() {
        let mut heap = GuestHeap::new(0x1001, 0x2000);
        // start rounds up to 0x1004.
        let a = heap.alloc(3).unwrap();
        assert_eq!(a % 4, 0);
        assert_eq!(a, 0x1004);
        let b = heap.alloc(1).unwrap();
        assert_eq!(b % 4, 0);
        // 3 rounds up to 4, so b should be right after a's 4-byte block.
        assert_eq!(b, 0x1008);
    }

    #[test]
    fn alloc_free_realloc_reuses_freed_block() {
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let a = heap.alloc(16).unwrap();
        heap.free(a).unwrap();
        let b = heap.alloc(16).unwrap();
        assert_eq!(a, b, "freed block should be reused by a same-size alloc");
    }

    #[test]
    fn alloc_free_coalesces_adjacent_blocks() {
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let a = heap.alloc(16).unwrap();
        let b = heap.alloc(16).unwrap();
        let c = heap.alloc(16).unwrap();
        heap.free(a).unwrap();
        heap.free(c).unwrap();
        heap.free(b).unwrap();
        // Everything freed and coalesced back into one block: a single
        // alloc of the whole managed region should now succeed.
        let big = heap.alloc(0x1000 - 16 * 3).unwrap();
        assert_eq!(big, a);
    }

    #[test]
    fn alloc_exhaustion_returns_out_of_memory_err() {
        let mut heap = GuestHeap::new(0x1000, 0x1010); // 16 bytes total
        heap.alloc(16).unwrap();
        let err = heap.alloc(4).unwrap_err();
        match err {
            GuestHeapError::OutOfMemory { requested, .. } => assert_eq!(requested, 4),
            other => panic!("expected OutOfMemory, got {other:?}"),
        }
    }

    #[test]
    fn double_free_is_detected_as_an_error() {
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let a = heap.alloc(16).unwrap();
        heap.free(a).unwrap();
        let err = heap.free(a).unwrap_err();
        assert_eq!(err, GuestHeapError::DoubleOrInvalidFree { addr: a });
    }

    #[test]
    fn free_of_never_allocated_address_is_detected() {
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let err = heap.free(0x1234).unwrap_err();
        assert_eq!(err, GuestHeapError::DoubleOrInvalidFree { addr: 0x1234 });
    }

    #[test]
    fn empty_heap_range_always_out_of_memory() {
        let mut heap = GuestHeap::new(0x2000, 0x1000); // end <= start
        assert!(heap.alloc(1).is_err());
    }

    #[test]
    fn c_string_round_trip() {
        let mut mem = FlatMemory::new(0x100);
        let n = write_c_string(&mut mem, 0x10, b"hello");
        assert_eq!(n, 6); // 5 bytes + NUL
        assert_eq!(mem.read_u8(0x15), 0);
        assert_eq!(read_c_string(&mem, 0x10), b"hello");
    }

    #[test]
    fn c_string_empty_round_trip() {
        let mut mem = FlatMemory::new(0x10);
        write_c_string(&mut mem, 0, b"");
        assert_eq!(read_c_string(&mem, 0), Vec::<u8>::new());
    }

    #[test]
    fn bptr_round_trip() {
        let addr = 0x1004u32;
        let bptr = bptr_from_addr(addr);
        assert_eq!(bptr, 0x401);
        assert_eq!(addr_from_bptr(bptr), addr);
    }

    #[test]
    fn bstr_round_trip() {
        let mut mem = FlatMemory::new(0x100);
        let n = write_bstr(&mut mem, 0x20, b"amiga");
        assert_eq!(n, 5);
        assert_eq!(mem.read_u8(0x20), 5);
        assert_eq!(read_bstr(&mem, 0x20), b"amiga");
    }

    #[test]
    fn bstr_truncates_at_255_bytes() {
        let mut mem = FlatMemory::new(0x400);
        let long = vec![b'x'; 300];
        let n = write_bstr(&mut mem, 0, &long);
        assert_eq!(n, 255);
        assert_eq!(mem.read_u8(0), 255);
        let round = read_bstr(&mem, 0);
        assert_eq!(round.len(), 255);
        assert!(round.iter().all(|&b| b == b'x'));
    }

    #[test]
    fn total_free_sums_disjoint_free_blocks() {
        let mut heap = GuestHeap::new(0x1000, 0x1000 + 48);
        assert_eq!(heap.total_free(), 48);
        let a = heap.alloc(16).unwrap();
        let _b = heap.alloc(16).unwrap();
        let _c = heap.alloc(16).unwrap();
        assert_eq!(heap.total_free(), 0);
        heap.free(a).unwrap();
        assert_eq!(heap.total_free(), 16);
    }

    #[test]
    fn largest_free_finds_the_biggest_block_even_when_fragmented() {
        let mut heap = GuestHeap::new(0x1000, 0x1000 + 48);
        let a = heap.alloc(16).unwrap();
        let _b = heap.alloc(16).unwrap();
        let _c = heap.alloc(16).unwrap();
        assert_eq!(heap.largest_free(), 0);
        // Free the first and third blocks (non-adjacent to each other,
        // so they don't coalesce into one bigger block): two 16-byte
        // free blocks, not one 32-byte one.
        heap.free(a).unwrap();
        heap.free(_c).unwrap();
        assert_eq!(heap.largest_free(), 16);
        assert_eq!(heap.total_free(), 32);
    }

    #[test]
    fn largest_free_is_zero_on_an_empty_heap() {
        let heap = GuestHeap::new(0x2000, 0x1000); // end <= start -> empty
        assert_eq!(heap.largest_free(), 0);
        assert_eq!(heap.total_free(), 0);
    }

    #[test]
    fn size_of_live_alloc_reports_the_rounded_size_and_none_when_unknown() {
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let a = heap.alloc(13).unwrap(); // rounds up to 16
        assert_eq!(heap.size_of_live_alloc(a), Some(16));
        assert_eq!(heap.size_of_live_alloc(0x1234), None);
        heap.free(a).unwrap();
        assert_eq!(heap.size_of_live_alloc(a), None);
    }

    #[test]
    fn bstr_exactly_255_bytes_is_not_truncated() {
        let mut mem = FlatMemory::new(0x400);
        let exact = vec![b'y'; 255];
        let n = write_bstr(&mut mem, 0, &exact);
        assert_eq!(n, 255);
        assert_eq!(read_bstr(&mem, 0), exact);
    }

    // -- Sanitizer mode: redzones, free quarantine, extents (issue #65) --

    #[test]
    fn redzones_off_by_default_is_byte_identical_to_plain_allocator() {
        // No `with_redzone_size`/`set_redzone_size` call at all: every
        // existing behavior (addresses, sizes, free-list state) must be
        // untouched by the sanitizer additions.
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        assert_eq!(heap.redzone_size(), 0);
        assert_eq!(heap.quarantine_budget(), 0);
        let a = heap.alloc(16).unwrap();
        assert_eq!(a, 0x1000);
        assert_eq!(heap.size_of_live_alloc(a), Some(16));
        let b = heap.alloc(16).unwrap();
        assert_eq!(b, 0x1010, "no redzone gap between consecutive allocs");
        heap.free(a).unwrap();
        let c = heap.alloc(16).unwrap();
        assert_eq!(
            c, a,
            "freed block reused immediately, as before quarantine existed"
        );
    }

    #[test]
    fn redzones_on_return_user_address_past_leading_redzone_with_plain_reported_size() {
        let mut heap = GuestHeap::new(0x1000, 0x3000).with_redzone_size(32);
        let a = heap.alloc(16).unwrap();
        // The whole block reserved is redzone(32) + user(16) + redzone(32),
        // starting at the heap base 0x1000, so the user address sits 32
        // bytes in.
        assert_eq!(a, 0x1000 + 32);
        // size_of_live_alloc must still report the plain user size, with
        // no trace of the redzones.
        assert_eq!(heap.size_of_live_alloc(a), Some(16));
    }

    #[test]
    fn consecutive_allocations_with_redzones_are_separated_by_at_least_two_redzones() {
        let mut heap = GuestHeap::new(0x1000, 0x4000).with_redzone_size(32);
        let a = heap.alloc(16).unwrap();
        let b = heap.alloc(16).unwrap();
        assert!(b > a, "b should be allocated after a");
        // Between the end of a's user bytes and the start of b's user
        // bytes there must be at least a's trailing redzone plus b's
        // leading redzone (64 bytes total here).
        let gap = b - (a + 16);
        assert!(
            gap >= 64,
            "expected at least two redzones (64 bytes) between allocations, got {gap}"
        );
    }

    #[test]
    fn redzone_request_that_only_fits_without_redzones_fails_out_of_memory() {
        // Exactly 16 bytes free: a plain 16-byte alloc fits, but with a
        // 32-byte redzone on each side it needs 80 bytes and must fail
        // cleanly rather than handing out an unguarded block.
        let mut heap = GuestHeap::new(0x1000, 0x1010).with_redzone_size(32);
        let err = heap.alloc(16).unwrap_err();
        match err {
            GuestHeapError::OutOfMemory { requested, .. } => assert_eq!(requested, 16),
            other => panic!("expected OutOfMemory, got {other:?}"),
        }
    }

    #[test]
    fn huge_request_with_redzones_saturates_instead_of_overflowing() {
        let mut heap = GuestHeap::new(0x1000, 0x2000).with_redzone_size(32);
        // Near-u32::MAX size plus two redzones would overflow u32 if
        // computed naively; it must fail cleanly instead.
        let err = heap.alloc(u32::MAX - 8).unwrap_err();
        match err {
            GuestHeapError::OutOfMemory { .. } => {}
            other => panic!("expected OutOfMemory, got {other:?}"),
        }
    }

    #[test]
    fn quarantine_holds_a_freed_address_back_from_immediate_reuse() {
        let mut heap = GuestHeap::new(0x1000, 0x2000).with_quarantine_budget(1024);
        let a = heap.alloc(16).unwrap();
        heap.free(a).unwrap();
        // The freed block is still within budget (16 bytes << 1024), so
        // it should not have been released back to the free list yet: a
        // same-size alloc must land somewhere else.
        let b = heap.alloc(16).unwrap();
        assert_ne!(
            a, b,
            "quarantined address must not be handed out immediately"
        );
    }

    #[test]
    fn quarantine_drains_fifo_once_over_budget_and_the_space_is_reusable() {
        // Budget holds exactly one 16-byte block; a second free must
        // push the first back out to the free list (oldest first).
        let mut heap = GuestHeap::new(0x1000, 0x2000).with_quarantine_budget(16);
        let a = heap.alloc(16).unwrap();
        let b = heap.alloc(16).unwrap();
        heap.free(a).unwrap(); // quarantine now holds exactly 16 bytes (at budget, not over)
        heap.free(b).unwrap(); // pushes quarantine to 32 bytes, over budget: drains `a` first

        // `a`'s block should now be back in the free list and coalesced
        // with any adjacent space, so it's allocatable again.
        let c = heap.alloc(16).unwrap();
        assert_eq!(
            c, a,
            "drained block should be released in FIFO order and reusable"
        );
    }

    #[test]
    fn zero_quarantine_budget_behaves_like_immediate_free() {
        let mut heap = GuestHeap::new(0x1000, 0x2000).with_quarantine_budget(0);
        let a = heap.alloc(16).unwrap();
        heap.free(a).unwrap();
        let b = heap.alloc(16).unwrap();
        assert_eq!(a, b, "budget 0 disables quarantining entirely");
    }

    #[test]
    fn lowering_quarantine_budget_drains_blocks_that_no_longer_fit() {
        let mut heap = GuestHeap::new(0x1000, 0x2000).with_quarantine_budget(1024);
        let a = heap.alloc(16).unwrap();
        heap.free(a).unwrap();
        // Still well within the free list as "not free" (quarantined).
        assert_eq!(heap.total_free(), heap.free_bytes());
        // Shrinking the budget below what's queued must drain it
        // immediately, not just on the next free.
        heap.set_quarantine_budget(0);
        let b = heap.alloc(16).unwrap();
        assert_eq!(
            a, b,
            "shrinking the budget should have drained the queued block"
        );
    }

    #[test]
    fn recently_freed_info_answers_for_a_freed_address_and_not_for_a_stranger() {
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let a = heap.alloc(16).unwrap();
        assert_eq!(
            heap.recently_freed_info(a),
            None,
            "still live, not freed yet"
        );
        heap.free(a).unwrap();
        let info = heap.recently_freed_info(a).expect("a was just freed");
        assert_eq!(info.addr, a);
        assert_eq!(info.size, 16);
        // A never-allocated address must not spuriously match.
        assert_eq!(heap.recently_freed_info(0x1234), None);
    }

    #[test]
    fn recently_freed_info_reports_the_most_recent_free_serial_order() {
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let a = heap.alloc(16).unwrap();
        heap.free(a).unwrap();
        let first = heap.recently_freed_info(a).unwrap();
        let b = heap.alloc(16).unwrap();
        assert_eq!(a, b, "immediately reusable: quarantine is off by default");
        heap.free(b).unwrap();
        let second = heap.recently_freed_info(a).unwrap();
        assert!(
            second.serial > first.serial,
            "serial must increase monotonically"
        );
    }

    #[test]
    fn extent_of_live_alloc_is_exact_with_redzones_and_alignment_slack() {
        let mut heap = GuestHeap::new(0x1000, 0x3000).with_redzone_size(32);
        // Request 13 (true "requested" size) which aligns up to 16: 3
        // bytes of alignment slack between the requested data and the
        // trailing redzone.
        let a = heap.alloc_with_requested(13, 13).unwrap();
        let extent = heap.extent_of_live_alloc(a).expect("a is live");
        assert_eq!(extent.user_start, a);
        assert_eq!(extent.user_size, 16);
        assert_eq!(extent.requested_size, 13);
        assert_eq!(extent.block_start, a - 32, "leading redzone is 32 bytes");
        assert_eq!(
            extent.block_end,
            a + 16 + 32,
            "trailing redzone is 32 bytes"
        );

        // Derived ranges, unambiguous from the extent alone:
        let leading_redzone = extent.block_start..extent.user_start;
        let requested_data = extent.user_start..(extent.user_start + extent.requested_size);
        let alignment_slack =
            (extent.user_start + extent.requested_size)..(extent.user_start + extent.user_size);
        let trailing_redzone = (extent.user_start + extent.user_size)..extent.block_end;

        assert_eq!(leading_redzone, (a - 32)..a);
        assert_eq!(requested_data, a..(a + 13));
        assert_eq!(alignment_slack, (a + 13)..(a + 16));
        assert_eq!(trailing_redzone, (a + 16)..(a + 16 + 32));
        assert_eq!(heap.extent_of_live_alloc(0x1234), None);
    }

    #[test]
    fn requested_size_larger_than_the_reservation_is_clamped_so_slack_never_inverts() {
        // A caller that overstates `requested` (more than the block it
        // actually reserved) must not produce an extent whose
        // alignment-slack range runs backwards -- a shadow-map poisoner
        // computes that length as `user_size - requested_size` and would
        // underflow. The guest can never legitimately touch more than
        // was reserved, so clamping is the only well-ordered answer.
        let mut heap = GuestHeap::new(0x1000, 0x3000).with_redzone_size(32);
        let a = heap.alloc_with_requested(16, 999).unwrap();
        let extent = heap.extent_of_live_alloc(a).expect("a is live");
        assert_eq!(extent.user_size, 16);
        assert_eq!(extent.requested_size, 16, "clamped down to the reservation");
        assert_eq!(
            extent.user_size - extent.requested_size,
            0,
            "slack is empty, not a wrapped-around huge value"
        );
        assert!(extent.user_start + extent.requested_size <= extent.block_end);
    }

    #[test]
    fn extent_of_live_alloc_has_empty_redzone_ranges_when_redzones_disabled() {
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let a = heap.alloc(16).unwrap();
        let extent = heap.extent_of_live_alloc(a).unwrap();
        assert_eq!(extent.block_start, extent.user_start);
        assert_eq!(extent.block_end, extent.user_start + extent.user_size);
    }

    #[test]
    fn free_bytes_excludes_redzones_and_quarantined_blocks() {
        let total = 0x100;
        let plain = GuestHeap::new(0x1000, 0x1000 + total);
        assert_eq!(plain.total_free(), total);

        let mut redzoned = GuestHeap::new(0x1000, 0x1000 + total).with_redzone_size(16);
        let a = redzoned.alloc(16).unwrap();
        // 16 (redzone) + 16 (user) + 16 (redzone) = 48 bytes consumed.
        assert_eq!(redzoned.total_free(), total - 48);

        let mut quarantined = GuestHeap::new(0x1000, 0x1000 + total).with_quarantine_budget(1024);
        let b = quarantined.alloc(16).unwrap();
        quarantined.free(b).unwrap();
        assert_eq!(
            quarantined.total_free(),
            total - 16,
            "quarantined block must not count as free"
        );

        redzoned.free(a).unwrap();
    }
}
