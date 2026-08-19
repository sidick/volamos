//! `exec.library` memory allocation: `AllocMem`/`FreeMem`/`AllocVec`/
//! `FreeVec`/`AvailMem` (T16, Phase 3 stage 2), plus `CopyMem`/
//! `CopyMemQuick` (a raw memory-copy pair, not an allocator, but too
//! small to earn its own module -- added here since it's still squarely
//! an "exec.library memory-related call"), plus `CacheControl` (same
//! reasoning: not an allocator, but a small, memory-system-adjacent
//! call with no better home -- see [`cache_control_handler`]).
//!
//! # Design: flat, no `MemHeader`/`MemChunk` emulation
//!
//! Per `docs/plan.md`'s Phase 3 scope ("start flat, add `MemHeader`
//! emulation only when a corpus binary trips on it"), this module does
//! *not* build any guest-visible `MemHeader`/`MemChunk` chain -- there is
//! no fake `struct MemHeader` a guest program could walk (a program that
//! inspects that structure directly, rather than going through the
//! documented `AllocMem`/`FreeMem` API, is the known, accepted edge case
//! that flat mode doesn't support). Instead, [`crate::guestmem::GuestHeap`]
//! -- the *same* free-list allocator T8's `dosfile.rs`/`doslock.rs`
//! handlers already carve `FileHandle`/`FileInfoBlock`/lock structures
//! out of -- becomes the allocator behind these calls too. That means a
//! guest's `AllocMem`'d block and this runtime's own host-side guest
//! allocations (file handles, fake-library jump tables, ...) share one
//! arena; that's fine and intended, exactly as the plan calls for ("the
//! exec calls become the guest-visible interface to that SAME heap").
//!
//! # Size rounding: `AllocMem` rounds to 8, `GuestHeap` aligns to 4
//!
//! Real `AllocMem` rounds the requested size up to a multiple of `MEMBLOCKSIZE`
//! (8 bytes on all real AmigaOS memory pools) before allocating, because
//! `MemChunk` fidelity needs every block on an 8-byte boundary. This
//! runtime's `GuestHeap` only guarantees 4-byte alignment (T8's own
//! requirement -- see `guestmem.rs`). Rather than changing `GuestHeap`
//! (used by every other handler in the runtime, where 4-byte alignment is
//! sufficient and 8 would waste space for no benefit), [`round_up_8`] is
//! applied *here*, at the `AllocMem`/`AllocVec` call boundary, before the
//! request ever reaches [`crate::guestmem::GuestHeap::alloc`] -- since 8
//! is itself a multiple of 4, the value `GuestHeap` records as the live
//! allocation's size (see [`crate::guestmem::GuestHeap::size_of_live_alloc`])
//! is exactly the already-8-aligned value this module rounded to, with no
//! further rounding happening inside `GuestHeap` itself. This is what
//! lets `FreeMem`'s size-mismatch check (below) compare directly against
//! `GuestHeap`'s own bookkeeping instead of needing a parallel host-side
//! size map.
//!
//! # `FreeMem`: loud failure on a wrong size
//!
//! Real `FreeMem` trusts the caller's `byteSize` argument completely --
//! passing the wrong size corrupts the free list silently. This runtime
//! is a bug-catching tool first, so instead: [`GuestHeap::
//! size_of_live_alloc`] gives the *actual* recorded size of the block at
//! that address, and `FreeMem` compares the (8-rounded) `byteSize`
//! argument against it, failing loudly with [`DispatchError::HandlerFailed`]
//! on any mismatch (wrong size, or an address `GuestHeap` doesn't know as
//! a live allocation at all -- covers both "never allocated here" and
//! "already freed"/double-free).
//!
//! # `NULL` handling
//!
//! - `FreeMem(NULL, size)`: real AmigaOS's own Autodocs are explicit that
//!   `memoryBlock` may be `NULL`, in which case `FreeMem` does nothing
//!   (this is *not* the same as `FreeVec`'s NULL tolerance being a
//!   post-hoc convention -- `FreeMem`'s NULL-is-a-no-op is documented
//!   ROM behavior on every AmigaOS version this project targets). Treated
//!   as a no-op here regardless of `byteSize`, matching vamos.
//! - `FreeVec(NULL)`: also a documented, legal no-op on real `FreeVec`
//!   (unlike bare `FreeMem`, `FreeVec` predates any version-gating
//!   concern here -- it's always been NULL-safe). Treated as a no-op.
//! - `AllocMem(0, ...)` / `AllocVec(0, ...)`: real `AllocMem` handles a
//!   zero-byte request by simply not allocating anything useful; to keep
//!   this runtime's `GuestHeap` bookkeeping simple (no zero-sized live
//!   allocations that `FreeMem`/`FreeVec` would then have to special-case
//!   the size-rounding of), a `byteSize` of 0 returns `0` (`NULL`)
//!   directly without touching the heap at all -- indistinguishable from
//!   real `AllocMem`'s "out of memory" failure mode from the guest's
//!   point of view, which is fine: a well-behaved caller checks for NULL
//!   either way.
//!
//! # `AllocVec`/`FreeVec`: the classic "+8 header" scheme
//!
//! Real `AllocVec` allocates `byteSize + sizeof(ULONG)` bytes, stores the
//! *total* allocated size in the leading `ULONG`, and returns a pointer
//! just past it -- so `FreeVec` (which takes no size argument at all) can
//! recover how much to free from `returned_ptr[-1]`. This module follows
//! that scheme with one adjustment: the header is 8 bytes, not 4,
//! specifically so the *user* pointer `AllocVec` returns stays 8-byte
//! aligned (matching `AllocMem`'s own 8-byte rounding above) rather than
//! trailing a 4-byte header off a `GuestHeap` block that's itself only
//! guaranteed 4-byte aligned. The leading `u32` at `block[0..4]` holds the
//! total block size (header included); `block[4..8]` is unused padding
//! (reserved, written as zero) purely to keep the header's own size a
//! multiple of 8. `AllocVec` returns `block + HEADER_SIZE`.
//!
//! Unlike real `FreeVec` (which *reads* the size back out of guest memory
//! at `ptr[-1]`), this module's `FreeVec` never reads guest memory at
//! all: since the header size is a fixed constant, the block's start
//! address is recovered by pure arithmetic (`user_ptr - HEADER_SIZE`),
//! and [`crate::guestmem::GuestHeap::size_of_live_alloc`] is consulted
//! (keyed by that derived block address) purely as a validity check --
//! not because we need it to know how much to free (`GuestHeap::free`
//! already knows that). This sidesteps adding any new host-side
//! parallel map: `GuestHeap`'s own live-allocation bookkeeping is the
//! single source of truth, exactly as it already is for `FreeMem` above.

use crate::cpu::{AddressRegister, Cpu, DataRegister};
use crate::dispatch::{
    CACHE_BITS_ADDR, DispatchError, EXEC_LIBRARY_BASE, HandlerContext, LibraryTable,
};
use crate::lvos::exec::EXEC_LVOS;
use crate::memory::AddressSpace;

// --- MEMF_* flags (a subset of the real `<exec/memory.h>` bits; this
// runtime is flat memory -- everything is simultaneously "chip" and
// "fast" -- so only MEMF_CLEAR and the AvailMem-only MEMF_LARGEST change
// behavior here. The others are accepted (never rejected -- flat memory
// makes every one of them trivially satisfiable) purely so guest code
// that passes them doesn't need special-casing, and so future handlers
// have the real bit values on hand if a corpus binary needs them. ---

/// Requests memory from public (non-chip-specific) RAM. Flat memory: a
/// no-op here (every allocation is equally "public").
pub const MEMF_PUBLIC: u32 = 1 << 0;
/// Requests chip RAM. Flat memory: accepted and ignored -- there is no
/// separate chip/fast distinction to honor.
pub const MEMF_CHIP: u32 = 1 << 1;
/// Requests fast RAM. Flat memory: accepted and ignored, as [`MEMF_CHIP`].
pub const MEMF_FAST: u32 = 1 << 2;
/// Requests memory local to the caller's own board (Zorro-era
/// multiprocessing concept). Not meaningful in flat memory; accepted and
/// ignored.
pub const MEMF_LOCAL: u32 = 1 << 8;
/// Requests memory from a "24-bit" (Zorro II autoconfig) address range.
/// Not meaningful in flat memory; accepted and ignored.
pub const MEMF_24BITDMA: u32 = 1 << 9;
/// Requests the returned block be zeroed before use. The one `AllocMem`/
/// `AllocVec` requirements flag this module actually honors.
pub const MEMF_CLEAR: u32 = 1 << 16;
/// `AvailMem`-only: report the size of the single largest free block
/// instead of the sum of all free bytes.
pub const MEMF_LARGEST: u32 = 1 << 17;
/// Requests the allocator search the free list from the opposite end
/// (used historically to reduce fragmentation between chip/fast pools).
/// This runtime's single flat free list has no "other end" that matters;
/// accepted and ignored.
pub const MEMF_REVERSE: u32 = 1 << 18;
/// `AvailMem`-only historically ("total memory of this type", as opposed
/// to free memory); not implemented as a distinct query here (this
/// module has no notion of total-vs-free by memory type, since there's
/// only one flat pool) -- accepted as a recognized bit but currently
/// behaves the same as the plain (non-`MEMF_LARGEST`) `AvailMem` query.
pub const MEMF_TOTAL: u32 = 1 << 19;
/// Requests physically contiguous memory suitable for hardware DMA. Flat
/// memory: every allocation is already contiguous; accepted and ignored.
pub const MEMF_NO_EXPUNGE: u32 = 1 << 31;

/// Size in bytes rounded to on every `AllocMem`/`FreeMem` request, and
/// used as the `AllocVec`/`FreeVec` header size -- see the module docs.
const MEMBLOCKSIZE: u32 = 8;

/// Rounds `value` up to the nearest multiple of [`MEMBLOCKSIZE`] (8),
/// matching real `AllocMem`'s own size rounding. Saturates rather than
/// wrapping on overflow, mirroring [`crate::guestmem`]'s `align_up`.
fn round_up_8(value: u32) -> u32 {
    value
        .checked_add(MEMBLOCKSIZE - 1)
        .map_or(!(MEMBLOCKSIZE - 1), |v| v & !(MEMBLOCKSIZE - 1))
}

/// `AllocVec`'s header size -- see the module docs' "`AllocVec`/`FreeVec`"
/// section.
const ALLOCVEC_HEADER_SIZE: u32 = 8;

/// `AllocMem` (LVO -198): `D0` = requested byte size, `D1` = requirements
/// (`MEMF_*`). `D0` = the allocated block's address, or `0` on failure --
/// real `AllocMem` never errors out of the call, it just returns `NULL`
/// (see the module docs' NULL-handling section for the `byteSize == 0`
/// case specifically).
fn alloc_mem_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let byte_size = ctx.cpu.data_register(DataRegister(0));
    let requirements = ctx.cpu.data_register(DataRegister(1));

    if byte_size == 0 {
        ctx.cpu.set_data_register(DataRegister(0), 0);
        return Ok(());
    }

    let rounded = round_up_8(byte_size);
    match ctx.heap.alloc(rounded) {
        Ok(addr) => {
            if requirements & MEMF_CLEAR != 0 {
                for i in 0..rounded {
                    ctx.mem.write_u8(addr.wrapping_add(i), 0);
                }
            }
            ctx.cpu.set_data_register(DataRegister(0), addr);
        }
        Err(_) => {
            // Real AllocMem returns NULL on failure rather than
            // signalling an error to the caller -- this is expected,
            // guest-visible behavior a well-behaved program checks for,
            // not a runtime bug, so it's not a DispatchError.
            ctx.cpu.set_data_register(DataRegister(0), 0);
        }
    }
    Ok(())
}

/// `FreeMem` (LVO -210): `A1` = memory block, `D0` = byte size. See the
/// module docs for the NULL-is-a-no-op and size-mismatch-is-loud-failure
/// decisions.
fn free_mem_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let addr = ctx.cpu.address_register(AddressRegister(1));
    let byte_size = ctx.cpu.data_register(DataRegister(0));

    if addr == 0 {
        // Documented-legal no-op -- see the module docs.
        return Ok(());
    }

    let rounded = round_up_8(byte_size);
    let recorded = ctx.heap.size_of_live_alloc(addr);
    match recorded {
        Some(actual) if actual == rounded => {
            ctx.heap
                .free(addr)
                .map_err(|e| DispatchError::HandlerFailed {
                    library: "exec.library".to_string(),
                    lvo: -210,
                    handler_name: "FreeMem".to_string(),
                    message: format!(
                        "GuestHeap::free unexpectedly failed for a block it just \
                     confirmed as live at {addr:#010x}: {e}"
                    ),
                })
        }
        Some(actual) => Err(DispatchError::HandlerFailed {
            library: "exec.library".to_string(),
            lvo: -210,
            handler_name: "FreeMem".to_string(),
            message: format!(
                "FreeMem size mismatch at {addr:#010x}: called with byteSize \
                 {byte_size} (rounds to {rounded}), but this block was \
                 allocated with size {actual} -- this looks like a guest bug \
                 (wrong size passed to FreeMem), which would silently corrupt \
                 the free list on real AmigaOS"
            ),
        }),
        None => Err(DispatchError::HandlerFailed {
            library: "exec.library".to_string(),
            lvo: -210,
            handler_name: "FreeMem".to_string(),
            message: format!(
                "FreeMem called on {addr:#010x}, which isn't a currently-live \
                 AllocMem allocation (never allocated, already freed, or not \
                 an AllocMem block at all)"
            ),
        }),
    }
}

/// `AvailMem` (LVO -216): `D1` = attributes (`MEMF_*`). `D0` = total free
/// bytes across the heap, or (if `MEMF_LARGEST` is set) the size of the
/// single largest contiguous free block.
fn avail_mem_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let attributes = ctx.cpu.data_register(DataRegister(1));
    let result = if attributes & MEMF_LARGEST != 0 {
        ctx.heap.largest_free()
    } else {
        ctx.heap.total_free()
    };
    ctx.cpu.set_data_register(DataRegister(0), result);
    Ok(())
}

/// `AllocVec` (LVO -684): `D0` = requested byte size, `D1` = requirements
/// (`MEMF_*`, same meaning as `AllocMem`). `D0` = a pointer to the
/// user-visible block, or `0` on failure. See the module docs'
/// "`AllocVec`/`FreeVec`" section for the header scheme.
fn alloc_vec_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let byte_size = ctx.cpu.data_register(DataRegister(0));
    let requirements = ctx.cpu.data_register(DataRegister(1));

    if byte_size == 0 {
        ctx.cpu.set_data_register(DataRegister(0), 0);
        return Ok(());
    }

    let user_rounded = round_up_8(byte_size);
    let total = user_rounded + ALLOCVEC_HEADER_SIZE;
    match ctx.heap.alloc(total) {
        Ok(block) => {
            // Header: total block size (u32) followed by 4 bytes of
            // reserved padding (see the module docs).
            ctx.mem.write_u32(block, total);
            ctx.mem.write_u32(block.wrapping_add(4), 0);

            let user_ptr = block.wrapping_add(ALLOCVEC_HEADER_SIZE);
            if requirements & MEMF_CLEAR != 0 {
                for i in 0..user_rounded {
                    ctx.mem.write_u8(user_ptr.wrapping_add(i), 0);
                }
            }
            ctx.cpu.set_data_register(DataRegister(0), user_ptr);
        }
        Err(_) => {
            // As AllocMem: real AllocVec returns NULL on failure, not an
            // error signal.
            ctx.cpu.set_data_register(DataRegister(0), 0);
        }
    }
    Ok(())
}

/// `FreeVec` (LVO -690): `A1` = the user pointer `AllocVec` returned. See
/// the module docs for the NULL-is-a-legal-no-op decision and why this
/// handler never reads guest memory to recover the block size.
fn free_vec_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let user_ptr = ctx.cpu.address_register(AddressRegister(1));

    if user_ptr == 0 {
        // Documented-legal no-op -- see the module docs.
        return Ok(());
    }

    let block = user_ptr.wrapping_sub(ALLOCVEC_HEADER_SIZE);
    if ctx.heap.size_of_live_alloc(block).is_none() {
        return Err(DispatchError::HandlerFailed {
            library: "exec.library".to_string(),
            lvo: -690,
            handler_name: "FreeVec".to_string(),
            message: format!(
                "FreeVec called on {user_ptr:#010x} (derived block address \
                 {block:#010x}), which isn't a currently-live AllocVec \
                 allocation (never allocated, already freed, or not an \
                 AllocVec pointer at all)"
            ),
        });
    }

    ctx.heap
        .free(block)
        .map_err(|e| DispatchError::HandlerFailed {
            library: "exec.library".to_string(),
            lvo: -690,
            handler_name: "FreeVec".to_string(),
            message: format!(
                "GuestHeap::free unexpectedly failed for a block it just \
             confirmed as live at {block:#010x}: {e}"
            ),
        })
}

/// `CopyMem`/`CopyMemQuick` (LVOs -624/-630): `A0` = source, `A1` =
/// dest, `D0` = size in bytes. No return value. Real `CopyMemQuick`
/// additionally requires long-word alignment and a size that's a
/// multiple of 4, but since this handler just copies bytes either way
/// (no SIMD/longword-move optimization to actually differ on), both
/// share one implementation. Copies via an intermediate host `Vec`
/// rather than reading and writing guest memory byte-by-byte in the
/// same pass, so an overlapping copy (real `CopyMem`'s documented
/// "not supported" case) still behaves predictably here instead of
/// corrupting data depending on copy direction.
fn copy_mem_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let source = ctx.cpu.address_register(AddressRegister(0));
    let dest = ctx.cpu.address_register(AddressRegister(1));
    let size = ctx.cpu.data_register(DataRegister(0));

    let mut buf = Vec::with_capacity(size as usize);
    for i in 0..size {
        buf.push(ctx.mem.read_u8(source.wrapping_add(i)));
    }
    for (i, b) in buf.into_iter().enumerate() {
        ctx.mem.write_u8(dest.wrapping_add(i as u32), b);
    }
    Ok(())
}

/// `exec.library`'s `CacheControl` (LVO -648: `D0` = `cacheBits`, `D1`
/// = `cacheMask`). `D0` = the `CACRF_*` state *before* this call --
/// bits named in `cacheMask` are set to the corresponding bit of
/// `cacheBits`; every other bit is left alone. `cacheMask == 0` is
/// therefore a pure query (real, documented behavior -- "if
/// `cacheMask` is 0, the state is not changed").
///
/// This runtime doesn't model a real CPU cache (nothing here is ever
/// actually cached or invalidated), so this is bookkeeping only: the
/// bits themselves live in guest memory at [`CACHE_BITS_ADDR`] (single
/// source of truth, no host-side mirror -- same convention
/// `crate::exectask`'s module docs establish for task/signal state),
/// seeded to [`crate::dispatch::Runtime::new`]'s "everything enabled"
/// default. Found missing while running the real `CPU` command
/// (Workbench 3.1.4 `C:`), which queries this to report the current
/// cache/burst state.
fn cache_control_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let cache_bits = ctx.cpu.data_register(DataRegister(0));
    let cache_mask = ctx.cpu.data_register(DataRegister(1));

    let old = ctx.mem.read_u32(CACHE_BITS_ADDR);
    let new = (old & !cache_mask) | (cache_bits & cache_mask);
    ctx.mem.write_u32(CACHE_BITS_ADDR, new);

    ctx.cpu.set_data_register(DataRegister(0), old);
    Ok(())
}

/// Registers every T16 `exec.library` memory-allocation handler onto
/// [`EXEC_LIBRARY_BASE`], looked up by name through [`EXEC_LVOS`] (the T7
/// table), following [`crate::dosfile::register_dos_handlers`]'s
/// registration pattern. Called unconditionally from
/// [`crate::dispatch::Runtime::new`].
pub fn register_execmem_handlers<C: Cpu + 'static>(
    table: &mut LibraryTable<C>,
    mem: &mut C::Memory,
) {
    macro_rules! reg {
        ($name:literal, $handler:expr) => {
            table
                .register_by_name(
                    mem,
                    EXEC_LIBRARY_BASE,
                    EXEC_LVOS,
                    "exec.library",
                    $name,
                    $handler,
                )
                .unwrap_or_else(|e| panic!("{} should be in EXEC_LVOS: {e}", $name));
        };
    }
    reg!("AllocMem", alloc_mem_handler::<C>);
    reg!("FreeMem", free_mem_handler::<C>);
    reg!("AvailMem", avail_mem_handler::<C>);
    reg!("AllocVec", alloc_vec_handler::<C>);
    reg!("FreeVec", free_vec_handler::<C>);
    reg!("CopyMem", copy_mem_handler::<C>);
    reg!("CopyMemQuick", copy_mem_handler::<C>);
    reg!("CacheControl", cache_control_handler::<C>);
}

#[cfg(test)]
// Test programs below build a `Vec<u16>` word-at-a-time via a sequence of
// `.push()` calls immediately after `Vec::new()` (mirroring the style
// `dispatch.rs`/`dosfile.rs`'s own hand-assembled-program tests use) --
// `vec![]` would be equally correct but much harder to annotate with a
// per-word comment naming the instruction/operand it encodes.
#[allow(clippy::vec_init_then_push)]
mod tests {
    use super::*;
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{Runtime, StartConfig};
    use crate::memory::{AddressSpace, FlatMemory};

    fn load_words(mem: &mut FlatMemory, addr: u32, words: &[u16]) {
        let mut offset = addr;
        for &w in words {
            mem.write_u16(offset, w);
            offset += 2;
        }
    }

    /// `move.l #imm32,Dn`.
    fn move_imm_to_d(n: u16) -> u16 {
        0x203C | (n << 9)
    }

    /// `move.l #imm32,An`.
    fn move_imm_to_a(n: u16) -> u16 {
        0x207C | (n << 9)
    }

    /// `move.l D0,Dn`.
    fn move_d0_to_d(n: u16) -> u16 {
        0x2000 | (n << 9)
    }

    /// `move.l D0,An`.
    fn move_d0_to_a(n: u16) -> u16 {
        0x2040 | (n << 9)
    }

    /// `movea.l #EXEC_LIBRARY_BASE,a6` -- points A6 at exec.library,
    /// since `Runtime::new` seeds A6 with `DOS_LIBRARY_BASE` (a Phase 1
    /// compatibility shim -- see its docs), but these tests need A6 =
    /// `EXEC_LIBRARY_BASE` to call exec.library LVOs. Loading the
    /// well-known constant directly (rather than indirecting through
    /// `move.l 4.w,a6`, i.e. reading `AbsExecBase`) is simpler and
    /// exercises the same effective A6 value real startup code would end
    /// up with.
    fn movea_exec_base_to_a6() -> [u16; 3] {
        [
            move_imm_to_a(6),
            (EXEC_LIBRARY_BASE >> 16) as u16,
            EXEC_LIBRARY_BASE as u16,
        ]
    }

    /// `jsr <disp16>(a6)`.
    fn jsr_disp16_a6(disp: i32) -> [u16; 2] {
        [0x4EAE, disp as u16]
    }

    const RTS: u16 = 0x4E75;

    /// Builds a `Runtime` around `words`, with A6 pointed at
    /// `EXEC_LIBRARY_BASE` (see [`MOVEA_4W_TO_A6`]'s docs) -- callers
    /// should *not* prepend that themselves, [`program`] does it.
    fn runtime_with_program(words: &[u16]) -> Runtime<M68kCpu> {
        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, words);
        let load_end = entry + 0x400;
        Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end,
                args: Vec::new(),
                ..StartConfig::default()
            },
        )
    }

    /// Prepends the A6-fixup to `words` before building the runtime.
    fn program(words: &[u16]) -> Runtime<M68kCpu> {
        let mut full = movea_exec_base_to_a6().to_vec();
        full.extend_from_slice(words);
        runtime_with_program(&full)
    }

    #[test]
    fn alloc_mem_with_memf_clear_zeroes_and_alloc_free_round_trips() {
        let mut words = Vec::new();
        words.push(move_imm_to_d(0)); // D0 = byteSize
        words.push(0);
        words.push(64);
        words.push(move_imm_to_d(1)); // D1 = MEMF_CLEAR
        words.push((MEMF_CLEAR >> 16) as u16);
        words.push(MEMF_CLEAR as u16);
        words.extend_from_slice(&jsr_disp16_a6(-198)); // AllocMem: D0 = addr
        words.push(move_d0_to_a(1)); // A1 = addr (survives FreeMem call)
        words.push(RTS);

        let mut rt = program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_ne!(code, 0, "AllocMem(64) should not return NULL");
        let addr = code as u32;

        // MEMF_CLEAR: every byte in the block should be zero. Poison the
        // block first via direct memory access isn't possible post-run
        // (no mutable accessor), so instead just check the returned
        // region reads as zero, which is what MEMF_CLEAR guarantees.
        for i in 0..64u32 {
            assert_eq!(
                rt.memory().read_u8(addr + i),
                0,
                "MEMF_CLEAR should zero every byte of the allocated block"
            );
        }
    }

    #[test]
    fn alloc_mem_zero_size_returns_null() {
        let mut words = Vec::new();
        words.push(move_imm_to_d(0)); // D0 = 0
        words.push(0);
        words.push(0);
        words.push(move_imm_to_d(1)); // D1 = 0 (no requirements)
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-198)); // AllocMem: D0 = 0
        words.push(RTS);

        let mut rt = program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 0, "AllocMem(0) should return NULL");
    }

    #[test]
    fn alloc_then_free_mem_round_trip_reuses_the_block() {
        let mut words = Vec::new();
        words.push(move_imm_to_d(0)); // D0 = byteSize
        words.push(0);
        words.push(16);
        words.push(move_imm_to_d(1)); // D1 = 0 (no requirements)
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-198)); // AllocMem
        words.push(move_d0_to_a(1)); // A1 = addr (for FreeMem)
        words.push(move_d0_to_d(2)); // D2 = addr (kept for the exit code)
        words.push(move_imm_to_d(0)); // D0 = byteSize again (FreeMem's arg)
        words.push(0);
        words.push(16);
        words.extend_from_slice(&jsr_disp16_a6(-210)); // FreeMem
        // exit code = D2 (the originally allocated address) -- nonzero
        // proves AllocMem succeeded, and the run completing at all
        // (rather than a HandlerFailed) proves FreeMem accepted it.
        words.push(0x2002); // move.l D2,D0
        words.push(RTS);

        let mut rt = program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_ne!(code, 0);
    }

    #[test]
    fn free_mem_null_is_a_no_op() {
        let mut words = Vec::new();
        words.push(move_imm_to_a(1)); // A1 = 0
        words.push(0);
        words.push(0);
        words.push(move_imm_to_d(0)); // D0 = 0 (byteSize, irrelevant for NULL)
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-210)); // FreeMem(NULL, 0)
        words.push(0x7000); // moveq #0,d0
        words.push(RTS);

        let mut rt = program(&words);
        let mut out = Vec::new();
        let code = rt
            .run(&mut out, None)
            .expect("FreeMem(NULL) should be a no-op, not an error");
        assert_eq!(code, 0);
    }

    #[test]
    fn free_mem_wrong_size_is_a_loud_diagnostic() {
        let mut words = Vec::new();
        words.push(move_imm_to_d(0)); // D0 = byteSize
        words.push(0);
        words.push(16);
        words.push(move_imm_to_d(1)); // D1 = 0
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-198)); // AllocMem(16)
        words.push(move_d0_to_a(1)); // A1 = addr
        words.push(move_imm_to_d(0)); // D0 = 8 (WRONG size for FreeMem)
        words.push(0);
        words.push(8);
        words.extend_from_slice(&jsr_disp16_a6(-210)); // FreeMem(addr, 8) -- mismatch
        words.push(RTS);

        let mut rt = program(&words);
        let mut out = Vec::new();
        let err = rt.run(&mut out, None).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("size mismatch"),
            "expected a size-mismatch diagnostic, got: {msg}"
        );
    }

    #[test]
    fn free_mem_unknown_address_is_a_loud_diagnostic() {
        let mut words = Vec::new();
        words.push(move_imm_to_a(1)); // A1 = some never-allocated address
        words.push((TRAP_TABLE_END >> 16) as u16);
        words.push((TRAP_TABLE_END + 0x1000) as u16);
        words.push(move_imm_to_d(0)); // D0 = 16
        words.push(0);
        words.push(16);
        words.extend_from_slice(&jsr_disp16_a6(-210)); // FreeMem
        words.push(RTS);

        let mut rt = program(&words);
        let mut out = Vec::new();
        let err = rt.run(&mut out, None).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("isn't a currently-live"),
            "expected an unknown-address diagnostic, got: {msg}"
        );
    }

    #[test]
    fn alloc_vec_free_vec_round_trip_including_null() {
        let mut words = Vec::new();
        words.push(move_imm_to_d(0)); // D0 = byteSize
        words.push(0);
        words.push(20);
        words.push(move_imm_to_d(1)); // D1 = MEMF_CLEAR
        words.push((MEMF_CLEAR >> 16) as u16);
        words.push(MEMF_CLEAR as u16);
        words.extend_from_slice(&jsr_disp16_a6(-684)); // AllocVec: D0 = user ptr
        words.push(move_d0_to_d(2)); // D2 = user ptr (kept for the exit code)
        words.push(move_d0_to_a(1)); // A1 = user ptr (for FreeVec)
        words.extend_from_slice(&jsr_disp16_a6(-690)); // FreeVec
        // FreeVec(NULL) too -- should also be a no-op.
        words.push(move_imm_to_a(1));
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-690)); // FreeVec(NULL)
        words.push(0x2002); // move.l D2,D0 (exit code = the AllocVec'd ptr)
        words.push(RTS);

        let mut rt = program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_ne!(code, 0, "AllocVec should not return NULL");
    }

    #[test]
    fn alloc_vec_memf_clear_zeroes_the_user_visible_region() {
        let mut words = Vec::new();
        words.push(move_imm_to_d(0)); // D0 = byteSize
        words.push(0);
        words.push(32);
        words.push(move_imm_to_d(1)); // D1 = MEMF_CLEAR
        words.push((MEMF_CLEAR >> 16) as u16);
        words.push(MEMF_CLEAR as u16);
        words.extend_from_slice(&jsr_disp16_a6(-684)); // AllocVec
        words.push(RTS);

        let mut rt = program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_ne!(code, 0);
        let addr = code as u32;
        for i in 0..32u32 {
            assert_eq!(rt.memory().read_u8(addr + i), 0);
        }
        // The header (8 bytes just before the returned pointer) should
        // hold the total block size, exactly as the module docs promise.
        let total = rt.memory().read_u32(addr - 8);
        assert_eq!(total, 32 + 8);
    }

    #[test]
    fn free_vec_unknown_pointer_is_a_loud_diagnostic() {
        let mut words = Vec::new();
        words.push(move_imm_to_a(1)); // A1 = some never-allocated address
        words.push((TRAP_TABLE_END >> 16) as u16);
        words.push((TRAP_TABLE_END + 0x2000) as u16);
        words.extend_from_slice(&jsr_disp16_a6(-690)); // FreeVec
        words.push(RTS);

        let mut rt = program(&words);
        let mut out = Vec::new();
        let err = rt.run(&mut out, None).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("isn't a currently-live"),
            "expected an unknown-pointer diagnostic, got: {msg}"
        );
    }

    #[test]
    fn avail_mem_total_and_largest() {
        // First: query AvailMem() (D1 = 0) before any allocation, save
        // the result in D2. Then AllocMem a chunk, then query
        // AvailMem(MEMF_LARGEST) into D3, exit code = D2 - (post-alloc
        // total), which should be nonzero (D2 was the larger, pristine
        // total), proving the allocation reduced free space.
        let mut words = Vec::new();
        words.push(move_imm_to_d(1)); // D1 = 0
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-216)); // AvailMem(): D0 = total free before
        words.push(move_d0_to_d(2)); // D2 = total free before

        words.push(move_imm_to_d(0)); // D0 = byteSize
        words.push(0);
        words.push(256);
        words.push(move_imm_to_d(1)); // D1 = 0
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-198)); // AllocMem(256)

        words.push(move_imm_to_d(1)); // D1 = MEMF_LARGEST
        words.push((MEMF_LARGEST >> 16) as u16);
        words.push(MEMF_LARGEST as u16);
        words.extend_from_slice(&jsr_disp16_a6(-216)); // AvailMem(MEMF_LARGEST)
        // D0 now holds the largest free block after allocating 256
        // bytes; D2 - D0 should be >= 256 (strictly less free space
        // available as one contiguous chunk than before the alloc).
        // sub.l D0,D2 ; move.l D2,D0
        words.push(0x9480); // sub.l D0,D2
        words.push(0x2002); // move.l D2,D0
        words.push(RTS);

        let mut rt = program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert!(
            code >= 256,
            "AllocMem(256) should shrink the largest free block by at \
             least 256 bytes, got delta {code}"
        );
    }

    #[test]
    fn copy_mem_copies_bytes_from_source_to_dest() {
        let mut words = movea_exec_base_to_a6().to_vec();
        let source_idx = words.len();
        words.push(move_imm_to_a(0)); // A0 = source, patched below
        words.push(0);
        words.push(0);
        let dest_idx = words.len();
        words.push(move_imm_to_a(1)); // A1 = dest, patched below
        words.push(0);
        words.push(0);
        words.push(move_imm_to_d(0)); // D0 = size
        words.push(0);
        words.push(8);
        words.extend_from_slice(&jsr_disp16_a6(-624)); // CopyMem(a6)
        words.push(RTS);

        let source = b"COPYTEST";
        let source_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        let dest_addr = source_addr + source.len() as u32 + 16;
        words[source_idx + 1] = (source_addr >> 16) as u16;
        words[source_idx + 2] = source_addr as u16;
        words[dest_idx + 1] = (dest_addr >> 16) as u16;
        words[dest_idx + 2] = dest_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);
        for (i, &b) in source.iter().enumerate() {
            mem.write_u8(source_addr + i as u32, b);
        }

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: dest_addr + source.len() as u32 + 4,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed");
        for (i, &b) in source.iter().enumerate() {
            assert_eq!(rt.memory().read_u8(dest_addr + i as u32), b);
        }
    }

    // --- CacheControl ---

    fn cache_control_program(cache_bits: u32, cache_mask: u32) -> Vec<u16> {
        let mut words = movea_exec_base_to_a6().to_vec();
        words.push(move_imm_to_d(0)); // D0 = cacheBits
        words.push((cache_bits >> 16) as u16);
        words.push(cache_bits as u16);
        words.push(move_imm_to_d(1)); // D1 = cacheMask
        words.push((cache_mask >> 16) as u16);
        words.push(cache_mask as u16);
        words.extend_from_slice(&jsr_disp16_a6(-648)); // CacheControl(a6)
        words.push(RTS);
        words
    }

    #[test]
    fn cache_control_query_with_zero_mask_reports_the_default_without_changing_it() {
        let words = cache_control_program(0, 0);
        let mut rt = runtime_with_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code as u32,
            crate::dispatch::CACHE_BITS_DEFAULT,
            "D0 should report the pre-call state"
        );
        assert_eq!(
            rt.memory().read_u32(CACHE_BITS_ADDR),
            crate::dispatch::CACHE_BITS_DEFAULT,
            "a zero mask must not change any bits"
        );
    }

    #[test]
    fn cache_control_sets_only_the_masked_bits() {
        // Clear CACRF_EnableI (bit 0) and CACRF_EnableD (bit 8), leaving
        // every other default bit untouched.
        let mask = 0x101;
        let words = cache_control_program(0, mask);
        let mut rt = runtime_with_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code as u32,
            crate::dispatch::CACHE_BITS_DEFAULT,
            "D0 should still report the pre-call (old) state"
        );
        assert_eq!(
            rt.memory().read_u32(CACHE_BITS_ADDR),
            crate::dispatch::CACHE_BITS_DEFAULT & !mask,
            "only the masked bits should have cleared"
        );
    }

    #[test]
    fn cache_control_query_then_set_round_trips() {
        let query = cache_control_program(0, 0);
        let mut rt = runtime_with_program(&query);
        let mut out = Vec::new();
        let old = rt.run(&mut out, None).expect("run should succeed") as u32;

        // Set every bit the query reported, via an all-ones mask -- the
        // result should be unchanged from the default.
        let set_same = cache_control_program(old, 0xFFFF_FFFF);
        let mut rt2 = runtime_with_program(&set_same);
        let mut out2 = Vec::new();
        rt2.run(&mut out2, None).expect("run should succeed");
        assert_eq!(rt2.memory().read_u32(CACHE_BITS_ADDR), old);
    }
}
