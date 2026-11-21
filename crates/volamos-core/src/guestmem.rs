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
/// - Leading redzone: `[block_start, user_start)`. For an allocation made
///   with [`GuestHeap::alloc_aligned_for`] this also holds the alignment
///   gap (at most `align - 4` bytes), so it can be non-empty even with
///   redzones off.
/// - The caller's actual requested bytes: `[user_start, user_start +
///   requested_size)`.
/// - Alignment slack (the gap between what was requested and the
///   4-byte-rounded `user_size` that was actually reserved):
///   `[user_start + requested_size, user_start + user_size)`.
/// - Trailing redzone: `[user_start + user_size, block_end)`.
///
/// All four ranges are well-defined (possibly empty) regardless of
/// whether redzones are enabled: with redzones off and the default
/// 4-byte alignment, `block_start == user_start` and `block_end ==
/// user_start + user_size`, so both redzone ranges are empty and only
/// the requested/slack split remains.
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
    /// The memory map this heap's range was carved from, if it was built
    /// with [`GuestHeap::with_layout`]. Gives each free block a class
    /// (chip, slow, fast, ...), which drives [`GuestHeap::alloc_for`]'s
    /// placement and keeps free blocks from different regions from ever
    /// coalescing. `None` (every plain [`GuestHeap::new`]) is one
    /// anonymous range and behaves exactly as before the layout existed.
    layout: Option<crate::memmap::MemoryLayout>,
    /// First heap address of the layout's chip region (the `start` given
    /// to [`GuestHeap::with_layout`]), where its `MemHeader` reports
    /// `mh_Lower`.
    heap_start: u32,
    /// Bumped by every change to the free list; compared with
    /// [`Self::memlist_synced`] so the guest-visible `MemList` is only
    /// rewritten when something changed since it was last written.
    generation: u64,
    memlist_synced: u64,
    /// The `MemHeader`s [`GuestHeap::install_memlist`] built.
    memheaders: Vec<MemHeaderSlot>,
}

/// A `MemHeader` the heap keeps in sync with one region's free blocks.
#[derive(Debug, Clone, Copy)]
struct MemHeaderSlot {
    /// Guest address of the `struct MemHeader`.
    addr: u32,
    /// Index of the region in the layout.
    region: usize,
}

/// `sizeof(struct MemHeader)` (`<exec/memory.h>`): a 14-byte `Node`,
/// `mh_Attributes` (`UWORD`), `mh_First`, `mh_Lower`, `mh_Upper` and
/// `mh_Free` (4 bytes each). Confirmed `32` on Kickstart 3.1.
pub const MEMHEADER_SIZE: u32 = 32;
/// Bytes at the start of every non-chip region held back from the heap
/// for its `MemHeader` (32) and name string (up to 32): real exec keeps
/// each header at the start of the memory it describes, `mh_Lower` just
/// above it.
pub const MEMHEADER_PROLOGUE: u32 = 64;
const MH_ATTRIBUTES: u32 = 14;
const MH_FIRST: u32 = 16;
const MH_LOWER: u32 = 20;
const MH_UPPER: u32 = 24;
const MH_FREE: u32 = 28;
/// `NT_MEMORY` (`<exec/nodes.h>`): a `MemHeader`'s `ln_Type`.
const NT_MEMORY: u8 = 10;
/// Smallest free block a `MemChunk` (`mc_Next` + `mc_Bytes`, 8 bytes)
/// can describe.
const MEMCHUNK_SIZE: u32 = 8;

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
            layout: None,
            heap_start: 0,
            generation: 0,
            memlist_synced: 0,
            memheaders: Vec::new(),
        }
    }

    /// Creates a heap managing the part of each region of `layout` that
    /// lies inside `[start, end)`, one free block per region (never
    /// merged across a region boundary, even where two regions abut).
    /// `start` and `end` are rounded as in [`GuestHeap::new`]. This is
    /// the heap a run with a real memory map uses: see
    /// [`GuestHeap::alloc_for`] for how requirements pick a region.
    pub fn with_layout(layout: crate::memmap::MemoryLayout, start: u32, end: u32) -> Self {
        let mut heap = Self::new(0, 0);
        let start = u64::from(align_up(start));
        let end = u64::from(end & !3);
        heap.heap_start = start as u32;
        for r in layout.regions() {
            // A region other than chip starts with its MemHeader and name
            // (see MEMHEADER_PROLOGUE), held back from the heap.
            let first = if r.kind == crate::memmap::RegionKind::Chip {
                u64::from(r.base)
            } else {
                u64::from(r.base) + u64::from(MEMHEADER_PROLOGUE)
            };
            let lo = start.max(first);
            // Aligned down like `GuestHeap::new`'s end, so a region that
            // ends on an odd address still yields 4-byte-aligned blocks.
            let hi = end.min(r.end()) & !3;
            if hi > lo {
                heap.free.push(FreeBlock {
                    start: lo as u32,
                    size: (hi - lo) as u32,
                });
            }
        }
        heap.layout = Some(layout);
        heap
    }

    /// Builds one `MemHeader` per region of the layout and links them,
    /// highest priority first, into the `struct List` at `list_addr`
    /// (`ExecBase.MemList`), so a guest can enumerate memory the way
    /// real programs do (issue #318). The chip region's header and name
    /// go at `chip_header_addr` (real exec keeps it in the low memory it
    /// owns), every other region's at its own base, in the prologue
    /// [`GuestHeap::with_layout`] held back. Names, priorities and
    /// attribute words are the real Kickstart 3.1 ones. Does nothing
    /// without a layout. Call [`GuestHeap::sync_memlist`] afterwards (and
    /// it is called again, only when the free list has changed, wherever
    /// a guest could be about to look).
    pub fn install_memlist(
        &mut self,
        mem: &mut dyn AddressSpace,
        list_addr: u32,
        chip_header_addr: u32,
    ) {
        let Some(layout) = self.layout.clone() else {
            return;
        };
        crate::execlist::init_list_header(mem, list_addr);
        let mut order: Vec<usize> = (0..layout.regions().len()).collect();
        // Highest priority first; address breaks ties.
        order.sort_by_key(|&i| {
            (
                std::cmp::Reverse(layout.regions()[i].kind.priority()),
                layout.regions()[i].base,
            )
        });
        self.memheaders.clear();
        for i in order {
            let r = layout.regions()[i];
            let addr = if r.kind == crate::memmap::RegionKind::Chip {
                chip_header_addr
            } else {
                r.base
            };
            let name_addr = addr + MEMHEADER_SIZE;
            let name = r.kind.header_name().as_bytes();
            for (k, b) in name.iter().chain(std::iter::once(&0)).enumerate() {
                mem.write_u8(name_addr + k as u32, *b);
            }
            mem.write_u8(addr + crate::execlist::LN_TYPE, NT_MEMORY);
            mem.write_u8(addr + crate::execlist::LN_PRI, r.kind.priority() as u8);
            mem.write_u32(addr + crate::execlist::LN_NAME, name_addr);
            mem.write_u16(addr + MH_ATTRIBUTES, r.attributes() as u16);
            // The lowest address the header manages: past the loaded
            // program for chip, past the prologue for the others.
            let lower = if r.kind == crate::memmap::RegionKind::Chip {
                self.heap_start.max(r.base)
            } else {
                r.base + MEMHEADER_PROLOGUE
            };
            let upper = r.end().min(u64::from(u32::MAX)) as u32;
            // A chip region the program image already fills has nothing
            // left to manage: keep the range empty, never inverted.
            mem.write_u32(addr + MH_LOWER, lower.min(upper));
            mem.write_u32(addr + MH_UPPER, upper);
            mem.write_u32(addr + MH_FIRST, 0);
            mem.write_u32(addr + MH_FREE, 0);
            crate::execlist::add_tail_impl(mem, list_addr, addr);
            self.memheaders.push(MemHeaderSlot { addr, region: i });
        }
        // Force the first sync.
        self.memlist_synced = u64::MAX;
    }

    /// Rewrites each `MemHeader`'s `mh_First` chain and `mh_Free` to
    /// match the free list, if anything has changed since the last time.
    /// The chain is the region's free blocks in address order, each
    /// described by a `MemChunk` (`mc_Next`, `mc_Bytes`) written into the
    /// block's own first 8 bytes, as real exec does, over the 8-aligned part
    /// of each block (blocks too small for one are left out), and `mh_Free` is the sum of the chunks (as on
    /// Kickstart 3.1, where the two always agree).
    ///
    /// Lazy on purpose: callers invoke this where a guest could be about
    /// to enumerate memory (`Forbid`/`Disable`), not on every allocation,
    /// so allocation-heavy programs do not rewrite the list per call. The
    /// chunk bytes are written without the sanitizer's checks and marked
    /// valid, so a guest reading the chain it is entitled to read is
    /// never a violation.
    pub fn sync_memlist(&mut self, mem: &mut dyn AddressSpace) {
        if self.memheaders.is_empty() || self.memlist_synced == self.generation {
            return;
        }
        let Some(layout) = &self.layout else {
            return;
        };
        for slot in &self.memheaders {
            let region = layout.regions()[slot.region];
            // The free list is kept sorted by address and never merges
            // across regions, so a region's blocks come out in order. Each
            // chunk covers only the 8-aligned part of its block, as real
            // exec's chunks are 8-aligned (a block too small for one, or
            // whose aligned part is, is left out).
            let mut next_chunk: Option<(u32, u32)> = None;
            let mut first = 0u32;
            let mut total = 0u64;
            // Build back to front so each chunk can name its successor.
            for b in self.free.iter().rev() {
                if !region.contains(b.start) {
                    continue;
                }
                let start = (b.start + 7) & !7;
                let end = (b.start + b.size) & !7;
                if end < start + MEMCHUNK_SIZE {
                    continue;
                }
                let size = end - start;
                mem.clear_fresh_block(start, MEMCHUNK_SIZE);
                poke_u32(mem, start, next_chunk.map_or(0, |(n, _)| n));
                poke_u32(mem, start + 4, size);
                next_chunk = Some((start, size));
                first = start;
                total += u64::from(size);
            }
            mem.write_u32(slot.addr + MH_FIRST, first);
            mem.write_u32(slot.addr + MH_FREE, total.min(u64::from(u32::MAX)) as u32);
        }
        self.memlist_synced = self.generation;
    }

    /// Whether this heap was built with a memory layout
    /// ([`GuestHeap::with_layout`]).
    pub fn has_layout(&self) -> bool {
        self.layout.is_some()
    }

    /// Whether `addr` is in RAM this heap can manage: always true for a
    /// heap without a layout, otherwise inside one of its regions.
    pub fn is_ram(&self, addr: u32) -> bool {
        match &self.layout {
            None => true,
            Some(l) => l.region_index_at(addr).is_some(),
        }
    }

    /// `mh_Attributes` of the region containing `addr` (what `TypeOfMem`
    /// reports), or `None` if there is no layout or `addr` is outside
    /// every region.
    pub fn attributes_at(&self, addr: u32) -> Option<u32> {
        let layout = self.layout.as_ref()?;
        let i = layout.region_index_at(addr)?;
        Some(layout.regions()[i].attributes())
    }

    /// Total bytes of RAM (free or not) in regions whose attributes
    /// include every bit of `required`: `AvailMem(... | MEMF_TOTAL)`.
    /// `None` without a layout.
    pub fn capacity_matching(&self, required: u32) -> Option<u64> {
        Some(self.layout.as_ref()?.total_capacity(required))
    }

    /// Whether the free block at `start` is in a region whose attributes
    /// include every bit of `required`. Always true without a layout.
    fn block_matches(&self, start: u32, required: u32) -> bool {
        match &self.layout {
            None => true,
            Some(l) => l
                .region_index_at(start)
                .is_some_and(|i| l.regions()[i].attributes() & required == required),
        }
    }

    /// Placement rank of the free block at `start`: higher is tried
    /// first (the region's `MemHeader` priority; `0` without a layout).
    fn block_priority(&self, start: u32) -> i8 {
        self.layout
            .as_ref()
            .and_then(|l| {
                l.region_index_at(start)
                    .map(|i| l.regions()[i].kind.priority())
            })
            .unwrap_or(0)
    }

    /// The highest region priority among regions whose attributes include
    /// every bit of `required` (`0` without a layout, or when none match).
    fn best_priority_matching(&self, required: u32) -> i8 {
        self.layout
            .as_ref()
            .and_then(|l| {
                l.regions()
                    .iter()
                    .filter(|r| r.attributes() & required == required)
                    .map(|r| r.kind.priority())
                    .max()
            })
            .unwrap_or(0)
    }

    /// Whether `a` and `b` (a block start and another's) are in the
    /// same region, i.e. whether adjacent free blocks may merge.
    fn same_region(&self, a: u32, b: u32) -> bool {
        match &self.layout {
            None => true,
            Some(l) => l.region_index_at(a) == l.region_index_at(b),
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
    ///
    /// `mem` is the guest address space the block is being handed out
    /// *in*, and is required rather than optional: see
    /// [`Self::alloc_with_requested`]'s doc for why every allocation has
    /// to touch it.
    pub fn alloc(&mut self, mem: &mut dyn AddressSpace, size: u32) -> Result<u32, GuestHeapError> {
        self.alloc_with_requested(mem, size, size)
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
    ///
    /// # Why this takes the address space (issue #95)
    ///
    /// The heap recycles addresses, and under `--sanitize` the block it
    /// is about to hand out may still carry the previous tenant's
    /// [`crate::sanitize::PoisonReason::Freed`] marking -- writing over
    /// a byte does not clear that (the shadow map heals
    /// [`crate::sanitize::ShadowState::Uninit`] bytes but records and
    /// keeps [`crate::sanitize::ShadowState::Unaddressable`] ones). Left
    /// in place, the allocating handler's own header write or
    /// `MEMF_CLEAR` zeroing is reported as a use-after-free, and so is
    /// every subsequent write the guest makes into its brand-new buffer
    /// -- one violation per byte, which is slow enough to look like a
    /// hang.
    ///
    /// Clearing it belongs *here*, at the one point where a block leaves
    /// the heap's control, rather than in each caller: `AllocMem`/
    /// `AllocVec`/`AllocPooled` are only 3 of some four dozen sites
    /// across `dosfile`, `doslock`, `dosargs`, `dosseg`, `dosanchor`,
    /// `bsdsocket`, `intuition`, `graphics`, `locale` and more that
    /// carve guest structures out of this heap, *any* of which can be
    /// handed a block that `FreeMem`/`FreeVec`/`FreePooled` poisoned.
    /// Taking `mem` as a parameter is what makes that impossible to
    /// forget: a new allocation site cannot compile without supplying
    /// it. The whole *block* is cleared, redzones included -- nothing
    /// about the previous tenant applies to these addresses any more --
    /// and callers that want redzone/alignment-slack protection re-poison
    /// afterwards via `execmem::poison_allocation_edges`, which has to
    /// run after the handler's own initializing writes anyway.
    pub fn alloc_with_requested(
        &mut self,
        mem: &mut dyn AddressSpace,
        size: u32,
        requested: u32,
    ) -> Result<u32, GuestHeapError> {
        self.alloc_for(mem, size, requested, 0)
    }

    /// [`GuestHeap::alloc_with_requested`] restricted to regions whose
    /// attributes include every bit of `required` (`MEMF_CHIP`,
    /// `MEMF_FAST`, `MEMF_24BITDMA`, `MEMF_LOCAL`, ...), as real
    /// `AllocMem` selects a `MemHeader`: among the matching regions the
    /// highest-priority one that has a fitting block wins (so with no
    /// requirement fast RAM is preferred and chip is the fallback), and
    /// within a region the lowest-addressed fitting block is used.
    /// Requirements no region satisfies -- `MEMF_CHIP | MEMF_FAST`, or
    /// `MEMF_FAST` on a chip-only machine -- fail with
    /// [`GuestHeapError::OutOfMemory`], as real `AllocMem` returns NULL.
    /// `required == 0` places as a plain heap without a layout always did.
    pub fn alloc_for(
        &mut self,
        mem: &mut dyn AddressSpace,
        size: u32,
        requested: u32,
        required: u32,
    ) -> Result<u32, GuestHeapError> {
        self.alloc_aligned_for(mem, size, requested, required, 4)
    }

    /// [`GuestHeap::alloc_for`] with the returned address aligned to
    /// `align` bytes (4 or 8) instead of the heap's default 4.
    ///
    /// exec's `AllocMem` hands out blocks that sit on a `MEM_BLOCKSIZE`
    /// (8-byte) boundary -- `Allocate`'s autodoc: "All blocks, whether
    /// free or allocated, will be block aligned" -- and this is how the
    /// exec handlers get that without making every other allocation in
    /// the runtime pay 8-byte alignment (see the `execmem` module doc).
    ///
    /// The address is chosen *within* the free block that fits, not by
    /// rounding the heap's own cursor: when the first usable address is
    /// not already `align`-aligned, the gap is folded into the block's
    /// leading redzone (`user_start - block_start` is then `redzone +
    /// pad`), so the bytes stay accounted to the allocation, `free`
    /// returns them with it, and `--sanitize` poisons them like any other
    /// redzone byte. Each candidate free block is judged on its own pad,
    /// since it depends on where that block happens to start.
    pub fn alloc_aligned_for(
        &mut self,
        mem: &mut dyn AddressSpace,
        size: u32,
        requested: u32,
        required: u32,
        align: u32,
    ) -> Result<u32, GuestHeapError> {
        let (user_start, block_start, block_end) =
            self.alloc_untracked(size, requested, required, align)?;
        mem.clear_fresh_block(block_start, block_end - block_start);
        Ok(user_start)
    }

    /// Lets [`Self::alloc_abs`] claim `[start, end)` out of blocks the guest
    /// freed that the quarantine is still holding, returning them to the
    /// free list first. Only when the range is covered *entirely* by free
    /// blocks and quarantined ones, so a claim that cannot succeed does not
    /// shorten the quarantine for nothing; returns whether any block was
    /// released.
    ///
    /// This is not a use-after-free: the guest is explicitly asking for
    /// that memory back. It is also how real code asks for aligned memory
    /// -- allocate `size + align - 1`, `FreeMem` all of it, `AllocAbs` the
    /// aligned part (`mmu.library` does exactly this for its page tables)
    /// -- which under `--sanitize` would otherwise always fail, since the
    /// block it just freed is held in quarantine rather than free.
    fn release_quarantined_for(&mut self, start: u32, end: u32) -> bool {
        let overlapping = |s: u32, e: u32| s < end && e > start;
        let mut spans: Vec<(u32, u32)> = self
            .free
            .iter()
            .filter(|b| overlapping(b.start, b.start + b.size))
            .map(|b| (b.start, b.start + b.size))
            .chain(
                self.quarantine
                    .iter()
                    .filter(|q| overlapping(q.block_start, q.block_end))
                    .map(|q| (q.block_start, q.block_end)),
            )
            .collect();
        spans.sort_unstable();
        let mut covered_to = start;
        for (s, e) in spans {
            if s > covered_to {
                break;
            }
            covered_to = covered_to.max(e);
        }
        if covered_to < end {
            return false;
        }
        let mut released = false;
        let mut i = 0;
        while i < self.quarantine.len() {
            let q = self.quarantine[i];
            if overlapping(q.block_start, q.block_end) {
                self.quarantine.remove(i);
                self.quarantine_bytes -= q.block_end - q.block_start;
                self.release_block(q.block_start, q.block_end);
                released = true;
            } else {
                i += 1;
            }
        }
        released
    }

    /// Claims the exact range `[start, start + size)` as a live allocation,
    /// the way exec's `AllocAbs` does: `Some(start)` when the whole range
    /// lies in one free block (blocks the free quarantine is holding count
    /// as free here, see [`Self::release_quarantined_for`]), `None` (changing nothing) when any of it is
    /// already allocated, reserved, or outside the heap.
    ///
    /// Unlike [`Self::reserve_range`] the range is recorded as a live
    /// allocation, so the guest can `FreeMem` it later (the usual fate of
    /// `AllocAbs` memory) and `--sanitize` tracks it like any other block.
    /// `size` is the 4-byte-aligned size to reserve and `requested` the
    /// caller's true size, as in [`Self::alloc_with_requested`]; `start`
    /// must be 4-byte aligned. When redzones are enabled they are taken
    /// from the free space on either side of the range as far as it is
    /// free and in the same block -- a block right up against another
    /// allocation simply has a shorter (or no) redzone on that side,
    /// since the address is not the heap's to choose here.
    pub fn alloc_abs(
        &mut self,
        mem: &mut dyn AddressSpace,
        start: u32,
        size: u32,
        requested: u32,
    ) -> Option<u32> {
        if size == 0 || !start.is_multiple_of(4) || !size.is_multiple_of(4) {
            return None;
        }
        let end = start.checked_add(size)?;
        let in_one_free_block = |b: &FreeBlock| b.start <= start && end <= b.start + b.size;
        let idx = match self.free.iter().position(in_one_free_block) {
            Some(idx) => idx,
            // Not free yet, but perhaps only because a block the guest
            // freed is sitting in the quarantine: see `release_quarantined_for`.
            None if self.release_quarantined_for(start, end) => {
                self.free.iter().position(in_one_free_block)?
            }
            None => return None,
        };
        let free = self.free[idx];
        let free_end = free.start + free.size;
        let block_start = free.start.max(start.saturating_sub(self.redzone_size));
        let block_end = free_end.min(end.saturating_add(self.redzone_size));

        self.generation += 1;
        self.carve_free_block(idx, block_start, block_end);
        self.live.insert(
            start,
            LiveAlloc {
                user_size: size,
                requested: requested.min(size),
                block_start,
                block_end,
            },
        );
        mem.clear_fresh_block(block_start, block_end - block_start);
        Some(start)
    }

    /// The allocator proper: the bookkeeping half of
    /// [`Self::alloc_with_requested`], with no sanitizer involvement.
    /// Returns `(user_start, block_start, block_end)` so its caller can
    /// clear the shadow map over the whole block without a follow-up
    /// [`Self::extent_of_live_alloc`] lookup.
    ///
    /// Private, and deliberately so: every allocation a guest can ever
    /// see has to go through [`Self::alloc_with_requested`] to get its
    /// shadow bytes cleared, and a caller that could reach this directly
    /// could skip that. Within the module, the test-only `alloc_bare`
    /// shorthands use it for the placement/coalescing tests, which have
    /// no address space in play.
    fn alloc_untracked(
        &mut self,
        size: u32,
        requested: u32,
        required: u32,
        align: u32,
    ) -> Result<(u32, u32, u32), GuestHeapError> {
        assert!(align == 4 || align == 8, "unsupported alignment {align}");
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
            let available = self.largest_free_matching(required);
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
        //
        // With a layout, the candidates are narrowed to regions
        // satisfying `required` and ranked by region priority (highest
        // first), lowest address breaking ties. `free` is sorted by
        // address, so the first fitting block of the best *possible*
        // priority ends the scan: an allocation that finds room in the
        // preferred region costs what plain first-fit always did, and
        // only a full or absent preferred region makes it look further.
        // Without a layout every block has priority 0 and this is
        // exactly first-fit.
        let best_possible = self.best_priority_matching(required);
        let mut chosen: Option<(usize, i8)> = None;
        for (i, b) in self.free.iter().enumerate() {
            let pad = self.align_pad(b.start, align);
            if b.size < total_needed.saturating_add(pad) || !self.block_matches(b.start, required) {
                continue;
            }
            let prio = self.block_priority(b.start);
            if prio >= best_possible {
                chosen = Some((i, prio));
                break;
            }
            if chosen.is_none_or(|(_, best)| prio > best) {
                chosen = Some((i, prio));
            }
        }
        let Some((idx, _)) = chosen else {
            let available = self.largest_free_matching(required);
            return Err(GuestHeapError::OutOfMemory {
                requested: size,
                available,
            });
        };

        let block = self.free[idx];
        let block_start = block.start;
        // The alignment gap (0 for the default 4) is part of this block:
        // it extends the leading redzone, see `alloc_aligned_for`.
        let block_len = total_needed + self.align_pad(block_start, align);
        let block_end = block_start + block_len;
        let user_start = block_end - self.redzone_size - aligned_size;
        if block.size == block_len {
            self.free.remove(idx);
        } else {
            self.free[idx] = FreeBlock {
                start: block_end,
                size: block.size - block_len,
            };
        }

        self.generation += 1;
        self.live.insert(
            user_start,
            LiveAlloc {
                user_size: aligned_size,
                requested,
                block_start,
                block_end,
            },
        );
        Ok((user_start, block_start, block_end))
    }

    /// Bytes to skip at the front of a free block starting at
    /// `block_start` so that the user address (`block_start` + leading
    /// redzone + pad) lands on an `align` boundary. Always 0 for `align`
    /// 4, since every free block start and every redzone is a multiple
    /// of 4.
    fn align_pad(&self, block_start: u32, align: u32) -> u32 {
        let first_user = block_start.wrapping_add(self.redzone_size);
        first_user.wrapping_neg() & (align - 1)
    }

    /// Removes `[start, end)` from the free block at `idx`, which must
    /// contain it, leaving what remains before and after as free blocks in
    /// the same place so the list stays sorted by address. The one place
    /// that splits a free block around a fixed range
    /// ([`Self::reserve_range`], [`Self::alloc_abs`]).
    fn carve_free_block(&mut self, idx: usize, start: u32, end: u32) {
        let block = self.free[idx];
        let block_end = block.start + block.size;
        let head = (block.start < start).then(|| FreeBlock {
            start: block.start,
            size: start - block.start,
        });
        let tail = (end < block_end).then(|| FreeBlock {
            start: end,
            size: block_end - end,
        });
        self.free.remove(idx);
        // Insert tail first so head ends up before it at `idx`.
        if let Some(tail) = tail {
            self.free.insert(idx, tail);
        }
        if let Some(head) = head {
            self.free.insert(idx, head);
        }
    }

    /// Permanently removes `[start, start + len)` from the free list so
    /// [`Self::alloc`] can never hand it out -- for fixed guest addresses
    /// that are not ordinary RAM (the `--uae-debug` port, see
    /// [`crate::uaedebug`]). The range is not an allocation: nothing is
    /// recorded as live, it can't be freed, and it simply stops counting
    /// towards [`Self::total_free`]/[`Self::largest_free`].
    ///
    /// Returns `false` (changing nothing) unless the whole range lies
    /// inside a single free block -- i.e. it is already allocated, or
    /// overlaps the program image or the stack. `len == 0` is a no-op
    /// returning `true`.
    pub fn reserve_range(&mut self, start: u32, len: u32) -> bool {
        if len == 0 {
            return true;
        }
        let Some(end) = start.checked_add(len) else {
            return false;
        };
        let Some(idx) = self
            .free
            .iter()
            .position(|b| b.start <= start && end <= b.start + b.size)
        else {
            return false;
        };
        self.generation += 1;
        self.carve_free_block(idx, start, end);
        true
    }

    /// Removes `[start, end)` from the free list wherever it overlaps
    /// free blocks -- including a span crossing several of them (a
    /// stack reaching across a region boundary). Unlike
    /// [`GuestHeap::reserve_range`] it does not require the whole span
    /// to be free or in one block; parts that are not free are simply
    /// left as they are. Free blocks stay sorted and are never merged.
    pub fn reserve_overlapping(&mut self, start: u32, end: u32) {
        if end <= start {
            return;
        }
        self.generation += 1;
        let mut out = Vec::with_capacity(self.free.len() + 1);
        for b in self.free.drain(..) {
            let b_end = b.start + b.size;
            if b_end <= start || b.start >= end {
                out.push(b);
                continue;
            }
            if b.start < start {
                out.push(FreeBlock {
                    start: b.start,
                    size: start - b.start,
                });
            }
            if b_end > end {
                out.push(FreeBlock {
                    start: end,
                    size: b_end - end,
                });
            }
        }
        self.free = out;
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
        self.generation += 1;
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
            left.start + left.size == block.start && self.same_region(left.start, block.start)
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
            if block.start + block.size == right.start && self.same_region(block.start, right.start)
            {
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
    /// How many allocations are currently live.
    pub fn live_alloc_count(&self) -> usize {
        self.live.len()
    }

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

    /// [`GuestHeap::total_free`] counting only regions whose attributes
    /// include every bit of `required` (`AvailMem(MEMF_CHIP)` etc.).
    pub fn total_free_matching(&self, required: u32) -> u32 {
        let total: u64 = self
            .free
            .iter()
            .filter(|b| self.block_matches(b.start, required))
            .map(|b| u64::from(b.size))
            .sum();
        // A guest sees a 32-bit count; more than 4 GiB free saturates.
        total.min(u64::from(u32::MAX)) as u32
    }

    /// [`GuestHeap::largest_free`] over the matching regions only.
    pub fn largest_free_matching(&self, required: u32) -> u32 {
        self.free
            .iter()
            .filter(|b| self.block_matches(b.start, required))
            .map(|b| b.size)
            .max()
            .unwrap_or(0)
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

/// Writes a big-endian longword without the sanitizer's checks.
fn poke_u32(mem: &mut dyn AddressSpace, addr: u32, value: u32) {
    for (i, b) in value.to_be_bytes().into_iter().enumerate() {
        mem.poke_u8(addr.wrapping_add(i as u32), b);
    }
}

/// Rounds `value` up to the nearest multiple of 4, saturating at `u32::MAX`
/// rather than wrapping to 0.
fn align_up(value: u32) -> u32 {
    value.checked_add(3).map(|v| v & !3).unwrap_or(!3)
}

/// Most nodes any host-side walk of a guest-owned list or chain visits
/// before giving up. The structures are in guest memory, so a guest bug
/// (adding a node twice makes `succ` point at itself; a `TAG_MORE` that
/// points back) can make them cyclic, and the host must not spin forever
/// on that (issue #361). Far beyond any real list.
pub const MAX_GUEST_LIST_WALK: u32 = 1 << 20;

/// Whether `[addr, addr + len)` is wholly backed by guest memory. A guest
/// buffer length comes straight from a register, so handlers that copy it
/// into a host `Vec` (`Write`, `FWrite`, `CopyMem`, ...) check this first:
/// a length past the end of guest memory can't be a real buffer, and
/// `vec![0; 0xFFFFFFFF]` plus a 4-billion-iteration loop is not a
/// reasonable answer to it (issue #361). `len` is a `u64` so callers can
/// pass a product like `size * count` unreduced.
pub fn guest_range_mapped(mem: &dyn AddressSpace, addr: u32, len: u64) -> bool {
    match u32::try_from(len) {
        Ok(len) => len == 0 || mem.is_mapped(addr, len),
        Err(_) => false,
    }
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

/// Test-only shorthands for [`GuestHeap::alloc`]/
/// [`GuestHeap::alloc_with_requested`] without an address space.
///
/// The allocator's own unit tests are about placement, rounding,
/// coalescing and the quarantine -- none of which involve the sanitizer.
/// The shadow-clearing half that `alloc` adds on top is covered where it
/// can be observed, against a real `FlatMemory`: see
/// `alloc_clears_a_recycled_blocks_freed_poison` below and
/// `execmem::tests::a_recycled_alloc_vec_block_is_no_longer_a_freed_block`.
#[cfg(test)]
impl GuestHeap {
    fn alloc_bare(&mut self, size: u32) -> Result<u32, GuestHeapError> {
        self.alloc_bare_req(size, size)
    }

    fn alloc_bare_req(&mut self, size: u32, requested: u32) -> Result<u32, GuestHeapError> {
        self.alloc_untracked(size, requested, 0, 4)
            .map(|(addr, _, _)| addr)
    }
}

#[cfg(test)]
mod layout_heap_tests {
    use super::*;
    use crate::execmem::{MEMF_24BITDMA, MEMF_CHIP, MEMF_FAST, MEMF_LOCAL};
    use crate::memmap::{MemoryLayout, MemoryRegion, RegionKind};
    use crate::memory::FlatMemory;

    const MIB: u32 = 0x10_0000;
    const MB_BASE: u32 = 0x0780_0000;

    /// Chip 2 MiB, Zorro II fast 4 MiB, A4000-style CPU RAM 8 MiB.
    fn a4000ish() -> MemoryLayout {
        MemoryLayout::new(vec![
            MemoryRegion::new(RegionKind::Chip, 0, 2 * MIB),
            MemoryRegion::new(RegionKind::Fast, 2 * MIB, 4 * MIB),
            MemoryRegion::new(RegionKind::Accelerator, MB_BASE, 8 * MIB),
        ])
        .unwrap()
    }

    fn heap() -> (GuestHeap, FlatMemory) {
        (
            GuestHeap::with_layout(a4000ish(), 0x4000, 0x8000_0000),
            FlatMemory::from_layout(&a4000ish()),
        )
    }

    #[test]
    fn with_layout_makes_one_free_block_per_region_clipped_to_the_range() {
        let (h, _) = heap();
        // Each non-chip region keeps a MEMHEADER_PROLOGUE at its start for
        // its MemHeader and name.
        let p = MEMHEADER_PROLOGUE;
        assert_eq!(
            h.free_bytes(),
            (2 * MIB - 0x4000) + (4 * MIB - p) + (8 * MIB - p)
        );
        assert_eq!(h.total_free_matching(MEMF_CHIP), 2 * MIB - 0x4000);
        assert_eq!(h.total_free_matching(MEMF_FAST), 12 * MIB - 2 * p);
        // A range that starts inside the second region drops the first.
        let h = GuestHeap::with_layout(a4000ish(), 3 * MIB, 0x8000_0000);
        assert_eq!(h.total_free_matching(MEMF_CHIP), 0);
        assert_eq!(h.total_free_matching(MEMF_FAST), 3 * MIB + 8 * MIB - p);
    }

    #[test]
    fn any_prefers_the_highest_priority_region_and_chip_stays_in_chip() {
        let (mut h, mut mem) = heap();
        // As measured on a real A4000: MEMF_ANY and MEMF_FAST land in
        // the 32-bit CPU RAM (priority 30), MEMF_CHIP in chip.
        let any = h.alloc_for(&mut mem, 64, 64, 0).unwrap();
        let fast = h.alloc_for(&mut mem, 64, 64, MEMF_FAST).unwrap();
        let chip = h.alloc_for(&mut mem, 64, 64, MEMF_CHIP).unwrap();
        assert!((MB_BASE..MB_BASE + 8 * MIB).contains(&any));
        assert!((MB_BASE..MB_BASE + 8 * MIB).contains(&fast));
        assert!(chip < 2 * MIB);
        assert_eq!(h.attributes_at(chip), Some(0x703));
        assert_eq!(h.attributes_at(fast), Some(0x505));
    }

    #[test]
    fn dma_and_local_requirements_skip_regions_without_the_bit() {
        let (mut h, mut mem) = heap();
        // Real A4000: FAST|24BITDMA skips the 32-bit RAM and lands in
        // Zorro II fast RAM.
        let z2 = h
            .alloc_for(&mut mem, 64, 64, MEMF_FAST | MEMF_24BITDMA)
            .unwrap();
        assert!((2 * MIB..6 * MIB).contains(&z2));
        assert_eq!(h.attributes_at(z2), Some(0x605));
        // LOCAL excludes Zorro II (the 0x605 header has no LOCAL bit).
        let local = h
            .alloc_for(&mut mem, 64, 64, MEMF_FAST | MEMF_LOCAL)
            .unwrap();
        assert!(local >= MB_BASE);
    }

    #[test]
    fn an_unsatisfiable_requirement_fails_like_alloc_mem_returning_null() {
        let (mut h, mut mem) = heap();
        // No header is both chip and fast.
        assert!(matches!(
            h.alloc_for(&mut mem, 64, 64, MEMF_CHIP | MEMF_FAST),
            Err(GuestHeapError::OutOfMemory { available: 0, .. })
        ));
        // And fast on a chip-only machine.
        let mut chip_only = GuestHeap::with_layout(
            MemoryLayout::new(vec![MemoryRegion::new(RegionKind::Chip, 0, 2 * MIB)]).unwrap(),
            0x4000,
            2 * MIB,
        );
        assert!(chip_only.alloc_for(&mut mem, 64, 64, MEMF_FAST).is_err());
        assert!(
            chip_only.alloc_for(&mut mem, 64, 64, 0).is_ok(),
            "ANY falls back to chip"
        );
    }

    #[test]
    fn an_exhausted_preferred_region_falls_back_to_the_next_by_priority() {
        let (mut h, mut mem) = heap();
        // Fill the CPU RAM completely; ANY must now come from Zorro II.
        let all = 8 * MIB - MEMHEADER_PROLOGUE;
        let big = h
            .alloc_for(&mut mem, all, all, MEMF_FAST | MEMF_LOCAL)
            .unwrap();
        assert_eq!(big, MB_BASE + MEMHEADER_PROLOGUE);
        let next = h.alloc_for(&mut mem, 64, 64, 0).unwrap();
        assert!((2 * MIB..6 * MIB).contains(&next), "{next:#X}");
    }

    #[test]
    fn free_blocks_in_abutting_regions_never_merge() {
        // The legacy layout's chip and accelerator regions touch at 2 MiB.
        let layout = MemoryLayout::legacy_exact(4 * MIB);
        let mut mem = FlatMemory::from_layout(&layout);
        let mut h = GuestHeap::with_layout(layout, 0x4000, 4 * MIB);
        let chip_free = h.total_free_matching(MEMF_CHIP);
        // Take everything, then give it back: the two regions must come
        // back as two blocks, not one 4 MiB block spanning both.
        let c = h
            .alloc_for(&mut mem, chip_free, chip_free, MEMF_CHIP)
            .unwrap();
        let fast = 2 * MIB - MEMHEADER_PROLOGUE;
        let f = h.alloc_for(&mut mem, fast, fast, MEMF_FAST).unwrap();
        h.free(c).unwrap();
        h.free(f).unwrap();
        assert_eq!(h.largest_free_matching(MEMF_CHIP), chip_free);
        assert_eq!(h.largest_free(), 2 * MIB - MEMHEADER_PROLOGUE);
        assert_eq!(h.free.len(), 2);
    }

    #[test]
    fn capacity_matching_counts_whole_regions_free_or_not() {
        let (mut h, mut mem) = heap();
        h.alloc_for(&mut mem, MIB, MIB, MEMF_CHIP).unwrap();
        assert_eq!(h.capacity_matching(MEMF_CHIP), Some(u64::from(2 * MIB)));
        assert_eq!(h.capacity_matching(MEMF_FAST), Some(u64::from(12 * MIB)));
        assert_eq!(GuestHeap::new(0, 0x1000).capacity_matching(0), None);
    }

    /// Walks a region's `mh_First` chain in guest memory.
    fn chain(mem: &FlatMemory, header: u32) -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        let mut c = mem.peek_u32(header + MH_FIRST);
        while c != 0 {
            out.push((c, mem.peek_u32(c + 4)));
            c = mem.peek_u32(c);
        }
        out
    }

    fn memlist_heap() -> (GuestHeap, FlatMemory, u32) {
        let layout = MemoryLayout::legacy_exact(4 * MIB);
        let mut mem = FlatMemory::from_layout(&layout);
        let mut h = GuestHeap::with_layout(layout, 0x4000, u32::MAX);
        h.install_memlist(&mut mem, 0x100, 0x200);
        h.sync_memlist(&mut mem);
        // Region order is priority order: the accelerator header first.
        let fast_header = mem.peek_u32(0x100); // lh_Head
        (h, mem, fast_header)
    }

    #[test]
    fn memlist_follows_the_free_list_and_only_rewrites_when_it_changed() {
        let (mut h, mut mem, hdr) = memlist_heap();
        let before = chain(&mem, hdr);
        assert_eq!(before.len(), 1, "one free block: {before:?}");
        let free_before = mem.peek_u32(hdr + MH_FREE);

        // An allocation changes the free list; the header lags until the
        // next sync (the documented laziness) and then follows it.
        let a = h.alloc_for(&mut mem, 0x1000, 0x1000, 0).unwrap();
        assert_eq!(mem.peek_u32(hdr + MH_FREE), free_before, "not yet");
        h.sync_memlist(&mut mem);
        assert_eq!(mem.peek_u32(hdr + MH_FREE), free_before - 0x1000);
        assert_eq!(chain(&mem, hdr)[0].0, a + 0x1000);

        // Nothing changed since: a second sync must not touch memory.
        mem.poke_u8(a + 0x1000, 0xEE);
        h.sync_memlist(&mut mem);
        assert_eq!(mem.peek_u8(a + 0x1000), 0xEE, "clean sync is a no-op");

        // Freeing makes a second chunk (the hole) and the sums still agree.
        let b = h.alloc_for(&mut mem, 0x100, 0x100, 0).unwrap();
        h.free(a).unwrap();
        h.sync_memlist(&mut mem);
        let c = chain(&mem, hdr);
        assert_eq!(c.len(), 2, "{c:?}");
        assert!(
            c[0].0 < b && b < c[1].0,
            "the live block sits between the chunks"
        );
        let sum: u32 = c.iter().map(|&(_, n)| n).sum();
        assert_eq!(sum, mem.peek_u32(hdr + MH_FREE));
    }

    #[test]
    fn memchunks_are_eight_byte_aligned_even_for_four_byte_blocks() {
        let (mut h, mut mem, hdr) = memlist_heap();
        // Host-internal allocations are only 4-aligned; leave odd holes.
        let keep: Vec<u32> = (0..6)
            .map(|_| h.alloc_for(&mut mem, 12, 12, 0).unwrap())
            .collect();
        for &k in keep.iter().step_by(2) {
            h.free(k).unwrap();
        }
        h.sync_memlist(&mut mem);
        for (start, size) in chain(&mem, hdr) {
            assert_eq!(start % 8, 0, "chunk at {start:#x}");
            assert_eq!(size % 8, 0, "chunk size {size:#x}");
        }
    }

    #[test]
    fn a_chip_region_the_program_fills_gets_an_empty_not_inverted_range() {
        // Program/heap start beyond the chip region's end.
        let layout = MemoryLayout::legacy_exact(0x4_0000);
        let mut mem = FlatMemory::from_layout(&layout);
        let mut h = GuestHeap::with_layout(layout, 0x8_0000, u32::MAX);
        h.install_memlist(&mut mem, 0x100, 0x200);
        h.sync_memlist(&mut mem);
        let hdr = mem.peek_u32(0x100);
        let lower = mem.peek_u32(hdr + MH_LOWER);
        let upper = mem.peek_u32(hdr + MH_UPPER);
        assert!(lower <= upper, "{lower:#x}..{upper:#x}");
        assert_eq!(mem.peek_u32(hdr + MH_FREE), 0);
    }

    #[test]
    fn reading_the_chunk_chain_is_not_a_sanitizer_violation() {
        let layout = MemoryLayout::legacy_exact(4 * MIB);
        let mut mem = FlatMemory::from_layout(&layout);
        mem.enable_sanitizer();
        let mut h = GuestHeap::with_layout(layout, 0x4000, u32::MAX);
        h.set_quarantine_budget(0); // freed blocks return to the free list at once
        h.install_memlist(&mut mem, 0x100, 0x200);
        // Free a block so its bytes carry the Freed poison, then sync.
        let a = h.alloc_for(&mut mem, 0x100, 0x100, 0).unwrap();
        h.free(a).unwrap();
        h.sync_memlist(&mut mem);
        let hdr = mem.peek_u32(0x100);
        // Walk the chain the way a guest does, through the checked path.
        let mut c = AddressSpace::read_u32(&mem, hdr + MH_FIRST);
        while c != 0 {
            let _bytes = AddressSpace::read_u32(&mem, c + 4);
            c = AddressSpace::read_u32(&mem, c);
        }
        assert!(
            mem.shadow().unwrap().violations().is_empty(),
            "walking MemList must not be reported"
        );
    }

    #[test]
    fn a_region_ending_on_an_odd_address_gives_aligned_blocks() {
        // legacy_exact doesn't round, so a total of 0x4_0006 ends mid-longword.
        let layout = MemoryLayout::legacy_exact(0x4_0006);
        let mut mem = FlatMemory::from_layout(&layout);
        let mut h = GuestHeap::with_layout(layout, 0x4000, u32::MAX);
        assert_eq!(h.free_bytes() % 4, 0);
        let a = h.alloc_for(&mut mem, 8, 8, 0).unwrap();
        assert_eq!(a % 4, 0);
    }

    #[test]
    fn placement_scan_stops_at_the_first_fit_in_the_best_region() {
        // A fragmented chip heap must not be searched when fast RAM has
        // room: the allocation lands in fast RAM, lowest address first.
        let (mut h, mut mem) = heap();
        let mut keep = Vec::new();
        for _ in 0..50 {
            keep.push(h.alloc_for(&mut mem, 64, 64, MEMF_CHIP).unwrap());
            let gap = h.alloc_for(&mut mem, 64, 64, MEMF_CHIP).unwrap();
            h.free(gap).unwrap();
        }
        let a = h.alloc_for(&mut mem, 64, 64, 0).unwrap();
        let b = h.alloc_for(&mut mem, 64, 64, 0).unwrap();
        let first = MB_BASE + MEMHEADER_PROLOGUE;
        assert_eq!(a, first, "first block of the top-priority region");
        assert_eq!(b, first + 64);
    }

    #[test]
    fn reserve_overlapping_carves_a_span_across_region_boundaries() {
        let layout = MemoryLayout::legacy_exact(4 * MIB);
        let mut h = GuestHeap::with_layout(layout, 0x4000, 4 * MIB);
        let before = h.free_bytes();
        // A 192 KiB stack straddling the chip/accelerator boundary; the
        // accelerator region's first MEMHEADER_PROLOGUE bytes were
        // already out of the free list.
        h.reserve_overlapping(2 * MIB - 0x1_0000, 2 * MIB + 0x2_0000);
        assert_eq!(h.free_bytes(), before - 0x3_0000 + MEMHEADER_PROLOGUE);
        assert_eq!(h.free.len(), 2, "each region keeps its own block");
        // Spans that miss every block, or are empty, change nothing.
        h.reserve_overlapping(8 * MIB, 9 * MIB);
        h.reserve_overlapping(0x9000, 0x9000);
        assert_eq!(h.free_bytes(), before - 0x3_0000 + MEMHEADER_PROLOGUE);
    }

    #[test]
    fn alloc_abs_claims_the_exact_range_and_free_gives_it_back() {
        let mut mem = FlatMemory::new(0x4000);
        let mut h = GuestHeap::new(0x1000, 0x3000);
        let before = h.free_bytes();
        assert_eq!(h.alloc_abs(&mut mem, 0x1800, 0x100, 0xF0), Some(0x1800));
        assert_eq!(h.free_bytes(), before - 0x100);
        assert_eq!(h.size_of_live_alloc(0x1800), Some(0x100));
        // The free list now has a hole: the same range cannot be claimed
        // again, nor a range that merely touches it.
        assert_eq!(h.alloc_abs(&mut mem, 0x1800, 0x100, 0x100), None);
        assert_eq!(h.alloc_abs(&mut mem, 0x1880, 0x100, 0x100), None);
        assert_eq!(h.alloc_abs(&mut mem, 0x1700, 0x120, 0x120), None);
        // Either side of the hole is still free and claimable.
        assert_eq!(h.alloc_abs(&mut mem, 0x1900, 0x40, 0x40), Some(0x1900));
        assert_eq!(h.alloc_abs(&mut mem, 0x1700, 0x100, 0x100), Some(0x1700));
        // Freed, they are one free range again.
        for addr in [0x1800, 0x1900, 0x1700] {
            h.free(addr).unwrap();
        }
        assert_eq!(h.free_bytes(), before);
        assert_eq!(h.alloc_abs(&mut mem, 0x1000, 0x2000, 0x2000), Some(0x1000));
    }

    #[test]
    fn alloc_abs_refuses_what_is_not_free_heap() {
        let mut mem = FlatMemory::new(0x4000);
        let mut h = GuestHeap::new(0x1000, 0x2000);
        let live = h.alloc(&mut mem, 0x100).unwrap();
        // Overlapping a live allocation, outside the heap, running off
        // its end, misaligned, and empty ranges all fail and change
        // nothing.
        let before = h.free_bytes();
        assert_eq!(h.alloc_abs(&mut mem, live, 0x10, 0x10), None);
        assert_eq!(h.alloc_abs(&mut mem, 0x800, 0x10, 0x10), None);
        assert_eq!(h.alloc_abs(&mut mem, 0x1FF0, 0x20, 0x20), None);
        assert_eq!(h.alloc_abs(&mut mem, 0x1802, 0x10, 0x10), None);
        assert_eq!(h.alloc_abs(&mut mem, 0x1800, 0, 0), None);
        assert_eq!(h.free_bytes(), before);
    }

    /// The aligned-allocation idiom: allocate `size + align - 1`, free all
    /// of it, `AllocAbs` the aligned part. With the quarantine on, the freed
    /// block is held rather than free, and `AllocAbs` must still get it.
    #[test]
    fn alloc_abs_claims_memory_the_quarantine_is_holding() {
        let mut mem = FlatMemory::new(0x1_0000);
        let mut h = GuestHeap::new(0x1000, 0x8000)
            .with_redzone_size(32)
            .with_quarantine_budget(DEFAULT_QUARANTINE_BUDGET);
        let block = h.alloc(&mut mem, 0x2000 + 0x3FFF).unwrap();
        let aligned = (block + 0x3FFF) & !0x3FFF;
        h.free(block).unwrap();
        // The freed block is quarantined, not free.
        assert!(h.free.iter().all(|b| b.start > block + 0x2000));
        assert_eq!(
            h.alloc_abs(&mut mem, aligned, 0x2000, 0x2000),
            Some(aligned)
        );
        assert_eq!(h.size_of_live_alloc(aligned), Some(0x2000));
        // What is left of the freed block is free again, and the claim can
        // itself be freed.
        h.free(aligned).unwrap();
    }

    /// A claim that cannot succeed leaves the quarantine alone: here half of
    /// the range is a live allocation.
    #[test]
    fn a_failed_alloc_abs_does_not_release_the_quarantine() {
        let mut mem = FlatMemory::new(0x1_0000);
        let mut h = GuestHeap::new(0x1000, 0x8000)
            .with_redzone_size(32)
            .with_quarantine_budget(DEFAULT_QUARANTINE_BUDGET);
        let a = h.alloc(&mut mem, 0x100).unwrap();
        let live = h.alloc(&mut mem, 0x100).unwrap();
        h.free(a).unwrap();
        let held = h.quarantine.len();
        assert_eq!(held, 1);
        // Spans the quarantined block and the live one after it.
        assert_eq!(h.alloc_abs(&mut mem, a, 0x200, 0x200), None);
        assert_eq!(h.quarantine.len(), held, "nothing released");
        assert!(h.size_of_live_alloc(live).is_some());
    }

    #[test]
    fn alloc_abs_takes_redzones_from_the_free_space_around_it() {
        let mut mem = FlatMemory::new(0x4000);
        let mut h = GuestHeap::new(0x1000, 0x3000).with_redzone_size(32);
        assert_eq!(h.alloc_abs(&mut mem, 0x1800, 0x100, 0x100), Some(0x1800));
        let extent = h.extent_of_live_alloc(0x1800).unwrap();
        assert_eq!((extent.block_start, extent.block_end), (0x17E0, 0x1920));
        // Right against the start of the heap there is no room for a
        // leading redzone, which is not a reason to refuse the range.
        assert_eq!(h.alloc_abs(&mut mem, 0x1000, 0x40, 0x40), Some(0x1000));
        let extent = h.extent_of_live_alloc(0x1000).unwrap();
        assert_eq!(extent.block_start, 0x1000);
        h.free(0x1800).unwrap();
        h.free(0x1000).unwrap();
    }

    #[test]
    fn a_plain_heap_ignores_requirements_exactly_as_before() {
        let mut mem = FlatMemory::new(0x2000);
        let mut h = GuestHeap::new(0x100, 0x2000);
        assert!(h.alloc_for(&mut mem, 16, 16, MEMF_CHIP | MEMF_FAST).is_ok());
        assert_eq!(h.attributes_at(0x100), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::FlatMemory;
    use crate::sanitize::{PoisonReason, ShadowState};

    #[test]
    fn alloc_returns_4_byte_aligned_addresses() {
        let mut heap = GuestHeap::new(0x1001, 0x2000);
        // start rounds up to 0x1004.
        let a = heap.alloc_bare(3).unwrap();
        assert_eq!(a % 4, 0);
        assert_eq!(a, 0x1004);
        let b = heap.alloc_bare(1).unwrap();
        assert_eq!(b % 4, 0);
        // 3 rounds up to 4, so b should be right after a's 4-byte block.
        assert_eq!(b, 0x1008);
    }

    /// Issue #355: `alloc_aligned_for(.., 8)` lands on an 8-byte boundary
    /// wherever the free block starts, with and without redzones, and the
    /// alignment gap goes back to the free list with the allocation.
    #[test]
    fn alloc_aligned_for_8_lands_on_an_8_byte_boundary_and_frees_the_gap_355() {
        for redzone in [0, 4, 16, 32] {
            for start in [0x1000, 0x1004] {
                let mut mem = FlatMemory::new(0x4000);
                let mut heap = GuestHeap::new(start, 0x3000).with_redzone_size(redzone);
                let free_before = heap.free_bytes();
                let a = heap.alloc_aligned_for(&mut mem, 100, 100, 0, 8).unwrap();
                assert_eq!(a % 8, 0, "start {start:#x} redzone {redzone}");
                let ext = heap.extent_of_live_alloc(a).unwrap();
                assert!(ext.block_start <= a - redzone, "gap precedes the redzone");
                assert_eq!(ext.block_end, a + 100 + redzone);
                // A second, default-aligned block must follow without overlap.
                let b = heap.alloc_bare(8).unwrap();
                assert!(b >= ext.block_end);
                heap.free(b).unwrap();
                heap.free(a).unwrap();
                assert_eq!(heap.free_bytes(), free_before, "nothing leaked");
            }
        }
    }

    /// `alloc_aligned_for` still reports `OutOfMemory` when only the
    /// alignment gap would make the request not fit.
    #[test]
    fn alloc_aligned_for_counts_the_alignment_gap_when_fitting_355() {
        let mut mem = FlatMemory::new(0x4000);
        // 0x1004..0x1010 is 12 bytes; an 8-aligned 12-byte block needs
        // 4 bytes of gap and so does not fit.
        let mut heap = GuestHeap::new(0x1004, 0x1010);
        assert!(heap.alloc_aligned_for(&mut mem, 12, 12, 0, 8).is_err());
        assert!(heap.alloc_aligned_for(&mut mem, 8, 8, 0, 8).is_ok());
    }

    #[test]
    fn alloc_free_realloc_reuses_freed_block() {
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let a = heap.alloc_bare(16).unwrap();
        heap.free(a).unwrap();
        let b = heap.alloc_bare(16).unwrap();
        assert_eq!(a, b, "freed block should be reused by a same-size alloc");
    }

    #[test]
    fn alloc_clears_a_recycled_blocks_freed_poison() {
        // Issue #95, at the one place the fix lives. Every guest-visible
        // allocation in the runtime -- `AllocMem` and `AllocVec`, but
        // equally the `FileHandle` a dos `Open` carves out, the
        // `FileLock` a `Lock` carves out, a `ReadArgs` argument buffer
        // and three dozen more -- comes out of here, and any of them can
        // be handed a block that `FreeMem`/`FreeVec` poisoned. Clearing
        // it here is what makes that true for all of them at once
        // (`alloc` cannot even be called without an address space to
        // clear), so this test works the heap directly rather than
        // through any one handler.
        let mut mem = FlatMemory::new(0x2000);
        mem.enable_sanitizer();
        let mut heap = GuestHeap::new(0x1000, 0x2000).with_redzone_size(DEFAULT_REDZONE_SIZE);

        let a = heap.alloc(&mut mem, 16).unwrap();
        // Exactly what `execmem::poison_freed_block` does before a
        // `FreeVec`, then release the block for immediate reuse (the
        // quarantine is off, so `free` puts it straight back).
        let extent = heap.extent_of_live_alloc(a).unwrap();
        mem.shadow_mut().unwrap().mark_unaddressable(
            extent.block_start,
            extent.block_end - extent.block_start,
            PoisonReason::Freed,
        );
        heap.free(a).unwrap();

        let b = heap.alloc(&mut mem, 16).unwrap();
        assert_eq!(a, b, "the test needs the block to actually be recycled");
        let shadow = mem.shadow().unwrap();
        for addr in extent.block_start..extent.block_end {
            assert_eq!(
                shadow.state(addr),
                ShadowState::Valid,
                "{addr:#x}: nothing about the previous tenant of a recycled \
                 block applies to it any more, redzone bytes included"
            );
        }
    }

    #[test]
    fn alloc_free_coalesces_adjacent_blocks() {
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let a = heap.alloc_bare(16).unwrap();
        let b = heap.alloc_bare(16).unwrap();
        let c = heap.alloc_bare(16).unwrap();
        heap.free(a).unwrap();
        heap.free(c).unwrap();
        heap.free(b).unwrap();
        // Everything freed and coalesced back into one block: a single
        // alloc of the whole managed region should now succeed.
        let big = heap.alloc_bare(0x1000 - 16 * 3).unwrap();
        assert_eq!(big, a);
    }

    #[test]
    fn alloc_exhaustion_returns_out_of_memory_err() {
        let mut heap = GuestHeap::new(0x1000, 0x1010); // 16 bytes total
        heap.alloc_bare(16).unwrap();
        let err = heap.alloc_bare(4).unwrap_err();
        match err {
            GuestHeapError::OutOfMemory { requested, .. } => assert_eq!(requested, 4),
            other => panic!("expected OutOfMemory, got {other:?}"),
        }
    }

    #[test]
    fn double_free_is_detected_as_an_error() {
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let a = heap.alloc_bare(16).unwrap();
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
        assert!(heap.alloc_bare(1).is_err());
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
        let a = heap.alloc_bare(16).unwrap();
        let _b = heap.alloc_bare(16).unwrap();
        let _c = heap.alloc_bare(16).unwrap();
        assert_eq!(heap.total_free(), 0);
        heap.free(a).unwrap();
        assert_eq!(heap.total_free(), 16);
    }

    #[test]
    fn largest_free_finds_the_biggest_block_even_when_fragmented() {
        let mut heap = GuestHeap::new(0x1000, 0x1000 + 48);
        let a = heap.alloc_bare(16).unwrap();
        let _b = heap.alloc_bare(16).unwrap();
        let _c = heap.alloc_bare(16).unwrap();
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
        let a = heap.alloc_bare(13).unwrap(); // rounds up to 16
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
        let a = heap.alloc_bare(16).unwrap();
        assert_eq!(a, 0x1000);
        assert_eq!(heap.size_of_live_alloc(a), Some(16));
        let b = heap.alloc_bare(16).unwrap();
        assert_eq!(b, 0x1010, "no redzone gap between consecutive allocs");
        heap.free(a).unwrap();
        let c = heap.alloc_bare(16).unwrap();
        assert_eq!(
            c, a,
            "freed block reused immediately, as before quarantine existed"
        );
    }

    #[test]
    fn redzones_on_return_user_address_past_leading_redzone_with_plain_reported_size() {
        let mut heap = GuestHeap::new(0x1000, 0x3000).with_redzone_size(32);
        let a = heap.alloc_bare(16).unwrap();
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
        let a = heap.alloc_bare(16).unwrap();
        let b = heap.alloc_bare(16).unwrap();
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
        let err = heap.alloc_bare(16).unwrap_err();
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
        let err = heap.alloc_bare(u32::MAX - 8).unwrap_err();
        match err {
            GuestHeapError::OutOfMemory { .. } => {}
            other => panic!("expected OutOfMemory, got {other:?}"),
        }
    }

    #[test]
    fn quarantine_holds_a_freed_address_back_from_immediate_reuse() {
        let mut heap = GuestHeap::new(0x1000, 0x2000).with_quarantine_budget(1024);
        let a = heap.alloc_bare(16).unwrap();
        heap.free(a).unwrap();
        // The freed block is still within budget (16 bytes << 1024), so
        // it should not have been released back to the free list yet: a
        // same-size alloc must land somewhere else.
        let b = heap.alloc_bare(16).unwrap();
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
        let a = heap.alloc_bare(16).unwrap();
        let b = heap.alloc_bare(16).unwrap();
        heap.free(a).unwrap(); // quarantine now holds exactly 16 bytes (at budget, not over)
        heap.free(b).unwrap(); // pushes quarantine to 32 bytes, over budget: drains `a` first

        // `a`'s block should now be back in the free list and coalesced
        // with any adjacent space, so it's allocatable again.
        let c = heap.alloc_bare(16).unwrap();
        assert_eq!(
            c, a,
            "drained block should be released in FIFO order and reusable"
        );
    }

    #[test]
    fn zero_quarantine_budget_behaves_like_immediate_free() {
        let mut heap = GuestHeap::new(0x1000, 0x2000).with_quarantine_budget(0);
        let a = heap.alloc_bare(16).unwrap();
        heap.free(a).unwrap();
        let b = heap.alloc_bare(16).unwrap();
        assert_eq!(a, b, "budget 0 disables quarantining entirely");
    }

    #[test]
    fn lowering_quarantine_budget_drains_blocks_that_no_longer_fit() {
        let mut heap = GuestHeap::new(0x1000, 0x2000).with_quarantine_budget(1024);
        let a = heap.alloc_bare(16).unwrap();
        heap.free(a).unwrap();
        // Still well within the free list as "not free" (quarantined).
        assert_eq!(heap.total_free(), heap.free_bytes());
        // Shrinking the budget below what's queued must drain it
        // immediately, not just on the next free.
        heap.set_quarantine_budget(0);
        let b = heap.alloc_bare(16).unwrap();
        assert_eq!(
            a, b,
            "shrinking the budget should have drained the queued block"
        );
    }

    #[test]
    fn recently_freed_info_answers_for_a_freed_address_and_not_for_a_stranger() {
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let a = heap.alloc_bare(16).unwrap();
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
        let a = heap.alloc_bare(16).unwrap();
        heap.free(a).unwrap();
        let first = heap.recently_freed_info(a).unwrap();
        let b = heap.alloc_bare(16).unwrap();
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
        let a = heap.alloc_bare_req(13, 13).unwrap();
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
        let a = heap.alloc_bare_req(16, 999).unwrap();
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
        let a = heap.alloc_bare(16).unwrap();
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
        let a = redzoned.alloc_bare(16).unwrap();
        // 16 (redzone) + 16 (user) + 16 (redzone) = 48 bytes consumed.
        assert_eq!(redzoned.total_free(), total - 48);

        let mut quarantined = GuestHeap::new(0x1000, 0x1000 + total).with_quarantine_budget(1024);
        let b = quarantined.alloc_bare(16).unwrap();
        quarantined.free(b).unwrap();
        assert_eq!(
            quarantined.total_free(),
            total - 16,
            "quarantined block must not count as free"
        );

        redzoned.free(a).unwrap();
    }

    #[test]
    fn reserve_range_removes_it_from_the_free_list() {
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        assert!(heap.reserve_range(0x1800, 12));
        assert_eq!(heap.total_free(), 0x1000 - 12);
        assert_eq!(heap.largest_free(), 0x1800 - 0x1000);
    }

    #[test]
    fn allocations_never_overlap_a_reserved_range() {
        let (start, len) = (0x1800, 12);
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        assert!(heap.reserve_range(start, len));
        let mut seen = 0;
        while let Ok(a) = heap.alloc_bare(8) {
            assert!(
                a + 8 <= start || a >= start + len,
                "{a:#x} overlaps the reserved range"
            );
            seen += 1;
        }
        // Everything but the reserved 12 bytes was usable: 12 is not a
        // multiple of 8, so one 4-byte sliver at most is left over.
        assert!(seen >= (0x1000 - 12) / 8 - 2, "only {seen} allocations");
    }

    #[test]
    fn reserve_range_at_the_edges_and_when_unavailable() {
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        assert!(heap.reserve_range(0x1000, 8), "start of the block");
        assert!(heap.reserve_range(0x1FF8, 8), "end of the block");
        assert_eq!(heap.total_free(), 0x1000 - 16);
        assert!(!heap.reserve_range(0x1000, 4), "already reserved");
        assert!(!heap.reserve_range(0x1FFC, 8), "runs past the heap");
        assert!(!heap.reserve_range(0x0800, 4), "outside the heap");
        assert!(!heap.reserve_range(u32::MAX - 1, 8), "address overflow");
        assert!(heap.reserve_range(0x1800, 0), "empty range is a no-op");
        assert_eq!(heap.total_free(), 0x1000 - 16);
    }

    #[test]
    fn reserve_range_fails_over_a_live_allocation() {
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let a = heap.alloc_bare(16).unwrap();
        assert!(!heap.reserve_range(a, 4));
        assert!(!heap.reserve_range(a + 8, 16), "straddling the allocation");
        heap.free(a).unwrap();
        assert!(heap.reserve_range(a, 4));
    }

    #[test]
    fn guest_range_mapped_rejects_lengths_past_guest_memory_361() {
        let mem = crate::memory::FlatMemory::new(0x1000);
        assert!(guest_range_mapped(&mem, 0x10, 0x100));
        assert!(guest_range_mapped(&mem, 0xFFFF_0000, 0), "empty is fine");
        assert!(!guest_range_mapped(&mem, 0x10, 0x1000));
        assert!(!guest_range_mapped(&mem, 0x10, u64::from(u32::MAX)));
        assert!(!guest_range_mapped(&mem, 0, 1 << 32), "wider than u32");
    }
}
