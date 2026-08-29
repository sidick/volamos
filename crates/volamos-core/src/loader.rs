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
//! - `HUNK_RELOC32SHORT` (0x3FC)
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
//! *or* with symbol/debug info left in still load. `HUNK_LIB` (link
//! library archives) is still not supported -- that's a different format
//! entirely (an indexed collection of object modules for a linker to pull
//! from, not something `LoadSeg` ever sees).
//!
//! # Overlay files (`HUNK_OVERLAY` / `HUNK_BREAK`)
//!
//! A hunk executable whose `HUNK_HEADER` declares a hunk range
//! (`first_hunk..=last_hunk`) that's a strict prefix of the full hunk
//! table (`last_hunk + 1 < table_size`) is an overlay file's *root node*:
//! only its own hunks are loaded up front, terminated by a `HUNK_OVERLAY`
//! (0x3F5) block instead of running off the end of the table. That block
//! carries the overlay manager's own bookkeeping data (see [`OverlayInfo`])
//! -- ground truth for its exact layout confirmed by disassembling a real
//! `SLink`-linked overlay executable (`AExplorer`, from Aminet) against
//! the AmigaDOS Manual's "Overlays" chapter; every field offset the
//! manager's own compiled code reads matches the documented layout
//! exactly. [`parse`] returns the root node's hunks plus
//! [`HunkFile::overlay`] when this shape is detected, instead of failing.
//!
//! The remaining hunks live in one or more *overlay nodes* later in the
//! file, each its own `HUNK_HEADER`...hunks...`HUNK_BREAK` (0x3F6) block,
//! loaded on demand by the overlay manager (guest code shipped inside the
//! root node) via `LoadSeg(NULL, table, fh)` -- see [`parse_overlay_node`],
//! which parses one such node given the file offset the overlay manager
//! seeks to before calling it (that offset itself comes from the
//! `HUNK_OVERLAY` table's `ot_FilePosition` field, at runtime, not
//! something this parser needs to track). A node's hunks continue the
//! root's global hunk numbering (`first_hunk` can be nonzero) and their
//! relocations may target already-loaded ancestor hunks outside the
//! node's own range, so [`OverlayNode`] reports [`OverlayNode::first_hunk`]
//! and leaves cross-node relocation-target validation to the caller
//! (this parser alone can't know whether a target hunk index is valid
//! without the full tree's hunk count on hand).
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
const HUNK_RELOC32SHORT: u32 = 0x3FC;
const HUNK_OVERLAY: u32 = 0x3F5;
const HUNK_BREAK: u32 = 0x3F6;

/// Mask for the memory-flag bits (`MEMF_CHIP`/`MEMF_FAST`/extended-flag
/// marker) that can be packed into the top bits of a hunk-size longword in
/// `HUNK_HEADER` **and** of a `HUNK_CODE`/`HUNK_DATA`/`HUNK_BSS` type
/// longword in the file body (same encoding both places: bit 30 =
/// chip, bit 31 = fast, both = an extra longword of memory attributes
/// follows). We don't act on the flags (no chip/fast distinction in
/// this emulator's flat address space) but we do need to mask them off
/// to recover the real size/type -- found via the real `DiskSpeed` 4.2
/// benchmark, whose data hunk is marked `MEMF_CHIP` (`0x400003EA`) for
/// its trackdisk I/O buffers.
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
    /// The root node's `first_hunk`/`last_hunk` range doesn't cover the
    /// whole hunk table (i.e. this is an overlay file) but the root node's
    /// hunks aren't followed by the required `HUNK_OVERLAY` marker.
    ExpectedOverlayMarker { hunk_index: usize, found: u32 },
    /// The header's `first_hunk` is greater than `last_hunk`, or (for a
    /// root node specifically) `first_hunk` isn't `0`, or a declared hunk
    /// range extends past the header's own `table_size`.
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
            LoadError::ExpectedOverlayMarker { hunk_index, found } => write!(
                f,
                "hunk {hunk_index}: expected HUNK_OVERLAY (0x3F5) to follow the root node's \
                 partial hunk range, found {found:#x}"
            ),
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

/// The `HUNK_OVERLAY` block's payload, verbatim -- exactly the longwords
/// `oh_OVTab` points to at runtime (starting at the tree-depth element,
/// per the table in the module docs), not reinterpreted into a richer
/// structure here since its internal layout (ordinate array, then
/// 7-longword `SymTab` entries) is specific to the hierarchical overlay
/// manager and this parser doesn't need to understand it to load nodes --
/// only the overlay *manager* (guest code) reads it, driving `LoadSeg`
/// calls this runtime services like any other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayTable {
    /// The `l+1` longwords following the `HUNK_OVERLAY` block's own
    /// length field `l` (see the module docs -- this off-by-one is the
    /// real on-disk convention, confirmed against a real overlay file).
    pub raw: Vec<u32>,
}

/// A root node's overlay metadata: the [`OverlayTable`] itself, plus the
/// full hunk table size from the root `HUNK_HEADER` (`t_size`) -- needed
/// by a caller building the runtime `oh_Segments` array, whose length
/// this defines (see the module docs and the AmigaDOS Manual's
/// `OverlayHeader` description).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayInfo {
    pub table: OverlayTable,
    /// Total hunk count across the whole overlay tree (root + every
    /// node), i.e. the root `HUNK_HEADER`'s `table_size`.
    pub total_hunks: usize,
}

/// A fully parsed hunk executable: an ordered list of hunks (hunk 0 is
/// conventionally the entry hunk). `overlay` is `Some` when this is an
/// overlay file's root node -- see the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkFile {
    pub hunks: Vec<Hunk>,
    pub overlay: Option<OverlayInfo>,
}

/// One overlay node's hunks, as parsed by [`parse_overlay_node`]. Unlike
/// [`HunkFile::hunks`] (always 0-indexed), `hunks[i]` here is the node's
/// `i`th hunk in file order but the *global* hunk index (continuing the
/// root's numbering) is `first_hunk + i` -- callers loading this into a
/// shared runtime segment table need that offset. Relocation targets
/// inside these hunks (`Hunk::relocs`' `target_hunk`) are also global
/// indices, and may point outside `hunks` entirely (at an
/// already-resident ancestor node's hunk) -- this parser doesn't validate
/// them, since it has no way to know the full tree's hunk count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayNode {
    pub first_hunk: usize,
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

    /// Steps back over the longword just read, so it can be re-read by the
    /// next `read_u32`. Used to hand an implicit hunk terminator back to the
    /// outer loop (see the `HUNK_END` handling in [`parse`]).
    fn unread_u32(&mut self) {
        debug_assert!(self.pos >= 4);
        self.pos -= 4;
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

/// A parsed `HUNK_HEADER`'s own fields, returned alongside the hunks
/// [`parse_node`] reads for its declared range.
struct HeaderInfo {
    table_size: usize,
    first_hunk: usize,
    last_hunk: usize,
}

/// Reads one node's `HUNK_HEADER` and the hunk bodies for its declared
/// `first_hunk..=last_hunk` range (the *global* indices; the returned
/// `Vec<Hunk>` is 0-indexed by *position within this node*, i.e.
/// `hunks[i]` is global hunk `first_hunk + i`). Shared by [`parse`] (the
/// root node, which requires `first_hunk == 0`) and
/// [`parse_overlay_node`] (any later node, `first_hunk` typically
/// nonzero). Does not validate `Reloc32::target_hunk` against any global
/// hunk count -- callers do that themselves, since only they know whether
/// cross-node targets are in range (see the module docs).
fn parse_node(r: &mut Reader<'_>) -> Result<(HeaderInfo, Vec<Hunk>), LoadError> {
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
    if first_hunk > last_hunk || last_hunk >= table_size {
        return Err(LoadError::BadHunkRange {
            first: first_hunk,
            last: last_hunk,
        });
    }

    // Unlike the (now-historical) assumption that a header always
    // declares `table_size` size entries, a header only ever declares
    // sizes for the hunks *it* is about to load -- `last_hunk - first_hunk
    // + 1` entries, which just happens to equal `table_size` in the
    // common (non-overlay, first_hunk == 0) case. Confirmed against a
    // real overlay node's on-disk header, whose size table has exactly
    // one entry for its one hunk despite `table_size` naming the whole
    // tree's hunk count.
    let n_sizes = last_hunk - first_hunk + 1;
    let mut declared_sizes = Vec::with_capacity(n_sizes);
    for _ in 0..n_sizes {
        let raw = r.read_u32()?;
        let longwords = raw & !HUNK_SIZE_FLAGS_MASK;
        declared_sizes.push(longwords as usize * 4);
    }

    let mut hunks = Vec::with_capacity(n_sizes);
    for (i, &reserved_size) in declared_sizes.iter().enumerate() {
        let hunk_index = first_hunk + i;
        let raw_body_type = r.read_u32()?;
        // Memory-flag bits (see HUNK_SIZE_FLAGS_MASK's doc) apply to
        // body type words too; both bits set means an extra longword of
        // memory attributes follows the type word -- consume and ignore
        // it (this runtime has one flat memory type).
        let body_type = raw_body_type & !HUNK_SIZE_FLAGS_MASK;
        if raw_body_type & HUNK_SIZE_FLAGS_MASK == HUNK_SIZE_FLAGS_MASK
            && matches!(body_type, HUNK_CODE | HUNK_DATA | HUNK_BSS)
        {
            r.read_u32()?;
        }
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
            // Mask memory-flag bits here too: the "next hunk's body type"
            // put-back case below can see a flagged CODE/DATA/BSS word
            // (unread_u32 rewinds, so the outer loop re-reads the raw
            // word and does its own masking/extra-longword handling).
            let block_type = r.read_u32()? & !HUNK_SIZE_FLAGS_MASK;
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
                // HUNK_RELOC32SHORT and HUNK_DREL32 are two different
                // linker-assigned IDs for the identical on-disk format and
                // fixup arithmetic (confirmed against
                // <https://amiga-dev.wikidot.com/file-format:hunk>, which
                // documents HUNK_DREL32 as "handled exactly the same as
                // HUNK_RELOC32SHORT") -- same uint16 count/hunk-number/
                // offsets list, same absolute mem[loc] += target_hunk_addr
                // arithmetic as HUNK_RELOC32, same longword realignment
                // after. Different real linkers emit one ID or the other
                // for this identical optimization (found while auditing
                // this file's hunk-type coverage against the spec, not
                // from a specific corpus binary yet).
                HUNK_DREL32 | HUNK_RELOC32SHORT => {
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
                // A new hunk body implicitly ends the current hunk: HUNK_END
                // is not required between hunks, and real linkers omit it
                // (Commodore's own `Installer` does). LoadSeg accepts this,
                // so put the block type back and let the outer loop read it
                // as the next hunk's body. HUNK_OVERLAY/HUNK_BREAK are the
                // same story one level up: they terminate the *node* (not
                // just this hunk), so they're put back too, for the node
                // reader (parse/parse_overlay_node) to interpret once this
                // hunk-range loop is done.
                HUNK_CODE | HUNK_DATA | HUNK_BSS | HUNK_OVERLAY | HUNK_BREAK => {
                    r.unread_u32();
                    break;
                }
                other => {
                    return Err(LoadError::UnknownBlockType {
                        hunk_index,
                        found: other,
                    });
                }
            }
        }

        // Validate relocation offsets now that we know this hunk's size
        // (an intra-hunk check, always valid regardless of node/root
        // context). Cross-hunk target validity is the caller's job (see
        // this function's doc).
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

    Ok((
        HeaderInfo {
            table_size,
            first_hunk,
            last_hunk,
        },
        hunks,
    ))
}

/// Parses a hunk executable's bytes into a [`HunkFile`]: the root node's
/// hunks (index 0 is the entry hunk), plus [`HunkFile::overlay`] if the
/// root's header only declares a prefix of the full hunk table -- see the
/// module docs' "Overlay files" section.
///
/// This only interprets the file structure; it does not decide where
/// anything is loaded in guest memory (see [`load`] for that).
pub fn parse(bytes: &[u8]) -> Result<HunkFile, LoadError> {
    let mut r = Reader::new(bytes);
    let (header, hunks) = parse_node(&mut r)?;

    if header.first_hunk != 0 {
        // A root node always starts numbering at 0; a nonzero first_hunk
        // here would mean this "file" is actually a bare overlay node
        // with no root, which isn't a loadable top-level executable.
        return Err(LoadError::BadHunkRange {
            first: header.first_hunk,
            last: header.last_hunk,
        });
    }

    // Root-only relocation-target validation: a root hunk's relocations
    // can only ever target other root hunks (forward references into
    // not-yet-loaded overlay nodes go through the overlay manager's
    // symbol table instead, never a plain RELOC32), so `hunks.len()` is
    // the right bound here specifically -- unlike parse_overlay_node,
    // which can't assume that.
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

    let overlay = if header.last_hunk + 1 == header.table_size {
        None
    } else {
        let marker = r.read_u32()?;
        if marker != HUNK_OVERLAY {
            return Err(LoadError::ExpectedOverlayMarker {
                hunk_index: header.last_hunk + 1,
                found: marker,
            });
        }
        // Table size is l+1 longwords, not l -- a real, if unorthogonal,
        // on-disk convention (see the module docs), confirmed against a
        // real overlay executable's raw bytes.
        let l = r.read_u32()? as usize;
        let mut raw = Vec::with_capacity(l + 1);
        for _ in 0..=l {
            raw.push(r.read_u32()?);
        }
        Some(OverlayInfo {
            table: OverlayTable { raw },
            total_hunks: header.table_size,
        })
    };

    Ok(HunkFile { hunks, overlay })
}

/// Parses one overlay node's hunks (its own `HUNK_HEADER` through
/// `HUNK_BREAK`/EOF) starting at `file_offset` bytes into `bytes` -- the
/// file position an overlay manager `Seek()`s to before calling
/// `LoadSeg(NULL, table, fh)` (see the module docs). A trailing
/// `HUNK_BREAK` is consumed if present but not required (the last node in
/// a file may simply end at EOF, matching the AmigaDOS Manual's own
/// leniency here -- "It is not required at the end of the root node",
/// and real linkers extend the same leniency to the very last node).
pub fn parse_overlay_node(bytes: &[u8], file_offset: usize) -> Result<OverlayNode, LoadError> {
    let slice = bytes.get(file_offset..).ok_or(LoadError::UnexpectedEof)?;
    let mut r = Reader::new(slice);
    let (header, hunks) = parse_node(&mut r)?;
    Ok(OverlayNode {
        first_hunk: header.first_hunk,
        hunks,
    })
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

    /// Builds a two-code-hunk file, optionally omitting the `HUNK_END` that
    /// would normally separate hunk 0 from hunk 1. Commodore's `Installer`
    /// is laid out this way.
    fn build_two_hunk_code_file(code0: &[u8], code1: &[u8], end_after_first: bool) -> Vec<u8> {
        let mut buf = Vec::new();
        push_u32(&mut buf, HUNK_HEADER);
        push_u32(&mut buf, 0); // no resident library names
        push_u32(&mut buf, 2); // table_size: 2 hunks
        push_u32(&mut buf, 0); // first_hunk
        push_u32(&mut buf, 1); // last_hunk
        push_u32(&mut buf, (code0.len() / 4) as u32);
        push_u32(&mut buf, (code1.len() / 4) as u32);

        push_u32(&mut buf, HUNK_CODE);
        push_u32(&mut buf, (code0.len() / 4) as u32);
        buf.extend_from_slice(code0);
        if end_after_first {
            push_u32(&mut buf, HUNK_END);
        }

        push_u32(&mut buf, HUNK_CODE);
        push_u32(&mut buf, (code1.len() / 4) as u32);
        buf.extend_from_slice(code1);
        push_u32(&mut buf, HUNK_END);
        buf
    }

    /// `HUNK_END` between hunks is optional: a new hunk body ends the
    /// previous hunk. Both spellings must parse identically.
    #[test]
    fn hunk_end_between_hunks_is_optional() {
        let code0 = [0x70, 0x00, 0x4E, 0x75]; // moveq #0,d0 ; rts
        let code1 = [0x70, 0x01, 0x4E, 0x75]; // moveq #1,d0 ; rts

        let with_end = parse(&build_two_hunk_code_file(&code0, &code1, true)).expect("with END");
        let without_end =
            parse(&build_two_hunk_code_file(&code0, &code1, false)).expect("without END");

        for file in [&with_end, &without_end] {
            assert_eq!(file.hunks.len(), 2);
            assert_eq!(file.hunks[0].kind, HunkKind::Code);
            assert_eq!(file.hunks[0].data, code0);
            assert_eq!(file.hunks[1].kind, HunkKind::Code);
            assert_eq!(file.hunks[1].data, code1);
        }
        assert_eq!(with_end.hunks, without_end.hunks);
    }

    /// The omitted-`HUNK_END` case is realistic only if it still works with
    /// a `HUNK_RELOC32` block between the hunk body and the implicit end --
    /// real linker output almost always has relocs there, unlike the bare
    /// two-hunk fixture above.
    #[test]
    fn hunk_end_between_hunks_is_optional_after_a_reloc_block() {
        let code0 = [0x70, 0x00, 0x4E, 0x75]; // moveq #0,d0 ; rts
        let code1 = [0x70, 0x01, 0x4E, 0x75]; // moveq #1,d0 ; rts

        let mut buf = Vec::new();
        push_u32(&mut buf, HUNK_HEADER);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 2);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 1);
        push_u32(&mut buf, (code0.len() / 4) as u32);
        push_u32(&mut buf, (code1.len() / 4) as u32);

        push_u32(&mut buf, HUNK_CODE);
        push_u32(&mut buf, (code0.len() / 4) as u32);
        buf.extend_from_slice(&code0);
        // A RELOC32 block pointing at hunk 1, then straight into hunk 1's
        // body with no HUNK_END in between.
        push_u32(&mut buf, HUNK_RELOC32);
        push_u32(&mut buf, 1); // one reloc
        push_u32(&mut buf, 1); // target hunk 1
        push_u32(&mut buf, 0); // offset 0
        push_u32(&mut buf, 0); // terminate RELOC32 groups

        push_u32(&mut buf, HUNK_CODE);
        push_u32(&mut buf, (code1.len() / 4) as u32);
        buf.extend_from_slice(&code1);
        push_u32(&mut buf, HUNK_END);

        let file = parse(&buf).expect("reloc block then implicit end should parse");
        assert_eq!(file.hunks.len(), 2);
        assert_eq!(file.hunks[0].kind, HunkKind::Code);
        assert_eq!(file.hunks[0].data, code0);
        assert_eq!(file.hunks[0].relocs.len(), 1);
        assert_eq!(file.hunks[0].relocs[0].target_hunk, 1);
        assert_eq!(file.hunks[1].kind, HunkKind::Code);
        assert_eq!(file.hunks[1].data, code1);
    }

    /// The implicit-end match arm covers all three hunk-body types, not
    /// just `HUNK_CODE` -- a `HUNK_BSS` (no body bytes, just a repeated
    /// size field) must also be able to follow a `HUNK_END`-less hunk.
    #[test]
    fn hunk_end_between_hunks_is_optional_before_a_bss_hunk() {
        let code0 = [0x70, 0x00, 0x4E, 0x75]; // moveq #0,d0 ; rts
        let bss_longwords: u32 = 4; // 16 bytes of BSS

        let mut buf = Vec::new();
        push_u32(&mut buf, HUNK_HEADER);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 2);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 1);
        push_u32(&mut buf, (code0.len() / 4) as u32);
        push_u32(&mut buf, bss_longwords);

        push_u32(&mut buf, HUNK_CODE);
        push_u32(&mut buf, (code0.len() / 4) as u32);
        buf.extend_from_slice(&code0);
        // No HUNK_END: straight into hunk 1's HUNK_BSS body.

        push_u32(&mut buf, HUNK_BSS);
        push_u32(&mut buf, bss_longwords);
        push_u32(&mut buf, HUNK_END);

        let file = parse(&buf).expect("BSS hunk after implicit end should parse");
        assert_eq!(file.hunks.len(), 2);
        assert_eq!(file.hunks[0].kind, HunkKind::Code);
        assert_eq!(file.hunks[1].kind, HunkKind::Bss);
        assert_eq!(file.hunks[1].reserved_size, bss_longwords as usize * 4);
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

    /// `HUNK_RELOC32SHORT` (0x3FC) is a distinct block-type ID from
    /// `HUNK_DREL32` (0x3F7), but the spec documents them as byte-for-byte
    /// the identical on-disk format and fixup arithmetic -- different real
    /// linkers pick one ID or the other for the same optimization. Same
    /// shape as `inter_hunk_drel32_applies_like_reloc32_despite_the_name`,
    /// just with `HUNK_RELOC32SHORT` in place of `HUNK_DREL32`.
    #[test]
    fn inter_hunk_reloc32short_applies_same_as_drel32() {
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
        push_u32(&mut buf, HUNK_RELOC32SHORT);
        buf.extend_from_slice(&2u16.to_be_bytes()); // count = 2 offsets
        buf.extend_from_slice(&1u16.to_be_bytes()); // target hunk 1
        buf.extend_from_slice(&0u16.to_be_bytes()); // offset 0
        buf.extend_from_slice(&4u16.to_be_bytes()); // offset 4
        buf.extend_from_slice(&0u16.to_be_bytes()); // terminate (count=0)
        // Odd number of u16 reads again -- same mid-longword realignment
        // as the HUNK_DREL32 test.
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

    /// Builds an overlay-shaped root node (`table_size` > its own hunk
    /// range) with one real `HUNK_CODE` hunk, followed by whatever bytes
    /// `after` supplies verbatim (the caller controls whether that's a
    /// real `HUNK_OVERLAY` block or something else).
    fn build_overlay_shaped_root(table_size: u32, after: &[u8]) -> Vec<u8> {
        let code = [0x70u8, 0x00, 0x4E, 0x75]; // moveq #0,d0 ; rts
        let mut buf = Vec::new();
        push_u32(&mut buf, HUNK_HEADER);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, table_size); // table_size (whole tree)
        push_u32(&mut buf, 0); // first_hunk
        push_u32(&mut buf, 0); // last_hunk (root is just hunk 0)
        push_u32(&mut buf, 1); // hunk 0 size: 1 longword

        push_u32(&mut buf, HUNK_CODE);
        push_u32(&mut buf, 1);
        buf.extend_from_slice(&code);
        push_u32(&mut buf, HUNK_END);

        buf.extend_from_slice(after);
        buf
    }

    #[test]
    fn rejects_overlay_shaped_root_without_a_real_overlay_marker() {
        let mut after = Vec::new();
        push_u32(&mut after, 0xDEAD); // not HUNK_OVERLAY
        let buf = build_overlay_shaped_root(3, &after);
        let err = parse(&buf).unwrap_err();
        assert_eq!(
            err,
            LoadError::ExpectedOverlayMarker {
                hunk_index: 1,
                found: 0xDEAD
            }
        );
    }

    #[test]
    fn parses_overlay_root_and_captures_the_overlay_table() {
        let mut after = Vec::new();
        push_u32(&mut after, HUNK_OVERLAY);
        push_u32(&mut after, 2); // l = 2 -> table is l+1 = 3 longwords
        push_u32(&mut after, 3); // od (tree depth)
        push_u32(&mut after, 0); // ordinate[0]
        push_u32(&mut after, 0); // ordinate[1]
        let buf = build_overlay_shaped_root(3, &after);

        let file = parse(&buf).expect("overlay root should parse");
        assert_eq!(file.hunks.len(), 1, "only the root's own hunk range");
        let overlay = file.overlay.expect("overlay info should be captured");
        assert_eq!(overlay.total_hunks, 3);
        assert_eq!(overlay.table.raw, vec![3, 0, 0]);
    }

    #[test]
    fn parse_overlay_node_reads_hunks_at_the_given_file_offset_with_global_numbering() {
        // A node whose HUNK_HEADER declares first_hunk = last_hunk = 2
        // (continuing a root's global numbering), preceded by some
        // unrelated filler bytes at the start of the buffer to exercise
        // the file_offset parameter.
        let filler = [0xAAu8; 16];
        let code = [0x70u8, 0x01, 0x4E, 0x75]; // moveq #1,d0 ; rts

        let mut buf = Vec::new();
        buf.extend_from_slice(&filler);
        let node_offset = buf.len();

        push_u32(&mut buf, HUNK_HEADER);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 3); // table_size (whole tree)
        push_u32(&mut buf, 2); // first_hunk
        push_u32(&mut buf, 2); // last_hunk
        push_u32(&mut buf, 1); // hunk 2's size: 1 longword

        push_u32(&mut buf, HUNK_CODE);
        push_u32(&mut buf, 1);
        buf.extend_from_slice(&code);
        push_u32(&mut buf, HUNK_BREAK);

        let node = parse_overlay_node(&buf, node_offset).expect("node should parse");
        assert_eq!(node.first_hunk, 2);
        assert_eq!(node.hunks.len(), 1);
        assert_eq!(node.hunks[0].data, code);
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
