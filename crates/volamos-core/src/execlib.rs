//! `exec.library`'s `InitResident`/`MakeLibrary` mechanics: scanning a
//! [`crate::dosseg::build_seglist`]-loaded seglist for a `struct
//! Resident` (romtag), reading the `RTF_AUTOINIT` init table it points
//! to, decoding both real vector-table encodings, and the host-side
//! equivalent of `MakeLibrary` itself (jump-table + `struct Library`
//! header construction on the guest heap).
//!
//! This is Phase L1 of `library-device-loading-plan.md`: **pure
//! mechanics, ships inert** -- nothing in this module runs guest code or
//! is wired into `OpenLibrary` yet (that's L2's trampoline primitive and
//! L3's state machine). Every function here is a plain host-side
//! transformation over already-relocated guest memory.
//!
//! # Sources
//!
//! NDK 3.2 autodocs (`exec.doc`: `InitResident`, `MakeLibrary`), NDK
//! includes (`exec/resident.i`, `exec/libraries.i`, `exec/nodes.i`), RKRM
//! Libraries ch. 18 ("Exec Libraries"), and direct byte-level inspection
//! of the real `scspill.library` binary (`~/amiga/sasc/libs/`) --
//! `library-device-loading-plan.md` §1 records the exact byte offsets and
//! ground truth this module is built from; see that plan for the fuller
//! citations. Every constant/offset below is cited at its own
//! declaration rather than only in this module preamble.
//!
//! # `struct Resident` scanning
//!
//! [`find_resident`] walks a loaded seglist's segments (via
//! [`crate::dosseg`]'s own BPTR-chain framing -- [`crate::dosseg::
//! SEG_HEADER_SIZE`]/[`crate::dosseg::NEXT_SEG_OFFSET`] are `pub(crate)`
//! for exactly this reuse) looking for `RTC_MATCHWORD` (`$4AFC`) at an
//! even address whose following longword (`RT_MATCHTAG`) points back at
//! the matchword's own address -- the same self-referential check real
//! `InitResident` uses to distinguish a genuine romtag from an
//! incidental `$4AFC` occurring in code or data. Real disk libraries
//! conventionally place a one-instruction "safety net" (`MOVEQ #-1,D0;
//! RTS` per RKRM, though `scspill.library` actually uses `MOVEQ #0,D0;
//! RTS` -- ground truth, not guessed) immediately before the Resident, so
//! that a `LoadSeg`ed-but-never-`InitResident`ed library jumped straight
//! into by mistake at least returns cleanly instead of executing romtag
//! bytes as code. This module does not assert on that opcode -- it's a
//! convention, not something `InitResident` (or this scan) depends on --
//! it just scans every even address of every segment's payload.
//!
//! # `MakeLibrary`
//!
//! [`make_library`] reimplements real exec's `MakeLibrary`: allocate
//! `negsize + dSize` off the guest heap (`negsize` = `numVectors *
//! LIB_VECTSIZE` rounded up to a longword multiple -- V36+ behavior, so
//! the base lands longword-aligned), lay down one 6-byte `JMP abs.l`
//! (`$4EF9` + a 32-bit absolute target) per vector walking downward from
//! the base, zero the positive-offset data area, and fill in the `struct
//! Library` header's `Node`/version/size fields from the `struct
//! Resident` that drove the load. `InitStruct` (a non-`NULL` `structure`
//! argument) is out of scope for L1 -- every on-hand corpus target has
//! `structure == NULL` (plan §1.5) -- so [`make_library`] fails loudly,
//! naming the library, rather than silently skipping it; implementing
//! `InitStruct`'s byte-coded interpreter (`exec/initializers.i`) is L6.
//!
//! [`GuestHeap::alloc`] already 4-byte-aligns every allocation it hands
//! back (see that module's doc), and `negsize` is itself rounded up to a
//! multiple of 4 by [`make_library`] before it's added to the allocation
//! start to compute `base` -- so `base` (allocation-start + a multiple of
//! 4) is unconditionally longword-aligned too, satisfying V36+
//! `MakeLibrary`'s own alignment guarantee for free, with no extra
//! rounding logic needed at the `base` end.

use crate::dosseg::{NEXT_SEG_OFFSET, SEG_HEADER_SIZE};
use crate::guestmem::{GuestHeap, GuestHeapError, addr_from_bptr};
use crate::memory::AddressSpace;

/// `RTC_MATCHWORD` (`exec/resident.i`): the fixed value every real
/// `struct Resident` starts with, and what [`find_resident`] scans for.
pub const RTC_MATCHWORD: u16 = 0x4AFC;

/// `RTF_AUTOINIT` (`exec/resident.i`, `RT_FLAGS` bit): set on every
/// library found in this runtime's corpus so far (plan §1.5) -- the
/// `RT_INIT` field points at a four-longword `MakeLibrary` argument
/// table (see [`AutoInit`]) rather than being executable init code
/// directly.
pub const RTF_AUTOINIT: u8 = 0x80;

/// `NT_LIBRARY` (`exec/nodes.i`). Declared locally rather than reusing
/// `dispatch.rs`'s private copy of the same constant, per this phase's
/// brief -- refactoring `dispatch.rs` isn't in scope for an inert L1
/// module. Same value, same source.
pub const NT_LIBRARY: u8 = 9;
/// `NT_DEVICE` (`exec/nodes.i`). See [`NT_LIBRARY`]'s note on why this
/// isn't reused from `dispatch.rs`.
pub const NT_DEVICE: u8 = 3;

/// `sizeof(struct Resident)` (`exec/resident.i`): `RT_MATCHWORD` (2) +
/// `RT_MATCHTAG` (4) + `RT_ENDSKIP` (4) + `RT_FLAGS`/`RT_VERSION`/
/// `RT_TYPE`/`RT_PRI` (1 each, 4) + `RT_NAME` (4) + `RT_IDSTRING` (4) +
/// `RT_INIT` (4) = 26.
const RESIDENT_SIZE: u32 = 26;

/// `LIB_VECTSIZE` (`exec/libraries.i`): every jump-table entry
/// `MakeLibrary` builds is a 6-byte `JMP abs.l` instruction.
const LIB_VECTSIZE: u32 = 6;

/// The `JMP` opcode word for absolute-long addressing mode (M68000
/// Programmer's Reference Manual: `JMP` with effective-address mode/
/// register `111/001`, encoded as `0100 1110 1111 1001`). Followed by a
/// 4-byte absolute target, for a 6-byte instruction -- exactly
/// [`LIB_VECTSIZE`].
const JMP_ABS_L_OPCODE: u16 = 0x4EF9;

/// A cap on how many segments [`find_resident`] will walk before giving
/// up and returning `None`, so a corrupted or (adversarially)
/// self-looping `next_seg` chain can't hang this scan forever. Every
/// seglist this runtime itself builds ([`crate::dosseg::build_seglist`])
/// is acyclic and far shorter than this, so the cap is never hit on a
/// legitimate load.
const MAX_SEGLIST_SEGMENTS: u32 = 4096;

/// A cap on how many entries [`read_vectors`] will decode before giving
/// up loudly, so a vector table missing its terminator (`$FFFF`/`-1` for
/// the word-displacement form, `$FFFFFFFF` for the absolute-pointer
/// form) can't be read forever. Real libraries have at most a few dozen
/// vectors; this is generous headroom above that.
const MAX_VECTORS: usize = 256;

/// `sizeof(struct Library)` (`exec/libraries.h`): `Node` (14) +
/// `lib_Flags`/`lib_pad` (1 each, 2) + `lib_NegSize`/`lib_PosSize` (2
/// each, 4) + `lib_Version`/`lib_Revision` (2 each, 4) + `lib_IdString`
/// (4) + `lib_Sum` (4) + `lib_OpenCnt` (2) = 34. `MakeLibrary` rejects a
/// `dSize` smaller than this (there'd be no room for the header it's
/// about to write) -- see [`make_library`].
const LIB_STRUCT_SIZE: u32 = 34;

/// `struct Node`'s `ln_Type` byte offset within `struct Library`
/// (`ln_Succ`/`ln_Pred`, 4 bytes each, then `ln_Type`).
const LIB_NODE_TYPE_OFFSET: u32 = 8;
/// `struct Node`'s `ln_Name` (`APTR`) byte offset: `ln_Succ`/`ln_Pred`
/// (8) + `ln_Type`/`ln_Pri` (1 each, 2) = 10.
const LIB_NODE_NAME_OFFSET: u32 = 10;
/// `lib_Flags` byte offset: `Node` (14).
const LIB_FLAGS_OFFSET: u32 = 14;
/// `lib_NegSize` byte offset: `Node` (14) + `lib_Flags`/`lib_pad` (2) =
/// 16.
const LIB_NEGSIZE_OFFSET: u32 = 16;
/// `lib_PosSize` byte offset: immediately after `lib_NegSize` (18).
const LIB_POSSIZE_OFFSET: u32 = 18;
/// `lib_Version` byte offset: `lib_NegSize`/`lib_PosSize` (2 each) past
/// [`LIB_NEGSIZE_OFFSET`] = 20.
const LIB_VERSION_OFFSET: u32 = 20;
/// `lib_Revision` byte offset: immediately after `lib_Version` (22).
const LIB_REVISION_OFFSET: u32 = 22;
/// `lib_IdString` (`APTR`) byte offset: `lib_Version`/`lib_Revision` (2
/// each) past [`LIB_VERSION_OFFSET`] = 24.
const LIB_IDSTRING_OFFSET: u32 = 24;

/// A parsed `struct Resident` (`exec/resident.i`), read out of guest
/// memory by [`read_resident`]. Field names match the NDK's `RT_*`
/// tokens (minus the `RT_` prefix), lowercased.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resident {
    /// The guest address this Resident was read from (i.e. the address
    /// of `RT_MATCHWORD` itself -- what [`find_resident`] returns).
    pub addr: u32,
    /// `RT_MATCHWORD`: always [`RTC_MATCHWORD`] for anything
    /// [`find_resident`] would have found.
    pub match_word: u16,
    /// `RT_MATCHTAG`: always `addr` itself, by the same self-reference
    /// check.
    pub match_tag: u32,
    /// `RT_ENDSKIP`: unused by this runtime (see the plan's §1.2 note).
    pub end_skip: u32,
    /// `RT_FLAGS`: see [`RTF_AUTOINIT`].
    pub flags: u8,
    /// `RT_VERSION`: becomes `lib_Version` (see [`make_library`]).
    pub version: u8,
    /// `RT_TYPE`: [`NT_LIBRARY`] or [`NT_DEVICE`].
    pub node_type: u8,
    /// `RT_PRI`: a signed byte (`BYTE` in NDK terms).
    pub pri: i8,
    /// `RT_NAME`: a guest `APTR` to a C string (already relocated by
    /// [`crate::dosseg::build_seglist`]), e.g. `"scspill.library"`.
    pub name_ptr: u32,
    /// `RT_IDSTRING`: a guest `APTR` to a C string.
    pub id_string_ptr: u32,
    /// `RT_INIT`: for an [`RTF_AUTOINIT`] library, a guest `APTR` to the
    /// four-longword table [`read_autoinit`] reads.
    pub init_ptr: u32,
}

/// The `RTF_AUTOINIT` init table (`library-device-loading-plan.md` §1.3;
/// confirmed against the real `scspill.library`'s own `RT_INIT` table):
/// four consecutive guest longwords at [`Resident::init_ptr`], in this
/// exact order -- `dSize`, `vectors`, `structure`, `initFunc`. These are
/// exactly `MakeLibrary`'s own `dSize`/`vectors`/`structure`/`init`
/// arguments (just `initFunc` isn't consumed by [`make_library`] itself
/// -- calling it is L2/L3's trampoline's job, not this pure-mechanics
/// module's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoInit {
    /// Size in bytes of the library base's positive-offset data area;
    /// must be `>=` [`LIB_STRUCT_SIZE`] (see [`make_library`]).
    pub d_size: u32,
    /// Guest `APTR` to the vector table -- see [`read_vectors`] for the
    /// two encodings it may be in.
    pub vectors: u32,
    /// Guest `APTR` to an `InitStruct`-format data table, or `0`
    /// (`NULL`) to skip it. Non-`NULL` isn't implemented yet (see
    /// [`make_library`]).
    pub structure: u32,
    /// Guest `APTR` to the library's own init function, called (`D0` =
    /// libBase, `A0` = segList, `A6` = ExecBase) after `MakeLibrary`
    /// finishes -- not called by this module (L2/L3's job).
    pub init_func: u32,
}

/// The result of a successful [`make_library`] call: both the guest
/// heap's own allocation-start address (`negsize + dSize` bytes,
/// starting [`negsize`](make_library) bytes *before* `base`) and the
/// resulting library base. A later phase frees the allocation on a
/// failed open via `alloc_addr` (not `base` -- [`GuestHeap::free`]
/// requires the exact address [`GuestHeap::alloc`] returned, which is
/// the allocation start, not the base in the middle of it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MadeLibrary {
    /// The guest heap allocation's own start address -- pass this, not
    /// [`Self::base`], to [`GuestHeap::free`].
    pub alloc_addr: u32,
    /// The library base address: what a caller stores in `A6` and calls
    /// negative-offset vectors against.
    pub base: u32,
}

/// Errors [`make_library`] can report. Both non-heap variants are the
/// two structural-failure cases `library-device-loading-plan.md`'s L1
/// scope explicitly defers/rejects rather than mishandling silently --
/// see [`make_library`]'s doc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MakeLibraryError {
    /// `dSize` was smaller than [`LIB_STRUCT_SIZE`] -- there'd be no room
    /// for the `struct Library` header `MakeLibrary` is about to write.
    DataSizeTooSmall { d_size: u32 },
    /// The `structure` (`InitStruct` table) argument was non-`NULL`.
    /// L1 doesn't implement the `InitStruct` byte-code interpreter (see
    /// this module's doc) -- every on-hand corpus target has
    /// `structure == NULL`, so this is a loud, named failure rather than
    /// a silent skip.
    InitStructNotImplemented { library_name: String },
    /// The guest heap couldn't satisfy the `negsize + dSize` allocation.
    Heap(GuestHeapError),
}

impl std::fmt::Display for MakeLibraryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MakeLibraryError::DataSizeTooSmall { d_size } => write!(
                f,
                "MakeLibrary: dSize {d_size} is smaller than sizeof(struct Library) \
                 ({LIB_STRUCT_SIZE})"
            ),
            MakeLibraryError::InitStructNotImplemented { library_name } => write!(
                f,
                "MakeLibrary: library {library_name:?} has a non-NULL InitStruct table -- \
                 InitStruct interpretation isn't implemented yet (deferred to a later phase, \
                 see library-device-loading-plan.md L6)"
            ),
            MakeLibraryError::Heap(e) => write!(f, "MakeLibrary: {e}"),
        }
    }
}

impl std::error::Error for MakeLibraryError {}

/// Scans every segment of the seglist identified by `first_bptr` (the
/// same BPTR [`crate::dosseg::DosState::load_seg`] returns) for a
/// `struct Resident` -- see the module docs for the scan/validity check.
/// Returns the guest address of the first one found (the address of its
/// `RT_MATCHWORD`), scanning segments in load order and, within a
/// segment, in ascending address order, so the *first* valid romtag
/// found is returned (matching real `InitResident`'s own linear-scan
/// convention). `None` if no segment contains a valid romtag, or if the
/// BPTR chain doesn't terminate within [`MAX_SEGLIST_SEGMENTS`] (treated
/// as corrupt, not looped over forever).
pub fn find_resident(mem: &dyn AddressSpace, first_bptr: u32) -> Option<u32> {
    let mut bptr = first_bptr;
    let mut segments_visited = 0u32;

    while bptr != 0 {
        segments_visited += 1;
        if segments_visited > MAX_SEGLIST_SEGMENTS {
            return None;
        }

        // Per crate::dosseg's framing: `bptr` addresses the segment's
        // own `next_seg` field, which sits NEXT_SEG_OFFSET bytes into
        // the segment's allocation.
        let next_seg_field_addr = addr_from_bptr(bptr);
        let alloc_addr = next_seg_field_addr.wrapping_sub(NEXT_SEG_OFFSET);
        let seg_length = mem.read_u32(alloc_addr);
        let payload_addr = alloc_addr.wrapping_add(SEG_HEADER_SIZE);
        let payload_end = alloc_addr.wrapping_add(seg_length);

        let mut addr = payload_addr;
        while addr.wrapping_add(RESIDENT_SIZE) <= payload_end {
            if mem.read_u16(addr) == RTC_MATCHWORD && mem.read_u32(addr.wrapping_add(2)) == addr {
                return Some(addr);
            }
            addr = addr.wrapping_add(2);
        }

        bptr = mem.read_u32(next_seg_field_addr);
    }

    None
}

/// Reads the 26-byte `struct Resident` at `addr` (which [`find_resident`]
/// already validated as a genuine romtag) -- see [`Resident`]'s field
/// docs for each offset's source.
pub fn read_resident(mem: &dyn AddressSpace, addr: u32) -> Resident {
    Resident {
        addr,
        match_word: mem.read_u16(addr),
        match_tag: mem.read_u32(addr.wrapping_add(2)),
        end_skip: mem.read_u32(addr.wrapping_add(6)),
        flags: mem.read_u8(addr.wrapping_add(10)),
        version: mem.read_u8(addr.wrapping_add(11)),
        node_type: mem.read_u8(addr.wrapping_add(12)),
        pri: mem.read_u8(addr.wrapping_add(13)) as i8,
        name_ptr: mem.read_u32(addr.wrapping_add(14)),
        id_string_ptr: mem.read_u32(addr.wrapping_add(18)),
        init_ptr: mem.read_u32(addr.wrapping_add(22)),
    }
}

/// Reads the four-longword `RTF_AUTOINIT` table at `rt_init` (a
/// [`Resident::init_ptr`] value) -- see [`AutoInit`]'s doc for the field
/// order/source.
pub fn read_autoinit(mem: &dyn AddressSpace, rt_init: u32) -> AutoInit {
    AutoInit {
        d_size: mem.read_u32(rt_init),
        vectors: mem.read_u32(rt_init.wrapping_add(4)),
        structure: mem.read_u32(rt_init.wrapping_add(8)),
        init_func: mem.read_u32(rt_init.wrapping_add(12)),
    }
}

/// Decodes the vector table at `vectors_addr` (an [`AutoInit::vectors`]
/// value) into absolute guest addresses, in LVO order (index 0 = the
/// `Open` vector, LVO -6). Per `library-device-loading-plan.md` §1.3
/// item 2, real `MakeLibrary` accepts two encodings, disambiguated by
/// the first word at `vectors_addr`:
///
/// - **Word-displacement form** (common in Commodore ROM-lineage disk
///   libraries): if that first word is `$FFFF`, it's a format flag (not
///   itself a vector), followed by `UWORD` displacements relative to
///   `vectors_addr` itself, terminated by another `$FFFF` word.
/// - **Absolute-pointer form** (what `scspill.library` uses): otherwise,
///   an array of absolute 32-bit `APTR`s starting at `vectors_addr`,
///   terminated by `$FFFFFFFF`.
///
/// Returns `Err` (rather than looping forever) if more than
/// [`MAX_VECTORS`] entries are decoded without hitting a terminator --
/// see [`MAX_VECTORS`]'s doc.
pub fn read_vectors(mem: &dyn AddressSpace, vectors_addr: u32) -> Result<Vec<u32>, String> {
    let mut result = Vec::new();

    if mem.read_u16(vectors_addr) == 0xFFFF {
        let mut idx: u32 = 1; // entry 0 is the $FFFF format flag, not a vector.
        loop {
            if result.len() >= MAX_VECTORS {
                return Err(format!(
                    "vector table at {vectors_addr:#010x} (word-displacement form) exceeded \
                     {MAX_VECTORS} entries without a terminating $FFFF word -- treating as \
                     corrupt rather than scanning forever"
                ));
            }
            let word = mem.read_u16(vectors_addr.wrapping_add(idx * 2));
            if word == 0xFFFF {
                break;
            }
            let disp = word as i16 as i32;
            result.push(vectors_addr.wrapping_add(disp as u32));
            idx += 1;
        }
    } else {
        let mut idx: u32 = 0;
        loop {
            if result.len() >= MAX_VECTORS {
                return Err(format!(
                    "vector table at {vectors_addr:#010x} (absolute-pointer form) exceeded \
                     {MAX_VECTORS} entries without a terminating $FFFFFFFF longword -- \
                     treating as corrupt rather than scanning forever"
                ));
            }
            let entry = mem.read_u32(vectors_addr.wrapping_add(idx * 4));
            if entry == 0xFFFF_FFFF {
                break;
            }
            result.push(entry);
            idx += 1;
        }
    }

    Ok(result)
}

/// Rounds `value` up to the nearest multiple of 4. A small local
/// duplicate of [`crate::guestmem`]'s private `align_up` helper (that
/// one isn't `pub`) -- same one-line technique, used here for
/// `MakeLibrary`'s own `negsize` rounding (see the module doc's V36+
/// alignment note), a logically distinct rounding from `GuestHeap`'s
/// internal allocation rounding even though the arithmetic matches.
fn round_up_longword(value: u32) -> u32 {
    value.checked_add(3).map(|v| v & !3).unwrap_or(!3)
}

/// Host-side `MakeLibrary`: allocates `negsize + dSize` bytes off `heap`
/// (`negsize` = `vectors.len() * LIB_VECTSIZE` rounded up to a longword
/// multiple, per the module doc), writes one 6-byte `JMP abs.l` per
/// vector starting at `base - LIB_VECTSIZE` and walking downward (so
/// `vectors[0]` -- the `Open` vector, LVO -6 -- lands at `base - 6`,
/// `vectors[1]` at `base - 12`, ...), zeroes the `d_size`-byte
/// positive-offset data area, and fills in the `struct Library` header
/// fields sourced from `resident` (see the per-field offset constants
/// above this function).
///
/// `lib_Flags` is written as `0`, not `resident.flags`: `RTF_*` (romtag)
/// and `LIBF_*` (library) flag bytes are different namespaces with
/// different bit meanings (`LIBF_SUMMING`/`LIBF_CHANGED`/`LIBF_SUMUSED`/
/// `LIBF_DELEXP` describe runtime checksum/expunge state, nothing a
/// freshly-made library has yet) -- real `MakeLibrary` doesn't copy
/// `RTF_AUTOINIT` or any other romtag bit into `lib_Flags` either.
/// `lib_Revision` is likewise left `0`: the `struct Resident` has no
/// revision field at all (only `rt_Version`); a library's own
/// `initFunc`/`InitStruct` step is what would set a real revision, and
/// L1 doesn't run either.
///
/// Fails loudly (never silently skips or corrupts memory) if `d_size` is
/// too small to hold `struct Library`'s own header
/// ([`MakeLibraryError::DataSizeTooSmall`]), if `structure` is non-`NULL`
/// ([`MakeLibraryError::InitStructNotImplemented`] -- see the module
/// doc), or if the heap allocation itself fails
/// ([`MakeLibraryError::Heap`]).
pub fn make_library(
    mem: &mut dyn AddressSpace,
    heap: &mut GuestHeap,
    resident: &Resident,
    d_size: u32,
    structure: u32,
    vectors: &[u32],
    library_name: &str,
) -> Result<MadeLibrary, MakeLibraryError> {
    if structure != 0 {
        return Err(MakeLibraryError::InitStructNotImplemented {
            library_name: library_name.to_string(),
        });
    }
    if d_size < LIB_STRUCT_SIZE {
        return Err(MakeLibraryError::DataSizeTooSmall { d_size });
    }

    let negsize = round_up_longword(vectors.len() as u32 * LIB_VECTSIZE);
    let total = negsize.saturating_add(d_size);
    let alloc_addr = heap.alloc(total).map_err(MakeLibraryError::Heap)?;
    let base = alloc_addr.wrapping_add(negsize);

    for i in 0..d_size {
        mem.write_u8(base.wrapping_add(i), 0);
    }

    let mut vector_addr = base;
    for &target in vectors {
        vector_addr = vector_addr.wrapping_sub(LIB_VECTSIZE);
        mem.write_u16(vector_addr, JMP_ABS_L_OPCODE);
        mem.write_u32(vector_addr.wrapping_add(2), target);
    }

    mem.write_u8(base.wrapping_add(LIB_NODE_TYPE_OFFSET), resident.node_type);
    mem.write_u32(base.wrapping_add(LIB_NODE_NAME_OFFSET), resident.name_ptr);
    mem.write_u8(base.wrapping_add(LIB_FLAGS_OFFSET), 0);
    mem.write_u16(base.wrapping_add(LIB_NEGSIZE_OFFSET), negsize as u16);
    mem.write_u16(base.wrapping_add(LIB_POSSIZE_OFFSET), d_size as u16);
    mem.write_u16(
        base.wrapping_add(LIB_VERSION_OFFSET),
        resident.version as u16,
    );
    mem.write_u16(base.wrapping_add(LIB_REVISION_OFFSET), 0);
    mem.write_u32(
        base.wrapping_add(LIB_IDSTRING_OFFSET),
        resident.id_string_ptr,
    );

    Ok(MadeLibrary { alloc_addr, base })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dosseg::build_seglist;
    use crate::guestmem::{bptr_from_addr, read_c_string, write_c_string};
    use crate::loader;
    use crate::memory::FlatMemory;

    // --- test helpers: hand-build an already-relocated seglist ---
    //
    // Unlike dosseg.rs's own tests, these don't need loader::parse/
    // HUNK_RELOC32 fidelity -- find_resident/read_resident etc. only
    // care about final, already-relocated guest bytes, and every address
    // involved is chosen by the test itself before writing, so there's
    // nothing to relocate: just write the seglist framing directly.

    /// Writes a single-segment seglist (`seg_length`/`next_seg` header +
    /// `payload`) starting at `alloc_addr`, returning its `first_bptr`.
    fn write_single_segment(mem: &mut FlatMemory, alloc_addr: u32, payload: &[u8]) -> u32 {
        let seg_length = SEG_HEADER_SIZE + payload.len() as u32;
        mem.write_u32(alloc_addr, seg_length);
        mem.write_u32(alloc_addr + NEXT_SEG_OFFSET, 0); // last segment
        let payload_addr = alloc_addr + SEG_HEADER_SIZE;
        for (i, &b) in payload.iter().enumerate() {
            mem.write_u8(payload_addr + i as u32, b);
        }
        bptr_from_addr(alloc_addr + NEXT_SEG_OFFSET)
    }

    /// The non-flags/version/type fields [`write_resident`] fills in
    /// beyond the always-varying `addr`/`flags`/`version`/`node_type`
    /// arguments -- bundled into a struct purely to keep
    /// `write_resident`'s own argument count reasonable (clippy's
    /// `too_many_arguments`), not because these three are conceptually
    /// one unit any more than the others are.
    #[derive(Default, Clone, Copy)]
    struct ResidentPtrs {
        name_ptr: u32,
        id_string_ptr: u32,
        init_ptr: u32,
    }

    /// Writes a valid `struct Resident` at `addr`, self-referencing per
    /// the matchtag check, with the given flags/version/type/name/
    /// idstring/init pointers.
    fn write_resident(
        mem: &mut FlatMemory,
        addr: u32,
        flags: u8,
        version: u8,
        node_type: u8,
        ptrs: ResidentPtrs,
    ) {
        mem.write_u16(addr, RTC_MATCHWORD);
        mem.write_u32(addr + 2, addr); // RT_MATCHTAG: self-pointing
        mem.write_u32(addr + 6, 0); // RT_ENDSKIP: unused
        mem.write_u8(addr + 10, flags);
        mem.write_u8(addr + 11, version);
        mem.write_u8(addr + 12, node_type);
        mem.write_u8(addr + 13, 0); // RT_PRI
        mem.write_u32(addr + 14, ptrs.name_ptr);
        mem.write_u32(addr + 18, ptrs.id_string_ptr);
        mem.write_u32(addr + 22, ptrs.init_ptr);
    }

    // --- find_resident ---

    #[test]
    fn find_resident_locates_romtag_not_at_payload_start() {
        let mut mem = FlatMemory::new(0x1000);
        let alloc_addr = 0x100u32;
        let payload_addr = alloc_addr + SEG_HEADER_SIZE;
        // 8 bytes of filler (e.g. the safety-net instruction plus pad)
        // before the Resident actually starts.
        let resident_addr = payload_addr + 8;
        let payload = vec![0u8; 8 + RESIDENT_SIZE as usize];
        let first_bptr = write_single_segment(&mut mem, alloc_addr, &payload);
        write_resident(
            &mut mem,
            resident_addr,
            0x80,
            3,
            NT_LIBRARY,
            ResidentPtrs::default(),
        );

        assert_eq!(find_resident(&mem, first_bptr), Some(resident_addr));
    }

    #[test]
    fn find_resident_returns_none_when_absent() {
        let mut mem = FlatMemory::new(0x1000);
        let alloc_addr = 0x100u32;
        let payload = vec![0u8; 64];
        let first_bptr = write_single_segment(&mut mem, alloc_addr, &payload);
        assert_eq!(find_resident(&mem, first_bptr), None);
    }

    #[test]
    fn find_resident_skips_matchword_with_wrong_matchtag() {
        let mut mem = FlatMemory::new(0x1000);
        let alloc_addr = 0x100u32;
        let payload_addr = alloc_addr + SEG_HEADER_SIZE;
        let false_positive_addr = payload_addr + 4;
        let real_addr = payload_addr + 40;
        let payload = vec![0u8; 80];
        let first_bptr = write_single_segment(&mut mem, alloc_addr, &payload);

        // A $4AFC word whose following longword does NOT point back at
        // itself -- must be skipped, not mistaken for a romtag.
        mem.write_u16(false_positive_addr, RTC_MATCHWORD);
        mem.write_u32(false_positive_addr + 2, 0);

        write_resident(
            &mut mem,
            real_addr,
            0x80,
            1,
            NT_LIBRARY,
            ResidentPtrs::default(),
        );

        assert_eq!(find_resident(&mem, first_bptr), Some(real_addr));
    }

    #[test]
    fn find_resident_walks_into_a_later_segment() {
        let mut mem = FlatMemory::new(0x2000);
        let seg0_addr = 0x100u32;
        let seg0_payload = [0u8; 16];
        let seg0_len = SEG_HEADER_SIZE + seg0_payload.len() as u32;
        mem.write_u32(seg0_addr, seg0_len);
        let seg0_payload_addr = seg0_addr + SEG_HEADER_SIZE;
        for (i, &b) in seg0_payload.iter().enumerate() {
            mem.write_u8(seg0_payload_addr + i as u32, b);
        }

        let seg1_addr = 0x200u32;
        let resident_addr = seg1_addr + SEG_HEADER_SIZE + 4;
        let seg1_payload = vec![0u8; 40];
        let seg1_bptr = write_single_segment(&mut mem, seg1_addr, &seg1_payload);
        write_resident(
            &mut mem,
            resident_addr,
            0x80,
            2,
            NT_LIBRARY,
            ResidentPtrs::default(),
        );

        // Chain seg0 -> seg1.
        mem.write_u32(seg0_addr + NEXT_SEG_OFFSET, seg1_bptr);
        let first_bptr = bptr_from_addr(seg0_addr + NEXT_SEG_OFFSET);

        assert_eq!(find_resident(&mem, first_bptr), Some(resident_addr));
    }

    #[test]
    fn find_resident_of_zero_bptr_is_none() {
        let mem = FlatMemory::new(0x100);
        assert_eq!(find_resident(&mem, 0), None);
    }

    // --- read_resident / read_autoinit ---

    #[test]
    fn read_resident_reads_every_field() {
        let mut mem = FlatMemory::new(0x200);
        let addr = 0x40u32;
        write_resident(
            &mut mem,
            addr,
            0x81,
            6,
            NT_DEVICE,
            ResidentPtrs {
                name_ptr: 0x1000,
                id_string_ptr: 0x2000,
                init_ptr: 0x3000,
            },
        );
        let r = read_resident(&mem, addr);
        assert_eq!(r.addr, addr);
        assert_eq!(r.match_word, RTC_MATCHWORD);
        assert_eq!(r.match_tag, addr);
        assert_eq!(r.flags, 0x81);
        assert_eq!(r.version, 6);
        assert_eq!(r.node_type, NT_DEVICE);
        assert_eq!(r.name_ptr, 0x1000);
        assert_eq!(r.id_string_ptr, 0x2000);
        assert_eq!(r.init_ptr, 0x3000);
    }

    #[test]
    fn read_autoinit_reads_four_longwords_in_order() {
        let mut mem = FlatMemory::new(0x100);
        let rt_init = 0x10u32;
        mem.write_u32(rt_init, 0xEE); // dSize
        mem.write_u32(rt_init + 4, 0x50); // vectors
        mem.write_u32(rt_init + 8, 0); // structure
        mem.write_u32(rt_init + 12, 0x20); // initFunc
        let ai = read_autoinit(&mem, rt_init);
        assert_eq!(
            ai,
            AutoInit {
                d_size: 0xEE,
                vectors: 0x50,
                structure: 0,
                init_func: 0x20,
            }
        );
    }

    // --- read_vectors ---

    #[test]
    fn read_vectors_absolute_form() {
        let mut mem = FlatMemory::new(0x100);
        let addr = 0x10u32;
        mem.write_u32(addr, 0x1000);
        mem.write_u32(addr + 4, 0x2000);
        mem.write_u32(addr + 8, 0x3000);
        mem.write_u32(addr + 12, 0xFFFF_FFFF);
        let vectors = read_vectors(&mem, addr).unwrap();
        assert_eq!(vectors, vec![0x1000, 0x2000, 0x3000]);
    }

    #[test]
    fn read_vectors_word_displacement_form_matches_absolute_form() {
        let mut mem = FlatMemory::new(0x100);
        let addr = 0x40u32;
        // Same three targets as read_vectors_absolute_form, encoded as
        // displacements relative to `addr` itself.
        let targets = [addr + 0x30, addr - 0x10, addr + 0x1000];
        mem.write_u16(addr, 0xFFFF); // format flag
        mem.write_u16(addr + 2, 0x30i16 as u16);
        mem.write_u16(addr + 4, (-0x10i16) as u16);
        mem.write_u16(addr + 6, 0x1000i16 as u16);
        mem.write_u16(addr + 8, 0xFFFF); // terminator

        let vectors = read_vectors(&mem, addr).unwrap();
        assert_eq!(vectors, targets);
    }

    #[test]
    fn read_vectors_unterminated_table_errors_loudly() {
        let mut mem = FlatMemory::new(0x2000);
        let addr = 0x10u32;
        // Fill far more than MAX_VECTORS entries, none of them the
        // $FFFFFFFF terminator.
        for i in 0..(MAX_VECTORS as u32 + 10) {
            mem.write_u32(addr + i * 4, 0x1000 + i);
        }
        let err = read_vectors(&mem, addr).unwrap_err();
        assert!(err.contains("exceeded"));
    }

    // --- make_library ---

    fn dummy_resident(name_ptr: u32, id_string_ptr: u32) -> Resident {
        Resident {
            addr: 0,
            match_word: RTC_MATCHWORD,
            match_tag: 0,
            end_skip: 0,
            flags: RTF_AUTOINIT,
            version: 6,
            node_type: NT_LIBRARY,
            pri: 0,
            name_ptr,
            id_string_ptr,
            init_ptr: 0,
        }
    }

    #[test]
    fn make_library_writes_jump_table_walking_downward_from_base() {
        let mut mem = FlatMemory::new(0x4000);
        let mut heap = GuestHeap::new(0x100, 0x3000);
        let resident = dummy_resident(0, 0);
        let vectors = vec![0x1111_1111, 0x2222_2222, 0x3333_3333];

        let made = make_library(&mut mem, &mut heap, &resident, 34, 0, &vectors, "test").unwrap();

        assert_eq!(mem.read_u16(made.base - 6), JMP_ABS_L_OPCODE);
        assert_eq!(mem.read_u32(made.base - 4), 0x1111_1111);
        assert_eq!(mem.read_u16(made.base - 12), JMP_ABS_L_OPCODE);
        assert_eq!(mem.read_u32(made.base - 10), 0x2222_2222);
        assert_eq!(mem.read_u16(made.base - 18), JMP_ABS_L_OPCODE);
        assert_eq!(mem.read_u32(made.base - 16), 0x3333_3333);
    }

    #[test]
    fn make_library_negsize_rounds_up_to_longword_multiple() {
        let mut mem = FlatMemory::new(0x4000);
        let mut heap = GuestHeap::new(0x100, 0x3000);
        let resident = dummy_resident(0, 0);

        // 6 vectors * 6 bytes = 36, already a multiple of 4.
        let six_vectors = vec![0u32; 6];
        let made6 = make_library(&mut mem, &mut heap, &resident, 34, 0, &six_vectors, "t").unwrap();
        assert_eq!(made6.base - made6.alloc_addr, 36);

        // 5 vectors * 6 bytes = 30, rounds up to 32.
        let five_vectors = vec![0u32; 5];
        let made5 =
            make_library(&mut mem, &mut heap, &resident, 34, 0, &five_vectors, "t").unwrap();
        assert_eq!(made5.base - made5.alloc_addr, 32);
    }

    #[test]
    fn make_library_zeroes_the_data_area() {
        let mut mem = FlatMemory::new(0x4000);
        // Pre-fill the region with garbage so zeroing is actually
        // exercised, not just coincidentally already-zero. d_size = 40,
        // sizeof(struct Library) = LIB_STRUCT_SIZE (34) -- everything
        // from LIB_STRUCT_SIZE up to d_size is pure data area with no
        // header field ever written into it, so that's what this test
        // checks stayed zeroed (the header fields below LIB_STRUCT_SIZE
        // are deliberately overwritten by the header-writing step that
        // runs after the zero-fill, so asserting on those would be
        // asserting on the wrong thing).
        for a in 0x100..0x200u32 {
            mem.write_u8(a, 0xAA);
        }
        let mut heap = GuestHeap::new(0x100, 0x3000);
        let resident = dummy_resident(0, 0);
        let vectors = vec![0u32; 2];

        let made = make_library(&mut mem, &mut heap, &resident, 40, 0, &vectors, "t").unwrap();
        for i in LIB_STRUCT_SIZE..40 {
            assert_eq!(
                mem.read_u8(made.base + i),
                0,
                "byte {i} of data area not zeroed"
            );
        }
    }

    #[test]
    fn make_library_header_fields_are_correct() {
        let mut mem = FlatMemory::new(0x4000);
        let mut heap = GuestHeap::new(0x100, 0x3000);
        let name_ptr = 0x50u32;
        let id_ptr = 0x60u32;
        write_c_string(&mut mem, name_ptr, b"testlib.library");
        write_c_string(&mut mem, id_ptr, b"testlib.library 6.1");
        let mut resident = dummy_resident(name_ptr, id_ptr);
        resident.node_type = NT_LIBRARY;
        resident.version = 6;
        let vectors = vec![0u32; 4];

        let made = make_library(&mut mem, &mut heap, &resident, 34, 0, &vectors, "t").unwrap();

        assert_eq!(mem.read_u8(made.base + LIB_NODE_TYPE_OFFSET), NT_LIBRARY);
        assert_eq!(mem.read_u32(made.base + LIB_NODE_NAME_OFFSET), name_ptr);
        assert_eq!(mem.read_u16(made.base + LIB_NEGSIZE_OFFSET), 24); // 4*6
        assert_eq!(mem.read_u16(made.base + LIB_POSSIZE_OFFSET), 34);
        assert_eq!(mem.read_u16(made.base + LIB_VERSION_OFFSET), 6);
        assert_eq!(mem.read_u32(made.base + LIB_IDSTRING_OFFSET), id_ptr);
        assert_eq!(
            read_c_string(&mem, mem.read_u32(made.base + LIB_NODE_NAME_OFFSET)),
            b"testlib.library"
        );
    }

    #[test]
    fn make_library_rejects_too_small_dsize() {
        let mut mem = FlatMemory::new(0x1000);
        let mut heap = GuestHeap::new(0x100, 0x900);
        let resident = dummy_resident(0, 0);
        let err = make_library(&mut mem, &mut heap, &resident, 33, 0, &[], "t").unwrap_err();
        assert!(matches!(
            err,
            MakeLibraryError::DataSizeTooSmall { d_size: 33 }
        ));
    }

    #[test]
    fn make_library_rejects_non_null_structure_naming_the_library() {
        let mut mem = FlatMemory::new(0x1000);
        let mut heap = GuestHeap::new(0x100, 0x900);
        let resident = dummy_resident(0, 0);
        let err = make_library(
            &mut mem,
            &mut heap,
            &resident,
            34,
            0xABCD,
            &[],
            "myfancy.library",
        )
        .unwrap_err();
        match err {
            MakeLibraryError::InitStructNotImplemented { library_name } => {
                assert_eq!(library_name, "myfancy.library");
            }
            other => panic!("expected InitStructNotImplemented, got {other:?}"),
        }
    }

    // --- opt-in: real scspill.library, never committed to this repo ---
    //
    // Same posture as this codebase's other real-media tests: skip
    // cleanly (not a failure) when the file isn't present on this
    // machine, since it's local SAS/C-distribution content that can't be
    // vendored into the repo.

    #[test]
    fn real_scspill_library_loads_and_makes_a_library() {
        let path = "/Users/simond/amiga/sasc/libs/scspill.library";
        if !std::path::Path::new(path).exists() {
            eprintln!(
                "skipping real_scspill_library_loads_and_makes_a_library: {path} not present"
            );
            return;
        }
        let bytes = std::fs::read(path).expect("read scspill.library");
        let file = loader::parse(&bytes).expect("parse scspill.library as a hunk executable");

        let mut heap = GuestHeap::new(0x1000, 0x8000);
        let mut mem = FlatMemory::new(0x1_0000);
        let seglist = build_seglist(&file, &mut heap, &mut mem).expect("build_seglist");

        let resident_addr =
            find_resident(&mem, seglist.first_bptr).expect("scspill.library has a Resident");
        let resident = read_resident(&mem, resident_addr);
        assert_eq!(resident.flags, RTF_AUTOINIT);
        assert_eq!(resident.node_type, NT_LIBRARY);
        assert_eq!(resident.version, 6);
        assert_eq!(read_c_string(&mem, resident.name_ptr), b"scspill.library");

        let autoinit = read_autoinit(&mem, resident.init_ptr);
        assert_eq!(autoinit.d_size, 0xEE);
        assert_eq!(autoinit.structure, 0);

        let vectors = read_vectors(&mem, autoinit.vectors).expect("decode vector table");
        assert_eq!(
            vectors.len(),
            6,
            "Open/Close/Expunge/Reserved + 2 user functions"
        );

        let made = make_library(
            &mut mem,
            &mut heap,
            &resident,
            autoinit.d_size,
            autoinit.structure,
            &vectors,
            "scspill.library",
        )
        .expect("make_library should succeed for scspill.library");
        assert_ne!(made.base, 0);
    }
}
