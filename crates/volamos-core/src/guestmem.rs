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
//! - [`STACK_SIZE`]: a fixed 64 KiB region at the top of guest memory is
//!   reserved for the stack (growing downward from the top, as
//!   `Runtime::new` already sets `A7`). 64 KiB is generous for the kind
//!   of small CLI programs this runtime targets and leaves plenty of
//!   headroom before Phase 3 needs anything smarter (guard pages, stack
//!   growth, etc.).
//! - The heap occupies the space between the end of the loaded program
//!   and the base of the stack region (`stack base = memory length -
//!   STACK_SIZE`, 4-byte aligned).
//!
//! Because heap start depends on where the loaded program ends, and
//! [`crate::dispatch::Runtime::new`] doesn't (yet) know that -- its
//! signature is `new(cpu, mem, entry)`, deliberately left untouched here
//! per the Phase 2 task split (T12 owns growing it into a start-config
//! struct that threads the real load end through) -- `Runtime::new`
//! instead builds a *default* [`GuestHeap`] running from
//! [`HEAP_DEFAULT_START`] to the computed stack base. This default is a
//! placeholder: for any program whose loaded hunks extend past
//! [`HEAP_DEFAULT_START`], the default heap region overlaps the program
//! image, so callers that need a heap that doesn't stomp on their
//! program should build their own `GuestHeap` (typically starting at the
//! end of the highest loaded hunk) and install it via
//! [`crate::dispatch::Runtime::set_heap`] before running. T12 will wire
//! the real load-end-based start through automatically once its
//! start-config struct lands.

use crate::memory::AddressSpace;

/// Size in bytes of the guest stack region, reserved at the top of guest
/// memory. Chosen as a generous fixed size for the small CLI-style
/// programs this runtime targets; Phase 3+ can revisit (guard pages,
/// growth) if that ever proves insufficient.
pub const STACK_SIZE: u32 = 64 * 1024;

/// Default heap start address used by [`crate::dispatch::Runtime::new`]
/// before the real load-end is threaded through (see module docs). This
/// is deliberately just past the reserved trap-table region; callers
/// whose loaded program extends beyond this address must install their
/// own [`GuestHeap`] via `Runtime::set_heap`.
pub const HEAP_DEFAULT_START: u32 = crate::backend::TRAP_TABLE_END;

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
    /// Live allocations: `addr -> size`, so `free(addr)` knows how big a
    /// block to return and can detect unknown addresses.
    live: std::collections::HashMap<u32, u32>,
}

impl GuestHeap {
    /// Creates a heap managing `[start, end)`. `start` and `end` are
    /// rounded to keep the managed region 4-byte aligned (`start` up,
    /// `end` down); if that leaves nothing to manage (`end <= start`),
    /// the heap is created empty (every `alloc` call returns
    /// `OutOfMemory`).
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
        }
    }

    /// Allocates `size` bytes, returning the 4-byte-aligned guest address
    /// of the start of the block, or [`GuestHeapError::OutOfMemory`] if
    /// no free block is large enough. `size` is rounded up to a multiple
    /// of 4 so every allocation's end (and hence the next allocation's
    /// start) stays aligned; a request for `0` bytes still consumes a
    /// (minimal, 0-sized) accounted block so a subsequent `free` on it is
    /// well-defined.
    pub fn alloc(&mut self, size: u32) -> Result<u32, GuestHeapError> {
        let aligned_size = align_up(size);

        // First-fit: good enough for the sizes/allocation counts this
        // runtime deals with, and simple to keep correct.
        let Some(idx) = self.free.iter().position(|b| b.size >= aligned_size) else {
            let available = self.free.iter().map(|b| b.size).max().unwrap_or(0);
            return Err(GuestHeapError::OutOfMemory {
                requested: size,
                available,
            });
        };

        let block = self.free[idx];
        let addr = block.start;
        if block.size == aligned_size {
            self.free.remove(idx);
        } else {
            self.free[idx] = FreeBlock {
                start: block.start + aligned_size,
                size: block.size - aligned_size,
            };
        }

        self.live.insert(addr, aligned_size);
        Ok(addr)
    }

    /// Frees a block previously returned by [`GuestHeap::alloc`].
    ///
    /// Returns [`GuestHeapError::DoubleOrInvalidFree`] if `addr` isn't
    /// the start address of a currently-live allocation (already freed,
    /// or never allocated by this heap) rather than silently corrupting
    /// the free list or aborting -- callers (dos.library handlers) can
    /// turn that into a guest-visible error instead of UB.
    pub fn free(&mut self, addr: u32) -> Result<(), GuestHeapError> {
        let Some(size) = self.live.remove(&addr) else {
            return Err(GuestHeapError::DoubleOrInvalidFree { addr });
        };

        let mut block = FreeBlock { start: addr, size };

        // Coalesce with the free list. Keep it sorted by start so
        // adjacency checks are a simple neighbor comparison.
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
        Ok(())
    }

    /// Total free bytes remaining across all free blocks (not
    /// necessarily allocatable as one contiguous chunk). Mostly useful
    /// for tests/diagnostics.
    pub fn free_bytes(&self) -> u32 {
        self.free.iter().map(|b| b.size).sum()
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
    fn bstr_exactly_255_bytes_is_not_truncated() {
        let mut mem = FlatMemory::new(0x400);
        let exact = vec![b'y'; 255];
        let n = write_bstr(&mut mem, 0, &exact);
        assert_eq!(n, 255);
        assert_eq!(read_bstr(&mem, 0), exact);
    }
}
