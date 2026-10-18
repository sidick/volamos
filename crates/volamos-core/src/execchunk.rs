//! `exec.library`'s `Allocate`/`Deallocate`: the raw, address-ordered,
//! coalescing free-list allocator that operates directly on a
//! caller-supplied `struct MemHeader`/`struct MemChunk` chain. Found
//! needed running the real `AmiSnap` binary (`~/src/amisnap`).
//!
//! # Design: a real chunk chain, deliberately separate from `AllocMem`
//!
//! `crate::execmem`'s module docs explain why *that* module's
//! `AllocMem`/`FreeMem`/`AllocVec`/`FreeVec` deliberately don't build a
//! guest-visible `MemHeader`/`MemChunk` chain -- Phase 3 scope was
//! "start flat, add real chunk emulation only when a corpus binary
//! trips on it" (`docs/plan.md`). `Allocate`/`Deallocate` are that
//! trip, but they don't actually require reopening that decision:
//! per `Allocate`'s own NDK/AROS-documented `EXAMPLE`, the *intended*
//! use is a caller-built **private** pool -- `AllocMem` one big block,
//! then hand-initialize a `MemHeader` pointing at a single `MemChunk`
//! spanning it, and suballocate from that with `Allocate`/`Deallocate`
//! directly. Neither function ever needs to touch this runtime's own
//! flat [`crate::guestmem::GuestHeap`] at all -- they operate purely on
//! whatever chunk chain already lives at the guest `MemHeader` address
//! the caller passes in `A0`. So this module is fully independent of
//! `execmem.rs`'s flat model; both coexist because they answer
//! different real AmigaOS APIs over different memory.
//!
//! # Algorithm
//!
//! The classic real-exec algorithm (NDK headers document the struct
//! layout, not the algorithm -- traced against general Amiga systems-
//! programming references and AROS's `Allocate()` doc comment/`EXAMPLE`
//! for the intended usage shape): `mh_First` chains `MemChunk`s in
//! ascending-address order. `Allocate` walks the chain first-fit,
//! taking the requested (8-byte-rounded, [`crate::execmem`]'s own
//! `MEM_BLOCKSIZE` convention) bytes off the *front* of the first
//! chunk big enough (shrinking it in place, or unlinking it entirely
//! if the fit is exact). `Deallocate` walks the chain to find the
//! correct address-ordered insertion point, then coalesces the freed
//! block with its predecessor and/or successor chunk if they're
//! exactly adjacent in memory -- keeping the free list from
//! fragmenting into ever-smaller chunks over a long-running pool's
//! lifetime, exactly like a real one would.
//!
//! # Sanitizer interaction (issue #83, Tier 1)
//!
//! `--sanitize`'s heap detectors (issues #65/#66) originally only
//! poisoned the shadow map around `AllocMem`/`AllocVec`/`AllocPooled`
//! -- not here, even though this is exactly where a C runtime's
//! `malloc` actually lives (libnix/newlib take a few big chunks via
//! `AllocVec`, then suballocate from them with `Allocate`). Issue #83's
//! Tier 1 closes that gap for this module: a successful `Allocate`
//! marks its returned range [`crate::sanitize::ShadowState::Uninit`],
//! and `Deallocate` marks the freed range
//! [`crate::sanitize::ShadowState::Unaddressable`] with
//! [`crate::sanitize::PoisonReason::Freed`] -- the same shadow states
//! `crate::execmem`'s `poison_allocation_edges`/`poison_freed_block`
//! apply to `AllocMem`/`FreeMem`. Tier 2 (redzones around the returned
//! block, to catch *overruns*) is explicitly out of scope here: it
//! would mean `Allocate` handing back an inner pointer and taking more
//! from the chunk than asked, which changes the guest-visible pool
//! layout (`AvailMem`-equivalent walks of `mh_First` would see
//! different numbers) -- a bigger, separate decision.
//!
//! There's a wrinkle `crate::execmem`'s analogous helpers don't have:
//! `AllocMem`'s free list lives entirely in *host* memory
//! ([`crate::guestmem::GuestHeap`]), invisible to the shadow map, so
//! poisoning a freed `AllocMem` block can never collide with the
//! allocator's own bookkeeping. This module's free list is the
//! opposite -- `mc_Next`/`mc_Bytes` live *in guest memory*, read and
//! written through the exact same checked [`AddressSpace::read_u32`]/
//! `write_u32` path a guest access would use. Naively poisoning a
//! freed chunk's *entire* extent therefore poisons its own 8-byte
//! `MemChunk` header too -- and since a poisoned byte never heals on a
//! checked write (only `Uninit` does, see `crate::sanitize`'s
//! `check_write_byte`), the *very next* `Allocate`/`Deallocate` call
//! that walks past this still-free chunk (to read its `mc_Bytes`, or to
//! rewrite its header while splitting or coalescing it) would report a
//! violation against the allocator's own legitimate bookkeeping --
//! not a guest bug at all. A quick mental trace confirms this isn't a
//! theoretical corner case: `malloc(n); free(p); malloc(m)` with `m <
//! n` is an everyday pattern, and the second `malloc`'s partial split
//! of the just-freed chunk writes a brand new header into bytes this
//! module would otherwise have just marked `Freed`.
//!
//! The fix kept within Tier 1's "no layout change" scope: a `MemChunk`
//! header that remains (or becomes) part of the *live* free list is
//! always kept [`crate::sanitize::ShadowState::Valid`], via
//! [`exempt_chunk_header`], regardless of what poison it may have
//! inherited. Concretely:
//! - [`deallocate_handler`] poisons the *entire* requested
//!   `[block, block + size)` range first, then -- only when `block`
//!   itself remains the live chain node afterwards (i.e. it did *not*
//!   merge into its predecessor) -- re-exempts its own 8-byte header.
//!   A block that *did* merge into its predecessor is no longer
//!   referenced by the free list at all, so no exemption is needed:
//!   its whole extent, former header included, can stay poisoned.
//! - [`allocate_handler`]'s partial-fit path always exempts the fresh
//!   `leftover` chunk's header the moment it writes one, since that
//!   address can land anywhere inside a chunk's body that an earlier
//!   `Deallocate` poisoned wholesale.
//!
//! This does narrow Tier 1's use-after-free coverage slightly: the
//! first 8 bytes of a freed block that is *not* merged into a
//! predecessor stay `Valid` rather than `Freed` for as long as it sits
//! on the free list unmerged and unreallocated (a dangling pointer
//! into exactly those 8 bytes goes undetected in that window), and a
//! chunk absorbed into a neighbor via coalescing keeps its old header
//! bytes `Valid` too (coalescing only transfers the absorbed chunk's
//! *size* into the surviving node, it doesn't retroactively poison the
//! bytes that stopped being a header). Both are deliberate, documented
//! trade-offs -- the alternative is spurious violations against the
//! allocator itself on every realistic malloc/free workload, which
//! would make `--sanitize` actively worse than doing nothing here.
//! Tests below exercise a use-after-free into the *body* of a freed
//! block (well past its header) precisely because that's the coverage
//! this Tier 1 fix actually, reliably provides.

use crate::cpu::{AddressRegister, Cpu, DataRegister};
use crate::dispatch::{DispatchError, EXEC_LIBRARY_BASE, HandlerContext, LibraryTable};
use crate::lvos::exec::EXEC_LVOS;
use crate::memory::AddressSpace;
use crate::sanitize::PoisonReason;

/// `mh_First`: `struct MemChunk*`, offset 16 within `struct MemHeader`
/// (`mh_Node` `struct Node` 14 bytes + `mh_Attributes` `UWORD` 2 bytes
/// = 16), per `<exec/memory.h>`.
const MH_FIRST: u32 = 16;
/// `mh_Free`: `ULONG`, offset 28 (16 + `mh_Lower`/`mh_Upper` `APTR` 4
/// each = 24, + `mh_Free` itself starts right after).
const MH_FREE: u32 = 28;
/// `sizeof(struct MemHeader)`: 28 + `mh_Free`'s own 4 bytes = 32.
#[cfg(test)]
const MEMHEADER_SIZE: u32 = 32;

/// `mc_Next`: `struct MemChunk*`, offset 0.
const MC_NEXT: u32 = 0;
/// `mc_Bytes`: `ULONG`, offset 4.
const MC_BYTES: u32 = 4;
/// `sizeof(struct MemChunk)`, also real AmigaOS's `MEM_BLOCKSIZE`
/// alignment/rounding unit for every request through this API (same
/// value [`crate::execmem`] rounds `AllocMem`/`AllocVec` requests to,
/// for the same underlying reason -- every chunk boundary must stay
/// aligned to `sizeof(struct MemChunk)`).
const MEMCHUNK_SIZE: u32 = 8;

fn round_up_to_memchunk(value: u32) -> u32 {
    value
        .checked_add(MEMCHUNK_SIZE - 1)
        .map_or(!(MEMCHUNK_SIZE - 1), |v| v & !(MEMCHUNK_SIZE - 1))
}

/// Marks `[addr, addr + len)` [`crate::sanitize::ShadowState::Uninit`]
/// in the sanitizer shadow map, if one is installed (`--sanitize`); a
/// no-op otherwise. The heap-detector half of issue #83's Tier 1 for
/// `Allocate` -- see the module doc's "Sanitizer interaction" section.
///
/// Unlike `crate::execmem`'s `poison_allocation_edges`, there's no heap
/// lookup here: `Allocate` never touches [`crate::guestmem::GuestHeap`]
/// (see this module's top-level doc), so there's no
/// `extent_of_live_alloc` to query. The handler already has the exact
/// `addr`/`len` of the block it's about to hand back, which is
/// everything that lookup would have produced anyway.
///
/// Real `Allocate` takes no `requirements`/`MEMF_*` argument at all
/// (confirmed against the NDK `exec.doc` autodoc entry -- its only
/// parameters are `memHeader` and `byteSize`) and its autodoc never
/// describes the returned block as cleared, so (unlike `AllocMem`)
/// there's no `MEMF_CLEAR`-equivalent case to special-case here: every
/// successful `Allocate` hands back debris from the guest's own pool,
/// exactly what `Uninit` means.
fn poison_allocated_chunk<C: Cpu>(ctx: &mut HandlerContext<'_, C>, addr: u32, len: u32) {
    let Some(shadow) = ctx.mem.shadow_mut() else {
        return;
    };
    shadow.mark_uninit(addr, len);
}

/// Marks `[addr, addr + len)`
/// [`crate::sanitize::ShadowState::Unaddressable`] with
/// [`PoisonReason::Freed`], if a shadow map is installed; a no-op
/// otherwise. The other half of issue #83's Tier 1 -- see the module
/// doc's "Sanitizer interaction" section for why `deallocate_handler`
/// must still re-exempt the live chain node's own header afterwards in
/// the no-merge-with-predecessor case.
fn poison_freed_chunk<C: Cpu>(ctx: &mut HandlerContext<'_, C>, addr: u32, len: u32) {
    let Some(shadow) = ctx.mem.shadow_mut() else {
        return;
    };
    shadow.mark_unaddressable(addr, len, PoisonReason::Freed);
}

/// Clears any sanitizer poison over one `MemChunk`'s 8-byte
/// `mc_Next`/`mc_Bytes` header at `chunk`, if a shadow map is
/// installed. A no-op otherwise.
///
/// Must be called immediately whenever `chunk`'s header is written (or
/// re-written) as part of the free list remaining live structure --
/// see the module doc's "Sanitizer interaction" section for why: once
/// `Deallocate` poisons a freed block's whole extent, this is what
/// keeps the allocator's *own* subsequent reads/writes of that exact
/// header, made through the same checked [`AddressSpace`] path a guest
/// access would use, from being reported as a violation against
/// itself.
fn exempt_chunk_header<C: Cpu>(ctx: &mut HandlerContext<'_, C>, chunk: u32) {
    let Some(shadow) = ctx.mem.shadow_mut() else {
        return;
    };
    shadow.mark_valid(chunk, MEMCHUNK_SIZE);
}

/// `Allocate` (LVO -186): `A0` = `struct MemHeader*`, `D0` = requested
/// byte size. `D0` = allocated block, or `0` (`NULL`) if nothing big
/// enough was free. A `byteSize` of `0` always returns `NULL`
/// (matches real `Allocate`'s own documented behavior, traced against
/// AROS's `rom/exec/allocate.c`).
fn allocate_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let free_list = ctx.cpu.address_register(AddressRegister(0));
    let requested = ctx.cpu.data_register(DataRegister(0));

    if requested == 0 {
        ctx.cpu.set_data_register(DataRegister(0), 0);
        return Ok(());
    }
    let size = round_up_to_memchunk(requested);

    if ctx.mem.read_u32(free_list + MH_FREE) < size {
        ctx.cpu.set_data_register(DataRegister(0), 0);
        return Ok(());
    }

    let mut prev: u32 = 0;
    let mut cur = ctx.mem.read_u32(free_list + MH_FIRST);
    while cur != 0 {
        let bytes = ctx.mem.read_u32(cur + MC_BYTES);
        if bytes >= size {
            let next = ctx.mem.read_u32(cur + MC_NEXT);
            let remaining = bytes - size;
            let link_target = if remaining == 0 {
                // Exact fit: the whole chunk is consumed, unlink it.
                next
            } else {
                // Take the front of the chunk, shrink what's left and
                // re-link the chain through it.
                let leftover = cur + size;
                // Exempt *before* writing: the leftover's header can
                // land anywhere inside the chunk's old body, which an
                // earlier Deallocate may have poisoned wholesale, and
                // a checked write to an already-`Unaddressable` byte
                // is recorded as a violation rather than healed (see
                // `crate::sanitize`'s `check_write_byte`) -- so doing
                // this after the writes below would itself be the
                // false positive. See the module doc's "Sanitizer
                // interaction" section.
                exempt_chunk_header(ctx, leftover);
                ctx.mem.write_u32(leftover + MC_NEXT, next);
                ctx.mem.write_u32(leftover + MC_BYTES, remaining);
                leftover
            };
            if prev == 0 {
                ctx.mem.write_u32(free_list + MH_FIRST, link_target);
            } else {
                ctx.mem.write_u32(prev + MC_NEXT, link_target);
            }

            let free = ctx.mem.read_u32(free_list + MH_FREE);
            ctx.mem.write_u32(free_list + MH_FREE, free - size);
            // After every chain write above, per `poison_allocated_chunk`'s
            // doc -- marks the whole returned range (including its own
            // former header bytes, whatever state they were in) Uninit.
            poison_allocated_chunk(ctx, cur, size);
            ctx.cpu.set_data_register(DataRegister(0), cur);
            return Ok(());
        }
        prev = cur;
        cur = ctx.mem.read_u32(cur + MC_NEXT);
    }

    // Walked the whole chain without finding a big-enough chunk (can
    // happen even though mh_Free >= size: enough free bytes exist in
    // total, but split across chunks too small individually --
    // fragmentation real Allocate() reports the same way).
    ctx.cpu.set_data_register(DataRegister(0), 0);
    Ok(())
}

/// `Deallocate` (LVO -192): `A0` = `struct MemHeader*`, `A1` =
/// `memoryBlock`, `D0` = byte size (must match the size originally
/// requested from [`allocate_handler`], real `Deallocate`'s own
/// documented contract -- trusted here exactly like `crate::execmem`'s
/// `FreeMem` trusts its own `byteSize` argument). No return value. A
/// `NULL` `memoryBlock` or zero `byteSize` is treated as a no-op,
/// matching this runtime's established convention for the free half of
/// every other alloc/free pair (`FreeMem`/`FreeVec`, see
/// `crate::execmem`'s module docs).
fn deallocate_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let free_list = ctx.cpu.address_register(AddressRegister(0));
    let block = ctx.cpu.address_register(AddressRegister(1));
    let requested = ctx.cpu.data_register(DataRegister(0));

    if block == 0 || requested == 0 {
        return Ok(());
    }
    let size = round_up_to_memchunk(requested);

    // Walk to the address-ordered insertion point.
    let mut prev: u32 = 0;
    let mut cur = ctx.mem.read_u32(free_list + MH_FIRST);
    while cur != 0 && cur < block {
        prev = cur;
        cur = ctx.mem.read_u32(cur + MC_NEXT);
    }

    // Try merging into the predecessor first (its end address exactly
    // meets the freed block's start).
    let (node, mut node_bytes) = if prev != 0 && prev + ctx.mem.read_u32(prev + MC_BYTES) == block {
        let merged = ctx.mem.read_u32(prev + MC_BYTES) + size;
        ctx.mem.write_u32(prev + MC_BYTES, merged);
        (prev, merged)
    } else {
        // No merge: link the freed block in as its own new chunk.
        ctx.mem.write_u32(block + MC_NEXT, cur);
        ctx.mem.write_u32(block + MC_BYTES, size);
        if prev == 0 {
            ctx.mem.write_u32(free_list + MH_FIRST, block);
        } else {
            ctx.mem.write_u32(prev + MC_NEXT, block);
        }
        (block, size)
    };

    // Try merging with the successor (its start address exactly meets
    // this node's end address) -- covers both the fresh-node and the
    // merged-into-predecessor cases, so up to three adjacent chunks
    // (prev, freed block, next) can collapse into one.
    if cur != 0 && node + node_bytes == cur {
        let cur_bytes = ctx.mem.read_u32(cur + MC_BYTES);
        let cur_next = ctx.mem.read_u32(cur + MC_NEXT);
        node_bytes += cur_bytes;
        ctx.mem.write_u32(node + MC_BYTES, node_bytes);
        ctx.mem.write_u32(node + MC_NEXT, cur_next);
    }

    let free = ctx.mem.read_u32(free_list + MH_FREE);
    ctx.mem.write_u32(free_list + MH_FREE, free + size);

    // Issue #83 Tier 1: poison exactly the extent the caller just
    // handed back, after all the relinking above (never before -- a
    // write to an already-poisoned byte is recorded as a violation
    // against itself, not healed, see `crate::sanitize`'s
    // `check_write_byte`). `node == block` means the freed block did
    // *not* merge into its predecessor and so remains the live chain
    // node itself (whether or not it also absorbed a successor) --
    // its own 8-byte header must stay valid, or the very next
    // Allocate/Deallocate call that walks past it would trip over its
    // own bookkeeping. See the module doc's "Sanitizer interaction"
    // section for the full reasoning.
    poison_freed_chunk(ctx, block, size);
    if node == block {
        exempt_chunk_header(ctx, block);
    }
    Ok(())
}

/// Registers this module's `exec.library` raw-chunk-allocator handlers,
/// looked up by name through [`EXEC_LVOS`], following
/// [`crate::execmem::register_execmem_handlers`]'s registration
/// pattern. Called unconditionally from
/// [`crate::dispatch::Runtime::new`].
pub fn register_execchunk_handlers<C: Cpu + 'static>(
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
    reg!("Allocate", allocate_handler::<C>);
    reg!("Deallocate", deallocate_handler::<C>);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{Runtime, StartConfig};
    use crate::memory::FlatMemory;

    fn load_words<M: AddressSpace>(mem: &mut M, addr: u32, words: &[u16]) {
        let mut offset = addr;
        for &w in words {
            mem.write_u16(offset, w);
            offset += 2;
        }
    }

    fn move_imm_to_a(n: u16) -> u16 {
        0x207C | (n << 9)
    }
    fn move_imm_to_d(n: u16) -> u16 {
        0x203C | (n << 9)
    }
    fn move_d0_to_a(n: u16) -> u16 {
        0x2040 | (n << 9)
    }
    fn move_d0_to_d(n: u16) -> u16 {
        0x2000 | (n << 9)
    }
    fn jsr_disp16_a6(disp: i32) -> [u16; 2] {
        [0x4EAE, disp as u16]
    }
    /// `move.l (d16,An),Dm` -- reads the long at `An + disp` into `Dm`,
    /// for exercising a guest read at a specific offset from a pointer
    /// without needing a separate address-arithmetic instruction.
    fn move_disp_a_to_d(src_a: u16, disp: u16, dst_d: u16) -> [u16; 2] {
        [0x2028 | (dst_d << 9) | src_a, disp]
    }
    const RTS: u16 = 0x4E75;

    fn movea_exec_base_to_a6() -> [u16; 3] {
        [
            move_imm_to_a(6),
            (EXEC_LIBRARY_BASE >> 16) as u16,
            EXEC_LIBRARY_BASE as u16,
        ]
    }

    /// A scratch `MemHeader` address, and the pool memory right after
    /// it -- both comfortably inside the flat memory these tests
    /// allocate, well clear of the trap table/program area.
    const MH_ADDR: u32 = 0x1_0000;
    const POOL_ADDR: u32 = MH_ADDR + MEMHEADER_SIZE;
    const POOL_SIZE: u32 = 256;

    /// Hand-builds a private pool exactly like `Allocate`'s own
    /// documented `EXAMPLE`: one `MemHeader` whose `mh_First` is a
    /// single `MemChunk` spanning the whole pool.
    fn init_pool<M: AddressSpace>(mem: &mut M) {
        mem.write_u32(MH_ADDR + MH_FIRST, POOL_ADDR);
        mem.write_u32(MH_ADDR + MH_FREE, POOL_SIZE);
        mem.write_u32(POOL_ADDR + MC_NEXT, 0);
        mem.write_u32(POOL_ADDR + MC_BYTES, POOL_SIZE);
    }

    /// As [`init_pool`], but starts with an empty free list (every
    /// byte "allocated") -- for tests that hand-carve the pool into
    /// pieces themselves rather than going through `Allocate`.
    fn init_empty_pool<M: AddressSpace>(mem: &mut M) {
        mem.write_u32(MH_ADDR + MH_FIRST, 0);
        mem.write_u32(MH_ADDR + MH_FREE, 0);
    }

    /// Builds a runtime with `words` as the program and the pool
    /// region already initialized via `pool_init`, before the program
    /// (which typically calls `Allocate`/`Deallocate` against it)
    /// ever runs.
    fn runtime_with_program_and_pool(
        words: &[u16],
        pool_init: impl FnOnce(&mut FlatMemory),
    ) -> Runtime<M68kCpu> {
        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, words);
        pool_init(&mut mem);
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

    #[test]
    fn allocate_exact_fit_unlinks_the_only_chunk() {
        let mut words = movea_exec_base_to_a6().to_vec();
        words.push(move_imm_to_a(0));
        words.push((MH_ADDR >> 16) as u16);
        words.push(MH_ADDR as u16);
        words.push(move_imm_to_d(0));
        words.push(0);
        words.push(POOL_SIZE as u16); // D0 = POOL_SIZE (whole pool)
        words.extend_from_slice(&jsr_disp16_a6(-186)); // Allocate
        words.push(RTS);

        let mut rt = runtime_with_program_and_pool(&words, init_pool);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code as u32, POOL_ADDR, "should hand back the whole pool");
        assert_eq!(
            rt.memory().read_u32(MH_ADDR + MH_FIRST),
            0,
            "list now empty"
        );
        assert_eq!(rt.memory().read_u32(MH_ADDR + MH_FREE), 0);
    }

    #[test]
    fn allocate_partial_fit_shrinks_the_chunk_from_the_front() {
        let mut words = movea_exec_base_to_a6().to_vec();
        words.push(move_imm_to_a(0));
        words.push((MH_ADDR >> 16) as u16);
        words.push(MH_ADDR as u16);
        words.push(move_imm_to_d(0));
        words.push(0);
        words.push(64); // D0 = 64, less than the 256-byte pool
        words.extend_from_slice(&jsr_disp16_a6(-186)); // Allocate
        words.push(RTS);

        let mut rt = runtime_with_program_and_pool(&words, init_pool);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code as u32, POOL_ADDR, "allocated from the front");

        let mem = rt.memory();
        let new_first = mem.read_u32(MH_ADDR + MH_FIRST);
        assert_eq!(new_first, POOL_ADDR + 64, "leftover chunk starts after it");
        assert_eq!(mem.read_u32(new_first + MC_BYTES), POOL_SIZE - 64);
        assert_eq!(mem.read_u32(new_first + MC_NEXT), 0);
        assert_eq!(mem.read_u32(MH_ADDR + MH_FREE), POOL_SIZE - 64);
    }

    #[test]
    fn allocate_when_nothing_fits_returns_null() {
        let mut words = movea_exec_base_to_a6().to_vec();
        words.push(move_imm_to_a(0));
        words.push((MH_ADDR >> 16) as u16);
        words.push(MH_ADDR as u16);
        words.push(move_imm_to_d(0));
        words.push(0);
        words.push((POOL_SIZE + 8) as u16); // more than the whole pool
        words.extend_from_slice(&jsr_disp16_a6(-186)); // Allocate
        words.push(RTS);

        let mut rt = runtime_with_program_and_pool(&words, init_pool);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 0);
        assert_eq!(
            rt.memory().read_u32(MH_ADDR + MH_FREE),
            POOL_SIZE,
            "a failed Allocate must not touch mh_Free"
        );
    }

    #[test]
    fn allocate_zero_bytes_returns_null_without_touching_the_pool() {
        let mut words = movea_exec_base_to_a6().to_vec();
        words.push(move_imm_to_a(0));
        words.push((MH_ADDR >> 16) as u16);
        words.push(MH_ADDR as u16);
        words.push(move_imm_to_d(0));
        words.push(0);
        words.push(0); // D0 = 0
        words.extend_from_slice(&jsr_disp16_a6(-186)); // Allocate
        words.push(RTS);

        let mut rt = runtime_with_program_and_pool(&words, init_pool);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 0);
        assert_eq!(rt.memory().read_u32(MH_ADDR + MH_FREE), POOL_SIZE);
    }

    #[test]
    fn end_to_end_allocate_then_deallocate_fully_coalesces_back() {
        // Allocate 64 bytes, then Deallocate that exact block/size --
        // the pool should end up byte-for-byte back to its initial
        // single-chunk state.
        let mut words = movea_exec_base_to_a6().to_vec();
        words.push(move_imm_to_a(0));
        words.push((MH_ADDR >> 16) as u16);
        words.push(MH_ADDR as u16);
        words.push(move_imm_to_d(0));
        words.push(0);
        words.push(64);
        words.extend_from_slice(&jsr_disp16_a6(-186)); // Allocate -> D0
        words.push(move_d0_to_a(1)); // A1 = allocated block
        words.push(move_imm_to_a(0));
        words.push((MH_ADDR >> 16) as u16);
        words.push(MH_ADDR as u16);
        words.push(move_imm_to_d(0));
        words.push(0);
        words.push(64);
        words.extend_from_slice(&jsr_disp16_a6(-192)); // Deallocate
        words.push(RTS);

        let mut rt = runtime_with_program_and_pool(&words, init_pool);
        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed");

        let mem = rt.memory();
        assert_eq!(mem.read_u32(MH_ADDR + MH_FIRST), POOL_ADDR);
        assert_eq!(mem.read_u32(POOL_ADDR + MC_BYTES), POOL_SIZE);
        assert_eq!(mem.read_u32(POOL_ADDR + MC_NEXT), 0);
        assert_eq!(mem.read_u32(MH_ADDR + MH_FREE), POOL_SIZE);
    }

    #[test]
    fn deallocate_merges_a_middle_hole_with_both_neighbors() {
        // Carve the pool into three consecutive 64-byte blocks by hand
        // (bypassing Allocate), free the middle one, then the two
        // outer ones -- exercising merge-with-prev, merge-with-next,
        // and the "both at once" triple-merge path.
        let mut words = movea_exec_base_to_a6().to_vec();
        // Deallocate(mh, POOL_ADDR + 64, 64) -- the middle block.
        words.push(move_imm_to_a(0));
        words.push((MH_ADDR >> 16) as u16);
        words.push(MH_ADDR as u16);
        words.push(move_imm_to_a(1));
        words.push(((POOL_ADDR + 64) >> 16) as u16);
        words.push((POOL_ADDR + 64) as u16);
        words.push(move_imm_to_d(0));
        words.push(0);
        words.push(64);
        words.extend_from_slice(&jsr_disp16_a6(-192)); // Deallocate
        // Deallocate(mh, POOL_ADDR, 64) -- the first block, merges
        // forward into the just-freed middle one.
        words.push(move_imm_to_a(0));
        words.push((MH_ADDR >> 16) as u16);
        words.push(MH_ADDR as u16);
        words.push(move_imm_to_a(1));
        words.push((POOL_ADDR >> 16) as u16);
        words.push(POOL_ADDR as u16);
        words.push(move_imm_to_d(0));
        words.push(0);
        words.push(64);
        words.extend_from_slice(&jsr_disp16_a6(-192)); // Deallocate
        // Deallocate(mh, POOL_ADDR + 128, POOL_SIZE - 128) -- the
        // last block, merges backward into everything freed so far.
        words.push(move_imm_to_a(0));
        words.push((MH_ADDR >> 16) as u16);
        words.push(MH_ADDR as u16);
        words.push(move_imm_to_a(1));
        words.push(((POOL_ADDR + 128) >> 16) as u16);
        words.push((POOL_ADDR + 128) as u16);
        words.push(move_imm_to_d(0));
        words.push(0);
        words.push((POOL_SIZE - 128) as u16);
        words.extend_from_slice(&jsr_disp16_a6(-192)); // Deallocate
        words.push(move_d0_to_d(0)); // no-op, just to end on RTS cleanly
        words.push(RTS);

        let mut rt = runtime_with_program_and_pool(&words, init_empty_pool);
        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed");

        let mem = rt.memory();
        assert_eq!(
            mem.read_u32(MH_ADDR + MH_FIRST),
            POOL_ADDR,
            "all three blocks fully coalesced back into one chunk"
        );
        assert_eq!(mem.read_u32(POOL_ADDR + MC_BYTES), POOL_SIZE);
        assert_eq!(mem.read_u32(POOL_ADDR + MC_NEXT), 0);
        assert_eq!(mem.read_u32(MH_ADDR + MH_FREE), POOL_SIZE);
    }

    // -- Issue #83 Tier 1: sanitizer shadow-map marking --------------

    /// Builds a program that calls `Allocate(MH_ADDR, 64)`, leaving the
    /// returned block in `D0`. Shared by several of the tests below.
    fn allocate_64_words() -> Vec<u16> {
        let mut words = movea_exec_base_to_a6().to_vec();
        words.push(move_imm_to_a(0));
        words.push((MH_ADDR >> 16) as u16);
        words.push(MH_ADDR as u16);
        words.push(move_imm_to_d(0));
        words.push(0);
        words.push(64);
        words.extend_from_slice(&jsr_disp16_a6(-186)); // Allocate
        words
    }

    #[test]
    fn allocate_marks_the_returned_block_uninit_when_sanitized() {
        let words = [allocate_64_words(), vec![RTS]].concat();

        let mut rt = runtime_with_program_and_pool(&words, init_pool);
        rt.memory_mut().enable_sanitizer();
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        let addr = code as u32;
        assert_eq!(addr, POOL_ADDR);

        let shadow = rt.memory().shadow().expect("sanitizer was enabled above");
        for i in 0..64u32 {
            assert_eq!(
                shadow.state(addr + i),
                crate::sanitize::ShadowState::Uninit,
                "byte {i} of a fresh Allocate'd block should be Uninit, not valid \
                 or poisoned -- real Allocate never clears its blocks"
            );
        }
    }

    #[test]
    fn allocate_does_not_touch_the_shadow_map_when_sanitize_is_off() {
        // `ctx.mem.shadow_mut()` must be a no-op when `--sanitize` was
        // never turned on -- same guarantee `execmem`'s analogous
        // helpers give.
        let words = [allocate_64_words(), vec![RTS]].concat();

        let mut rt = runtime_with_program_and_pool(&words, init_pool);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code as u32, POOL_ADDR);
        assert!(rt.memory().shadow().is_none());
    }

    #[test]
    fn deallocate_marks_the_freed_blocks_body_unaddressable_as_freed() {
        // The returned block's own header (its first 8 bytes) is
        // deliberately exempted from this -- see the module doc's
        // "Sanitizer interaction" section and the dedicated test below
        // -- so this checks a byte well inside the body instead.
        let mut words = allocate_64_words();
        words.push(move_d0_to_a(1)); // A1 = the allocated block
        words.push(move_imm_to_a(0));
        words.push((MH_ADDR >> 16) as u16);
        words.push(MH_ADDR as u16);
        words.push(move_imm_to_d(0));
        words.push(0);
        words.push(64);
        words.extend_from_slice(&jsr_disp16_a6(-192)); // Deallocate
        words.push(RTS);

        let mut rt = runtime_with_program_and_pool(&words, init_pool);
        rt.memory_mut().enable_sanitizer();
        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed");

        let shadow = rt.memory().shadow().expect("sanitizer was enabled above");
        assert_eq!(
            shadow.state(POOL_ADDR + 32),
            crate::sanitize::ShadowState::Unaddressable
        );
        assert_eq!(
            shadow.poison_reason(POOL_ADDR + 32),
            Some(crate::sanitize::PoisonReason::Freed)
        );
    }

    #[test]
    fn deallocate_leaves_a_standalone_chunks_own_header_valid() {
        // The documented Tier 1 limitation: a freed block that remains
        // (or becomes) a live free-list node of its own -- i.e. it did
        // not get absorbed into its predecessor -- must keep its own
        // 8-byte mc_Next/mc_Bytes header shadow-Valid, or the very next
        // Allocate/Deallocate call that walks past it would trip a
        // violation against the allocator's own bookkeeping. See the
        // module doc's "Sanitizer interaction" section.
        let mut words = allocate_64_words();
        words.push(move_d0_to_a(1));
        words.push(move_imm_to_a(0));
        words.push((MH_ADDR >> 16) as u16);
        words.push(MH_ADDR as u16);
        words.push(move_imm_to_d(0));
        words.push(0);
        words.push(64);
        words.extend_from_slice(&jsr_disp16_a6(-192)); // Deallocate
        words.push(RTS);

        let mut rt = runtime_with_program_and_pool(&words, init_pool);
        rt.memory_mut().enable_sanitizer();
        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed");

        let shadow = rt.memory().shadow().expect("sanitizer was enabled above");
        assert_eq!(
            shadow.state(POOL_ADDR),
            crate::sanitize::ShadowState::Valid,
            "the freed block's own header must stay valid: it remains live \
             free-list structure"
        );
        assert_eq!(
            shadow.violation_count(),
            0,
            "the allocator's own bookkeeping writes must never self-trigger \
             a violation"
        );
    }

    #[test]
    fn a_real_use_after_free_through_allocate_is_reported() {
        // The actual repro issue #83 is about: a C `malloc` (which is
        // `AllocVec` + `Allocate` under libnix/newlib) use-after-free
        // must be caught, exactly like the equivalent `AllocMem`-based
        // one already is (see `execmem`'s
        // `a_genuine_use_after_free_is_still_reported`). Reads 4 bytes
        // at offset 16 into the block -- inside the body, past the
        // 8-byte header this module deliberately leaves unpoisoned.
        let mut words = allocate_64_words();
        words.push(move_d0_to_a(1)); // A1 = the block (kept across the free)
        words.push(move_imm_to_a(0));
        words.push((MH_ADDR >> 16) as u16);
        words.push(MH_ADDR as u16);
        words.push(move_imm_to_d(0));
        words.push(0);
        words.push(64);
        words.extend_from_slice(&jsr_disp16_a6(-192)); // Deallocate
        words.extend_from_slice(&move_disp_a_to_d(1, 16, 2)); // UAF read
        words.push(RTS);

        let mut rt = runtime_with_program_and_pool(&words, init_pool);
        rt.memory_mut().enable_sanitizer();
        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed");

        let shadow = rt.memory().shadow().expect("sanitizer was enabled above");
        assert_eq!(
            shadow.violation_count(),
            1,
            "reading through a pointer whose block was Deallocate'd must be reported"
        );
        let violation = &shadow.violations()[0];
        assert_eq!(violation.kind, crate::sanitize::ViolationKind::InvalidRead);
        assert_eq!(violation.reason, Some(crate::sanitize::PoisonReason::Freed));
    }

    #[test]
    fn an_uninitialized_read_through_allocate_is_reported_when_enabled() {
        // Mirrors `execmem`'s uninit-read coverage: only observable
        // once `--sanitize-uninit` (`ShadowMap::report_uninit`) is also
        // turned on, which plain `--sanitize` does not do by default.
        let mut words = allocate_64_words();
        words.push(move_d0_to_a(1)); // A1 = the block
        words.extend_from_slice(&move_disp_a_to_d(1, 16, 2)); // read before any write
        words.push(RTS);

        let mut rt = runtime_with_program_and_pool(&words, init_pool);
        rt.memory_mut().enable_sanitizer();
        rt.memory_mut().shadow_mut().unwrap().report_uninit = true;
        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed");

        let shadow = rt.memory().shadow().expect("sanitizer was enabled above");
        assert_eq!(
            shadow.violation_count(),
            1,
            "reading a freshly Allocate'd block before writing it must be reported \
             once uninitialized-read reporting is on"
        );
        let violation = &shadow.violations()[0];
        assert_eq!(violation.kind, crate::sanitize::ViolationKind::UninitRead);
    }

    #[test]
    fn malloc_pattern_allocate_free_then_reallocate_smaller_causes_no_false_positive() {
        // The false-positive this Tier 1 design exists to avoid:
        // Allocate(128) -- Deallocate(128) -- Allocate(64) reclaims the
        // front of the very chunk Deallocate just poisoned. Without
        // the header exemptions in `allocate_handler`/
        // `deallocate_handler`, the second Allocate's own chain walk
        // and leftover-chunk split would each trip a violation against
        // the allocator's own bookkeeping, not a guest bug.
        let mut words = movea_exec_base_to_a6().to_vec();
        words.push(move_imm_to_a(0));
        words.push((MH_ADDR >> 16) as u16);
        words.push(MH_ADDR as u16);
        words.push(move_imm_to_d(0));
        words.push(0);
        words.push(128);
        words.extend_from_slice(&jsr_disp16_a6(-186)); // Allocate(128)
        words.push(move_d0_to_a(1));
        words.push(move_imm_to_a(0));
        words.push((MH_ADDR >> 16) as u16);
        words.push(MH_ADDR as u16);
        words.push(move_imm_to_d(0));
        words.push(0);
        words.push(128);
        words.extend_from_slice(&jsr_disp16_a6(-192)); // Deallocate(128)
        words.push(move_imm_to_a(0));
        words.push((MH_ADDR >> 16) as u16);
        words.push(MH_ADDR as u16);
        words.push(move_imm_to_d(0));
        words.push(0);
        words.push(64);
        words.extend_from_slice(&jsr_disp16_a6(-186)); // Allocate(64) -- reuses it
        words.push(RTS);

        let mut rt = runtime_with_program_and_pool(&words, init_pool);
        rt.memory_mut().enable_sanitizer();
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code as u32, POOL_ADDR, "the reallocation should succeed");

        let shadow = rt.memory().shadow().expect("sanitizer was enabled above");
        assert_eq!(
            shadow.violation_count(),
            0,
            "an ordinary alloc/free/realloc-smaller cycle must never self-trigger \
             a violation against the allocator's own free-list bookkeeping"
        );
        assert_eq!(
            shadow.state(POOL_ADDR),
            crate::sanitize::ShadowState::Uninit,
            "the reallocated range must read back as freshly Uninit"
        );
    }
}
