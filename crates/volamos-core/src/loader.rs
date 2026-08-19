//! A minimal loader for AmigaOS "hunk" executables.
//!
//! This is intentionally narrow: enough to parse and load simple,
//! non-overlaid CLI binaries (the kind produced by `vasm -Fhunkexe` or a
//! plain single/multi-hunk linker output). It supports:
//!
//! - `HUNK_HEADER` (0x3F3)
//! - `HUNK_CODE`   (0x3E9)
//! - `HUNK_DATA`   (0x3EA)
//! - `HUNK_BSS`    (0x3EB)
//! - `HUNK_RELOC32`(0x3EC)
//! - `HUNK_DREL32` (0x3F7)
//! - `HUNK_END`    (0x3F2)
//!
//! `HUNK_DREL32` (found running the real `PhxAss` assembler -- itself a
//! `.lha` archive from Aminet -- against a trivial test source: its own
//! executable uses this hunk type). Despite the name suggesting a
//! self-relative ("data-relative") fixup, the real AmigaOS ROM loader
//! treats it identically to `HUNK_RELOC32SHORT` (confirmed against
//! <https://amiga-dev.wikidot.com/file-format:hunk>, which documents
//! `HUNK_DREL32` as "handled exactly the same as `HUNK_RELOC32SHORT`"):
//! same *absolute* `mem[loc] += target_hunk_addr` arithmetic as
//! `HUNK_RELOC32`, just a more compact on-disk list encoding --
//! `uint16` count/hunk-number/offsets instead of `HUNK_RELOC32`'s
//! `uint32` fields (realigned to a 4-byte boundary after the
//! `count == 0`-terminated list, since 16-bit entries can leave the
//! read position mid-longword).
//!
//! `HUNK_SYMBOL` (0x3F0) and `HUNK_DEBUG` (0x3F1) blocks are recognized and
//! skipped (their contents are discarded) so binaries built with `-nosym`
//! *or* with symbol/debug info left in still load. Overlays (`HUNK_LIB`,
//! `HUNK_OVERLAY`, or a header whose first/last hunk range is a strict
//! subset of the hunk table) are not supported.
//!
//! All values in a hunk file are big-endian 32-bit words ("longwords").
//!
//! # API
//!
//! [`parse`] turns raw bytes into a [`HunkFile`] (hunk kinds, contents,
//! declared sizes, and unresolved relocations). [`load`] then lays those
//! hunks into a caller-provided [`AddressSpace`] starting at a base
//! address, applies `RELOC32` fixups, and returns the resulting
//! [`LoadResult`] (entry point + per-hunk load addresses).
//!
//! Splitting parse from load keeps the format parser independent of any
//! particular memory layout policy (the caller decides base address and
//! alignment/padding between hunks).

use crate::memory::AddressSpace;

// --- Hunk type identifiers (top byte reserved for future flag bits) ---

const HUNK_HEADER: u32 = 0x3F3;
const HUNK_CODE: u32 = 0x3E9;
const HUNK_DATA: u32 = 0x3EA;
const HUNK_BSS: u32 = 0x3EB;
const HUNK_RELOC32: u32 = 0x3EC;
const HUNK_SYMBOL: u32 = 0x3F0;
const HUNK_DEBUG: u32 = 0x3F1;
const HUNK_END: u32 = 0x3F2;
const HUNK_DREL32: u32 = 0x3F7;

/// Mask for the memory-flag bits (`MEMF_CHIP`/`MEMF_FAST`/extended-flag
/// marker) that can be packed into the top bits of a hunk-size longword in
/// `HUNK_HEADER`. We don't act on the flags (no chip/fast distinction in
/// this emulator's flat address space) but we do need to mask them off to
/// recover the real size in longwords.
const HUNK_SIZE_FLAGS_MASK: u32 = 0xC000_0000;

/// Errors that can occur while parsing or loading a hunk executable.
///
/// Parsing never panics on malformed input; every failure mode is a
/// variant here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    /// The file is shorter than a well-formed hunk file requires at the
    /// point a read was attempted.
    UnexpectedEof,
    /// The first longword wasn't `HUNK_HEADER` (0x3F3).
    BadMagic { found: u32 },
    /// The header's `first_hunk`/`last_hunk` range doesn't cover the
    /// whole hunk table, i.e. this is an overlay file, which isn't
    /// supported.
    OverlaysNotSupported,
    /// The header's `first_hunk` is greater than `last_hunk`.
    BadHunkRange { first: usize, last: usize },
    /// While reading hunk bodies, encountered a hunk-type longword that
    /// isn't a valid hunk body start (`HUNK_CODE`/`HUNK_DATA`/`HUNK_BSS`)
    /// where one was expected.
    ExpectedHunkBody { hunk_index: usize, found: u32 },
    /// While reading the blocks that follow a hunk body (relocations,
    /// symbols, debug info, end-of-hunk), encountered a longword that
    /// isn't a recognized block type.
    UnknownBlockType { hunk_index: usize, found: u32 },
    /// A `HUNK_CODE`/`HUNK_DATA` body's declared size (from its own
    /// length longword) doesn't fit within the size reserved for that
    /// hunk in the header's size table.
    HunkBodyTooLarge { hunk_index: usize },
    /// A `HUNK_RELOC32` entry refers to a target hunk index that doesn't
    /// exist in this file.
    RelocTargetOutOfRange { hunk_index: usize, target: usize },
    /// A `HUNK_RELOC32` entry's offset falls outside the referencing
    /// hunk's own bounds.
    RelocOffsetOutOfRange { hunk_index: usize, offset: u32 },
    /// The file declares zero hunks.
    NoHunks,
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::UnexpectedEof => write!(f, "unexpected end of file"),
            LoadError::BadMagic { found } => {
                write!(
                    f,
                    "not a hunk executable: expected HUNK_HEADER (0x3F3), found {found:#x}"
                )
            }
            LoadError::OverlaysNotSupported => write!(f, "overlay hunk files are not supported"),
            LoadError::BadHunkRange { first, last } => {
                write!(
                    f,
                    "invalid hunk range in header: first_hunk={first} > last_hunk={last}"
                )
            }
            LoadError::ExpectedHunkBody { hunk_index, found } => write!(
                f,
                "hunk {hunk_index}: expected HUNK_CODE/HUNK_DATA/HUNK_BSS, found {found:#x}"
            ),
            LoadError::UnknownBlockType { hunk_index, found } => {
                write!(f, "hunk {hunk_index}: unrecognized block type {found:#x}")
            }
            LoadError::HunkBodyTooLarge { hunk_index } => {
                write!(
                    f,
                    "hunk {hunk_index}: body larger than its declared header size"
                )
            }
            LoadError::RelocTargetOutOfRange { hunk_index, target } => write!(
                f,
                "hunk {hunk_index}: HUNK_RELOC32 refers to nonexistent hunk {target}"
            ),
            LoadError::RelocOffsetOutOfRange { hunk_index, offset } => write!(
                f,
                "hunk {hunk_index}: HUNK_RELOC32 offset {offset:#x} is outside the hunk"
            ),
            LoadError::NoHunks => write!(f, "hunk file declares zero hunks"),
        }
    }
}

impl std::error::Error for LoadError {}

/// What kind of hunk a [`Hunk`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HunkKind {
    /// Executable code, loaded verbatim.
    Code,
    /// Initialized data, loaded verbatim.
    Data,
    /// Uninitialized data; occupies space but has no file content (loaded
    /// as zero-filled).
    Bss,
}

/// A single 32-bit relocation within a hunk: the longword at `offset`
/// (relative to the start of the hunk it belongs to) needs the load
/// address of `target_hunk` added to it. Built from either
/// `HUNK_RELOC32` or `HUNK_DREL32` (see the module docs -- both apply
/// identically despite `HUNK_DREL32`'s on-disk encoding differing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reloc32 {
    /// Byte offset within the owning hunk of the longword to fix up.
    pub offset: u32,
    /// Index (into [`HunkFile::hunks`]) of the hunk whose load address
    /// should be added at `offset`.
    pub target_hunk: usize,
}

/// One parsed hunk: its kind, content (empty for BSS), the size in bytes
/// reserved for it (from the header's size table, with the memory-flag
/// bits masked off), and any 32-bit relocations that apply to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub kind: HunkKind,
    /// File content for `Code`/`Data` hunks. Always empty for `Bss`.
    pub data: Vec<u8>,
    /// Size in bytes to reserve for this hunk when laid out in memory.
    /// For `Code`/`Data` this is always `>= data.len()` (padded up to
    /// the header's declared size, normally equal).
    pub reserved_size: usize,
    /// 32-bit relocations that apply within this hunk.
    pub relocs: Vec<Reloc32>,
}

/// A fully parsed hunk executable: an ordered list of hunks (hunk 0 is
/// conventionally the entry hunk).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkFile {
    pub hunks: Vec<Hunk>,
}

/// The result of [`load`]ing a [`HunkFile`] into memory: where each hunk
/// ended up, and where execution should begin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadResult {
    /// Guest address of the first instruction to execute (the start of
    /// hunk 0).
    pub entry: u32,
    /// Guest load address of each hunk, indexed the same as
    /// [`HunkFile::hunks`].
    pub hunk_addrs: Vec<u32>,
    /// The first guest address *after* every loaded hunk (hunks are
    /// packed back-to-back, each padded up to a 4-byte boundary, so this
    /// is already 4-byte aligned). Callers building a
    /// [`crate::dispatch::StartConfig`] pass this as `load_end`, so the
    /// guest heap starts right after the loaded program instead of
    /// risking overlap with it.
    pub end: u32,
}

/// A tiny cursor over a byte slice that reads big-endian 32-bit words and
/// turns short reads into [`LoadError::UnexpectedEof`] instead of
/// panicking.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn read_u32(&mut self) -> Result<u32, LoadError> {
        let end = self.pos.checked_add(4).ok_or(LoadError::UnexpectedEof)?;
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or(LoadError::UnexpectedEof)?;
        self.pos = end;
        Ok(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
    }

    /// Reads exactly `n` bytes.
    fn read_bytes(&mut self, n: usize) -> Result<Vec<u8>, LoadError> {
        let end = self.pos.checked_add(n).ok_or(LoadError::UnexpectedEof)?;
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or(LoadError::UnexpectedEof)?;
        self.pos = end;
        Ok(slice.to_vec())
    }

    /// Skips `n` longwords (used for resident-library-name tables and
    /// `HUNK_DEBUG` payloads we don't interpret).
    fn skip_longwords(&mut self, n: usize) -> Result<(), LoadError> {
        let nbytes = n.checked_mul(4).ok_or(LoadError::UnexpectedEof)?;
        let end = self
            .pos
            .checked_add(nbytes)
            .ok_or(LoadError::UnexpectedEof)?;
        if end > self.bytes.len() {
            return Err(LoadError::UnexpectedEof);
        }
        self.pos = end;
        Ok(())
    }

    /// Reads a big-endian 16-bit word (used by `HUNK_DREL32`'s more
    /// compact list encoding -- see the module docs).
    fn read_u16(&mut self) -> Result<u16, LoadError> {
        let end = self.pos.checked_add(2).ok_or(LoadError::UnexpectedEof)?;
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or(LoadError::UnexpectedEof)?;
        self.pos = end;
        Ok(u16::from_be_bytes([slice[0], slice[1]]))
    }

    /// Realigns to the next 4-byte boundary if `read_u16` calls left the
    /// position mid-longword (the rest of the hunk format is entirely
    /// longword-based, so a `HUNK_DREL32` list -- an odd number of
    /// 16-bit reads -- must pad back up before the next block-type
    /// longword is read).
    fn align_to_longword(&mut self) {
        if !self.pos.is_multiple_of(4) {
            self.pos += 2;
        }
    }
}

/// Parses a hunk executable's bytes into a [`HunkFile`].
///
/// This only interprets the file structure; it does not decide where
/// anything is loaded in guest memory (see [`load`] for that).
pub fn parse(bytes: &[u8]) -> Result<HunkFile, LoadError> {
    let mut r = Reader::new(bytes);

    let magic = r.read_u32()?;
    if magic != HUNK_HEADER {
        return Err(LoadError::BadMagic { found: magic });
    }

    // Resident-library name table: a sequence of (length-in-longwords,
    // name) entries terminated by a zero length. In practice this is
    // almost always immediately 0 (no resident library names) for a
    // plain CLI binary; we skip any entries present since we don't act
    // on resident-library preloading.
    loop {
        let n = r.read_u32()?;
        if n == 0 {
            break;
        }
        r.skip_longwords(n as usize)?;
    }

    let table_size = r.read_u32()? as usize;
    let first_hunk = r.read_u32()? as usize;
    let last_hunk = r.read_u32()? as usize;

    if table_size == 0 {
        return Err(LoadError::NoHunks);
    }
    if first_hunk > last_hunk {
        return Err(LoadError::BadHunkRange {
            first: first_hunk,
            last: last_hunk,
        });
    }
    // We don't support overlays: the loadable range must be the whole
    // table (first_hunk == 0, last_hunk == table_size - 1).
    if first_hunk != 0 || last_hunk + 1 != table_size {
        return Err(LoadError::OverlaysNotSupported);
    }

    let mut declared_sizes = Vec::with_capacity(table_size);
    for _ in 0..table_size {
        let raw = r.read_u32()?;
        let longwords = raw & !HUNK_SIZE_FLAGS_MASK;
        declared_sizes.push(longwords as usize * 4);
    }

    let mut hunks = Vec::with_capacity(table_size);
    for (hunk_index, &reserved_size) in declared_sizes.iter().enumerate() {
        let body_type = r.read_u32()?;
        let (kind, data) = match body_type {
            HUNK_CODE | HUNK_DATA => {
                let n_longwords = r.read_u32()? as usize;
                let data = r.read_bytes(n_longwords * 4)?;
                if data.len() > reserved_size {
                    return Err(LoadError::HunkBodyTooLarge { hunk_index });
                }
                let kind = if body_type == HUNK_CODE {
                    HunkKind::Code
                } else {
                    HunkKind::Data
                };
                (kind, data)
            }
            HUNK_BSS => {
                // HUNK_BSS repeats its size (in longwords) here even
                // though it's also present in the header's size table;
                // consume it (it should agree with `reserved_size`, but
                // we don't require an exact match since some tools pad
                // the header entry).
                r.read_u32()?;
                (HunkKind::Bss, Vec::new())
            }
            other => {
                return Err(LoadError::ExpectedHunkBody {
                    hunk_index,
                    found: other,
                });
            }
        };

        let mut relocs = Vec::new();
        loop {
            let block_type = r.read_u32()?;
            match block_type {
                HUNK_RELOC32 => loop {
                    let count = r.read_u32()?;
                    if count == 0 {
                        break;
                    }
                    let target_hunk = r.read_u32()? as usize;
                    for _ in 0..count {
                        let offset = r.read_u32()?;
                        relocs.push(Reloc32 {
                            offset,
                            target_hunk,
                        });
                    }
                },
                HUNK_DREL32 => {
                    loop {
                        let count = r.read_u16()?;
                        if count == 0 {
                            break;
                        }
                        let target_hunk = r.read_u16()? as usize;
                        for _ in 0..count {
                            let offset = r.read_u16()? as u32;
                            relocs.push(Reloc32 {
                                offset,
                                target_hunk,
                            });
                        }
                    }
                    r.align_to_longword();
                }
                HUNK_SYMBOL => loop {
                    let name_longwords = r.read_u32()?;
                    if name_longwords == 0 {
                        break;
                    }
                    r.skip_longwords(name_longwords as usize)?; // symbol name
                    r.read_u32()?; // symbol value (offset within hunk)
                },
                HUNK_DEBUG => {
                    let n_longwords = r.read_u32()?;
                    r.skip_longwords(n_longwords as usize)?;
                }
                HUNK_END => break,
                other => {
                    return Err(LoadError::UnknownBlockType {
                        hunk_index,
                        found: other,
                    });
                }
            }
        }

        // Validate relocation offsets now that we know this hunk's size;
        // target-hunk validity is checked below once all hunks exist.
        for reloc in &relocs {
            if (reloc.offset as usize)
                .checked_add(4)
                .is_none_or(|end| end > reserved_size)
            {
                return Err(LoadError::RelocOffsetOutOfRange {
                    hunk_index,
                    offset: reloc.offset,
                });
            }
        }

        hunks.push(Hunk {
            kind,
            data,
            reserved_size,
            relocs,
        });
    }

    for (hunk_index, hunk) in hunks.iter().enumerate() {
        for reloc in &hunk.relocs {
            if reloc.target_hunk >= hunks.len() {
                return Err(LoadError::RelocTargetOutOfRange {
                    hunk_index,
                    target: reloc.target_hunk,
                });
            }
        }
    }

    Ok(HunkFile { hunks })
}

/// Lays out `file`'s hunks contiguously in `mem` starting at `base`
/// (hunks are placed back-to-back, each padded up to a 4-byte boundary),
/// writes their content (zero-filling BSS), applies all `RELOC32`
/// fixups, and returns the resulting entry point and per-hunk addresses.
///
/// Relocation semantics match the standard AmigaOS convention: the
/// longword already present at a relocation's offset is treated as an
/// addend, and the target hunk's load address is added to it in place
/// (`mem[addr] += hunk_addrs[target_hunk]`). A freshly-assembled object
/// normally has `0` (or, for same-hunk self-references, an intra-hunk
/// offset) stored there; this loader does not assume which, it just adds.
///
/// The entry point is defined as the load address of hunk 0.
pub fn load(
    file: &HunkFile,
    mem: &mut dyn AddressSpace,
    base: u32,
) -> Result<LoadResult, LoadError> {
    if file.hunks.is_empty() {
        return Err(LoadError::NoHunks);
    }

    // First pass: assign each hunk a load address, packing them
    // contiguously (4-byte aligned) starting at `base`.
    let mut hunk_addrs = Vec::with_capacity(file.hunks.len());
    let mut cursor = base;
    for hunk in &file.hunks {
        hunk_addrs.push(cursor);
        let size = hunk.reserved_size as u32;
        let padded = size.wrapping_add(3) & !3;
        cursor = cursor.wrapping_add(padded);
    }

    // Second pass: write content (BSS is left/zeroed).
    for (hunk, &addr) in file.hunks.iter().zip(&hunk_addrs) {
        match hunk.kind {
            HunkKind::Code | HunkKind::Data => {
                for (i, &byte) in hunk.data.iter().enumerate() {
                    mem.write_u8(addr.wrapping_add(i as u32), byte);
                }
                // Zero any padding between the actual content and the
                // hunk's reserved size (e.g. a HUNK_CODE whose declared
                // header size is larger than its body, which is legal
                // though unusual).
                for i in hunk.data.len()..hunk.reserved_size {
                    mem.write_u8(addr.wrapping_add(i as u32), 0);
                }
            }
            HunkKind::Bss => {
                for i in 0..hunk.reserved_size {
                    mem.write_u8(addr.wrapping_add(i as u32), 0);
                }
            }
        }
    }

    // Third pass: apply relocations now that every hunk has an address.
    for (hunk, &addr) in file.hunks.iter().zip(&hunk_addrs) {
        for reloc in &hunk.relocs {
            let loc = addr.wrapping_add(reloc.offset);
            let target_addr = hunk_addrs[reloc.target_hunk];
            let existing = mem.read_u32(loc);
            mem.write_u32(loc, existing.wrapping_add(target_addr));
        }
    }

    Ok(LoadResult {
        entry: hunk_addrs[0],
        hunk_addrs,
        end: cursor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::FlatMemory;

    /// Appends a big-endian u32 to `buf`.
    fn push_u32(buf: &mut Vec<u8>, v: u32) {
        buf.extend_from_slice(&v.to_be_bytes());
    }

    /// Builds a minimal single-hunk HUNK_HEADER + HUNK_CODE (+ optional
    /// RELOC32) + HUNK_END file. `code` must be a multiple of 4 bytes.
    /// `relocs` is `(offset, target_hunk)` pairs, all folded into one
    /// RELOC32 block (matching what real linkers emit).
    fn build_single_hunk_code_file(code: &[u8], relocs: &[(u32, u32)]) -> Vec<u8> {
        assert_eq!(
            code.len() % 4,
            0,
            "test helper requires longword-aligned code"
        );
        let mut buf = Vec::new();
        push_u32(&mut buf, HUNK_HEADER);
        push_u32(&mut buf, 0); // no resident library names
        push_u32(&mut buf, 1); // table_size: 1 hunk
        push_u32(&mut buf, 0); // first_hunk
        push_u32(&mut buf, 0); // last_hunk
        push_u32(&mut buf, (code.len() / 4) as u32); // hunk 0 size (longwords)

        push_u32(&mut buf, HUNK_CODE);
        push_u32(&mut buf, (code.len() / 4) as u32);
        buf.extend_from_slice(code);

        if !relocs.is_empty() {
            push_u32(&mut buf, HUNK_RELOC32);
            push_u32(&mut buf, relocs.len() as u32);
            push_u32(&mut buf, relocs[0].1); // target hunk (single group)
            for &(offset, _target) in relocs {
                push_u32(&mut buf, offset);
            }
            push_u32(&mut buf, 0); // terminate RELOC32 groups
        }

        push_u32(&mut buf, HUNK_END);
        buf
    }

    #[test]
    fn parses_minimal_single_code_hunk() {
        // moveq #0,d0 ; rts
        let code = [0x70, 0x00, 0x4E, 0x75];
        let bytes = build_single_hunk_code_file(&code, &[]);

        let file = parse(&bytes).expect("should parse");
        assert_eq!(file.hunks.len(), 1);
        assert_eq!(file.hunks[0].kind, HunkKind::Code);
        assert_eq!(file.hunks[0].data, code);
        assert_eq!(file.hunks[0].reserved_size, 4);
        assert!(file.hunks[0].relocs.is_empty());
    }

    #[test]
    fn loads_single_hunk_and_sets_entry_point() {
        let code = [0x70, 0x00, 0x4E, 0x75]; // moveq #0,d0 ; rts
        let bytes = build_single_hunk_code_file(&code, &[]);
        let file = parse(&bytes).unwrap();

        let mut mem = FlatMemory::new(0x1000);
        let result = load(&file, &mut mem, 0x400).unwrap();

        assert_eq!(result.entry, 0x400);
        assert_eq!(result.hunk_addrs, vec![0x400]);
        assert_eq!(mem.read_u32(0x400), 0x7000_4E75);
    }

    #[test]
    fn intra_hunk_reloc32_adds_own_load_address() {
        // A single hunk that references its own base address at offset 0
        // (as if `dc.l hunk0_start` had been assembled with an initial
        // addend of 0), followed by two NOPs to pad it to two longwords.
        let mut code = vec![0u8; 8];
        // offset 0..4 starts as 0 (addend), offset 4..8 is a NOP (0x4E71
        // 0x4E71 as two words, i.e. 0x4E71_4E71).
        code[4..8].copy_from_slice(&0x4E71_4E71u32.to_be_bytes());
        let bytes = build_single_hunk_code_file(&code, &[(0, 0)]);
        let file = parse(&bytes).unwrap();

        let mut mem = FlatMemory::new(0x3000);
        let result = load(&file, &mut mem, 0x2000).unwrap();

        assert_eq!(result.hunk_addrs[0], 0x2000);
        // The relocated longword at offset 0 should now hold the hunk's
        // own load address (0 addend + 0x2000).
        assert_eq!(mem.read_u32(0x2000), 0x2000);
        assert_eq!(mem.read_u32(0x2004), 0x4E71_4E71);
    }

    #[test]
    fn inter_hunk_reloc32_targets_second_hunk() {
        // Two hunks: hunk 0 (code) has one reloc pointing at hunk 1
        // (data). We build the file by hand since the single-hunk helper
        // doesn't cover multi-hunk layouts.
        let mut buf = Vec::new();
        push_u32(&mut buf, HUNK_HEADER);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 2); // table_size
        push_u32(&mut buf, 0); // first_hunk
        push_u32(&mut buf, 1); // last_hunk
        push_u32(&mut buf, 1); // hunk 0 size: 1 longword
        push_u32(&mut buf, 1); // hunk 1 size: 1 longword

        // Hunk 0: HUNK_CODE containing one longword (addend 0), reloc32
        // against hunk 1, then HUNK_END.
        push_u32(&mut buf, HUNK_CODE);
        push_u32(&mut buf, 1);
        push_u32(&mut buf, 0); // addend placeholder
        push_u32(&mut buf, HUNK_RELOC32);
        push_u32(&mut buf, 1); // one offset
        push_u32(&mut buf, 1); // target hunk 1
        push_u32(&mut buf, 0); // offset 0 within hunk 0
        push_u32(&mut buf, 0); // terminate reloc groups
        push_u32(&mut buf, HUNK_END);

        // Hunk 1: HUNK_DATA, one longword, no relocs.
        push_u32(&mut buf, HUNK_DATA);
        push_u32(&mut buf, 1);
        push_u32(&mut buf, 0xDEAD_BEEF);
        push_u32(&mut buf, HUNK_END);

        let file = parse(&buf).unwrap();
        assert_eq!(file.hunks.len(), 2);

        let mut mem = FlatMemory::new(0x1000);
        let result = load(&file, &mut mem, 0x100).unwrap();

        // Hunk 0 at 0x100 (4 bytes), hunk 1 immediately after at 0x104.
        assert_eq!(result.hunk_addrs, vec![0x100, 0x104]);
        assert_eq!(mem.read_u32(0x100), 0x104); // relocated pointer to hunk 1
        assert_eq!(mem.read_u32(0x104), 0xDEAD_BEEF);
    }

    #[test]
    fn inter_hunk_drel32_applies_like_reloc32_despite_the_name() {
        // Same shape as inter_hunk_reloc32_targets_second_hunk, but two
        // offsets (an odd count -> the u16-based list ends mid-longword
        // and needs realigning before HUNK_END is read) against
        // HUNK_DREL32 instead, to confirm it's parsed with the
        // RELOC32SHORT-style u16 list and applied with the same
        // absolute-add arithmetic as HUNK_RELOC32 -- not a self-relative
        // subtraction, despite the "DREL" name (see the module docs).
        let mut buf = Vec::new();
        push_u32(&mut buf, HUNK_HEADER);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 2);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 1);
        push_u32(&mut buf, 2); // hunk 0 size: 2 longwords
        push_u32(&mut buf, 1); // hunk 1 size: 1 longword

        push_u32(&mut buf, HUNK_CODE);
        push_u32(&mut buf, 2);
        push_u32(&mut buf, 0); // addend placeholder, offset 0
        push_u32(&mut buf, 0); // addend placeholder, offset 4
        push_u32(&mut buf, HUNK_DREL32);
        buf.extend_from_slice(&2u16.to_be_bytes()); // count = 2 offsets
        buf.extend_from_slice(&1u16.to_be_bytes()); // target hunk 1
        buf.extend_from_slice(&0u16.to_be_bytes()); // offset 0
        buf.extend_from_slice(&4u16.to_be_bytes()); // offset 4
        buf.extend_from_slice(&0u16.to_be_bytes()); // terminate (count=0)
        // Odd number of u16 reads (count,hunk,off,off,terminator = 5)
        // leaves the position mid-longword; a real file pads with 2
        // zero bytes here so the next block-type u32 read (HUNK_END,
        // below) lands back on a longword boundary -- the parser must
        // consume that padding rather than just assuming it away.
        buf.extend_from_slice(&0u16.to_be_bytes());
        push_u32(&mut buf, HUNK_END);

        push_u32(&mut buf, HUNK_DATA);
        push_u32(&mut buf, 1);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, HUNK_END);

        let file = parse(&buf).unwrap();
        assert_eq!(file.hunks[0].relocs.len(), 2);

        let mut mem = FlatMemory::new(0x1000);
        let result = load(&file, &mut mem, 0x100).unwrap();

        // Hunk 0 at 0x100 (8 bytes), hunk 1 at 0x108.
        assert_eq!(result.hunk_addrs, vec![0x100, 0x108]);
        assert_eq!(mem.read_u32(0x100), 0x108, "offset 0 relocated");
        assert_eq!(mem.read_u32(0x104), 0x108, "offset 4 relocated");
    }

    #[test]
    fn bss_hunk_is_zero_filled_and_sized() {
        let mut buf = Vec::new();
        push_u32(&mut buf, HUNK_HEADER);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 2);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 1);
        push_u32(&mut buf, 1); // hunk 0: code, 1 longword
        push_u32(&mut buf, 4); // hunk 1: bss, 4 longwords (16 bytes)

        push_u32(&mut buf, HUNK_CODE);
        push_u32(&mut buf, 1);
        push_u32(&mut buf, 0x4E71_4E71); // two NOPs
        push_u32(&mut buf, HUNK_END);

        push_u32(&mut buf, HUNK_BSS);
        push_u32(&mut buf, 4);
        push_u32(&mut buf, HUNK_END);

        let file = parse(&buf).unwrap();
        assert_eq!(file.hunks[1].kind, HunkKind::Bss);
        assert_eq!(file.hunks[1].reserved_size, 16);

        let mut mem = FlatMemory::new(0x1000);
        let result = load(&file, &mut mem, 0x200).unwrap();
        assert_eq!(result.hunk_addrs, vec![0x200, 0x204]);
        for i in 0..16 {
            assert_eq!(mem.read_u8(0x204 + i), 0);
        }
    }

    #[test]
    fn masks_memory_flag_bits_from_header_size() {
        // A HUNK_HEADER size longword with the top two bits set (a
        // memory-flag marker) but a real size of 1 longword.
        let mut buf = Vec::new();
        push_u32(&mut buf, HUNK_HEADER);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 1);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 0xC000_0001); // MEMF flag bits set + size=1

        push_u32(&mut buf, HUNK_CODE);
        push_u32(&mut buf, 1);
        push_u32(&mut buf, 0x4E71_4E71);
        push_u32(&mut buf, HUNK_END);

        let file = parse(&buf).unwrap();
        assert_eq!(file.hunks[0].reserved_size, 4);
    }

    #[test]
    fn skips_symbol_and_debug_hunks() {
        let mut buf = Vec::new();
        push_u32(&mut buf, HUNK_HEADER);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 1);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 1);

        push_u32(&mut buf, HUNK_CODE);
        push_u32(&mut buf, 1);
        push_u32(&mut buf, 0x4E71_4E71);

        // HUNK_SYMBOL: one symbol "ab" (1 longword name, padded) + value,
        // then terminator.
        push_u32(&mut buf, HUNK_SYMBOL);
        push_u32(&mut buf, 1); // name is 1 longword
        buf.extend_from_slice(b"abc\0");
        push_u32(&mut buf, 0); // symbol value/offset
        push_u32(&mut buf, 0); // terminate symbol table

        // HUNK_DEBUG: 2 longwords of opaque debug data.
        push_u32(&mut buf, HUNK_DEBUG);
        push_u32(&mut buf, 2);
        push_u32(&mut buf, 0x1111_1111);
        push_u32(&mut buf, 0x2222_2222);

        push_u32(&mut buf, HUNK_END);

        let file = parse(&buf).expect("SYMBOL/DEBUG blocks should be skipped, not error");
        assert_eq!(file.hunks.len(), 1);
        assert_eq!(file.hunks[0].data, 0x4E71_4E71u32.to_be_bytes());
    }

    #[test]
    fn rejects_bad_magic() {
        let bytes = [0u8, 0, 0, 0]; // not HUNK_HEADER
        let err = parse(&bytes).unwrap_err();
        assert_eq!(err, LoadError::BadMagic { found: 0 });
    }

    #[test]
    fn rejects_truncated_file() {
        let mut buf = Vec::new();
        push_u32(&mut buf, HUNK_HEADER);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 1); // table_size 1
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 0);
        // Missing the size table entry and everything after it.
        let err = parse(&buf).unwrap_err();
        assert_eq!(err, LoadError::UnexpectedEof);
    }

    #[test]
    fn rejects_overlay_hunk_ranges() {
        let mut buf = Vec::new();
        push_u32(&mut buf, HUNK_HEADER);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 3); // table_size 3
        push_u32(&mut buf, 0); // first_hunk
        push_u32(&mut buf, 1); // last_hunk (only covers 0..=1, not 0..=2)
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 0);
        let err = parse(&buf).unwrap_err();
        assert_eq!(err, LoadError::OverlaysNotSupported);
    }

    #[test]
    fn rejects_reloc_target_out_of_range() {
        let mut buf = Vec::new();
        push_u32(&mut buf, HUNK_HEADER);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 1);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 1); // hunk 0: 1 longword

        push_u32(&mut buf, HUNK_CODE);
        push_u32(&mut buf, 1);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, HUNK_RELOC32);
        push_u32(&mut buf, 1);
        push_u32(&mut buf, 5); // target hunk 5 doesn't exist
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, HUNK_END);

        let err = parse(&buf).unwrap_err();
        assert_eq!(
            err,
            LoadError::RelocTargetOutOfRange {
                hunk_index: 0,
                target: 5
            }
        );
    }

    #[test]
    fn rejects_unknown_block_type() {
        let mut buf = Vec::new();
        push_u32(&mut buf, HUNK_HEADER);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 1);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 1);

        push_u32(&mut buf, HUNK_CODE);
        push_u32(&mut buf, 1);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 0xDEAD); // not a real block type
        let err = parse(&buf).unwrap_err();
        assert_eq!(
            err,
            LoadError::UnknownBlockType {
                hunk_index: 0,
                found: 0xDEAD
            }
        );
    }
}
