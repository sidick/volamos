//! `exec.library`'s `InitResident`/`MakeLibrary` mechanics: scanning a
//! [`crate::dosseg::build_seglist`]-loaded seglist for a `struct
//! Resident` (romtag), reading the `RTF_AUTOINIT` init table it points
//! to, decoding both real vector-table encodings, and the host-side
//! equivalent of `MakeLibrary` itself (jump-table + `struct Library`
//! header construction on the guest heap), plus [`init_struct`] (the
//! byte-coded `InitStruct` interpreter `MakeLibrary` applies to a
//! non-`NULL` `structure` argument).
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
//! Resident` that drove the load. A non-`NULL` `structure` is applied by
//! [`init_struct`] after those header defaults are written, so a table
//! can override them.
//!
//! [`GuestHeap::alloc`] already 4-byte-aligns every allocation it hands
//! back (see that module's doc), and `negsize` is itself rounded up to a
//! multiple of 4 by [`make_library`] before it's added to the allocation
//! start to compute `base` -- so `base` (allocation-start + a multiple of
//! 4) is unconditionally longword-aligned too, satisfying V36+
//! `MakeLibrary`'s own alignment guarantee for free, with no extra
//! rounding logic needed at the `base` end.

use crate::cpu::{AddressRegister, Cpu, DataRegister};
use crate::dispatch::{DispatchError, EXEC_BASE_LIBLIST_OFFSET, EXEC_LIBRARY_BASE, HandlerContext};
use crate::dosseg::{NEXT_SEG_OFFSET, SEG_HEADER_SIZE};
use crate::guestmem::{GuestHeap, GuestHeapError, addr_from_bptr, read_c_string};
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
/// `lib_Sum` (`ULONG`) byte offset: `lib_IdString` (24) + 4 = 28.
const LIB_SUM_OFFSET: u32 = 28;
/// `LIBF_SUMMING` (`exec/libraries.h`): a checksum is being computed.
const LIBF_SUMMING: u8 = 1 << 0;
/// `LIBF_CHANGED`: the library was just modified, so a stale `lib_Sum`
/// is expected rather than corruption.
const LIBF_CHANGED: u8 = 1 << 1;
/// `LIBF_SUMUSED`: the library wants its checksum maintained.
const LIBF_SUMUSED: u8 = 1 << 2;
/// `AN_LibChkSum` (`exec/alerts.h`): "Library checksum failure".
const AN_LIB_CHK_SUM: u32 = 0x0100_0003;
/// `lib_NegSize` byte offset: `Node` (14) + `lib_Flags`/`lib_pad` (2) =
/// 16.
const LIB_NEGSIZE_OFFSET: u32 = 16;
/// `lib_PosSize` byte offset: immediately after `lib_NegSize` (18).
const LIB_POSSIZE_OFFSET: u32 = 18;
/// `lib_Version` byte offset: `lib_NegSize`/`lib_PosSize` (2 each) past
/// [`LIB_NEGSIZE_OFFSET`] = 20.
const LIB_VERSION_OFFSET: u32 = 20;
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
    /// The `structure` (`InitStruct` table) argument held a malformed
    /// command (see [`init_struct`]).
    InitStruct(String),
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
            MakeLibraryError::InitStruct(e) => write!(f, "MakeLibrary: {e}"),
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
/// ([`MakeLibraryError::DataSizeTooSmall`]), if `structure` is malformed
/// ([`MakeLibraryError::InitStruct`]), or if the heap allocation itself
/// fails ([`MakeLibraryError::Heap`]).
pub fn make_library(
    mem: &mut dyn AddressSpace,
    heap: &mut GuestHeap,
    resident: &Resident,
    d_size: u32,
    structure: u32,
    vectors: &[u32],
) -> Result<MadeLibrary, MakeLibraryError> {
    build_library(mem, heap, d_size, structure, vectors, Some(resident))
}

/// The shared core of [`make_library`] and the guest-callable
/// `MakeLibrary` handler. `defaults` (the AUTOINIT path's `struct
/// Resident`) pre-fills the `Node` type/name, `lib_Version` and
/// `lib_IdString`; the guest-called form passes `None`, since real
/// `MakeLibrary` takes those only from `structure`'s `InitStruct` table
/// (or the caller's own later stores). `structure` is applied after the
/// defaults, so it can override them; `lib_NegSize`/`lib_PosSize` are
/// written last, as real `MakeLibrary` does.
fn build_library(
    mem: &mut dyn AddressSpace,
    heap: &mut GuestHeap,
    d_size: u32,
    structure: u32,
    vectors: &[u32],
    defaults: Option<&Resident>,
) -> Result<MadeLibrary, MakeLibraryError> {
    if d_size < LIB_STRUCT_SIZE {
        return Err(MakeLibraryError::DataSizeTooSmall { d_size });
    }

    let negsize = round_up_longword(vectors.len() as u32 * LIB_VECTSIZE);
    let total = negsize.saturating_add(d_size);
    let alloc_addr = heap
        .alloc(&mut *mem, total)
        .map_err(MakeLibraryError::Heap)?;
    let base = alloc_addr.wrapping_add(negsize);

    for i in 0..d_size {
        mem.write_u8(base.wrapping_add(i), 0);
    }

    write_jump_table(mem, base, vectors);

    if let Some(resident) = defaults {
        mem.write_u8(base.wrapping_add(LIB_NODE_TYPE_OFFSET), resident.node_type);
        mem.write_u32(base.wrapping_add(LIB_NODE_NAME_OFFSET), resident.name_ptr);
        mem.write_u16(
            base.wrapping_add(LIB_VERSION_OFFSET),
            resident.version as u16,
        );
        mem.write_u32(
            base.wrapping_add(LIB_IDSTRING_OFFSET),
            resident.id_string_ptr,
        );
    }
    if structure != 0 {
        init_struct(mem, structure, base, 0).map_err(MakeLibraryError::InitStruct)?;
    }
    mem.write_u16(base.wrapping_add(LIB_NEGSIZE_OFFSET), negsize as u16);
    mem.write_u16(base.wrapping_add(LIB_POSSIZE_OFFSET), d_size as u16);

    Ok(MadeLibrary { alloc_addr, base })
}

/// Writes one `JMP abs.l` per entry of `vectors`, walking downward from
/// `base` (`vectors[0]` lands at `base - 6`). Shared by `MakeLibrary` and
/// `MakeFunctions`.
fn write_jump_table(mem: &mut dyn AddressSpace, base: u32, vectors: &[u32]) {
    let mut vector_addr = base;
    for &target in vectors {
        vector_addr = vector_addr.wrapping_sub(LIB_VECTSIZE);
        mem.write_u16(vector_addr, JMP_ABS_L_OPCODE);
        mem.write_u32(vector_addr.wrapping_add(2), target);
    }
}

/// `InitStruct` (`exec.doc`; `exec/initializers.i` for the encodings the
/// assembler macros emit): interprets the byte-command stream at `table`
/// against the memory at `memory`, first zeroing `size` bytes of it (a
/// `size` of `0` skips the clear).
///
/// Each command byte is `ddssnnnn`, read from the next even address:
/// `dd` = `00` continue at the running destination (`nnnn`+1 items),
/// `01` continue and repeat one item `nnnn`+1 times, `10` destination
/// offset in the next byte, `11` offset in the next 24 bits; `ss` = `00`
/// longword, `01` word, `10` byte (word/longword sources start on an even
/// address; `11` is an error -- real exec raises an Alert). A zero
/// command byte ends the stream. The running destination starts at
/// `memory` and advances past whatever each command wrote.
pub fn init_struct(
    mem: &mut dyn AddressSpace,
    table: u32,
    memory: u32,
    size: u32,
) -> Result<(), String> {
    for i in 0..size {
        mem.write_u8(memory.wrapping_add(i), 0);
    }
    let mut it = table;
    let mut dest = memory;
    loop {
        it = it.wrapping_add(1) & !1;
        let cmd = mem.read_u8(it);
        it = it.wrapping_add(1);
        if cmd == 0 {
            return Ok(());
        }
        let dest_type = cmd >> 6;
        let width: u32 = match (cmd >> 4) & 3 {
            0 => 4,
            1 => 2,
            2 => 1,
            _ => {
                return Err(format!(
                    "InitStruct: command {cmd:#04x} at {:#010x} has the invalid source size \
                     ss=11",
                    it.wrapping_sub(1)
                ));
            }
        };
        let count = u32::from(cmd & 0xF) + 1;
        match dest_type {
            2 => {
                dest = memory.wrapping_add(u32::from(mem.read_u8(it)));
                it = it.wrapping_add(1);
            }
            3 => {
                let offset = (u32::from(mem.read_u8(it)) << 16)
                    | (u32::from(mem.read_u8(it.wrapping_add(1))) << 8)
                    | u32::from(mem.read_u8(it.wrapping_add(2)));
                dest = memory.wrapping_add(offset);
                it = it.wrapping_add(3);
            }
            _ => {}
        }
        if width > 1 {
            it = it.wrapping_add(1) & !1;
        }
        // Repeat mode reads its single source item once; count mode
        // consumes a fresh item per copy.
        let items = if dest_type == 1 { 1 } else { count };
        let mut item = 0;
        for i in 0..count {
            if i < items {
                item = match width {
                    4 => mem.read_u32(it),
                    2 => u32::from(mem.read_u16(it)),
                    _ => u32::from(mem.read_u8(it)),
                };
                it = it.wrapping_add(width);
            }
            match width {
                4 => mem.write_u32(dest, item),
                2 => mem.write_u16(dest, item as u16),
                _ => mem.write_u8(dest, item as u8),
            }
            dest = dest.wrapping_add(width);
        }
    }
}

// --- L3: the OpenLibrary open state machine ---
//
// `library-device-loading-plan.md` §2.1/§3 (phase L3): runs a disk
// library's real `InitResident`/`MakeLibrary`/`Open` sequence as a small
// host-side state machine driven by `crate::dispatch::ContinuationStack::
// trampoline` -- see that type's own doc for the underlying "push a resume
// address, let the ordinary dispatch loop run the guest subroutine
// natively, get control back once it returns" mechanism this all rests
// on. Nothing below steps the CPU itself; every phase transition happens
// either synchronously (phase 1's own scan/`MakeLibrary`, and the
// version check) or via a `trampoline`-pushed continuation (running the
// library's own `initFunc`/`Open` code, which may itself call other
// library functions -- exactly the case `execfmt.rs`'s `RawDoFmt`
// stepping loop can't support, and the reason this mechanism exists at
// all).

/// State carried from [`begin_open`]'s synchronous phase 1 into the
/// `initFunc` continuation (phase 2) -- see [`after_init`]. Not `Clone`/
/// `Debug`: it's only ever moved into exactly one `'static` closure.
struct PendingOpen {
    /// The name `OpenLibrary` was called with (for diagnostics and
    /// [`crate::dispatch::LibraryRegistry::register_loaded`]).
    name: String,
    /// [`MadeLibrary::alloc_addr`] -- freed via [`GuestHeap::free`] if
    /// `initFunc` refuses (returns `NULL`). `None` for a non-AUTOINIT
    /// library, which builds (and frees) its own base.
    alloc_addr: Option<u32>,
    /// The `BPTR` [`crate::dosfile::DosState::load_seg`] returned --
    /// unloaded via [`crate::dosfile::DosState::unload_seg`] if
    /// `initFunc` refuses, otherwise handed to
    /// [`crate::dispatch::LibraryRegistry::register_loaded`].
    seglist_bptr: u32,
    /// The version `OpenLibrary`/`OldOpenLibrary` was actually called
    /// with (captured before anything could clobber `D0` -- see
    /// [`crate::dispatch::open_library_handler`]'s doc).
    requested_version: u32,
    /// `A6` as it stood when `OpenLibrary` was entered -- the only
    /// register this whole state machine is on the hook to restore
    /// before the caller's own `rts` resumes (plan §2.1's register-
    /// preservation note; `D0`/`D1`/`A0`/`A1` are scratch, and `A6` is
    /// deliberately repointed at [`EXEC_LIBRARY_BASE`] then at the new
    /// library's own `base` along the way, for `initFunc`'s and `Open`'s
    /// respective calling conventions).
    caller_a6: u32,
}

/// The outcome of [`begin_open`]'s synchronous phase-1 attempt.
pub enum LoadAttempt {
    /// A real load was kicked off (either still running asynchronously
    /// via a pushed continuation, or -- if `initFunc` was `NULL` --
    /// already finished synchronously). Either way `D0`/`A6` are fully
    /// owned by this state machine from here on; the caller
    /// ([`crate::dispatch::open_library_common`]) must return
    /// immediately without touching them.
    Started,
    /// The file resolved on the `Vfs` but isn't a loadable `NT_LIBRARY` -- no continuation was pushed, nothing was left half-open
    /// (any partial seglist/heap allocation was already unwound). The
    /// caller should fall back to the fake-stub path, after logging
    /// `reason` loudly.
    StructuralFailure(String),
}

/// Phase 1 (`library-device-loading-plan.md` §2.1): `LoadSeg`s
/// `search_path`, scans for a `struct Resident`, validates it's an
/// `NT_LIBRARY` (a non-`RTF_AUTOINIT` one is handed to
/// [`begin_direct_init`]), reads its AUTOINIT table and vector
/// table, and runs [`make_library`]. On any structural failure, unwinds
/// whatever was partially built (`unload_seg`, and -- since `make_library`
/// is the last step -- there's never a `make_library` allocation to free
/// on a structural-failure path, only on the *behavioral* `initFunc`-
/// refused path [`after_init`] handles) and returns
/// [`LoadAttempt::StructuralFailure`] with a diagnostic naming why.
///
/// On success: if `initFunc` is `NULL`, skips straight to phase 2's
/// post-init logic inline (no guest code to run first -- plan §2.1 step
/// 1's shortcut). Otherwise saves the caller's `A6`, sets up `initFunc`'s
/// calling convention (`D0` = base, `A0` = segList *BPTR* -- per the
/// plan's own §1.3 text, real `InitResident` passes the BPTR, not a
/// resolved address -- `A6` = [`EXEC_LIBRARY_BASE`]), and trampolines
/// into it with a continuation that resumes at [`after_init`].
pub fn begin_open<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
    name: &str,
    search_path: &str,
    requested_version: u32,
) -> Result<LoadAttempt, DispatchError> {
    let caller_a6 = ctx.cpu.address_register(AddressRegister(6));

    let seglist_bptr = match ctx.dos.load_seg(ctx.heap, ctx.mem, search_path) {
        Ok(bptr) => bptr,
        Err(code) => {
            return Ok(LoadAttempt::StructuralFailure(format!(
                "LoadSeg failed (IoErr {code})"
            )));
        }
    };

    let Some(resident_addr) = find_resident(ctx.mem, seglist_bptr) else {
        let _ = ctx.dos.unload_seg(ctx.heap, seglist_bptr);
        return Ok(LoadAttempt::StructuralFailure(
            "no struct Resident (romtag) found -- not a loadable library".to_string(),
        ));
    };
    let resident = read_resident(ctx.mem, resident_addr);

    if resident.node_type != NT_LIBRARY {
        let _ = ctx.dos.unload_seg(ctx.heap, seglist_bptr);
        return Ok(LoadAttempt::StructuralFailure(format!(
            "Resident's node type is {} ({}), not NT_LIBRARY ({NT_LIBRARY}) -- opening a \
             .device via OpenLibrary is a caller bug",
            resident.node_type,
            if resident.node_type == NT_DEVICE {
                "NT_DEVICE"
            } else {
                "unknown"
            }
        )));
    }

    if resident.flags & RTF_AUTOINIT == 0 {
        return begin_direct_init(ctx, name, &resident, seglist_bptr, requested_version);
    }

    let autoinit = read_autoinit(ctx.mem, resident.init_ptr);
    let vectors = match read_vectors(ctx.mem, autoinit.vectors) {
        Ok(v) => v,
        Err(e) => {
            let _ = ctx.dos.unload_seg(ctx.heap, seglist_bptr);
            return Ok(LoadAttempt::StructuralFailure(e));
        }
    };

    let made = match make_library(
        ctx.mem,
        ctx.heap,
        &resident,
        autoinit.d_size,
        autoinit.structure,
        &vectors,
    ) {
        Ok(m) => m,
        Err(e) => {
            let _ = ctx.dos.unload_seg(ctx.heap, seglist_bptr);
            return Ok(LoadAttempt::StructuralFailure(e.to_string()));
        }
    };

    let pending = PendingOpen {
        name: name.to_string(),
        alloc_addr: Some(made.alloc_addr),
        seglist_bptr,
        requested_version,
        caller_a6,
    };

    if autoinit.init_func == 0 {
        after_init(ctx, pending, made.base)?;
    } else {
        ctx.cpu.set_data_register(DataRegister(0), made.base);
        ctx.cpu
            .set_address_register(AddressRegister(0), seglist_bptr);
        ctx.cpu
            .set_address_register(AddressRegister(6), EXEC_LIBRARY_BASE);
        ctx.continuations
            .trampoline(ctx.cpu, ctx.mem, autoinit.init_func, move |ctx| {
                let init_result = ctx.cpu.data_register(DataRegister(0));
                after_init(ctx, pending, init_result)
            });
    }

    Ok(LoadAttempt::Started)
}

/// Phase 2 (`library-device-loading-plan.md` §2.1): runs once `initFunc`
/// has returned (or immediately, for the `initFunc == 0` shortcut) with
/// `init_result` = what `initFunc` left in `D0` (or, for the shortcut,
/// [`begin_open`]'s own `made.base`, standing in for "init trivially
/// succeeded with its original base").
///
/// - `init_result == 0`: `initFunc` refused the open. Unloads the
///   seglist, frees the [`make_library`] allocation, restores the
///   caller's `A6`, sets `D0 = 0`. Nothing is registered -- a later
///   `OpenLibrary` of the same name gets a completely fresh attempt.
/// - Otherwise: `init_result` becomes the library's
///   base from here on -- a real `initFunc` must return the base, and
///   while it's conventionally the same value `make_library` handed it,
///   the AUTOINIT contract is "trust `initFunc`'s own `D0`", so that's
///   what this does (a non-AUTOINIT init passes the base it built or found on `LibList`, see [`begin_direct_init`]). Links the base's `struct Node` onto
///   `ExecBase.LibList` unless the library already `AddLibrary`d itself (the AddLibrary equivalent -- `ln_Name` already
///   points at the Resident's own name string, written by
///   [`make_library`], so no extra name-string bookkeeping is needed
///   here, unlike the built-in libraries' `write_library_list_nodes`).
///   Registers the library in [`crate::dispatch::LibraryRegistry`] as
///   [`crate::dispatch::LibraryKind::Loaded`]. Then checks `lib_Version`
///   (read fresh from guest memory -- `initFunc` may have raised it past
///   the Resident's own `rt_Version`) against `pending.requested_version`:
///   too low refuses the open (`D0 = 0`, caller's `A6` restored) but
///   *leaves the library registered/loaded* (matching real exec, which
///   keeps a version-refused library in memory -- a later, lower-version
///   request can still succeed). Otherwise trampolines into the `Open`
///   vector (`base - 6`, the jump-table's own `JMP` instruction --
///   perfectly valid as a call target) with `D0 = requested_version`,
///   `A6 = base` (plan §1.4's `OPEN` calling convention), resuming at
///   [`finish_open`].
fn after_init<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
    pending: PendingOpen,
    init_result: u32,
) -> Result<(), DispatchError> {
    if init_result == 0 {
        // Both unwind steps below are deliberately best-effort (`let _`),
        // an exception to this codebase's usual loud-failure discipline,
        // because the real AUTOINIT contract makes the base's fate
        // ambiguous here: per the `MakeLibrary` autodoc (plan §1.3 item
        // 4), a failing `initFunc` returns `NULL` *having freed the base
        // itself* -- a contract-compliant library will already have
        // `FreeMem`ed the `negsize + dSize` block through this same
        // [`GuestHeap`], so the host-side `free(alloc_addr)` is then a
        // double free that must be tolerated, not reported. A library
        // that *didn't* free its own base (like this runtime's own
        // `testlib_initfail` fixture) gets the block reclaimed here
        // instead, so neither style of failing library leaks.
        let _ = ctx.dos.unload_seg(ctx.heap, pending.seglist_bptr);
        if let Some(alloc_addr) = pending.alloc_addr {
            let _ = ctx.heap.free(alloc_addr);
        }
        ctx.cpu
            .set_address_register(AddressRegister(6), pending.caller_a6);
        ctx.cpu.set_data_register(DataRegister(0), 0);
        *ctx.call_detail = Some(format!(
            "library {:?} -> NULL (initFunc refused)",
            pending.name
        ));
        return Ok(());
    }

    let base = init_result;

    // A non-AUTOINIT init routine normally `AddLibrary`s itself (see
    // [`add_library_handler`]); only link the node if it hasn't.
    if find_lib_node(ctx.mem, |_, node| node == base).is_none() {
        crate::execlist::add_tail_impl(ctx.mem, LIBLIST_ADDR, base);
    }
    // `AddLibrary` registered the library under its `ln_Name`; if the
    // caller opened it by another spelling (e.g. a full path) drop that
    // entry so the base isn't registered twice.
    if let Some(prev) = ctx.registry.loaded_library_by_base(base) {
        let prev = prev.to_string();
        if prev != pending.name {
            ctx.registry.unregister_loaded(&prev);
        }
    }
    ctx.registry.register_loaded(
        &pending.name,
        base,
        pending.seglist_bptr,
        pending.alloc_addr,
    );

    // Real Kickstart checks `lib_Version` with a 68000 `CMP.W`, so only
    // the low word of the requested-version register is ever inspected
    // -- garbage in the high word (e.g. a caller passing $30000000) is
    // silently ignored rather than causing a spurious refusal (see the
    // AROS "Frontier 2" fix this mirrors). D0 itself is left untouched
    // for the trampoline into `Open` below, matching real hardware.
    let lib_version = u32::from(ctx.mem.read_u16(base.wrapping_add(LIB_VERSION_OFFSET)));
    let requested_version_word = pending.requested_version as u16 as u32;
    if lib_version < requested_version_word {
        ctx.cpu
            .set_address_register(AddressRegister(6), pending.caller_a6);
        ctx.cpu.set_data_register(DataRegister(0), 0);
        *ctx.call_detail = Some(format!(
            "library {:?} -> NULL (version {lib_version} < requested {})",
            pending.name, requested_version_word
        ));
        return Ok(());
    }

    let name = pending.name;
    let caller_a6 = pending.caller_a6;
    ctx.cpu
        .set_data_register(DataRegister(0), pending.requested_version);
    ctx.cpu.set_address_register(AddressRegister(6), base);
    ctx.continuations
        .trampoline(ctx.cpu, ctx.mem, base.wrapping_sub(6), move |ctx| {
            finish_open(ctx, &name, caller_a6)
        });
    Ok(())
}

/// Phase 3 (`library-device-loading-plan.md` §2.1): runs once the
/// library's own `Open` vector has returned. `D0` already holds `Open`'s
/// result (the base, or `NULL` if the library itself refused -- e.g. a
/// single-open device-style library already in use); this phase doesn't
/// touch it either way, it's already the correct final `OpenLibrary`
/// result. Restores the caller's `A6` (the one register this whole state
/// machine owns restoring) and records a snoop detail. An `Open`-refused
/// library stays loaded/registered, same reasoning as the version-refusal
/// case in [`after_init`] -- `OpenLibrary` failing doesn't mean the
/// library isn't in memory.
///
/// Shared verbatim by [`reopen`] (a registry `Loaded` hit) -- the repeat-
/// open protocol is exactly "version check, then `Open` again", so this
/// is the same completion either way.
fn finish_open<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
    name: &str,
    caller_a6: u32,
) -> Result<(), DispatchError> {
    let result = ctx.cpu.data_register(DataRegister(0));
    ctx.cpu.set_address_register(AddressRegister(6), caller_a6);
    *ctx.call_detail = Some(if result != 0 {
        format!("library {name:?} -> base {result:#010x} (loaded from disk)")
    } else {
        format!("library {name:?} -> NULL (Open refused)")
    });
    Ok(())
}

/// A repeat `OpenLibrary`/`OldOpenLibrary` of an already-[`crate::
/// dispatch::LibraryKind::Loaded`] library (`library-device-loading-
/// plan.md` §2.4): real exec doesn't cache the previous `Open` result --
/// the library maintains its own `lib_OpenCnt`, so a second open must
/// genuinely re-run the version check and call `Open` again. Shares
/// [`after_init`]'s version-check logic and [`finish_open`]'s completion,
/// just without a phase-1 `LoadSeg`/`MakeLibrary` step (the library is
/// already resident).
pub fn reopen<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
    name: &str,
    base: u32,
    requested_version: u32,
) -> Result<(), DispatchError> {
    let caller_a6 = ctx.cpu.address_register(AddressRegister(6));

    // See `after_init`'s matching comment: real Kickstart's version check
    // is a `CMP.W`, so only the low word of `requested_version` matters.
    let lib_version = u32::from(ctx.mem.read_u16(base.wrapping_add(LIB_VERSION_OFFSET)));
    let requested_version_word = requested_version as u16 as u32;
    if lib_version < requested_version_word {
        ctx.cpu.set_address_register(AddressRegister(6), caller_a6);
        ctx.cpu.set_data_register(DataRegister(0), 0);
        *ctx.call_detail = Some(format!(
            "library {name:?} -> NULL (version {lib_version} < requested {requested_version_word})"
        ));
        return Ok(());
    }

    let name = name.to_string();
    ctx.cpu
        .set_data_register(DataRegister(0), requested_version);
    ctx.cpu.set_address_register(AddressRegister(6), base);
    ctx.continuations
        .trampoline(ctx.cpu, ctx.mem, base.wrapping_sub(6), move |ctx| {
            finish_open(ctx, &name, caller_a6)
        });
    Ok(())
}

// --- L4: CloseLibrary via the Close vector ---

/// `CloseLibrary` for a [`crate::dispatch::LibraryKind::Loaded`] base
/// (`library-device-loading-plan.md` §2.4): trampolines into the
/// library's own Close vector (`base - 12`, LVO -12 per plan §1.4 -- `A6`
/// = libBase is the *only* input register the Close vector's calling
/// convention documents, unlike Open's `D0` = version). Called by
/// [`crate::dispatch::close_library_handler`], which has already
/// established (via [`crate::dispatch::LibraryRegistry::
/// loaded_library_by_base`]) that `base` really is a loaded library's base
/// and `name` its registered name.
///
/// Refcounting is the library's own job (its Open/Close vectors maintain
/// `lib_OpenCnt` themselves, same as [`reopen`]'s doc explains) -- this
/// function doesn't track opens/closes at all, it just runs the real
/// vector and, in [`finish_close`], acts on whatever `D0` it returns.
pub fn begin_close<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
    name: &str,
    base: u32,
) -> Result<(), DispatchError> {
    let caller_a6 = ctx.cpu.address_register(AddressRegister(6));
    // Captured now (rather than re-looked-up in finish_close) because
    // once the Close vector actually runs it may, on the last-close path,
    // be about to have its own registry entry removed -- simpler to carry
    // the one field finish_close needs than to re-derive it from a
    // registry that's mid-transition. The `expect` documents an invariant
    // close_library_handler already established: it only calls this
    // function for a `name` it just found via `loaded_library_by_base`,
    // so a `loaded_library` miss here would mean the registry changed
    // out from under us within a single handler dispatch -- a bug, not a
    // reachable runtime condition.
    let alloc_addr: Option<u32> = ctx
        .registry
        .loaded_library(name)
        .expect("begin_close is only called for a name just found in the registry")
        .alloc_addr;

    let name = name.to_string();
    ctx.cpu.set_address_register(AddressRegister(6), base);
    ctx.continuations
        .trampoline(ctx.cpu, ctx.mem, base.wrapping_sub(12), move |ctx| {
            finish_close(ctx, &name, base, alloc_addr, caller_a6)
        });
    Ok(())
}

/// Runs once the library's own Close vector has returned. `D0` is its
/// result, per the CLOSE vector's documented contract (`exec/libraries.i`
/// / RKRM ch. 18 "Exec Libraries", plan §1.4): `0` means the library
/// stays resident (there are still other opens, or it simply never
/// expunges on close) -- nothing further to do beyond restoring the
/// caller's `A6`. Non-zero is a `SegList` `BPTR`: the delayed-expunge
/// convention, where a library's Close vector itself decides "this was
/// the last open" (by checking its own decremented `lib_OpenCnt`) and
/// hands the seglist it stored at init time back to us instead of calling
/// `Expunge`/`RemLibrary` itself (`fixtures/testlib.s`'s `CloseFunc` is
/// exactly this idiom -- see that file's header comment).
///
/// On the delayed-expunge path, mirrors what real exec's own
/// `CloseLibrary` code does after a non-zero Close result (this is
/// documented exec behavior, not `RemLibrary`-specific -- RKRM ch. 18):
/// unlink the base's `struct Node` from `ExecBase.LibList`
/// ([`crate::execlist::remove_impl`] -- the `AddLibrary`-equivalent
/// counterpart to [`after_init`]'s `add_tail_impl`), drop both registry
/// entries via [`crate::dispatch::LibraryRegistry::unregister_loaded`] (a
/// later `OpenLibrary` of the same name must load fresh from disk, not
/// resolve to this now-dead base), `UnLoadSeg` the returned segList, then
/// free the [`make_library`] base allocation.
///
/// # Why the base allocation is freed *here*, not by the library
///
/// The `structure`/`initFunc` `MakeLibrary` autodoc (plan §1.3 item 4)
/// makes an *initFunc* that returns `NULL` responsible for freeing the
/// base itself (see [`after_init`]'s doc) -- but that's a completely
/// different moment in the library's lifecycle (a refused *open*, before
/// the library was ever added to `LibList`). Once a library is resident,
/// the conventional AUTOINIT contract is the reverse: `RemLibrary`
/// (RKRM ch. 18) is the one that frees `lib_NegSize + lib_PosSize` bytes
/// around the base *after* `Expunge` returns -- a library's own
/// Close/Expunge code frees nothing of its own base, only whatever it
/// separately allocated (here, nothing) plus the seglist. `CloseLibrary`'s
/// delayed-expunge path is exec performing exactly that `RemLibrary`-style
/// cleanup itself, immediately, since nothing else will.
///
/// # Loud, not best-effort, unlike [`after_init`]'s `NULL`-init cleanup
///
/// [`after_init`]'s unwind on a refused open is deliberately best-effort
/// because a *contract-compliant* library may have already freed its own
/// base, making a second `free` here an expected double-free to tolerate.
/// No such ambiguity exists on this path: a library's Close vector
/// returning a segList is an unambiguous "please unload me" instruction,
/// with no contract under which the library itself also frees the
/// seglist or the base -- so an `UnLoadSeg`/`free` failure here really is
/// the library handing back garbage (an unknown `BPTR`, or -- impossible
/// under this runtime's own bookkeeping, but not under a hypothetical
/// corrupted one -- an already-freed base), and per this codebase's usual
/// posture (`crate::dosseg`'s own "loud failure on an unknown seglist"
/// stance) that's a bug worth surfacing loudly, not swallowing.
///
/// Real `CloseLibrary` itself returns nothing (`exec.doc`: "RESULT: none"
/// -- unlike `Open`/`Close`, which are `RESULT: base`/`RESULT: 0 or
/// seglist`, `CloseLibrary` is documented void). This leaves `D0` exactly
/// as the Close vector left it rather than zeroing it -- matching real
/// exec, which doesn't clear it either, and nothing in this codebase
/// reads `CloseLibrary`'s own `D0` afterward.
fn finish_close<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
    name: &str,
    base: u32,
    alloc_addr: Option<u32>,
    caller_a6: u32,
) -> Result<(), DispatchError> {
    let close_result = ctx.cpu.data_register(DataRegister(0));
    ctx.cpu.set_address_register(AddressRegister(6), caller_a6);

    if close_result == 0 {
        *ctx.call_detail = Some(format!("library {name:?} closed (still resident)"));
        return Ok(());
    }

    let seglist_bptr = close_result;

    // A library whose Close vector expunged it itself (RemLibrary-style)
    // has already unlinked its node; unlinking twice would corrupt the list.
    if find_lib_node(ctx.mem, |_, node| node == base).is_some() {
        crate::execlist::remove_impl(ctx.mem, base);
    }
    ctx.registry.unregister_loaded(name);

    ctx.dos
        .unload_seg(ctx.heap, seglist_bptr)
        .map_err(|e| DispatchError::HandlerFailed {
            library: name.to_string(),
            lvo: -12,
            handler_name: "CloseLibrary".to_string(),
            message: format!(
                "Close vector returned segList {seglist_bptr:#010x} on the delayed-expunge \
                 path, but UnLoadSeg rejected it: {e}"
            ),
        })?;

    if let Some(alloc_addr) = alloc_addr {
        ctx.heap
            .free(alloc_addr)
            .map_err(|e| DispatchError::HandlerFailed {
                library: name.to_string(),
                lvo: -12,
                handler_name: "CloseLibrary".to_string(),
                message: format!(
                    "freeing library base allocation {alloc_addr:#010x} after expunge: {e}"
                ),
            })?;
    }

    *ctx.call_detail = Some(format!(
        "library {name:?} closed and expunged (base {base:#010x} freed)"
    ));
    Ok(())
}

// --- Non-RTF_AUTOINIT libraries and the exec calls a hand-rolled init needs ---

/// Guest address of `ExecBase.LibList` (the `struct List` header).
const LIBLIST_ADDR: u32 = EXEC_LIBRARY_BASE + EXEC_BASE_LIBLIST_OFFSET;

/// Walks `ExecBase.LibList` and returns the first node for which `pred`
/// holds.
fn find_lib_node(
    mem: &dyn AddressSpace,
    pred: impl Fn(&dyn AddressSpace, u32) -> bool,
) -> Option<u32> {
    let mut node = mem.read_u32(LIBLIST_ADDR);
    loop {
        let succ = mem.read_u32(node);
        if succ == 0 {
            return None;
        }
        if pred(mem, node) {
            return Some(node);
        }
        node = succ;
    }
}

/// The first `LibList` node whose `ln_Name` is `name` and whose address
/// isn't `skip` (`0` skips nothing).
fn find_lib_node_named(mem: &dyn AddressSpace, name: &[u8], skip: u32) -> Option<u32> {
    find_lib_node(mem, |mem, node| {
        let name_ptr = mem.read_u32(node.wrapping_add(LIB_NODE_NAME_OFFSET));
        node != skip && name_ptr != 0 && read_c_string(mem, name_ptr) == name
    })
}

/// Phase 1 for a non-`RTF_AUTOINIT` `NT_LIBRARY` Resident: real
/// `InitResident` jumps straight to `rt_Init` with `D0 = 0`, `A0` =
/// segList, `A6` = ExecBase (`exec.doc` `InitResident`), and that code
/// builds the library itself (`MakeLibrary` + `AddLibrary`) and returns
/// the base. There is no `make_library` here, so [`PendingOpen::alloc_addr`]
/// is `None` and the library owns its base's memory.
///
/// `exec.doc` only says `D0` is "usually the library or device base"; a
/// routine that added itself but returned `NULL` anyway is therefore
/// recovered by looking its Resident name up on `LibList` (ignoring a node
/// of that name that was already there beforehand -- e.g. the built-in
/// library a `--libs-dir` override is replacing). If neither yields a
/// base the open is refused like a `NULL` AUTOINIT `initFunc`.
fn begin_direct_init<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
    name: &str,
    resident: &Resident,
    seglist_bptr: u32,
    requested_version: u32,
) -> Result<LoadAttempt, DispatchError> {
    if resident.init_ptr == 0 {
        let _ = ctx.dos.unload_seg(ctx.heap, seglist_bptr);
        return Ok(LoadAttempt::StructuralFailure(
            "Resident isn't RTF_AUTOINIT and its RT_INIT is NULL -- nothing to run".to_string(),
        ));
    }
    let caller_a6 = ctx.cpu.address_register(AddressRegister(6));
    let resident_name = if resident.name_ptr != 0 {
        read_c_string(ctx.mem, resident.name_ptr)
    } else {
        Vec::new()
    };
    let preexisting = find_lib_node_named(ctx.mem, &resident_name, 0).unwrap_or(0);

    let pending = PendingOpen {
        name: name.to_string(),
        alloc_addr: None,
        seglist_bptr,
        requested_version,
        caller_a6,
    };
    ctx.cpu.set_data_register(DataRegister(0), 0);
    ctx.cpu
        .set_address_register(AddressRegister(0), seglist_bptr);
    ctx.cpu
        .set_address_register(AddressRegister(6), EXEC_LIBRARY_BASE);
    ctx.continuations
        .trampoline(ctx.cpu, ctx.mem, resident.init_ptr, move |ctx| {
            let mut base = ctx.cpu.data_register(DataRegister(0));
            if base == 0 && !resident_name.is_empty() {
                base = find_lib_node_named(ctx.mem, &resident_name, preexisting).unwrap_or(0);
            }
            after_init(ctx, pending, base)
        });
    Ok(LoadAttempt::Started)
}

/// The checksum `SumLibrary` stores in `lib_Sum`: the wrapping sum of the
/// 32-bit target address of every `JMP` in the jump table (the vector
/// addresses), `lib_NegSize / 6` entries below `base`. The exact real
/// algorithm isn't documented in the autodocs; only consistency between
/// `AddLibrary`/`SumLibrary`/`SetFunction` matters to a guest.
fn lib_checksum(mem: &dyn AddressSpace, base: u32) -> u32 {
    let negsize = u32::from(mem.read_u16(base.wrapping_add(LIB_NEGSIZE_OFFSET)));
    (1..=negsize / LIB_VECTSIZE).fold(0u32, |sum, i| {
        let entry = base.wrapping_sub(i * LIB_VECTSIZE);
        sum.wrapping_add(mem.read_u32(entry.wrapping_add(2)))
    })
}

/// `SumLibrary`'s core: returns `false` on a checksum mismatch that
/// real exec would raise `Alert(AN_LibChkSum)` for. Skips (returning
/// `true`) if a sum is already in progress. A stale sum on a
/// `LIBF_CHANGED` library is expected: it is recomputed and stored, and
/// `LIBF_CHANGED` cleared.
fn sum_library_impl(mem: &mut dyn AddressSpace, base: u32) -> bool {
    let flags_addr = base.wrapping_add(LIB_FLAGS_OFFSET);
    let flags = mem.read_u8(flags_addr);
    if flags & LIBF_SUMMING != 0 {
        return true;
    }
    mem.write_u8(flags_addr, flags | LIBF_SUMMING);
    let sum = lib_checksum(mem, base);
    let stored = mem.read_u32(base.wrapping_add(LIB_SUM_OFFSET));
    let ok = if flags & LIBF_CHANGED != 0 {
        mem.write_u32(base.wrapping_add(LIB_SUM_OFFSET), sum);
        true
    } else {
        stored == sum
    };
    mem.write_u8(flags_addr, flags & !(LIBF_SUMMING | LIBF_CHANGED));
    ok
}

/// Called after the jump table of `base` was modified (`SetFunction`):
/// keeps `lib_Sum` current for a library that asked for checksumming
/// (`LIBF_SUMUSED`), as real `SetFunction` does.
pub(crate) fn refresh_lib_sum(mem: &mut dyn AddressSpace, base: u32) {
    let flags_addr = base.wrapping_add(LIB_FLAGS_OFFSET);
    let flags = mem.read_u8(flags_addr);
    if flags & LIBF_SUMUSED != 0 {
        mem.write_u8(flags_addr, flags | LIBF_CHANGED);
        sum_library_impl(mem, base);
    }
}

/// `SumLibrary` (LVO -426): `A1` = library. Recomputes `lib_Sum` and
/// compares it with the stored value. A mismatch on a library not marked
/// `LIBF_CHANGED` raises `Alert(AN_LibChkSum)` -- a recovery alert, so,
/// like [`crate::exectask`]'s `Alert`, it is reported on `stderr` and the
/// call returns.
fn sum_library_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let lib = ctx.cpu.address_register(AddressRegister(1));
    if !sum_library_impl(ctx.mem, lib) {
        eprintln!(
            "volamos: Alert({AN_LIB_CHK_SUM:#010x}): AT_Recovery -- library {lib:#010x} \
             failed its SumLibrary checksum (GeneralError=0x00 SpecificError=0x0003)"
        );
    }
    Ok(())
}

/// `RemLibrary` (LVO -402): `A1` = library. Runs the library's own
/// Expunge vector (`base - 18`, `A6` = base) and, as `CloseLibrary` does
/// for Close, treats a non-zero result as the segList of a library that
/// agreed to go: its node is unlinked if still on `LibList`, its registry
/// entry dropped, the seglist unloaded, and a host-owned base allocation
/// (AUTOINIT) freed if the library hasn't already freed it itself. A zero
/// result means it refused (still open) and stays resident. Only
/// libraries loaded through the disk-library path or `AddLibrary` (the
/// registry's `Loaded` kind) are expunged; built-in libraries have no
/// Expunge vector, so `RemLibrary` of one is a no-op. Real exec's own
/// handling of Expunge's segList isn't documented in the autodocs; this
/// mirrors the Close path so the two stay consistent.
fn rem_library_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let base = ctx.cpu.address_register(AddressRegister(1));
    let Some(name) = ctx
        .registry
        .loaded_library_by_base(base)
        .map(str::to_string)
    else {
        return Ok(());
    };
    let alloc_addr = ctx
        .registry
        .loaded_library(&name)
        .and_then(|l| l.alloc_addr);
    let caller_a6 = ctx.cpu.address_register(AddressRegister(6));
    ctx.cpu.set_address_register(AddressRegister(6), base);
    ctx.continuations
        .trampoline(ctx.cpu, ctx.mem, base.wrapping_sub(18), move |ctx| {
            ctx.cpu.set_address_register(AddressRegister(6), caller_a6);
            let seglist = ctx.cpu.data_register(DataRegister(0));
            if seglist == 0 {
                *ctx.call_detail = Some(format!("library {name:?} refused to expunge"));
                return Ok(());
            }
            if find_lib_node(ctx.mem, |_, node| node == base).is_some() {
                crate::execlist::remove_impl(ctx.mem, base);
            }
            ctx.registry.unregister_loaded(&name);
            ctx.dos.unload_seg(ctx.heap, seglist).map_err(|e| {
                handler_failed(
                    -402,
                    "RemLibrary",
                    format!(
                        "Expunge of {name:?} returned segList {seglist:#010x}, but UnLoadSeg                          rejected it: {e}"
                    ),
                )
            })?;
            // A conventional Expunge frees its own base; only free what is
            // still a live host allocation.
            if let Some(alloc_addr) = alloc_addr
                && ctx.heap.size_of_live_alloc(alloc_addr).is_some()
            {
                ctx.heap.free(alloc_addr).map_err(|e| {
                    handler_failed(
                        -402,
                        "RemLibrary",
                        format!("freeing base {alloc_addr:#010x}: {e}"),
                    )
                })?;
            }
            *ctx.call_detail = Some(format!("library {name:?} expunged"));
            Ok(())
        });
    Ok(())
}

fn handler_failed(lvo: i32, handler_name: &str, message: String) -> DispatchError {
    DispatchError::HandlerFailed {
        library: "exec.library".to_string(),
        lvo,
        handler_name: handler_name.to_string(),
        message,
    }
}

/// `MakeLibrary` (LVO -84): `A0` = vectors, `A1` = InitStruct table (or
/// `NULL`), `A2` = init routine (or `NULL`), `D0` = dSize, `D1` = segList.
/// Builds the library with real `JMP` vectors via [`build_library`]; no
/// node name/type/version is filled in other than what `structure`
/// provides. If `init` is non-`NULL` it is run (`D0` = base, `A0` =
/// segList, `A6` = ExecBase) and its `D0` is the result -- on a `NULL`
/// result the init routine is responsible for freeing the base, as the
/// autodoc says. Returns `NULL` if the heap is exhausted.
fn make_library_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let vectors_ptr = ctx.cpu.address_register(AddressRegister(0));
    let structure = ctx.cpu.address_register(AddressRegister(1));
    let init = ctx.cpu.address_register(AddressRegister(2));
    let d_size = ctx.cpu.data_register(DataRegister(0));
    let seglist = ctx.cpu.data_register(DataRegister(1));

    if vectors_ptr == 0 {
        return Err(handler_failed(
            -84,
            "MakeLibrary",
            "called with a NULL vectors pointer".to_string(),
        ));
    }
    let vectors =
        read_vectors(ctx.mem, vectors_ptr).map_err(|e| handler_failed(-84, "MakeLibrary", e))?;
    let made = match build_library(ctx.mem, ctx.heap, d_size, structure, &vectors, None) {
        Ok(made) => made,
        Err(MakeLibraryError::Heap(_)) => {
            ctx.cpu.set_data_register(DataRegister(0), 0);
            return Ok(());
        }
        Err(e) => return Err(handler_failed(-84, "MakeLibrary", e.to_string())),
    };

    ctx.cpu.set_data_register(DataRegister(0), made.base);
    if init != 0 {
        ctx.cpu.set_address_register(AddressRegister(0), seglist);
        ctx.cpu
            .set_address_register(AddressRegister(6), EXEC_LIBRARY_BASE);
        ctx.continuations
            .trampoline(ctx.cpu, ctx.mem, init, |_ctx| Ok(()));
    }
    Ok(())
}

/// `MakeFunctions` (LVO -90): `A0` = target (the high end of the table,
/// typically the library base), `A1` = function array, `A2` =
/// displacement base. A zero `A2` means `A1` holds absolute longword
/// pointers terminated by `-1`; otherwise it holds word displacements
/// from `A2`, terminated by a `-1` word (no leading format word -- that's
/// `MakeLibrary`'s `vectors` convention, not this call's). Returns the
/// table size in bytes in `D0`.
fn make_functions_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let target = ctx.cpu.address_register(AddressRegister(0));
    let array = ctx.cpu.address_register(AddressRegister(1));
    let disp_base = ctx.cpu.address_register(AddressRegister(2));

    let mut vectors = Vec::new();
    loop {
        if vectors.len() >= MAX_VECTORS {
            return Err(handler_failed(
                -90,
                "MakeFunctions",
                format!(
                    "function array at {array:#010x} exceeded {MAX_VECTORS} entries without a \
                     -1 terminator"
                ),
            ));
        }
        let index = vectors.len() as u32;
        if disp_base == 0 {
            let entry = ctx.mem.read_u32(array.wrapping_add(index * 4));
            if entry == 0xFFFF_FFFF {
                break;
            }
            vectors.push(entry);
        } else {
            let word = ctx.mem.read_u16(array.wrapping_add(index * 2));
            if word == 0xFFFF {
                break;
            }
            vectors.push(disp_base.wrapping_add(word as i16 as i32 as u32));
        }
    }
    write_jump_table(ctx.mem, target, &vectors);
    ctx.cpu
        .set_data_register(DataRegister(0), vectors.len() as u32 * LIB_VECTSIZE);
    Ok(())
}

/// `InitStruct` (LVO -78): `A1` = table, `A2` = memory, `D0` = size. See
/// [`init_struct`].
fn init_struct_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let table = ctx.cpu.address_register(AddressRegister(1));
    let memory = ctx.cpu.address_register(AddressRegister(2));
    let size = ctx.cpu.data_register(DataRegister(0));
    init_struct(ctx.mem, table, memory, size).map_err(|e| handler_failed(-78, "InitStruct", e))
}

/// `AddLibrary` (LVO -396): `A1` = library. Sets `ln_Type` to
/// `NT_LIBRARY`, `Enqueue`s the node onto `ExecBase.LibList` by priority,
/// and registers it in [`crate::dispatch::LibraryRegistry`] (under its
/// `ln_Name`, with no seglist and no host-owned base allocation -- see
/// [`crate::dispatch::LoadedLibrary::alloc_addr`]) so a later
/// `OpenLibrary` of that name runs the library's own `Open` vector rather
/// than going back to disk. If this runs inside a disk load
/// ([`begin_direct_init`]) [`after_init`] later re-registers the entry
/// with the real seglist. Also sets `LIBF_SUMUSED` and computes `lib_Sum`
/// (see [`sum_library_handler`]).
fn add_library_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let lib = ctx.cpu.address_register(AddressRegister(1));
    ctx.mem
        .write_u8(lib.wrapping_add(LIB_NODE_TYPE_OFFSET), NT_LIBRARY);
    // Real AddLibrary marks the library as checksummed and freshly
    // changed, then sums it so the first SumLibrary agrees.
    let flags = ctx.mem.read_u8(lib.wrapping_add(LIB_FLAGS_OFFSET));
    ctx.mem.write_u8(
        lib.wrapping_add(LIB_FLAGS_OFFSET),
        flags | LIBF_SUMUSED | LIBF_CHANGED,
    );
    sum_library_impl(ctx.mem, lib);
    crate::execlist::enqueue_impl(ctx.mem, LIBLIST_ADDR, lib);
    let name_ptr = ctx.mem.read_u32(lib.wrapping_add(LIB_NODE_NAME_OFFSET));
    if name_ptr != 0 {
        let name = String::from_utf8_lossy(&read_c_string(ctx.mem, name_ptr)).into_owned();
        ctx.registry.register_loaded(&name, lib, 0, None);
    }
    Ok(())
}

/// Registers `MakeLibrary`, `MakeFunctions`, `InitStruct`, `AddLibrary`,
/// `SumLibrary` and `RemLibrary` onto [`EXEC_LIBRARY_BASE`], looked up by name through
/// [`crate::lvos::exec::EXEC_LVOS`]. Called unconditionally from
/// [`crate::dispatch::Runtime::new`].
pub fn register_execlib_handlers<C: Cpu + 'static>(
    table: &mut crate::dispatch::LibraryTable<C>,
    mem: &mut C::Memory,
) {
    macro_rules! reg {
        ($name:literal, $handler:expr) => {
            table
                .register_by_name(
                    mem,
                    EXEC_LIBRARY_BASE,
                    crate::lvos::exec::EXEC_LVOS,
                    "exec.library",
                    $name,
                    $handler,
                )
                .unwrap_or_else(|e| panic!("{} should be in EXEC_LVOS: {e}", $name));
        };
    }
    reg!("MakeLibrary", make_library_handler::<C>);
    reg!("MakeFunctions", make_functions_handler::<C>);
    reg!("InitStruct", init_struct_handler::<C>);
    reg!("AddLibrary", add_library_handler::<C>);
    reg!("SumLibrary", sum_library_handler::<C>);
    reg!("RemLibrary", rem_library_handler::<C>);
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

        let made = make_library(&mut mem, &mut heap, &resident, 34, 0, &vectors).unwrap();

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
        let made6 = make_library(&mut mem, &mut heap, &resident, 34, 0, &six_vectors).unwrap();
        assert_eq!(made6.base - made6.alloc_addr, 36);

        // 5 vectors * 6 bytes = 30, rounds up to 32.
        let five_vectors = vec![0u32; 5];
        let made5 = make_library(&mut mem, &mut heap, &resident, 34, 0, &five_vectors).unwrap();
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

        let made = make_library(&mut mem, &mut heap, &resident, 40, 0, &vectors).unwrap();
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

        let made = make_library(&mut mem, &mut heap, &resident, 34, 0, &vectors).unwrap();

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
        let err = make_library(&mut mem, &mut heap, &resident, 33, 0, &[]).unwrap_err();
        assert!(matches!(
            err,
            MakeLibraryError::DataSizeTooSmall { d_size: 33 }
        ));
    }

    /// Writes `bytes` at `addr` (test helper for InitStruct tables).
    fn put_bytes(mem: &mut FlatMemory, addr: u32, bytes: &[u8]) {
        for (i, b) in bytes.iter().enumerate() {
            mem.write_u8(addr + i as u32, *b);
        }
    }

    #[test]
    fn init_struct_decodes_the_initializers_i_macro_encodings() {
        let mut mem = FlatMemory::new(0x1000);
        // INITBYTE 1,$AA ; INITWORD 2,$BBCC ; INITLONG 4,$11223344 ;
        // INITBYTE 300,$EE (24-bit offset form) ; terminator.
        put_bytes(
            &mut mem,
            0x100,
            &[
                0xA0, 1, 0xAA, 0, // byte at +1
                0x90, 2, 0xBB, 0xCC, // word at +2
                0x80, 4, 0x11, 0x22, 0x33, 0x44, // long at +4
                0xE0, 0, 0x01, 0x2C, 0xEE, 0, // byte at +300
                0,
            ],
        );
        init_struct(&mut mem, 0x100, 0x400, 0).unwrap();
        assert_eq!(mem.read_u8(0x401), 0xAA);
        assert_eq!(mem.read_u16(0x402), 0xBBCC);
        assert_eq!(mem.read_u32(0x404), 0x1122_3344);
        assert_eq!(mem.read_u8(0x400 + 300), 0xEE);
    }

    #[test]
    fn init_struct_count_repeat_and_running_destination() {
        let mut mem = FlatMemory::new(0x1000);
        // dd=10 ss=10 n=2: three bytes at +0x10 (then a pad byte so the
        // next command starts on an even address); then dd=01 ss=10 n=3:
        // one source byte repeated four times at the running destination.
        put_bytes(&mut mem, 0x100, &[0xA2, 0x10, 1, 2, 3, 0, 0x63, 9, 0]);
        init_struct(&mut mem, 0x100, 0x400, 0).unwrap();
        assert_eq!(mem.read_u8(0x410), 1);
        assert_eq!(mem.read_u8(0x411), 2);
        assert_eq!(mem.read_u8(0x412), 3);
        for i in 0..4 {
            assert_eq!(mem.read_u8(0x413 + i), 9, "repeat copy {i}");
        }
        assert_eq!(mem.read_u8(0x417), 0, "nothing past the repeat");
    }

    #[test]
    fn init_struct_clears_size_first_and_rejects_bad_source_size() {
        let mut mem = FlatMemory::new(0x1000);
        for i in 0..16 {
            mem.write_u8(0x400 + i, 0xFF);
        }
        put_bytes(&mut mem, 0x100, &[0xA0, 1, 0x55, 0, 0]);
        init_struct(&mut mem, 0x100, 0x400, 8).unwrap();
        assert_eq!(mem.read_u8(0x400), 0);
        assert_eq!(mem.read_u8(0x401), 0x55);
        assert_eq!(mem.read_u8(0x407), 0);
        assert_eq!(mem.read_u8(0x408), 0xFF, "past `size` is untouched");

        put_bytes(&mut mem, 0x100, &[0xB0, 0, 0, 0]);
        assert!(init_struct(&mut mem, 0x100, 0x400, 0).is_err());
    }

    #[test]
    fn make_library_applies_structure_over_the_resident_defaults() {
        let mut mem = FlatMemory::new(0x1000);
        let mut heap = GuestHeap::new(0x100, 0x900);
        let resident = dummy_resident(0x1234, 0);
        // INITWORD lib_Version(20), 7.
        put_bytes(&mut mem, 0xF00, &[0x90, 20, 0, 7, 0]);
        let made = make_library(&mut mem, &mut heap, &resident, 34, 0xF00, &[]).unwrap();
        assert_eq!(mem.read_u16(made.base + LIB_VERSION_OFFSET), 7);
        assert_eq!(mem.read_u32(made.base + LIB_NODE_NAME_OFFSET), 0x1234);
        assert_eq!(mem.read_u16(made.base + LIB_POSSIZE_OFFSET), 34);
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
        )
        .expect("make_library should succeed for scspill.library");
        assert_ne!(made.base, 0);
    }

    /// Opt-in: drives a real [`crate::dispatch::Runtime`] through the L3
    /// `OpenLibrary` state machine against the real `scspill.library`
    /// binary -- its own `initFunc`/`Open` code runs for real, not just
    /// [`make_library`] in isolation (the test above). Same skip-if-
    /// absent posture as `real_scspill_library_loads_and_makes_a_library`.
    #[test]
    fn real_scspill_library_opens_end_to_end_via_openlibrary() {
        let libs_dir = "/Users/simond/amiga/sasc/libs";
        if !std::path::Path::new(libs_dir)
            .join("scspill.library")
            .exists()
        {
            eprintln!(
                "skipping real_scspill_library_opens_end_to_end_via_openlibrary: \
                 {libs_dir}/scspill.library not present"
            );
            return;
        }

        use super::loaded_library_e2e::{jsr, load_words, movea_dn, movea_imm, moveq};
        use crate::backend::{M68kCpu, TRAP_TABLE_END};
        use crate::dispatch::{EXEC_LIBRARY_BASE, Runtime, StartConfig};
        use crate::memory::FlatMemory;
        use crate::vfs::{Vfs, VfsConfig};

        const RTS: u16 = 0x4E75;
        let entry = TRAP_TABLE_END;
        let name = b"scspill.library\0";

        let mut words = Vec::new();
        movea_imm(&mut words, 1, 0); // A1 placeholder, patched below
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        words.push(moveq(0, 0)); // D0 = requested version 0
        jsr(&mut words, 6, -552); // OpenLibrary("scspill.library", 0)
        words.push(movea_dn(6, 0)); // A6 = D0 (returned base), also exit code
        words.push(RTS);
        let str_addr = entry + (words.len() as u32) * 2;
        words[1] = (str_addr >> 16) as u16;
        words[2] = str_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        crate::guestmem::write_c_string(&mut mem, str_addr, name);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: str_addr + name.len() as u32 + 4,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        rt.set_vfs(
            Vfs::new(VfsConfig {
                volumes: vec![("LIBS".to_string(), std::path::PathBuf::from(libs_dir))],
                assigns: vec![],
                auto_assign_root: None,
                cwd: "LIBS:".to_string(),
                ..Default::default()
            })
            .expect("build vfs"),
        );

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_ne!(
            code, 0,
            "OpenLibrary(\"scspill.library\", 0) should return a non-NULL base"
        );
    }

    /// L4 extension of the test above: after a real, successful
    /// `OpenLibrary("scspill.library", 0)`, also `CloseLibrary` it --
    /// running scspill's own real Close (and, on this single-open
    /// program, Expunge-equivalent) code natively, not a synthesized
    /// fixture's -- and assert the whole run still exits cleanly. Same
    /// skip-if-absent posture as the test above.
    #[test]
    fn real_scspill_library_closes_cleanly_after_a_real_open() {
        let libs_dir = "/Users/simond/amiga/sasc/libs";
        if !std::path::Path::new(libs_dir)
            .join("scspill.library")
            .exists()
        {
            eprintln!(
                "skipping real_scspill_library_closes_cleanly_after_a_real_open: \
                 {libs_dir}/scspill.library not present"
            );
            return;
        }

        use super::loaded_library_e2e::{jsr, load_words, move_an_dn, movea_dn, movea_imm, moveq};
        use crate::backend::{M68kCpu, TRAP_TABLE_END};
        use crate::dispatch::{EXEC_LIBRARY_BASE, Runtime, StartConfig};
        use crate::memory::FlatMemory;
        use crate::vfs::{Vfs, VfsConfig};

        const RTS: u16 = 0x4E75;
        let entry = TRAP_TABLE_END;
        let name = b"scspill.library\0";

        let mut words = Vec::new();
        movea_imm(&mut words, 1, 0); // A1 placeholder, patched below
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        words.push(moveq(0, 0)); // D0 = requested version 0
        jsr(&mut words, 6, -552); // OpenLibrary("scspill.library", 0) -> D0 = base
        words.push(movea_dn(3, 0)); // A3 = base (callee-saved, kept across the close)
        words.push(movea_dn(1, 0)); // A1 = base -- CloseLibrary's own argument
        // A6 is still EXEC_LIBRARY_BASE here: OpenLibrary is a "Real"
        // (ROM-resident) library call itself, dispatched via A6 =
        // EXEC_LIBRARY_BASE, and it only ever writes D0 -- the caller's
        // A6 is untouched, so no re-load is needed before this jsr.
        jsr(&mut words, 6, -414); // CloseLibrary(base)
        words.push(move_an_dn(3, 0)); // D0 = A3 (the base) -- final exit code,
        // independent of whatever scspill's own Close vector happened to
        // leave in D0 (real CloseLibrary is documented void -- see
        // execlib.rs's finish_close doc -- so this test doesn't assert
        // on D0's post-Close value at all).
        words.push(RTS);
        let str_addr = entry + (words.len() as u32) * 2;
        words[1] = (str_addr >> 16) as u16;
        words[2] = str_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        crate::guestmem::write_c_string(&mut mem, str_addr, name);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: str_addr + name.len() as u32 + 4,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        rt.set_vfs(
            Vfs::new(VfsConfig {
                volumes: vec![("LIBS".to_string(), std::path::PathBuf::from(libs_dir))],
                assigns: vec![],
                auto_assign_root: None,
                cwd: "LIBS:".to_string(),
                ..Default::default()
            })
            .expect("build vfs"),
        );

        let mut out = Vec::new();
        let code = rt
            .run(&mut out, None)
            .expect("run should succeed -- scspill's own Close/Expunge code must not fault");
        assert_ne!(
            code, 0,
            "the exit code (the base, moved back into D0 after the close) should still be \
             the non-NULL base OpenLibrary returned"
        );
    }
}

/// Phase L3 end-to-end tests: drive a real [`crate::dispatch::Runtime`]
/// through the full `OpenLibrary` disk-load state machine against
/// `fixtures/testlib`/`fixtures/testlib_initfail` (synthesized,
/// hand-authored `RTF_AUTOINIT` libraries -- see `fixtures/testlib.s`'s
/// own doc comment for exactly what each vector does and why). Unlike
/// the pure-mechanics tests above (in-process calls to `find_resident`/
/// `make_library`/...), every test here goes through real A-line trap
/// dispatch: the guest program itself executes `jsr -552(a6)`
/// (`OpenLibrary`), and once opened, `jsr -30(a6)` against the *library's
/// own* relocated code runs natively on the CPU backend -- no host
/// dispatch at all for that call, which is the whole architectural point
/// of loading a real library (`library-device-loading-plan.md` §1.4).
#[cfg(test)]
mod loaded_library_e2e {
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{DispatchError, EXEC_LIBRARY_BASE, Runtime, RuntimeError, StartConfig};
    use crate::memory::{AddressSpace, FlatMemory};
    use crate::vfs::{Vfs, VfsConfig};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    const TESTLIB: &[u8] = include_bytes!("../../../fixtures/testlib");
    const TESTLIB_INITFAIL: &[u8] = include_bytes!("../../../fixtures/testlib_initfail");

    /// Offsets into `fixtures/testlib`'s library base -- must match
    /// `fixtures/testlib.s`'s own `equ` constants exactly.
    const LIB_REVISION_OFFSET: u32 = 22;
    const SEGLIST_MARKER_OFFSET: u32 = 36;
    const ALLOCMEM_MARKER_OFFSET: u32 = 40;
    const INIT_MARKER: u16 = 0x2A2A;
    /// The heap cost of `InitFunc`'s own `AllocMem(4, MEMF_ANY)` call
    /// (`testlib.s`'s header comment) that's never freed -- real
    /// `AllocMem` rounds every request up to `execmem.rs`'s
    /// `MEMBLOCKSIZE` (8) block granularity, so a 4-byte request costs 8.
    /// See `last_close_expunge_leaks_nothing_beyond_initfuncs_own_deliberate_allocmem_block`'s
    /// doc for why this is expected, not a `CloseLibrary` leak.
    const INIT_ALLOCMEM_LEAK_BYTES: u32 = 8;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let path = std::env::temp_dir().join(format!("volamos-execlib-test-{tag}-{pid}-{n}"));
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

    /// Builds a temp dir with a `libs/` subdirectory containing `bytes`
    /// as `libs/<file_name>`, and a [`Vfs`] mapping `LIBS:` to it.
    fn vfs_with_libs_file(tag: &str, file_name: &str, bytes: &[u8]) -> (TempDir, Vfs) {
        let tmp = TempDir::new(tag);
        fs::create_dir(tmp.path().join("libs")).unwrap();
        fs::write(tmp.path().join("libs").join(file_name), bytes).unwrap();
        let vfs = Vfs::new(VfsConfig {
            volumes: vec![("SYS".to_string(), tmp.path().to_path_buf())],
            assigns: vec![("LIBS".to_string(), vec!["SYS:libs".to_string()])],
            auto_assign_root: None,
            cwd: "SYS:".to_string(),
            ..Default::default()
        })
        .expect("build vfs");
        (tmp, vfs)
    }

    /// As [`vfs_with_libs_file`], but mounts [`crate::dispatch::
    /// LIBS_DIR_OVERRIDE_VOLUME`] onto the file's directory instead of
    /// `LIBS:` -- for `--libs-dir` override tests. No `LIBS:` assign at
    /// all, so a miss on the override volume falls through to this
    /// runtime's normal (fake-stub/NULL) resolution, exactly like a
    /// real invocation with `--libs-dir` but no other `-V`/`-a` flags.
    fn vfs_with_override_file(tag: &str, file_name: &str, bytes: &[u8]) -> (TempDir, Vfs) {
        let tmp = TempDir::new(tag);
        fs::write(tmp.path().join(file_name), bytes).unwrap();
        let vfs = Vfs::new(VfsConfig {
            volumes: vec![(
                crate::dispatch::LIBS_DIR_OVERRIDE_VOLUME.to_string(),
                tmp.path().to_path_buf(),
            )],
            cwd: format!("{}:", crate::dispatch::LIBS_DIR_OVERRIDE_VOLUME),
            ..Default::default()
        })
        .expect("build vfs");
        (tmp, vfs)
    }

    pub(super) fn load_words(mem: &mut FlatMemory, addr: u32, words: &[u16]) {
        let mut offset = addr;
        for &w in words {
            mem.write_u16(offset, w);
            offset += 2;
        }
    }

    /// `movea.l #imm32,An` (3 words: opcode + hi + lo).
    pub(super) fn movea_imm(words: &mut Vec<u16>, an: u16, imm: u32) {
        words.push(0x207C | (an << 9));
        words.push((imm >> 16) as u16);
        words.push(imm as u16);
    }

    /// `move.l #imm32,Dn` (3 words: opcode + hi + lo).
    fn move_imm_dn(words: &mut Vec<u16>, dn: u16, imm: u32) {
        words.push(0x203C | (dn << 9));
        words.push((imm >> 16) as u16);
        words.push(imm as u16);
    }

    /// `movea.l Dx,An`.
    pub(super) fn movea_dn(an: u16, dn: u16) -> u16 {
        0x2040 | (an << 9) | dn
    }

    /// `move.l An,Dn`.
    pub(super) fn move_an_dn(an: u16, dn: u16) -> u16 {
        0x2000 | (dn << 9) | 0x008 | an
    }

    /// `movea.l Asrc,Adst` -- both source and dest addressing mode 001
    /// (An direct); unlike [`movea_dn`], which can only move a *data*
    /// register into an address register. Needed wherever a test keeps a
    /// value in a callee-saved address register (e.g. `A2`) across a
    /// library call and then needs it in a different address register
    /// (e.g. `A1`, for `CloseLibrary`'s own argument) without a data
    /// register roundtrip.
    pub(super) fn movea_an(dst_an: u16, src_an: u16) -> u16 {
        0x2000 | (dst_an << 9) | 0x48 | src_an
    }

    /// `jsr <disp16>(An)` (2 words: opcode + displacement).
    pub(super) fn jsr(words: &mut Vec<u16>, an: u16, disp: i32) {
        words.push(0x4EA8 | an);
        words.push(disp as u16);
    }

    /// `moveq #imm,Dn`.
    pub(super) fn moveq(dn: u16, imm: u8) -> u16 {
        0x7000 | (dn << 9) | u16::from(imm)
    }

    /// `move.w <disp16>(An),Dn` (2 words: opcode + displacement).
    fn move_w_disp(words: &mut Vec<u16>, an: u16, disp: i16, dn: u16) {
        words.push(0x3028 | (dn << 9) | an);
        words.push(disp as u16);
    }

    const RTS: u16 = 0x4E75;

    /// Runs `words` (with `lib_name` written just past the code and `A1`
    /// pre-patched to point at it) against a [`Vfs`] whose `LIBS:` volume
    /// contains `lib_bytes` as `lib_file_name`. Returns the run's exit
    /// code (i.e. whatever the program left in `D0` when it hit the exit
    /// stub) and the resulting [`Runtime`] (still alive, so a test can
    /// inspect [`Runtime::memory`] afterward).
    fn run_against_library(
        tag: &str,
        lib_file_name: &str,
        lib_bytes: &[u8],
        lib_name: &[u8],
        mut words: Vec<u16>,
    ) -> (Result<i32, RuntimeError>, Runtime<M68kCpu>) {
        let entry = TRAP_TABLE_END;
        // A1 is always patched at word index 1 (right after the first
        // `movea.l #imm32,a1` opcode word) by every test program below --
        // see each caller.
        let str_addr = entry + (words.len() as u32) * 2;
        words[1] = (str_addr >> 16) as u16;
        words[2] = str_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        crate::guestmem::write_c_string(&mut mem, str_addr, lib_name);

        let (_tmp, vfs) = vfs_with_libs_file(tag, lib_file_name, lib_bytes);
        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: str_addr + lib_name.len() as u32 + 4,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        rt.set_vfs(vfs);
        let mut out = Vec::new();
        let result = rt.run(&mut out, None);
        (result, rt)
    }

    /// As [`run_against_library`], but mounts `lib_bytes` onto the
    /// `--libs-dir` override volume ([`vfs_with_override_file`]) and
    /// calls [`Runtime::enable_libs_dir_override`] instead of
    /// configuring an ordinary `LIBS:` assign.
    fn run_against_override_library(
        tag: &str,
        lib_file_name: &str,
        lib_bytes: &[u8],
        lib_name: &[u8],
        mut words: Vec<u16>,
    ) -> (Result<i32, RuntimeError>, Runtime<M68kCpu>) {
        let entry = TRAP_TABLE_END;
        let str_addr = entry + (words.len() as u32) * 2;
        words[1] = (str_addr >> 16) as u16;
        words[2] = str_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        crate::guestmem::write_c_string(&mut mem, str_addr, lib_name);

        let (_tmp, vfs) = vfs_with_override_file(tag, lib_file_name, lib_bytes);
        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: str_addr + lib_name.len() as u32 + 4,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        rt.set_vfs(vfs);
        rt.enable_libs_dir_override();
        let mut out = Vec::new();
        let result = rt.run(&mut out, None);
        (result, rt)
    }

    /// Test 1 + 2 + 7 (see the delegated brief): opens `test.library`,
    /// calls its first user vector (LVO -30) *natively* -- no host
    /// dispatch at all for that call -- and returns the *library base*
    /// itself as the exit code (rather than the user vector's own D0),
    /// so the test can inspect the base's data area afterward: the
    /// `initFunc`-ran marker (`lib_Revision`), the `A0`=segList marker,
    /// and the AllocMem-result marker (proving the trampoline supports a
    /// *nested* library call from inside `initFunc` -- the L2 trampoline
    /// primitive's whole reason for existing).
    #[test]
    fn open_calls_user_vector_natively_and_init_really_ran() {
        let mut words = Vec::new();
        movea_imm(&mut words, 1, 0); // A1 placeholder, patched below
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        words.push(moveq(0, 0)); // D0 = requested version 0
        jsr(&mut words, 6, -552); // OpenLibrary("test.library", 0) -> D0 = base
        words.push(movea_dn(6, 0)); // A6 = base
        words.push(movea_dn(3, 0)); // A3 = base (kept, for the exit-code move below)
        jsr(&mut words, 6, -30); // call the first user vector natively
        words.push(moveq(0, 0)); // (result unused here -- see the OpenCnt test)
        words.push(move_an_dn(3, 0)); // D0 = base (the exit code)
        words.push(RTS);

        let (result, rt) = run_against_library(
            "open-user-vector",
            "test.library",
            TESTLIB,
            b"test.library\0",
            words,
        );
        let base = result.expect("run should succeed") as u32;
        assert_ne!(base, 0, "OpenLibrary(\"test.library\", 0) should succeed");

        let mem = rt.memory();
        assert_eq!(
            mem.read_u16(base + LIB_REVISION_OFFSET),
            INIT_MARKER,
            "initFunc should have run and written its marker into lib_Revision"
        );
        assert_ne!(
            mem.read_u32(base + SEGLIST_MARKER_OFFSET),
            0,
            "initFunc's A0 (segList BPTR) marker should be non-zero"
        );
        assert_ne!(
            mem.read_u32(base + ALLOCMEM_MARKER_OFFSET),
            0,
            "initFunc's nested AllocMem call (mid-init, via the L2 trampoline) should \
             have succeeded and stored a non-NULL pointer"
        );
    }

    /// Test 1's other half: the user vector's own return value (`moveq
    /// #42,d0`) really is what a native `jsr -30(a6)` produces -- proving
    /// the call executed the library's own relocated code, not a host
    /// trap.
    #[test]
    fn user_vector_return_value_comes_from_native_execution() {
        let mut words = Vec::new();
        movea_imm(&mut words, 1, 0);
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        words.push(moveq(0, 0));
        jsr(&mut words, 6, -552); // OpenLibrary -> D0 = base
        words.push(movea_dn(6, 0)); // A6 = base
        jsr(&mut words, 6, -30); // UserFunc -> D0 = 42
        words.push(RTS);

        let (result, _rt) = run_against_library(
            "open-user-vector-value",
            "test.library",
            TESTLIB,
            b"test.library\0",
            words,
        );
        assert_eq!(
            result.expect("run should succeed"),
            42,
            "the user vector's own moveq #42,d0 should reach the exit code untouched"
        );
    }

    /// Test 3: `lib_OpenCnt` is 1 after one open.
    #[test]
    fn open_cnt_is_one_after_a_single_open() {
        let mut words = Vec::new();
        movea_imm(&mut words, 1, 0);
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        words.push(moveq(0, 0));
        jsr(&mut words, 6, -552); // OpenLibrary -> D0 = base
        words.push(movea_dn(2, 0)); // A2 = base
        words.push(moveq(0, 0)); // D0 = 0 (clears the upper word before the .w load)
        move_w_disp(&mut words, 2, 32, 0); // D0 = lib_OpenCnt (word)
        words.push(RTS);

        let (result, _rt) = run_against_library(
            "opencnt-one",
            "test.library",
            TESTLIB,
            b"test.library\0",
            words,
        );
        assert_eq!(result.expect("run should succeed"), 1);
    }

    /// Test 3 continued: `lib_OpenCnt` is 2 after opening twice -- the
    /// second open must go through the real `Open` vector again (the
    /// [`crate::dispatch::LibraryKind::Loaded`] repeat-open path,
    /// [`reopen`]), which is the only thing that increments it a second
    /// time (`OpenLibrary` itself never touches `lib_OpenCnt` -- see
    /// `library-device-loading-plan.md` §2.4).
    #[test]
    fn open_cnt_is_two_after_opening_twice() {
        let mut words = Vec::new();
        movea_imm(&mut words, 1, 0);
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        words.push(moveq(0, 0));
        jsr(&mut words, 6, -552); // open #1
        words.push(movea_dn(2, 0)); // A2 = base
        // A second `movea.l #imm32,a1` placeholder starts here -- captured
        // dynamically (rather than hand-computed) so a future edit to the
        // instructions above this point can't silently desync the patch
        // offset below.
        let second_placeholder_idx = words.len();
        movea_imm(&mut words, 1, 0); // A1 placeholder (patched to the same string address)
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        words.push(moveq(0, 0));
        jsr(&mut words, 6, -552); // open #2 (registry Loaded hit -> reopen)
        words.push(moveq(0, 0));
        move_w_disp(&mut words, 2, 32, 0); // D0 = lib_OpenCnt
        words.push(RTS);

        // Two `movea.l #imm32,a1` placeholders now exist -- both must
        // point at the same string, so patch both explicitly rather than
        // relying on run_against_library's single-patch convention.
        let entry = TRAP_TABLE_END;
        let str_addr = entry + (words.len() as u32) * 2;
        words[1] = (str_addr >> 16) as u16;
        words[2] = str_addr as u16;
        words[second_placeholder_idx + 1] = (str_addr >> 16) as u16;
        words[second_placeholder_idx + 2] = str_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        let name = b"test.library\0";
        crate::guestmem::write_c_string(&mut mem, str_addr, name);
        let (_tmp, vfs) = vfs_with_libs_file("opencnt-two", "test.library", TESTLIB);
        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: str_addr + name.len() as u32 + 4,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        rt.set_vfs(vfs);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 2, "lib_OpenCnt should be 2 after two real opens");
    }

    /// Test 4: `OpenLibrary(name, 999)` -> `D0 == 0` (the version check
    /// itself, read directly as this tiny program's exit code).
    #[test]
    fn version_refusal_returns_null() {
        let mut words = Vec::new();
        movea_imm(&mut words, 1, 0);
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        move_imm_dn(&mut words, 0, 999); // D0 = requested version 999
        jsr(&mut words, 6, -552); // OpenLibrary(name, 999) -> D0 (exit code)
        words.push(RTS);

        let (result, _rt) = run_against_library(
            "version-refusal",
            "test.library",
            TESTLIB,
            b"test.library\0",
            words,
        );
        assert_eq!(
            result.expect("run should succeed"),
            0,
            "test.library's real lib_Version (1) is below the requested 999"
        );
    }

    /// Test 4 continued: the caller's `A6` is restored across a
    /// version-refused open -- read directly via `move.l a6,d0`
    /// immediately after the refused call, before anything else could
    /// touch `A6`.
    #[test]
    fn version_refusal_preserves_callers_a6() {
        let mut words = Vec::new();
        movea_imm(&mut words, 1, 0);
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        move_imm_dn(&mut words, 0, 999);
        jsr(&mut words, 6, -552); // refused -> D0 = 0
        words.push(move_an_dn(6, 0)); // D0 = A6 (should still be EXEC_LIBRARY_BASE)
        words.push(RTS);

        let (result, _rt) = run_against_library(
            "version-refusal-a6",
            "test.library",
            TESTLIB,
            b"test.library\0",
            words,
        );
        assert_eq!(
            result.expect("run should succeed") as u32,
            EXEC_LIBRARY_BASE,
            "A6 must be restored to the caller's own value after a version-refused open"
        );
    }

    /// Test 4 continued: real Kickstart checks `lib_Version` with a
    /// 68000 `CMP.W`, so garbage in the high word of the requested-
    /// version register is ignored -- `OpenLibrary(name, $30000000)`
    /// must succeed against `test.library`'s real `lib_Version` (1)
    /// exactly as `OpenLibrary(name, 0)` would, because only the low
    /// word ($0000) is ever compared. This mirrors a real AROS bug fix
    /// (a caller passing `$30000000` in D0 that real ROMs accept but a
    /// naive full-32-bit-compare implementation -- like AROS's, and
    /// like this runtime's before this test was added -- wrongly
    /// refuses).
    #[test]
    fn version_check_ignores_high_word_garbage() {
        let mut words = Vec::new();
        movea_imm(&mut words, 1, 0);
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        move_imm_dn(&mut words, 0, 0x3000_0000); // low word 0 <= lib_Version 1
        jsr(&mut words, 6, -552); // OpenLibrary(name, $30000000) -> D0 (exit code)
        words.push(RTS);

        let (result, _rt) = run_against_library(
            "version-high-word-garbage",
            "test.library",
            TESTLIB,
            b"test.library\0",
            words,
        );
        assert_ne!(
            result.expect("run should succeed"),
            0,
            "a CMP.W-style version check must ignore D0's high word and succeed"
        );
    }

    /// Builds and runs a program that calls `OpenLibrary("initfail.library",
    /// 0)` `n` times in a row (each one refused by `testlib_initfail`'s
    /// unconditionally-`NULL`-returning `initFunc`), then returns the
    /// [`Runtime`]'s guest heap's free-byte count afterward. `load_end` is
    /// a fixed constant (independent of `n`, generous enough for `n` up
    /// to a handful) so the heap's own starting address -- and hence this
    /// count -- is directly comparable across different `n` values; if
    /// [`after_init`]'s `NULL`-init-result cleanup ever leaked the
    /// seglist or [`make_library`] allocation, more failed attempts would
    /// consume more heap space and this count would drop with `n`.
    fn free_bytes_after_n_failed_opens(n: usize) -> u32 {
        let entry = TRAP_TABLE_END;
        let fixed_load_end = entry + 0x400;

        let mut words = Vec::new();
        let mut placeholder_indices = Vec::new();
        for _ in 0..n {
            placeholder_indices.push(words.len());
            movea_imm(&mut words, 1, 0);
            movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
            words.push(moveq(0, 0));
            jsr(&mut words, 6, -552);
        }
        words.push(RTS);

        let str_addr = entry + (words.len() as u32) * 2;
        for idx in placeholder_indices {
            words[idx + 1] = (str_addr >> 16) as u16;
            words[idx + 2] = str_addr as u16;
        }

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        let name = b"initfail.library\0";
        crate::guestmem::write_c_string(&mut mem, str_addr, name);
        assert!(
            str_addr + name.len() as u32 + 4 <= fixed_load_end,
            "fixed_load_end must stay generous enough for the largest n this test uses"
        );

        let (_tmp, vfs) = vfs_with_libs_file(
            &format!("initfail-{n}"),
            "initfail.library",
            TESTLIB_INITFAIL,
        );
        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: fixed_load_end,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        rt.set_vfs(vfs);
        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed");
        rt.heap_mut().free_bytes()
    }

    /// Test 5: an `initFunc` that returns `NULL` -> `OpenLibrary` returns
    /// `NULL`, and the seglist/[`make_library`] allocation are unwound
    /// cleanly, with no leak -- proven via heap state (per the delegated
    /// brief's own suggested alternative): 1 vs 5 failed opens of the
    /// same never-succeeding library leave the guest heap with exactly
    /// the same amount of free space, since a leak-free cleanup returns
    /// every byte a failed attempt touched.
    #[test]
    fn init_func_returning_null_fails_the_open_and_does_not_leak() {
        let free_after_one = free_bytes_after_n_failed_opens(1);
        let free_after_five = free_bytes_after_n_failed_opens(5);
        assert_eq!(
            free_after_one, free_after_five,
            "heap free space should be identical after 1 vs 5 failed opens of the same \
             library -- each initFunc-refused open must fully unwind its seglist and \
             make_library allocation, or repeated attempts would leak heap space"
        );
    }

    /// Test 6: a file that parses as a hunk executable but has no
    /// `struct Resident` -- `fixtures/hello` fits exactly (see
    /// `fixtures/README.md`), it's a plain two-hunk CLI program with no
    /// romtag at all. `OpenLibrary` should fall back to the pre-existing
    /// fake-stub path: a non-NULL base is returned, but calling a vector
    /// on it fails with the fake-library diagnostic ([`crate::dispatch::
    /// DispatchError::HandlerFailed`] naming the library) -- the same
    /// observable behavior the existing `open_library_of_unknown_name_
    /// found_on_disk_auto_creates_fake_and_succeeds` dispatch.rs test
    /// asserts for a library that isn't a hunk file at all. Asserting on
    /// stderr's loud fallback note directly is awkward from an in-process
    /// test (see the delegated brief); this asserts the *behavior* that
    /// note announces instead.
    #[test]
    fn library_with_no_resident_falls_back_to_fake_stub() {
        const HELLO: &[u8] = include_bytes!("../../../fixtures/hello");

        let mut words = Vec::new();
        movea_imm(&mut words, 1, 0);
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        words.push(moveq(0, 0));
        jsr(&mut words, 6, -552); // OpenLibrary -> D0 = fake base
        words.push(movea_dn(6, 0)); // A6 = fake base
        jsr(&mut words, 6, -6); // call an arbitrary vector on it
        words.push(RTS);

        let (result, _rt) = run_against_library(
            "no-resident",
            "nores.library",
            HELLO,
            b"nores.library\0",
            words,
        );
        match result.unwrap_err() {
            RuntimeError::Dispatch(DispatchError::HandlerFailed { library, lvo, .. }) => {
                assert_eq!(library, "nores.library");
                assert_eq!(lvo, -6);
            }
            other => panic!("expected a HandlerFailed naming nores.library, got {other:?}"),
        }
    }

    /// `--libs-dir` override: a real library present on the override
    /// volume loads and runs for real, exactly like the ordinary
    /// `LIBS:`-based tests above -- `OpenLibrary` returns a real base
    /// whose `initFunc` genuinely ran (`lib_Revision`'s marker).
    #[test]
    fn libs_dir_override_present_loads_the_real_library() {
        let mut words = Vec::new();
        movea_imm(&mut words, 1, 0);
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        words.push(moveq(0, 0));
        jsr(&mut words, 6, -552); // OpenLibrary -> D0 = base
        words.push(RTS);

        let (result, rt) = run_against_override_library(
            "override-present",
            "test.library",
            TESTLIB,
            b"test.library\0",
            words,
        );
        let base = result.expect("run should succeed") as u32;
        assert_ne!(
            base, 0,
            "OpenLibrary should succeed via the override volume"
        );
        assert_eq!(
            rt.memory().read_u16(base + LIB_REVISION_OFFSET),
            INIT_MARKER,
            "the override library's own initFunc should have really run"
        );
    }

    /// `--libs-dir` override: a miss (the override volume doesn't have
    /// this name) falls straight through to this runtime's normal
    /// resolution, unchanged -- here, no `LIBS:` at all and not a
    /// standard Workbench library, so `OpenLibrary` returns `NULL`
    /// exactly like it would with no `--libs-dir` configured at all.
    #[test]
    fn libs_dir_override_miss_falls_through_unchanged() {
        let mut words = Vec::new();
        movea_imm(&mut words, 1, 0);
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        words.push(moveq(0, 0));
        jsr(&mut words, 6, -552); // OpenLibrary("other.library", 0) -> D0
        words.push(RTS);

        // The override volume only has `test.library`; this program asks
        // for a different name, so nothing on it matches.
        let (result, _rt) = run_against_override_library(
            "override-miss",
            "test.library",
            TESTLIB,
            b"other.library\0",
            words,
        );
        assert_eq!(
            result.expect("run should succeed"),
            0,
            "an unmatched name should fall through to NULL, same as with no --libs-dir"
        );
    }

    /// `--libs-dir` override: a file present at the override's exact
    /// name but structurally invalid (no `Resident`/romtag -- the same
    /// `fixtures/hello` fixture [`library_with_no_resident_falls_back_to_fake_stub`]
    /// uses) is a **hard error**, not a silent fallback to the
    /// fake-stub path -- unlike the ordinary `LIBS:` tier, a
    /// `--libs-dir` hit was explicitly pointed at by the user, so a
    /// structural failure here is much more likely a real mistake
    /// worth surfacing loudly.
    #[test]
    fn libs_dir_override_present_but_broken_is_a_hard_error() {
        const HELLO: &[u8] = include_bytes!("../../../fixtures/hello");

        let mut words = Vec::new();
        movea_imm(&mut words, 1, 0);
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        words.push(moveq(0, 0));
        jsr(&mut words, 6, -552); // OpenLibrary -> should hard-error, never return
        words.push(RTS);

        let (result, _rt) = run_against_override_library(
            "override-broken",
            "nores.library",
            HELLO,
            b"nores.library\0",
            words,
        );
        match result.unwrap_err() {
            RuntimeError::Dispatch(DispatchError::HandlerFailed { handler_name, .. }) => {
                assert_eq!(handler_name, "OpenLibrary");
            }
            other => panic!("expected a hard HandlerFailed error, got {other:?}"),
        }
    }

    /// Test 8: after a successful load, `ExecBase.LibList` really is
    /// walkable and contains a node whose `ln_Name` reads
    /// `"test.library"` -- the `AddLibrary` equivalent
    /// [`crate::execlib::after_init`] performs via `execlist::
    /// add_tail_impl`.
    #[test]
    fn liblist_contains_the_loaded_library_by_name() {
        use crate::dispatch::EXEC_BASE_LIBLIST_OFFSET;
        use crate::execlist::{LH_HEAD, LN_NAME, LN_SUCC};
        use crate::guestmem::read_c_string;

        let mut words = Vec::new();
        movea_imm(&mut words, 1, 0);
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        words.push(moveq(0, 0));
        jsr(&mut words, 6, -552); // OpenLibrary -> D0 = base
        words.push(RTS);

        let (result, rt) =
            run_against_library("liblist", "test.library", TESTLIB, b"test.library\0", words);
        result.expect("run should succeed");

        let mem = rt.memory();
        let list_addr = EXEC_LIBRARY_BASE + EXEC_BASE_LIBLIST_OFFSET;
        let mut node = mem.read_u32(list_addr + LH_HEAD);
        let mut found = false;
        while mem.read_u32(node + LN_SUCC) != 0 {
            let name_ptr = mem.read_u32(node + LN_NAME);
            if read_c_string(mem, name_ptr) == b"test.library" {
                found = true;
                break;
            }
            node = mem.read_u32(node + LN_SUCC);
        }
        assert!(
            found,
            "ExecBase.LibList should contain a node named \"test.library\" after a \
             successful load"
        );
    }

    // --- L4: CloseLibrary end-to-end tests ---
    //
    // `library-device-loading-plan.md` §2.4 / phase L4's own brief: these
    // drive `fixtures/testlib`'s real `CloseFunc` (see testlib.s's header
    // comment for what it does -- decrement lib_OpenCnt, return the
    // stored segList exactly on the last close) through the same real
    // trap-dispatch path as the L3 tests above, now exercising
    // execlib.rs's begin_close/finish_close.

    /// Close with one of two opens outstanding must leave the library
    /// resident and fully callable: `lib_OpenCnt` lands on 1 (not 0), and
    /// a native call through its own jump table (LVO -30) still works --
    /// if the close had incorrectly unlinked/freed the library despite
    /// the remaining open, this call would be against no-longer-valid
    /// state (even though this runtime's `FlatMemory` wouldn't
    /// necessarily fault on it, `begin_close`'s heap `free`/`UnLoadSeg`
    /// wrongly firing here is exactly the bug this test exists to catch
    /// indirectly, via the final OpenCnt read landing on garbage instead
    /// of 1).
    #[test]
    fn close_with_one_of_two_opens_leaves_library_resident_and_functional() {
        let mut words = Vec::new();
        movea_imm(&mut words, 1, 0); // placeholder #1, patched below
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        words.push(moveq(0, 0));
        jsr(&mut words, 6, -552); // open #1 -> D0 = base
        words.push(movea_dn(2, 0)); // A2 = base (kept)

        let second_placeholder_idx = words.len();
        movea_imm(&mut words, 1, 0); // placeholder #2 (same string, patched below)
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        words.push(moveq(0, 0));
        jsr(&mut words, 6, -552); // open #2 -> D0 = base (same base, real reopen)

        // Close once: two opens are outstanding, so testlib's CloseFunc
        // decrements lib_OpenCnt to 1 (non-zero) and returns 0 -- the
        // library must stay resident.
        words.push(movea_an(1, 2)); // A1 = A2 (base)
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        jsr(&mut words, 6, -414); // CloseLibrary

        // Call the first user vector natively -- proves the library is
        // still genuinely callable post-close (discarded result; a crash
        // here would fail the whole `run`).
        words.push(movea_an(6, 2)); // A6 = A2 (base)
        jsr(&mut words, 6, -30); // UserFunc -> D0 = 42 (discarded below)

        words.push(moveq(0, 0)); // clear D0's upper word before the .w load
        move_w_disp(&mut words, 2, 32, 0); // D0 = lib_OpenCnt
        words.push(RTS);

        // Two placeholders -> patch both explicitly, same pattern as
        // open_cnt_is_two_after_opening_twice.
        let entry = TRAP_TABLE_END;
        let str_addr = entry + (words.len() as u32) * 2;
        words[1] = (str_addr >> 16) as u16;
        words[2] = str_addr as u16;
        words[second_placeholder_idx + 1] = (str_addr >> 16) as u16;
        words[second_placeholder_idx + 2] = str_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        let name = b"test.library\0";
        crate::guestmem::write_c_string(&mut mem, str_addr, name);
        let (_tmp, vfs) = vfs_with_libs_file("close-one-of-two", "test.library", TESTLIB);
        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: str_addr + name.len() as u32 + 4,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        rt.set_vfs(vfs);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect(
            "run should succeed -- including the native post-close user-vector call, which \
             would be against corrupted state if the close had wrongly expunged the library",
        );
        assert_eq!(
            code, 1,
            "lib_OpenCnt should be 1 (still resident) after closing one of two opens"
        );
    }

    /// The last close (only one open outstanding) must genuinely expunge
    /// the library: `ExecBase.LibList` no longer contains its node
    /// afterward -- the [`crate::execlist::remove_impl`] half of
    /// [`finish_close`]'s delayed-expunge path, proven the same way
    /// [`liblist_contains_the_loaded_library_by_name`] proves the
    /// opposite (`add_tail_impl`, on open).
    #[test]
    fn last_close_removes_the_liblist_node() {
        use crate::dispatch::EXEC_BASE_LIBLIST_OFFSET;
        use crate::execlist::{LH_HEAD, LN_NAME, LN_SUCC};
        use crate::guestmem::read_c_string;

        let mut words = Vec::new();
        movea_imm(&mut words, 1, 0);
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        words.push(moveq(0, 0));
        jsr(&mut words, 6, -552); // open -> D0 = base
        words.push(movea_dn(1, 0)); // A1 = base
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        jsr(&mut words, 6, -414); // CloseLibrary -- last close, expunges
        words.push(RTS);

        let (result, rt) = run_against_library(
            "expunge-liblist",
            "test.library",
            TESTLIB,
            b"test.library\0",
            words,
        );
        result.expect("run should succeed");

        let mem = rt.memory();
        let list_addr = EXEC_LIBRARY_BASE + EXEC_BASE_LIBLIST_OFFSET;
        let mut node = mem.read_u32(list_addr + LH_HEAD);
        let mut found = false;
        while mem.read_u32(node + LN_SUCC) != 0 {
            let name_ptr = mem.read_u32(node + LN_NAME);
            if read_c_string(mem, name_ptr) == b"test.library" {
                found = true;
                break;
            }
            node = mem.read_u32(node + LN_SUCC);
        }
        assert!(
            !found,
            "ExecBase.LibList should no longer contain test.library after the last close \
             expunged it"
        );
    }

    /// Builds and runs a program that either (a) opens `test.library`
    /// once and closes it once (the last close, triggering a real
    /// expunge), or (b) does neither -- just an immediate `rts` -- then
    /// returns the resulting [`Runtime`]'s guest heap's free-byte count.
    /// `load_end` is a fixed constant, independent of which branch runs,
    /// so the heap's own starting address (and hence this count) is
    /// directly comparable across both: identical free-byte counts after
    /// (a) and (b) is a strong end-to-end proof that a full open+close
    /// cycle -- both the seglist ([`crate::dosseg::DosState::unload_seg`])
    /// and the [`make_library`] base allocation ([`GuestHeap::free`]) --
    /// leaves absolutely nothing behind, matching
    /// `init_func_returning_null_fails_the_open_and_does_not_leak`'s own
    /// technique for the open-side failure path.
    fn free_bytes_after_full_open_close_cycle(open_and_close: bool) -> u32 {
        let entry = TRAP_TABLE_END;
        let fixed_load_end = entry + 0x400;

        let mut mem = FlatMemory::new(0x2_0000);
        let vfs_tag = if open_and_close {
            "expunge-noleak-opened"
        } else {
            "expunge-noleak-baseline"
        };
        let (_tmp, vfs) = vfs_with_libs_file(vfs_tag, "test.library", TESTLIB);

        let mut rt = if open_and_close {
            let mut words = Vec::new();
            movea_imm(&mut words, 1, 0); // placeholder, patched below
            movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
            words.push(moveq(0, 0));
            jsr(&mut words, 6, -552); // open -> D0 = base
            words.push(movea_dn(1, 0)); // A1 = base
            movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
            jsr(&mut words, 6, -414); // CloseLibrary -- last close, expunges
            words.push(RTS);

            let str_addr = entry + (words.len() as u32) * 2;
            words[1] = (str_addr >> 16) as u16;
            words[2] = str_addr as u16;
            let name = b"test.library\0";
            assert!(
                str_addr + name.len() as u32 + 4 <= fixed_load_end,
                "fixed_load_end must stay generous enough for this program"
            );

            load_words(&mut mem, entry, &words);
            crate::guestmem::write_c_string(&mut mem, str_addr, name);
            Runtime::new(
                M68kCpu::new(),
                mem,
                StartConfig {
                    entry,
                    load_end: fixed_load_end,
                    args: Vec::new(),
                    ..StartConfig::default()
                },
            )
        } else {
            load_words(&mut mem, entry, &[RTS]);
            Runtime::new(
                M68kCpu::new(),
                mem,
                StartConfig {
                    entry,
                    load_end: fixed_load_end,
                    args: Vec::new(),
                    ..StartConfig::default()
                },
            )
        };
        rt.set_vfs(vfs);
        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed");
        rt.heap_mut().free_bytes()
    }

    /// Test (plan §2.4 / L4 brief): a full open+close cycle of a library
    /// whose last close expunges it returns the heap to (almost) exactly
    /// as much free space as never having opened it at all -- see
    /// [`free_bytes_after_full_open_close_cycle`]'s doc for why this is a
    /// strong no-leak proof covering both the seglist and the
    /// [`make_library`] base allocation.
    ///
    /// "Almost": the two counts differ by exactly
    /// [`INIT_ALLOCMEM_LEAK_BYTES`] -- not a bug in `CloseLibrary`, but a
    /// deliberate, permanent feature of *this fixture*. `testlib.s`'s
    /// `InitFunc` calls `AllocMem(4, MEMF_ANY)` mid-init purely to prove
    /// the L2 trampoline supports a *nested* library call (see that
    /// file's header comment and `ALLOCMEM_MARKER_OFFSET`'s doc above);
    /// it stores the result as a marker and never frees it, and nothing
    /// in `CloseFunc` frees it either -- there's no reason it should
    /// (it's not part of the library's own `MakeLibrary`-allocated base
    /// or its seglist, just an ordinary allocation the library's `Open`
    /// contract never promises to release on `Close`). Real `AllocMem`
    /// also rounds every request up to an 8-byte block granularity
    /// (`execmem.rs`'s `MEMBLOCKSIZE`), so the 4-byte request costs 8.
    /// Found the hard way: this test originally asserted plain equality
    /// and failed with a genuine, reproducible 8-byte gap, which a
    /// `finish_close` debug trace traced to *outside* both the seglist's
    /// and the base's freed ranges entirely -- i.e. real, if
    /// fixture-specific, not a `CloseLibrary` bug.
    #[test]
    fn last_close_expunge_leaks_nothing_beyond_initfuncs_own_deliberate_allocmem_block() {
        let never_opened = free_bytes_after_full_open_close_cycle(false);
        let opened_then_closed = free_bytes_after_full_open_close_cycle(true);
        assert_eq!(
            never_opened - INIT_ALLOCMEM_LEAK_BYTES,
            opened_then_closed,
            "heap free space after a full open+close cycle should be exactly \
             INIT_ALLOCMEM_LEAK_BYTES less than never having opened test.library at all -- any \
             other difference means the expunge path leaked the seglist or the make_library \
             base allocation (the two things CloseLibrary's delayed-expunge path actually owns)"
        );
    }

    /// Reload after expunge: opening the same name again after its last
    /// close expunged it must be a genuinely fresh disk load -- not a
    /// stale cached base (that base is gone -- see
    /// [`last_close_removes_the_liblist_node`]) and not a
    /// [`crate::execlib::reopen`] (that path requires a still-registered
    /// [`crate::dispatch::LibraryKind::Loaded`] entry, which
    /// [`crate::dispatch::LibraryRegistry::unregister_loaded`] just
    /// removed). Proven exactly like the L3 init test
    /// (`open_calls_user_vector_natively_and_init_really_ran`): the
    /// reload's own `initFunc` must have run again (the `lib_Revision`
    /// marker is freshly written), and its own `Open` vector must have
    /// run too (`lib_OpenCnt` reads a fresh 1, not a continuation of the
    /// pre-expunge count).
    #[test]
    fn open_after_expunge_reloads_fresh_from_disk() {
        let mut words = Vec::new();
        movea_imm(&mut words, 1, 0); // placeholder #1, patched below
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        words.push(moveq(0, 0));
        jsr(&mut words, 6, -552); // open #1 -> D0 = base1
        words.push(movea_dn(1, 0)); // A1 = base1
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        jsr(&mut words, 6, -414); // CloseLibrary -- last close, expunges base1

        let second_placeholder_idx = words.len();
        movea_imm(&mut words, 1, 0); // placeholder #2 (same string, patched below)
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        words.push(moveq(0, 0));
        jsr(&mut words, 6, -552); // open #2 -> D0 = base2, a fresh load
        words.push(RTS); // exit code = base2

        let entry = TRAP_TABLE_END;
        let str_addr = entry + (words.len() as u32) * 2;
        words[1] = (str_addr >> 16) as u16;
        words[2] = str_addr as u16;
        words[second_placeholder_idx + 1] = (str_addr >> 16) as u16;
        words[second_placeholder_idx + 2] = str_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        let name = b"test.library\0";
        crate::guestmem::write_c_string(&mut mem, str_addr, name);
        let (_tmp, vfs) = vfs_with_libs_file("reload-after-expunge", "test.library", TESTLIB);
        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: str_addr + name.len() as u32 + 4,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        rt.set_vfs(vfs);
        let mut out = Vec::new();
        let base2 = rt.run(&mut out, None).expect("run should succeed") as u32;
        assert_ne!(
            base2, 0,
            "the second open, after the first's expunge, should succeed as a fresh load"
        );

        let mem = rt.memory();
        assert_eq!(
            mem.read_u16(base2 + LIB_REVISION_OFFSET),
            INIT_MARKER,
            "initFunc should have run again on the fresh reload, rewriting its marker"
        );
        assert_eq!(
            mem.read_u16(base2 + 32), // LIB_OPENCNT_OFFSET
            1,
            "lib_OpenCnt should be a fresh 1 after the reload's own Open vector ran, not a \
             continuation of the pre-expunge count"
        );
    }

    // --- Non-RTF_AUTOINIT libraries (issue #133) ---
    //
    // `fixtures/testlib_noauto*` (see testlib_noauto.s): an NT_LIBRARY
    // Resident with RT_FLAGS = 0 whose rt_Init is code that builds the
    // library itself via MakeLibrary (+ an InitStruct table) and AddLibrary.

    const TESTLIB_NOAUTO: &[u8] = include_bytes!("../../../fixtures/testlib_noauto");
    const TESTLIB_NOAUTO_NULLRET: &[u8] =
        include_bytes!("../../../fixtures/testlib_noauto_nullret");
    const TESTLIB_NOAUTO_REFUSE: &[u8] = include_bytes!("../../../fixtures/testlib_noauto_refuse");
    const NOAUTO_NAME: &[u8] = b"noauto.library\0";

    /// Number of `ExecBase.LibList` nodes named `name`.
    fn liblist_count(mem: &FlatMemory, name: &[u8]) -> usize {
        use crate::dispatch::EXEC_BASE_LIBLIST_OFFSET;
        use crate::execlist::{LH_HEAD, LN_NAME, LN_SUCC};
        let mut node = mem.read_u32(EXEC_LIBRARY_BASE + EXEC_BASE_LIBLIST_OFFSET + LH_HEAD);
        let mut count = 0;
        while mem.read_u32(node + LN_SUCC) != 0 {
            let name_ptr = mem.read_u32(node + LN_NAME);
            if name_ptr != 0 && crate::guestmem::read_c_string(mem, name_ptr) == name {
                count += 1;
            }
            node = mem.read_u32(node + LN_SUCC);
        }
        count
    }

    /// `OpenLibrary("noauto.library", 0)`, call the user vector (LVO -30)
    /// natively, exit with the library base.
    fn open_call_return_base_program() -> Vec<u16> {
        let mut words = Vec::new();
        movea_imm(&mut words, 1, 0);
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        words.push(moveq(0, 0));
        jsr(&mut words, 6, -552);
        words.push(movea_dn(6, 0));
        words.push(movea_dn(3, 0));
        jsr(&mut words, 6, -30);
        words.push(move_an_dn(3, 0));
        words.push(RTS);
        words
    }

    #[test]
    fn non_autoinit_library_builds_itself_and_opens() {
        let (result, rt) = run_against_library(
            "noauto-open",
            "noauto.library",
            TESTLIB_NOAUTO,
            NOAUTO_NAME,
            open_call_return_base_program(),
        );
        let base = result.expect("run should succeed") as u32;
        assert_ne!(
            base, 0,
            "OpenLibrary of a non-AUTOINIT library should succeed"
        );

        let mem = rt.memory();
        assert_eq!(
            mem.read_u8(base + 8),
            9,
            "ln_Type set by the InitStruct table"
        );
        assert_eq!(mem.read_u16(base + 20), 1, "lib_Version from InitStruct");
        assert_eq!(mem.read_u16(base + 22), 7, "lib_Revision from InitStruct");
        assert_eq!(mem.read_u16(base + 32), 1, "the Open vector ran once");
        assert_eq!(
            mem.read_u16(base - 6),
            0x4EF9,
            "vectors are real JMPs built by MakeLibrary, not traps"
        );
        assert_ne!(
            mem.read_u32(base + 34),
            0,
            "init stored its segList argument"
        );
        assert_eq!(
            liblist_count(mem, b"noauto.library"),
            1,
            "the library's own AddLibrary must not be doubled by the loader"
        );
    }

    #[test]
    fn non_autoinit_library_user_vector_runs_natively() {
        let mut words = Vec::new();
        movea_imm(&mut words, 1, 0);
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        words.push(moveq(0, 0));
        jsr(&mut words, 6, -552);
        words.push(movea_dn(6, 0));
        jsr(&mut words, 6, -30);
        words.push(RTS);
        let (result, _rt) = run_against_library(
            "noauto-user-vector",
            "noauto.library",
            TESTLIB_NOAUTO,
            NOAUTO_NAME,
            words,
        );
        assert_eq!(result.expect("run should succeed"), 42);
    }

    /// The second open finds the library through the registry entry its
    /// own `AddLibrary` created (re-registered with the seglist by the
    /// loader) and just runs `Open` again.
    #[test]
    fn non_autoinit_library_second_open_reuses_it_and_counts_opens() {
        let mut words = Vec::new();
        for _ in 0..2 {
            movea_imm(&mut words, 1, 0);
            movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
            words.push(moveq(0, 0));
            jsr(&mut words, 6, -552);
        }
        words.push(movea_dn(3, 0));
        move_w_disp(&mut words, 3, 32, 0); // D0 = lib_OpenCnt
        words.push(RTS);
        // Both OpenLibrary calls need the name patched in (the second
        // movea.l #imm,A1 starts at word 10: 3 + 3 + 1 + 2 words before it).
        let str_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        words[10] = (str_addr >> 16) as u16;
        words[11] = str_addr as u16;
        let (result, rt) = run_against_library(
            "noauto-two-opens",
            "noauto.library",
            TESTLIB_NOAUTO,
            NOAUTO_NAME,
            words,
        );
        assert_eq!(result.expect("run should succeed"), 2);
        assert_eq!(liblist_count(rt.memory(), b"noauto.library"), 1);
    }

    /// An init that adds itself but returns `NULL` is recovered by the
    /// `FindName` fallback on the Resident's name.
    #[test]
    fn non_autoinit_init_returning_null_after_addlibrary_is_found_by_name() {
        let (result, rt) = run_against_library(
            "noauto-nullret",
            "noauto.library",
            TESTLIB_NOAUTO_NULLRET,
            NOAUTO_NAME,
            open_call_return_base_program(),
        );
        let base = result.expect("run should succeed") as u32;
        assert_ne!(
            base, 0,
            "the added library should be found despite D0 = NULL"
        );
        assert_eq!(rt.memory().read_u16(base + 32), 1);
        assert_eq!(liblist_count(rt.memory(), b"noauto.library"), 1);
    }

    /// An init that returns `NULL` without adding anything refuses the
    /// open: `OpenLibrary` returns `NULL` (the program exits with D0).
    #[test]
    fn non_autoinit_init_refusing_fails_the_open() {
        let mut words = Vec::new();
        movea_imm(&mut words, 1, 0);
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        words.push(moveq(0, 0));
        jsr(&mut words, 6, -552);
        words.push(RTS);
        let (result, rt) = run_against_library(
            "noauto-refuse",
            "noauto.library",
            TESTLIB_NOAUTO_REFUSE,
            NOAUTO_NAME,
            words,
        );
        assert_eq!(result.expect("run should succeed"), 0);
        assert_eq!(liblist_count(rt.memory(), b"noauto.library"), 0);
    }

    /// Runs `OpenLibrary("noauto.library")` (+ `CloseLibrary` of the
    /// result when `close`), `opens` times, with a fixed `load_end`, and
    /// returns the guest heap's free bytes afterwards.
    fn noauto_free_bytes(tag: &str, lib_bytes: &[u8], opens: usize, close: bool) -> u32 {
        let entry = TRAP_TABLE_END;
        let fixed_load_end = entry + 0x400;
        let mut words = Vec::new();
        let mut placeholders = Vec::new();
        for _ in 0..opens {
            placeholders.push(words.len());
            movea_imm(&mut words, 1, 0);
            movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
            words.push(moveq(0, 0));
            jsr(&mut words, 6, -552);
            if close {
                words.push(movea_dn(1, 0));
                movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
                jsr(&mut words, 6, -414);
            }
        }
        words.push(RTS);
        let str_addr = entry + (words.len() as u32) * 2;
        for idx in placeholders {
            words[idx + 1] = (str_addr >> 16) as u16;
            words[idx + 2] = str_addr as u16;
        }
        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        crate::guestmem::write_c_string(&mut mem, str_addr, NOAUTO_NAME);
        assert!(str_addr + 20 <= fixed_load_end);
        let (_tmp, vfs) = vfs_with_libs_file(tag, "noauto.library", lib_bytes);
        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: fixed_load_end,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        rt.set_vfs(vfs);
        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed");
        rt.heap_mut().free_bytes()
    }

    #[test]
    fn non_autoinit_refused_opens_do_not_leak() {
        assert_eq!(
            noauto_free_bytes("noauto-refuse-1", TESTLIB_NOAUTO_REFUSE, 1, false),
            noauto_free_bytes("noauto-refuse-5", TESTLIB_NOAUTO_REFUSE, 5, false),
            "an init that frees its MakeLibrary base and returns NULL must leave nothing \
             behind (seglist unloaded, nothing double-freed)"
        );
    }

    /// The last close runs the library's own expunge (Remove + FreeMem of
    /// its own base) and returns the segList: the host must neither unlink
    /// the node a second time nor free the base, and nothing may leak.
    #[test]
    fn non_autoinit_open_close_cycle_leaks_nothing_and_reloads() {
        let never = noauto_free_bytes("noauto-cycle-0", TESTLIB_NOAUTO, 0, true);
        let once = noauto_free_bytes("noauto-cycle-1", TESTLIB_NOAUTO, 1, true);
        let thrice = noauto_free_bytes("noauto-cycle-3", TESTLIB_NOAUTO, 3, true);
        assert_eq!(never, once, "open+close must return the heap to baseline");
        assert_eq!(
            once, thrice,
            "repeated cycles (fresh reloads) must not leak"
        );
    }

    #[test]
    fn non_autoinit_library_is_gone_from_liblist_after_last_close() {
        let mut words = Vec::new();
        movea_imm(&mut words, 1, 0);
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        words.push(moveq(0, 0));
        jsr(&mut words, 6, -552);
        words.push(movea_dn(1, 0));
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        jsr(&mut words, 6, -414);
        words.push(RTS);
        let (result, rt) = run_against_library(
            "noauto-close-liblist",
            "noauto.library",
            TESTLIB_NOAUTO,
            NOAUTO_NAME,
            words,
        );
        result.expect("run should succeed");
        assert_eq!(liblist_count(rt.memory(), b"noauto.library"), 0);
    }

    // --- Guest-callable MakeLibrary / MakeFunctions / InitStruct / AddLibrary ---

    /// Runs `words` (no `Vfs`) after letting `setup` seed guest data below
    /// `load_end` (so the guest heap can't overlap it).
    fn run_words(
        words: Vec<u16>,
        setup: impl FnOnce(&mut FlatMemory),
    ) -> (Result<i32, RuntimeError>, Runtime<M68kCpu>) {
        run_words_sized(0x6_0000, words, setup)
    }

    fn run_words_sized(
        ram: usize,
        words: Vec<u16>,
        setup: impl FnOnce(&mut FlatMemory),
    ) -> (Result<i32, RuntimeError>, Runtime<M68kCpu>) {
        let entry = TRAP_TABLE_END;
        let mut mem = FlatMemory::new(ram);
        load_words(&mut mem, entry, &words);
        setup(&mut mem);
        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: 0x1_1000,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let result = rt.run(&mut out, None);
        (result, rt)
    }

    const DATA: u32 = 0x1_0000;

    fn put_u32s(mem: &mut FlatMemory, addr: u32, values: &[u32]) {
        for (i, v) in values.iter().enumerate() {
            mem.write_u32(addr + i as u32 * 4, *v);
        }
    }

    #[test]
    fn make_functions_handler_absolute_pointers() {
        let target = DATA + 0x800;
        let mut words = Vec::new();
        movea_imm(&mut words, 0, target);
        movea_imm(&mut words, 1, DATA);
        movea_imm(&mut words, 2, 0);
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        jsr(&mut words, 6, -90);
        words.push(RTS);
        let (result, rt) = run_words(words, |mem| {
            put_u32s(mem, DATA, &[0x1111, 0x2222, 0xFFFF_FFFF]);
        });
        assert_eq!(
            result.expect("run should succeed"),
            12,
            "table size in bytes"
        );
        let mem = rt.memory();
        assert_eq!(mem.read_u16(target - 6), 0x4EF9);
        assert_eq!(mem.read_u32(target - 4), 0x1111);
        assert_eq!(mem.read_u16(target - 12), 0x4EF9);
        assert_eq!(mem.read_u32(target - 10), 0x2222);
    }

    #[test]
    fn make_functions_handler_word_displacements() {
        let target = DATA + 0x800;
        let disp_base = DATA + 0x100;
        let mut words = Vec::new();
        movea_imm(&mut words, 0, target);
        movea_imm(&mut words, 1, DATA);
        movea_imm(&mut words, 2, disp_base);
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        jsr(&mut words, 6, -90);
        words.push(RTS);
        let (result, rt) = run_words(words, |mem| {
            // Displacements 0x10, -0x20 (negative), then the -1 terminator.
            mem.write_u16(DATA, 0x0010);
            mem.write_u16(DATA + 2, 0xFFE0);
            mem.write_u16(DATA + 4, 0xFFFF);
        });
        assert_eq!(result.expect("run should succeed"), 12);
        let mem = rt.memory();
        assert_eq!(mem.read_u32(target - 4), disp_base + 0x10);
        assert_eq!(mem.read_u32(target - 10), disp_base - 0x20);
    }

    #[test]
    fn init_struct_handler_initializes_memory() {
        let memory = DATA + 0x800;
        let mut words = Vec::new();
        movea_imm(&mut words, 1, DATA);
        movea_imm(&mut words, 2, memory);
        move_imm_dn(&mut words, 0, 8);
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        jsr(&mut words, 6, -78);
        words.push(RTS);
        let (result, rt) = run_words(words, |mem| {
            for i in 0..16 {
                mem.write_u8(memory + i, 0xFF);
            }
            // INITLONG 4,$11223344 ; INITBYTE 1,$AA ; end
            for (i, b) in [0x80u8, 4, 0x11, 0x22, 0x33, 0x44, 0xA0, 1, 0xAA, 0, 0]
                .iter()
                .enumerate()
            {
                mem.write_u8(DATA + i as u32, *b);
            }
        });
        result.expect("run should succeed");
        let mem = rt.memory();
        assert_eq!(mem.read_u8(memory), 0, "cleared first (size 8)");
        assert_eq!(mem.read_u8(memory + 1), 0xAA);
        assert_eq!(mem.read_u32(memory + 4), 0x1122_3344);
        assert_eq!(mem.read_u8(memory + 8), 0xFF, "past size is untouched");
    }

    /// Builds the argument registers for a `MakeLibrary` call.
    fn make_library_call(
        words: &mut Vec<u16>,
        vectors: u32,
        structure: u32,
        init: u32,
        d_size: u32,
        seglist: u32,
    ) {
        movea_imm(words, 0, vectors);
        movea_imm(words, 1, structure);
        movea_imm(words, 2, init);
        move_imm_dn(words, 0, d_size);
        move_imm_dn(words, 1, seglist);
        movea_imm(words, 6, EXEC_LIBRARY_BASE);
        jsr(words, 6, -84);
    }

    /// `MakeLibrary` builds the table and sizes, then runs `init` with
    /// `D0` = base, `A0` = segList, and its `D0` is the result.
    #[test]
    fn make_library_handler_runs_init_and_returns_its_result() {
        let init_code = DATA + 0x200;
        let mut words = Vec::new();
        make_library_call(&mut words, DATA, 0, init_code, 40, 0x77);
        words.push(RTS);
        let (result, rt) = run_words(words, |mem| {
            put_u32s(mem, DATA, &[0x1111, 0x2222, 0xFFFF_FFFF]);
            // init: movea.l d0,a1 ; move.w #$1234,22(a1) ; move.l a0,34(a1) ; rts
            let mut code = vec![movea_dn(1, 0), 0x337C, 0x1234, 22];
            code.push(0x2348); // move.l a0,d16(a1)
            code.push(34);
            code.push(RTS);
            load_words(mem, init_code, &code);
        });
        let base = result.expect("run should succeed") as u32;
        assert_ne!(base, 0);
        let mem = rt.memory();
        assert_eq!(mem.read_u16(base + 22), 0x1234, "init ran with D0 = base");
        assert_eq!(mem.read_u32(base + 34), 0x77, "init got the segList in A0");
        assert_eq!(mem.read_u16(base + 16), 12, "lib_NegSize");
        assert_eq!(mem.read_u16(base + 18), 40, "lib_PosSize");
        assert_eq!(mem.read_u16(base - 6), 0x4EF9);
        assert_eq!(mem.read_u32(base - 4), 0x1111);
        assert_eq!(mem.read_u32(base - 10), 0x2222);
    }

    #[test]
    fn make_library_handler_without_init_returns_the_base() {
        let mut words = Vec::new();
        make_library_call(&mut words, DATA, 0, 0, 34, 0);
        words.push(RTS);
        let (result, rt) = run_words(words, |mem| {
            put_u32s(mem, DATA, &[0x1111, 0xFFFF_FFFF]);
        });
        let base = result.expect("run should succeed") as u32;
        assert_ne!(base, 0);
        assert_eq!(rt.memory().read_u32(base - 4), 0x1111);
    }

    /// An `init` returning `NULL` makes `MakeLibrary` return `NULL`.
    #[test]
    fn make_library_handler_propagates_init_failure() {
        let init_code = DATA + 0x200;
        let mut words = Vec::new();
        make_library_call(&mut words, DATA, 0, init_code, 34, 0);
        words.push(RTS);
        let (result, _rt) = run_words(words, |mem| {
            put_u32s(mem, DATA, &[0x1111, 0xFFFF_FFFF]);
            load_words(mem, init_code, &[moveq(0, 0), RTS]);
        });
        assert_eq!(result.expect("run should succeed"), 0);
    }

    /// A guest program that builds a library with `MakeLibrary` (name via
    /// an InitStruct table), `AddLibrary`s it, then `OpenLibrary`s it by
    /// name with no disk file at all: `AddLibrary` made it findable, the
    /// node is typed `NT_LIBRARY`, and `Open` is the library's own code.
    #[test]
    fn add_library_makes_a_guest_built_library_openable_by_name() {
        let structure = DATA + 0x100; // InitStruct: INITLONG LN_NAME
        let name = DATA + 0x180;
        let open_code = DATA + 0x200;
        let mut words = Vec::new();
        make_library_call(&mut words, DATA, structure, 0, 34, 0);
        words.push(movea_dn(1, 0)); // A1 = base
        words.push(movea_dn(3, 0)); // A3 = base (kept)
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        jsr(&mut words, 6, -396); // AddLibrary(base)
        movea_imm(&mut words, 1, name);
        words.push(moveq(0, 0));
        jsr(&mut words, 6, -552); // OpenLibrary("mine.library", 0)
        words.push(RTS);
        let (result, rt) = run_words(words, |mem| {
            // Vector 0 (Open): move.l a6,d0 ; rts -- returns the base.
            put_u32s(mem, DATA, &[open_code, 0xFFFF_FFFF]);
            for (i, b) in [0x80u8, 10].iter().enumerate() {
                mem.write_u8(structure + i as u32, *b);
            }
            mem.write_u32(structure + 2, name);
            mem.write_u16(structure + 6, 0);
            crate::guestmem::write_c_string(mem, name, b"mine.library\0");
            load_words(mem, open_code, &[move_an_dn(6, 0), RTS]);
        });
        let opened = result.expect("run should succeed") as u32;
        assert_ne!(
            opened, 0,
            "OpenLibrary by name should hit the AddLibrary entry"
        );
        let mem = rt.memory();
        assert_eq!(liblist_count(mem, b"mine.library"), 1);
        assert_eq!(
            mem.read_u8(opened + 8),
            9,
            "AddLibrary sets ln_Type = NT_LIBRARY"
        );
    }

    // --- SumLibrary / RemLibrary / TypeOfMem / resources ---

    const TESTLIB_NOAUTO_KEEP: &[u8] = include_bytes!("../../../fixtures/testlib_noauto_keep");
    const LIB_FLAGS: u32 = 14;
    const LIB_SUM: u32 = 28;

    /// Guest-built library at the exit: `MakeLibrary` (2 vectors) +
    /// `AddLibrary`, leaving `base` in A3 for the caller to continue.
    fn build_and_add_library(words: &mut Vec<u16>) {
        make_library_call(words, DATA, 0, 0, 34, 0);
        words.push(movea_dn(3, 0));
        words.push(movea_an(1, 3));
        movea_imm(words, 6, EXEC_LIBRARY_BASE);
        jsr(words, 6, -396); // AddLibrary
    }

    #[test]
    fn add_library_computes_lib_sum_and_sum_library_agrees() {
        let mut words = Vec::new();
        build_and_add_library(&mut words);
        words.push(movea_an(1, 3));
        jsr(&mut words, 6, -426); // SumLibrary: must be happy
        words.push(move_an_dn(3, 0));
        words.push(RTS);
        let (result, rt) = run_words(words, |mem| {
            put_u32s(mem, DATA, &[0x1111, 0x2222, 0xFFFF_FFFF]);
        });
        let base = result.expect("run should succeed") as u32;
        let mem = rt.memory();
        assert_eq!(
            mem.read_u32(base + LIB_SUM),
            0x3333,
            "sum of vector targets"
        );
        let flags = mem.read_u8(base + LIB_FLAGS);
        assert_eq!(flags & super::LIBF_SUMUSED, super::LIBF_SUMUSED);
        assert_eq!(flags & (super::LIBF_CHANGED | super::LIBF_SUMMING), 0);
    }

    #[test]
    fn sum_library_impl_flags_a_stale_sum_unless_changed() {
        let mut mem = FlatMemory::new(0x1000);
        let base = 0x800;
        mem.write_u16(base + 16, 12); // lib_NegSize: two vectors
        mem.write_u32(base - 4, 5);
        mem.write_u32(base - 10, 7);
        mem.write_u32(base + LIB_SUM, 12);
        assert!(super::sum_library_impl(&mut mem, base), "matching sum");
        mem.write_u32(base - 4, 6);
        assert!(!super::sum_library_impl(&mut mem, base), "stale sum alerts");
        assert_eq!(mem.read_u32(base + LIB_SUM), 12, "stored sum is left alone");
        mem.write_u8(base + LIB_FLAGS, super::LIBF_CHANGED);
        assert!(
            super::sum_library_impl(&mut mem, base),
            "CHANGED is excused"
        );
        assert_eq!(mem.read_u32(base + LIB_SUM), 13);
        assert_eq!(mem.read_u8(base + LIB_FLAGS), 0, "CHANGED cleared");
    }

    /// `SetFunction` on an added library keeps `lib_Sum` current.
    #[test]
    fn set_function_refreshes_lib_sum_of_a_summed_library() {
        let mut words = Vec::new();
        build_and_add_library(&mut words);
        words.push(movea_an(1, 3)); // A1 = library
        movea_imm(&mut words, 0, 0x9999); // A0 = new entry
        words.push(moveq(0, -6i8 as u8)); // D0 = LVO -6
        jsr(&mut words, 6, -420); // SetFunction
        words.push(move_an_dn(3, 0));
        words.push(RTS);
        let (result, rt) = run_words(words, |mem| {
            put_u32s(mem, DATA, &[0x1111, 0x2222, 0xFFFF_FFFF]);
        });
        let base = result.expect("run should succeed") as u32;
        let mem = rt.memory();
        assert_eq!(mem.read_u32(base - 4), 0x9999, "vector patched");
        assert_eq!(
            mem.read_u32(base + LIB_SUM),
            super::lib_checksum(mem, base),
            "lib_Sum follows the patched vector"
        );
        assert_eq!(mem.read_u8(base + LIB_FLAGS) & super::LIBF_CHANGED, 0);
    }

    fn type_of_mem(addr: u32) -> u32 {
        type_of_mem_with_ram(0x30_0000, addr)
    }

    fn type_of_mem_with_ram(ram: usize, addr: u32) -> u32 {
        let mut words = Vec::new();
        movea_imm(&mut words, 1, addr);
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        jsr(&mut words, 6, -534);
        words.push(RTS);
        run_words_sized(ram, words, |_| {})
            .0
            .expect("run should succeed") as u32
    }

    #[test]
    fn type_of_mem_classifies_chip_fast_and_out_of_range() {
        use crate::execmem::{MEMF_CHIP, MEMF_FAST, MEMF_PUBLIC};
        let chip = MEMF_PUBLIC | MEMF_CHIP;
        let fast = MEMF_PUBLIC | MEMF_FAST;
        assert_eq!(type_of_mem(0), chip, "bottom of chip RAM");
        assert_eq!(type_of_mem(EXEC_LIBRARY_BASE), chip, "reserved region");
        assert_eq!(type_of_mem(DATA), chip, "64 KiB is below 2 MiB");
        assert_eq!(type_of_mem(0x1F_FFFF), chip, "last chip byte");
        assert_eq!(type_of_mem(0x20_0000), fast, "first fast byte");
        assert_eq!(type_of_mem(0x2F_FFFF), fast, "last byte of 3 MiB RAM");
        assert_eq!(type_of_mem(0x30_0000), 0, "first byte past RAM");
        assert_eq!(type_of_mem(0xFFFF_FFF0), 0);
    }

    /// With RAM smaller than 2 MiB the chip range is capped at the end of RAM.
    #[test]
    fn type_of_mem_caps_chip_range_at_small_ram() {
        use crate::execmem::{MEMF_CHIP, MEMF_PUBLIC};
        let ram = 0x10_0000;
        assert_eq!(type_of_mem_with_ram(ram, 0xF_FFFF), MEMF_PUBLIC | MEMF_CHIP);
        assert_eq!(type_of_mem_with_ram(ram, 0x10_0000), 0);
        assert_eq!(type_of_mem_with_ram(ram, 0x18_0000), 0);
    }

    /// Runs a program that `AddResource`s a node named "my.resource" (when
    /// `add`), optionally `RemResource`s it, then `OpenResource(name)`;
    /// returns (exit code, node address).
    fn resource_program(add: bool, rem: bool, open_name: &[u8]) -> (u32, u32) {
        let node = DATA + 0x100;
        let name = DATA + 0x180;
        let open_name_addr = DATA + 0x1C0;
        let mut words = Vec::new();
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        if add {
            movea_imm(&mut words, 1, node);
            jsr(&mut words, 6, -486); // AddResource
        }
        if rem {
            movea_imm(&mut words, 1, node);
            jsr(&mut words, 6, -492); // RemResource
        }
        movea_imm(&mut words, 1, open_name_addr);
        jsr(&mut words, 6, -498); // OpenResource
        words.push(RTS);
        let (result, _rt) = run_words(words, |mem| {
            mem.write_u32(node + 10, name);
            crate::guestmem::write_c_string(mem, name, b"my.resource\0");
            crate::guestmem::write_c_string(mem, open_name_addr, open_name);
        });
        (result.expect("run should succeed") as u32, node)
    }

    #[test]
    fn open_resource_finds_an_added_resource_by_name() {
        let (found, node) = resource_program(true, false, b"my.resource\0");
        assert_eq!(found, node);
    }

    #[test]
    fn open_resource_of_unknown_or_wrong_name_is_null() {
        assert_eq!(resource_program(false, false, b"my.resource\0").0, 0);
        assert_eq!(resource_program(true, false, b"other.resource\0").0, 0);
        assert_eq!(resource_program(true, false, b"My.Resource\0").0, 0);
    }

    #[test]
    fn rem_resource_makes_it_unfindable() {
        assert_eq!(resource_program(true, true, b"my.resource\0").0, 0);
    }

    /// `OpenLibrary`, optionally `CloseLibrary`, then `RemLibrary(base)`,
    /// with the base kept in A3.
    fn keep_program(close: bool, rem: bool) -> Vec<u16> {
        let mut words = Vec::new();
        movea_imm(&mut words, 1, 0);
        movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        words.push(moveq(0, 0));
        jsr(&mut words, 6, -552);
        words.push(movea_dn(3, 0));
        if close {
            words.push(movea_an(1, 3));
            movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
            jsr(&mut words, 6, -414);
        }
        if rem {
            words.push(movea_an(1, 3));
            movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
            jsr(&mut words, 6, -402);
        }
        words.push(RTS);
        words
    }

    #[test]
    fn rem_library_of_an_open_library_is_refused() {
        let (result, rt) = run_against_library(
            "keep-rem-open",
            "noauto.library",
            TESTLIB_NOAUTO_KEEP,
            NOAUTO_NAME,
            keep_program(false, true),
        );
        result.expect("run should succeed");
        assert_eq!(
            liblist_count(rt.memory(), b"noauto.library"),
            1,
            "Expunge returned 0 (open), so the library stays"
        );
    }

    #[test]
    fn rem_library_expunges_a_closed_resident_library() {
        let (result, rt) = run_against_library(
            "keep-rem-closed",
            "noauto.library",
            TESTLIB_NOAUTO_KEEP,
            NOAUTO_NAME,
            keep_program(true, true),
        );
        result.expect("run should succeed");
        assert_eq!(
            liblist_count(rt.memory(), b"noauto.library"),
            0,
            "Close left it resident; RemLibrary's Expunge removed it"
        );
    }

    #[test]
    fn close_without_rem_leaves_the_keep_library_resident() {
        let (result, rt) = run_against_library(
            "keep-close-only",
            "noauto.library",
            TESTLIB_NOAUTO_KEEP,
            NOAUTO_NAME,
            keep_program(true, false),
        );
        result.expect("run should succeed");
        assert_eq!(liblist_count(rt.memory(), b"noauto.library"), 1);
    }

    /// An open/close/RemLibrary cycle returns the heap to baseline, so
    /// neither the library's own base nor its seglist leaks or is
    /// double-freed.
    #[test]
    fn rem_library_cycle_leaks_nothing() {
        fn free_bytes(tag: &str, cycle: bool) -> u32 {
            let entry = TRAP_TABLE_END;
            let mut words = if cycle {
                keep_program(true, true)
            } else {
                vec![RTS]
            };
            let str_addr = entry + (words.len() as u32) * 2;
            if cycle {
                words[1] = (str_addr >> 16) as u16;
                words[2] = str_addr as u16;
            }
            let mut mem = FlatMemory::new(0x2_0000);
            load_words(&mut mem, entry, &words);
            crate::guestmem::write_c_string(&mut mem, str_addr, NOAUTO_NAME);
            let (_tmp, vfs) = vfs_with_libs_file(tag, "noauto.library", TESTLIB_NOAUTO_KEEP);
            let mut rt = Runtime::new(
                M68kCpu::new(),
                mem,
                StartConfig {
                    entry,
                    load_end: entry + 0x400,
                    args: Vec::new(),
                    ..StartConfig::default()
                },
            );
            rt.set_vfs(vfs);
            let mut out = Vec::new();
            rt.run(&mut out, None).expect("run should succeed");
            rt.heap_mut().free_bytes()
        }
        assert_eq!(
            free_bytes("keep-cycle-0", false),
            free_bytes("keep-cycle-1", true)
        );
    }
}
