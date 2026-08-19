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

use crate::cpu::{AddressRegister, Cpu, DataRegister};
use crate::dispatch::{DispatchError, EXEC_LIBRARY_BASE, HandlerContext, LibraryTable};
use crate::lvos::exec::EXEC_LVOS;
use crate::memory::AddressSpace;

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
}
