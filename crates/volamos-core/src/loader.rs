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
//! `HUNK_NAME` (0x3E8), `HUNK_SYMBOL` (0x3F0) and `HUNK_DEBUG` (0x3F1)
//! blocks are recognized wherever a hunk boundary allows one to appear --
//! immediately before a hunk's body (a real assembler-produced binary,
//! e.g. one built by `vasm`/`PhxAss` with source-line debug info left in,
//! can open a hunk with one or more `HUNK_DEBUG` blocks before its
//! `HUNK_CODE`), and in their traditional position after a body's
//! relocations. Any number of them can appear back-to-back in either
//! spot. This is so binaries built with `-nosym` *or* with symbol/debug
//! info left in still load. `HUNK_LIB` (link library archives) is still
//! not supported -- that's a different format entirely (an indexed
//! collection of object modules for a linker to pull from, not something
//! `LoadSeg` ever sees).
//!
//! Only `HUNK_NAME` is discarded outright (see [`skip_metadata_block`]).
//! `HUNK_DEBUG` and `HUNK_SYMBOL` are *captured*, not discarded -- their
//! raw bytes go into [`Hunk::debug_blocks`]/[`Hunk::symbol_blocks`]
//! (bounds-checked copies, same precedent as everything else here -- see
//! [`read_debug_block_payload`]/[`read_symbol_block_raw`]) but aren't
//! *interpreted* at parse time; see "Source-line info" and "Symbol
//! attribution" below. `HUNK_SYMBOL`'s on-disk shape is not the simple
//! longword-count-prefixed payload `HUNK_NAME` uses -- it's a list of
//! `{ name_length_longwords, name, value }` entries terminated by a zero
//! `name_length`, so capturing it means walking that list to find where
//! it ends (see [`read_symbol_block_raw`]), not just copying `n`
//! longwords like `HUNK_NAME`/`HUNK_DEBUG`.
//!
//! # Source-line info (`HUNK_DEBUG` / `LINE` blocks, issue #74)
//!
//! A `HUNK_DEBUG` block's payload can hold several different things,
//! identified by a 4-byte magic after a leading base-offset longword
//! (or, for one family below, by *no* magic at all). The one this
//! loader decodes is `LINE`: a source filename plus a list of `(source
//! line number, hunk-relative byte offset)` pairs. Confirmed
//! byte-for-byte against two real producers: a real `LawBreaker` binary
//! (magic `LINE`, filename `LawBreaker.asm`, built by an assembler) and
//! a real SAS/C 6.58 `sc DEBUG=LINE` object file (magic `LINE`,
//! filename `work:hello.c`, alongside sibling `OPTS`/`SRC6` blocks this
//! loader doesn't interpret). `OPTS`/`SRC6` (SAS/C) and `HEAD` (the
//! first four bytes of a `HEADDBGV01` directory block some assemblers
//! emit, indexing the file offsets of their own `HUNK_DEBUG` blocks --
//! redundant with the blocks already appearing in the hunk stream, so
//! not needed here) are recognized magics that this loader deliberately
//! does *not* act on.
//!
//! **`m68k-amigaos-gcc` is not covered.** Checked empirically against a
//! real local install (`-g -O0 -noixemul`): its `HUNK_DEBUG` payload is
//! stabs-format debug info, not `LINE` -- and per a reference decode
//! (alfishe/amiga-bootcamp's `hunk_debug_info.md`), the stabs family is
//! tagged `=APS` (SAS/C 6.x) or `=GCC`, "or no tag at all" for
//! `m68k-amigaos-gcc` specifically, which is exactly what was observed:
//! the four bytes where `LINE`/`OPTS`/`SRC6` carry an ASCII magic are
//! `00 00 00 10` here, not a magic at all. Stabs entries encode line
//! info as `N_SLINE` records against a separate string table -- a
//! different, more involved format this loader does not attempt to
//! parse (no magic to safely dispatch on, and it needs string-table
//! handling this format doesn't). An untagged/stabs block is simply
//! unrecognized here and skipped, same as any other unknown magic --
//! see [`DebugMagic`]. This matters because gcc-built programs are a
//! large share of what this loader ultimately serves diagnostics for;
//! they get no `file:line` from this code today, only assembler/SAS/C
//! binaries built with `LINE`-format debug info do.
//!
//! Magic dispatch is an explicit match on [`DebugMagic`] (see
//! [`parse_line_debug_block`]) rather than an ad hoc `if tag == "LINE"`,
//! specifically so a future `=APS`/`=GCC`/untagged-stabs decoder has an
//! obvious arm to add rather than a buried comparison to unpick.
//!
//! Decoding a hunk's captured `HUNK_DEBUG` blocks into a queryable
//! [`HunkLineInfo`] is lazy -- see [`Hunk::line_info`]'s doc for why --
//! and the offset-to-line lookup within one is a greatest-offset-<=-target
//! binary search, not an exact match, because the pairs are sparse (one
//! per source line, not per instruction). See [`HunkLineInfo::lookup`]
//! and [`LoadResult::lookup_line`] (which also subtracts a hunk's load
//! address to turn a guest PC into the hunk-relative offset this all
//! operates on).
//!
//! **The pairs aren't guaranteed to be offset-monotonic, or even in
//! line-number order.** A real `PhxAss` `LINEDEBUG` build
//! (`fixtures/linetest`, a repo-owned fixture built specifically to
//! exercise this) emits a data-hunk `LINE` block whose pairs, verbatim
//! in file order, are `(33, 0x0e), (36, 0x00), (37, 0x15)` -- offset
//! *decreasing* from the first pair to the second. Per PhxAss's own
//! author this is because line 33 is the `section data,data` directive
//! itself, and the offset it records lands inside the following
//! message string rather than at a clean boundary; nothing about *why*
//! needs to be understood here, only that it happens on a real,
//! unmodified assembler build, not just a theoretical malformed-input
//! case. [`Hunk::line_info`] always (re-)sorts by offset rather than
//! trusting file order for exactly this reason -- see
//! `real_linetest_fixture_matches_known_pairs` and
//! `non_monotonic_pairs_are_sorted_before_lookup` in this module's
//! tests. A lookup at an offset that several out-of-order-by-line-number
//! entries could plausibly "claim" (e.g. `0x10` for the pairs above)
//! resolves purely by offset, per the documented "greatest offset `<=`
//! target" rule -- never by line number and never by which pair
//! happened to appear first or last in the file.
//!
//! # Symbol attribution (`HUNK_SYMBOL`, issue #74 follow-up)
//!
//! `m68k-amigaos-gcc` builds carry no `LINE` data this loader can decode
//! (see above), but they do carry real `HUNK_SYMBOL` data -- a real
//! local `-g -O0 -noixemul` build has 47/15/19 named symbols across its
//! code/data/bss hunks, e.g. `_free` at `0x226e`. That's real, useful
//! attribution (`_free+0x5`) for a large share of the binaries this
//! loader otherwise has nothing but a bare hex address for.
//!
//! [`Hunk::symbol_table`] decodes a hunk's captured
//! [`Hunk::symbol_blocks`] into a [`SymbolTable`], lazily, the same
//! rationale as `HunkLineInfo`. [`SymbolTable::lookup`] is the same
//! greatest-offset-<=-target search as `LINE`'s (a symbol's value marks
//! where a routine *starts*, not every address inside it) -- but with
//! one addition: the span attributed to a table's *last* symbol is
//! capped at the owning hunk's own size, so a small symbol table can't
//! "match" an offset arbitrarily far past its final entry (a 3-symbol
//! table matching 40 KB past the last one and reporting a nonsense delta
//! was the concrete failure mode that prompted this cap). See
//! [`SymbolTable::lookup`]'s doc for the full reasoning.
//!
//! [`Hunk::locate`] and [`LoadResult::lookup_location`] combine the two
//! into a single [`Location`] lookup with a fixed precedence -- `LINE`
//! (`file:line`) first, `HUNK_SYMBOL` (`symbol+offset`) as a fallback,
//! nothing if neither has coverage -- so callers annotating diagnostics
//! have one entry point rather than reimplementing that ordering (or
//! calling two separate lookups) at every site.
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
const HUNK_NAME: u32 = 0x3E8;
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
    /// Raw payload bytes of every `HUNK_DEBUG` block that appeared for
    /// this hunk (in file order; a hunk can have more than one -- see
    /// the module docs), captured verbatim but *not* interpreted --
    /// see [`Hunk::line_info`], which decodes them lazily on demand.
    pub debug_blocks: Vec<Vec<u8>>,
    /// Raw bytes of every `HUNK_SYMBOL` block that appeared for this
    /// hunk (in file order; there can be more than one, same as
    /// `debug_blocks`), captured verbatim but *not* decoded into names
    /// -- see [`Hunk::symbol_table`], which does that lazily on demand.
    /// This is the `symbol+offset` fallback for binaries with no usable
    /// `LINE` data (most `m68k-amigaos-gcc` output -- see the module
    /// docs), added in issue #74's follow-up.
    pub symbol_blocks: Vec<Vec<u8>>,
}

impl Hunk {
    /// Decodes this hunk's captured [`Hunk::debug_blocks`] into
    /// source-line info, on demand.
    ///
    /// This is deliberately not done during [`parse`]: debug info can
    /// dominate a small file -- the real `LawBreaker` fixture used to
    /// develop this feature (issue #74) is 776 bytes total, 456 of
    /// which is debug data -- and the common case (no diagnostic ever
    /// needs to be annotated) shouldn't pay to decode line tables it
    /// will never query. `parse`/`load` always capture the raw block
    /// bytes (cheap: a bounds-checked copy, no interpretation -- see
    /// [`read_debug_block_payload`]); this method is where the actual
    /// per-block parsing happens. A caller that wants file:line for many
    /// diagnostics against the same binary should call this once per
    /// hunk and cache the resulting [`HunkLineInfo`], rather than
    /// re-decoding on every lookup (see [`LoadResult::lookup_line`],
    /// which does exactly that re-decoding and documents the tradeoff).
    pub fn line_info(&self) -> HunkLineInfo {
        let mut entries: Vec<(String, LineEntry)> = Vec::new();
        for block in &self.debug_blocks {
            if let Some((filename, block_entries)) = parse_line_debug_block(block) {
                entries.extend(block_entries.into_iter().map(|e| (filename.clone(), e)));
            }
        }
        // Always (re-)sort rather than trust file order: verified sorted
        // in both real producers checked for issue #74, but nothing in
        // the format guarantees it, and merging more than one LINE block
        // (see HunkLineInfo's docs) needs a sort across blocks regardless.
        entries.sort_by_key(|(_, e)| e.offset);
        HunkLineInfo { entries }
    }

    /// Decodes this hunk's captured [`Hunk::symbol_blocks`] into a
    /// queryable [`SymbolTable`], on demand -- same lazy-decode
    /// rationale as [`Hunk::line_info`] (see its doc): `parse`/`load`
    /// always capture the raw bytes cheaply (see
    /// [`read_symbol_block_raw`]), and turning them into `String`s and
    /// sorting by value only happens when a caller actually asks.
    pub fn symbol_table(&self) -> SymbolTable {
        let mut entries = Vec::new();
        for block in &self.symbol_blocks {
            entries.extend(parse_symbol_block(block));
        }
        entries.sort_by_key(|(_, value)| *value);
        SymbolTable { entries }
    }

    /// Best available [`Location`] for a hunk-relative `offset`: tries
    /// [`Hunk::line_info`] first (`file:line` is always more precise
    /// than a symbol when it's available), falls back to
    /// [`Hunk::symbol_table`] (`symbol+offset`, bounded -- see
    /// [`SymbolTable::lookup`]), and returns `None` if neither has
    /// anything to say, in which case the caller's existing "print the
    /// raw address" fallback applies. This is the one place the
    /// LINE-then-symbol precedence lives, per issue #74's coordinator
    /// follow-up, specifically so callers annotating diagnostics don't
    /// each reimplement the ordering (or call two lookups) themselves.
    pub fn locate(&self, offset: u32) -> Option<Location> {
        if let Some((file, line)) = self.line_info().lookup(offset) {
            return Some(Location::Line {
                file: file.to_string(),
                line,
            });
        }
        let table = self.symbol_table();
        let (name, delta) = table.lookup(offset, self.reserved_size as u32)?;
        Some(Location::Symbol {
            name: name.to_string(),
            offset: delta,
        })
    }
}

/// Best available source attribution for a hunk-relative offset (or,
/// via [`LoadResult::lookup_location`], a guest address): which of
/// `LINE` debug info or `HUNK_SYMBOL` data -- if either -- covers it.
/// See [`Hunk::locate`] for the precedence between the two variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// A `LINE` block's entry covers this offset exactly (greatest
    /// offset `<=` target -- see [`HunkLineInfo::lookup`]).
    Line { file: String, line: u32 },
    /// No `LINE` coverage, but a preceding `HUNK_SYMBOL` entry does,
    /// within [`SymbolTable::lookup`]'s bounded span. `offset` here is
    /// the delta from the symbol's own value, e.g. `Do_Law+0x12`.
    Symbol { name: String, offset: u32 },
}

/// A `HUNK_DEBUG` block's 4-byte payload magic (the four bytes right
/// after the leading base-offset longword -- see the module docs),
/// dispatched on explicitly so adding a decoder for a currently-ignored
/// family (`=APS`/`=GCC` stabs, or gcc's untagged stabs, per the
/// coordinator's finding on issue #74) is a matter of adding a match arm
/// here, not unpicking an `if` buried in a parse function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DebugMagic {
    /// Source-line table: filename + `(line, offset)` pairs. The only
    /// family this loader decodes -- see [`parse_line_debug_block`].
    Line,
    /// SAS/C 6.58 compiler-options block, emitted alongside `LINE`.
    /// Recognized so it doesn't fall through as "unknown", but its
    /// payload isn't interpreted.
    Opts,
    /// SAS/C 6.58 source-file-list block, emitted alongside `LINE`.
    /// Recognized but not interpreted, same as `Opts`.
    Src6,
    /// The first four bytes of a `HEADDBGV01` debug-block directory some
    /// assemblers emit (the LawBreaker binary's producer among them),
    /// indexing the file offsets of the `HUNK_DEBUG` blocks already
    /// present in the hunk stream. Redundant with those blocks, so not
    /// interpreted.
    Head,
    /// Anything else, including no recognizable magic at all -- e.g.
    /// `m68k-amigaos-gcc`'s stabs-format `HUNK_DEBUG` payload, which (per
    /// a reference decode of the format, cross-checked against a real
    /// local gcc build) carries no tag in this position at all. Not
    /// decoded by this loader; see the module docs' "gcc is not covered"
    /// note.
    Unrecognized,
}

impl DebugMagic {
    /// Classifies a 4-byte magic slice (already bounds-checked by the
    /// caller). Unknown bytes -- including gcc's untagged stabs, which
    /// simply don't spell any of the known magics -- classify as
    /// [`DebugMagic::Unrecognized`] rather than erroring: this loader
    /// only ever *skips* what it doesn't understand here.
    fn classify(magic: &[u8]) -> DebugMagic {
        match magic {
            b"LINE" => DebugMagic::Line,
            b"OPTS" => DebugMagic::Opts,
            b"SRC6" => DebugMagic::Src6,
            b"HEAD" => DebugMagic::Head,
            _ => DebugMagic::Unrecognized,
        }
    }
}

/// One `(source line number, hunk-relative byte offset)` pair decoded
/// from a `LINE` debug block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineEntry {
    pub offset: u32,
    pub line: u32,
}

/// Source-line info for one hunk, decoded on demand by [`Hunk::line_info`]
/// from that hunk's captured `HUNK_DEBUG` blocks. See the module docs
/// and issue #74 for the on-disk format (verified against a real
/// assembler's output for `LawBreaker` and SAS/C 6.58's `DEBUG=LINE`;
/// **not** produced for `m68k-amigaos-gcc` builds -- see the module
/// docs' "gcc is not covered" note).
///
/// Flattens every recognized `LINE` block into one offset-sorted table,
/// since a hunk can carry more than one -- e.g. one per `#include`d
/// source file.
///
/// # Overlapping offsets
/// If two `LINE` blocks both record an entry at the exact same
/// hunk-relative offset, the one from the block that was captured
/// *later* (i.e. appears later in the hunk's `HUNK_DEBUG` sequence)
/// wins: the sort in [`Hunk::line_info`] is stable, so of two
/// equal-offset entries the later one sorts second, and
/// [`HunkLineInfo::lookup`]'s "greatest offset <= target" search returns
/// the last of any tied group. This is an arbitrary but deterministic
/// tie-break -- the format doesn't specify what overlapping blocks are
/// supposed to mean.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HunkLineInfo {
    /// `(source filename, entry)`, sorted ascending by `entry.offset`.
    entries: Vec<(String, LineEntry)>,
}

impl HunkLineInfo {
    /// Looks up the source location for a hunk-relative byte `offset`:
    /// the entry with the greatest recorded offset that is `<= offset`.
    /// The pairs are sparse (one per source line, not per instruction),
    /// so an exact match usually doesn't exist -- this is why the search
    /// can't be a plain equality lookup. Returns `None` if `offset`
    /// precedes every recorded entry, or if there's no line info at all.
    pub fn lookup(&self, offset: u32) -> Option<(&str, u32)> {
        let idx = self.entries.partition_point(|(_, e)| e.offset <= offset);
        if idx == 0 {
            return None;
        }
        let (filename, entry) = &self.entries[idx - 1];
        Some((filename.as_str(), entry.line))
    }

    /// True if no `LINE` blocks (recognized or otherwise) contributed
    /// any entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Reads a big-endian `u32` from `bytes` at `pos`, or `None` if that
/// would run past the end -- the bounds-checked primitive
/// [`parse_line_debug_block`] builds on, since a `HUNK_DEBUG` payload is
/// untrusted file data that must never be indexed unchecked.
fn read_u32_be(bytes: &[u8], pos: usize) -> Option<u32> {
    let end = pos.checked_add(4)?;
    let slice = bytes.get(pos..end)?;
    Some(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

/// Trims trailing NUL padding (the on-disk convention shared by `LINE`
/// filenames and `HUNK_SYMBOL` names alike) and decodes what's left as
/// UTF-8, lossily. Amiga strings aren't guaranteed valid UTF-8 (the
/// native charset isn't UTF-8), so this decodes rather than rejects the
/// whole entry over a handful of unusual bytes -- a garbled-but-present
/// name is still more useful than none, and lossy decoding can't turn a
/// *correct* entry into a *wrong* one (only a cosmetically imperfect
/// name), which is the property that matters for untrusted file input.
fn decode_nul_padded_lossy(raw: &[u8]) -> String {
    let trimmed = match raw.iter().rposition(|&b| b != 0) {
        Some(last) => &raw[..=last],
        None => &raw[..0],
    };
    String::from_utf8_lossy(trimmed).into_owned()
}

/// Decodes one `HUNK_DEBUG` block's raw payload (as captured into
/// [`Hunk::debug_blocks`]) if -- and only if -- [`DebugMagic::classify`]
/// says it's `LINE`; any other magic (recognized-but-uninterpreted, or
/// genuinely unrecognized -- including a payload too short to even carry
/// one) returns `None` and contributes nothing, per issue #74's
/// "dispatch on the magic, skip anything unrecognized". `LINE` payloads
/// that are merely *malformed* (truncated, an absurd filename length, a
/// dangling trailing pair) also degrade to `None` or a truncated entry
/// list rather than erroring -- see [`HunkLineInfo`]'s and
/// [`Hunk::line_info`]'s docs: this is untrusted file input, and
/// reporting nothing is always safer than reporting the wrong line.
///
/// Payload layout (big-endian throughout, offsets relative to the start
/// of the payload, i.e. right after the block's own length longword):
/// ```text
/// 0   u32   base offset within the hunk (added to every pair's offset)
/// 4   4     magic, e.g. "LINE"
/// 8   u32   source filename length, in LONGWORDS  (LINE blocks only)
/// 12  N*4   filename, NUL-padded to that longword count
/// ..  then (line number: u32, hunk offset: u32) pairs to the end
/// ```
fn parse_line_debug_block(payload: &[u8]) -> Option<(String, Vec<LineEntry>)> {
    let base_offset = read_u32_be(payload, 0)?;
    let magic = payload.get(4..8)?;
    if DebugMagic::classify(magic) != DebugMagic::Line {
        return None;
    }

    let name_longwords = read_u32_be(payload, 8)? as usize;
    let name_bytes = name_longwords.checked_mul(4)?;
    let name_start = 12usize;
    let name_end = name_start.checked_add(name_bytes)?;
    let raw_name = payload.get(name_start..name_end)?;
    let filename = decode_nul_padded_lossy(raw_name);

    let mut entries = Vec::new();
    let mut pos = name_end;
    while let Some(entry) = read_line_entry_pair(payload, pos, base_offset) {
        entries.push(entry);
        pos += 8;
    }

    Some((filename, entries))
}

/// Reads one `(line number, hunk offset)` pair at payload byte `pos`
/// (`line` at `pos`, `offset` at `pos + 4`), or `None` if either half
/// runs past the end of `payload` -- covers both a fully truncated pair
/// and a dangling partial one (a line number present with no offset to
/// follow it), which [`parse_line_debug_block`]'s loop both treat the
/// same way: stop, don't error, keep whatever full pairs were already
/// found. `offset` is `base_offset`-adjusted here (wrapping, so a
/// pathological base offset can't panic) since every caller wants the
/// final hunk-relative offset, never the raw on-disk value.
fn read_line_entry_pair(payload: &[u8], pos: usize, base_offset: u32) -> Option<LineEntry> {
    let line = read_u32_be(payload, pos)?;
    let entry_offset = read_u32_be(payload, pos.checked_add(4)?)?;
    Some(LineEntry {
        offset: base_offset.wrapping_add(entry_offset),
        line,
    })
}

/// Decoded `HUNK_SYMBOL` data for one hunk: `(name, hunk-relative
/// value)` pairs, sorted ascending by value. Built by
/// [`Hunk::symbol_table`] from that hunk's captured
/// [`Hunk::symbol_blocks`]; see that method's doc for why decoding is
/// lazy. Added in issue #74's follow-up, as a `symbol+offset` fallback
/// for binaries (mostly `m68k-amigaos-gcc` output) that carry no usable
/// `LINE` data -- see [`Hunk::locate`] for how the two combine.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SymbolTable {
    entries: Vec<(String, u32)>,
}

impl SymbolTable {
    /// Looks up the symbol whose value is the greatest one `<= offset`
    /// -- the same "greatest offset `<=` target" search as
    /// [`HunkLineInfo::lookup`], and for the same underlying reason: a
    /// `HUNK_SYMBOL` value marks where a routine *starts*, not every
    /// address inside it. Returns `(name, offset - symbol_value)` on a
    /// hit.
    ///
    /// `hunk_size` bounds the span attributed to the table's *last*
    /// symbol. Every earlier symbol's span is already implicitly bounded
    /// by the next symbol's value (the binary search below guarantees
    /// `offset` is `<` it whenever a later entry exists) -- but nothing
    /// bounds the final entry without `hunk_size`, and an otherwise-tiny
    /// symbol table would happily "match" an offset arbitrarily far past
    /// its last entry, e.g. a 3-symbol table matching 40 KB past the
    /// last one and reporting a delta that's not remotely useful (the
    /// concrete case that prompted this cap, from issue #74's
    /// coordinator follow-up). Passing the owning hunk's own
    /// `Hunk::reserved_size` here (as [`Hunk::locate`] does) makes the
    /// cap exactly "the rest of this hunk" rather than an arbitrary
    /// constant -- there's nothing past a hunk's own end to attribute to
    /// anything.
    ///
    /// Returns `None` when the match would fall at or past that bound,
    /// when `offset` precedes every symbol, or when there are no symbols
    /// at all.
    pub fn lookup(&self, offset: u32, hunk_size: u32) -> Option<(&str, u32)> {
        let idx = self.entries.partition_point(|(_, value)| *value <= offset);
        if idx == 0 {
            return None;
        }
        let (name, value) = &self.entries[idx - 1];
        let bound = self.entries.get(idx).map_or(hunk_size, |(_, v)| *v);
        if offset >= bound {
            return None;
        }
        Some((name.as_str(), offset - value))
    }

    /// True if no symbols were captured/decoded.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Reads one `HUNK_SYMBOL` block's raw bytes, given that the
/// `HUNK_SYMBOL` type word has already been consumed. Walks the
/// `{ name_length_longwords, name, value }` list (terminated by a zero
/// name length) purely to find where the block ends -- the same shape
/// [`skip_metadata_block`] used to walk for every metadata block type
/// before `HUNK_DEBUG`/`HUNK_SYMBOL` grew their own capturing paths --
/// then returns the exact bytes spanned (terminator included) verbatim.
/// Captured, not decoded: the same "read now during `parse`, interpret
/// later on demand" split [`read_debug_block_payload`] uses for
/// `HUNK_DEBUG`. [`parse_symbol_block`] (via [`Hunk::symbol_table`]) is
/// what actually turns this into `(name, value)` pairs.
fn read_symbol_block_raw(r: &mut Reader<'_>) -> Result<Vec<u8>, LoadError> {
    let start = r.pos;
    loop {
        let name_longwords = r.read_u32()?;
        if name_longwords == 0 {
            break;
        }
        r.skip_longwords(name_longwords as usize)?; // symbol name
        r.read_u32()?; // symbol value (hunk-relative offset)
    }
    Ok(r.bytes[start..r.pos].to_vec())
}

/// Decodes a raw `HUNK_SYMBOL` block's bytes (as captured by
/// [`read_symbol_block_raw`] into [`Hunk::symbol_blocks`]) into `(name,
/// value)` pairs: repeating `{ name_length_longwords: u32, name: N*4
/// bytes NUL-padded, value: u32 }` entries, terminated by a zero name
/// length.
///
/// Degrades to however many entries parsed cleanly before hitting
/// something malformed, rather than panicking -- consistent with
/// [`parse_line_debug_block`]'s philosophy, even though in practice a
/// block captured straight out of a successful [`parse`] call is always
/// well-formed (the capture walk in [`read_symbol_block_raw`] already
/// validated it): this function doesn't lean on that guarantee, since a
/// [`Hunk`] can also be constructed directly with arbitrary bytes in
/// `symbol_blocks` (tests do exactly this).
fn parse_symbol_block(bytes: &[u8]) -> Vec<(String, u32)> {
    let mut entries = Vec::new();
    let mut pos = 0usize;
    while let Some((entry, next_pos)) = read_symbol_entry(bytes, pos) {
        entries.push(entry);
        pos = next_pos;
    }
    entries
}

/// Reads one `{ name_length_longwords, name, value }` symbol entry at
/// payload byte `pos`, returning the decoded `(name, value)` pair and
/// the byte position right after it. Returns `None` both for a proper
/// terminator (a zero name length -- not malformed, just "no more
/// entries") and for anything that runs past the end of `bytes` --
/// [`parse_symbol_block`]'s loop treats both the same way: stop, don't
/// error, keep whatever entries were already decoded.
fn read_symbol_entry(bytes: &[u8], pos: usize) -> Option<((String, u32), usize)> {
    let name_longwords = read_u32_be(bytes, pos)?;
    if name_longwords == 0 {
        return None; // proper terminator
    }
    let name_byte_len = (name_longwords as usize).checked_mul(4)?;
    let name_start = pos.checked_add(4)?;
    let name_end = name_start.checked_add(name_byte_len)?;
    let raw_name = bytes.get(name_start..name_end)?;
    let value = read_u32_be(bytes, name_end)?;
    Some(((decode_nul_padded_lossy(raw_name), value), name_end + 4))
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

impl LoadResult {
    /// Translates a guest address into `(source filename, line number)`,
    /// using `file`'s per-hunk line info (see [`Hunk::line_info`]) and
    /// this result's per-hunk load addresses: finds which hunk `addr`
    /// falls inside, converts to a hunk-relative offset by subtracting
    /// that hunk's load address, and looks the offset up with
    /// [`HunkLineInfo::lookup`]'s greatest-offset-<=-target search.
    ///
    /// `file` must be the [`HunkFile`] this [`LoadResult`] was produced
    /// from by [`load`] -- `load` borrows rather than owns it, so the
    /// caller already has it on hand; mismatched hunk counts between
    /// `self` and `file` just make this return `None` rather than panic.
    /// Returns `None` if `addr` isn't inside any loaded hunk, or the
    /// owning hunk has no line info covering it (no debug data, or the
    /// address precedes every recorded entry).
    ///
    /// This decodes the owning hunk's debug blocks fresh on every call
    /// (see [`Hunk::line_info`]'s doc on why that's lazy rather than
    /// precomputed); a caller doing this repeatedly for the same binary
    /// should cache per-hunk [`HunkLineInfo`] itself instead of calling
    /// this in a hot loop.
    pub fn lookup_line(&self, file: &HunkFile, addr: u32) -> Option<(String, u32)> {
        for (hunk, &hunk_addr) in file.hunks.iter().zip(&self.hunk_addrs) {
            if addr < hunk_addr {
                continue;
            }
            let offset = addr - hunk_addr;
            if offset as usize >= hunk.reserved_size {
                continue;
            }
            let info = hunk.line_info();
            return info
                .lookup(offset)
                .map(|(filename, line)| (filename.to_string(), line));
        }
        None
    }

    /// Best available [`Location`] for a guest address: `file:line` when
    /// covered by `LINE` debug info, else `symbol+offset` when covered
    /// by `HUNK_SYMBOL` data, else `None` (the caller's existing "print
    /// the raw address" fallback covers that case). This is
    /// [`lookup_line`](Self::lookup_line) generalized with the
    /// `HUNK_SYMBOL` fallback added in issue #74's follow-up -- keep
    /// using `lookup_line` directly at a site that only ever wants
    /// `file:line` specifically (e.g. something that formats source
    /// listings); use this one wherever "best available attribution"
    /// is what's wanted, which is every diagnostic site.
    ///
    /// Finds which hunk `addr` falls inside the same way `lookup_line`
    /// does, then delegates the LINE-vs-symbol precedence to
    /// [`Hunk::locate`] -- see that method's doc. Same laziness caveat
    /// as `lookup_line`: this decodes the owning hunk's debug/symbol
    /// blocks fresh on every call.
    pub fn lookup_location(&self, file: &HunkFile, addr: u32) -> Option<Location> {
        for (hunk, &hunk_addr) in file.hunks.iter().zip(&self.hunk_addrs) {
            if addr < hunk_addr {
                continue;
            }
            let offset = addr - hunk_addr;
            if offset as usize >= hunk.reserved_size {
                continue;
            }
            return hunk.locate(offset);
        }
        None
    }
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

/// True for the block types that can appear as loose metadata wherever a
/// hunk boundary allows one -- immediately before a hunk's body, or (their
/// traditional position) after one's relocations, and in any number back
/// to back. See the module docs and [`skip_metadata_block`], which does
/// the actual parsing.
///
/// Neither `HUNK_DEBUG` nor `HUNK_SYMBOL` is included here even though
/// both can appear in the same positions: unlike `HUNK_NAME`, neither is
/// discarded -- both are captured (see [`read_debug_block_payload`] and
/// [`read_symbol_block_raw`], and the module docs' "Source-line info"
/// section), so [`parse_node`] checks for them explicitly, ahead of this
/// check, rather than folding them into the generic skip path.
fn is_metadata_block(block_type: u32) -> bool {
    block_type == HUNK_NAME
}

/// Skips a `HUNK_NAME` block's payload, given that its type word has
/// already been consumed: a longword count `n`, then `n * 4` bytes of
/// payload we don't interpret (the same on-disk shape `HUNK_DEBUG` uses
/// -- see [`read_debug_block_payload`], which shares that much but
/// returns the bytes instead of discarding them). Shared by the
/// leading-position skip (in [`parse_node`]'s per-hunk loop, before the
/// body-type match) and the trailing-position skip (in the same
/// function's post-body block loop), so the two positions can't drift
/// apart on what a `HUNK_NAME` block looks like.
fn skip_metadata_block(r: &mut Reader<'_>, block_type: u32) -> Result<(), LoadError> {
    match block_type {
        HUNK_NAME => {
            let n_longwords = r.read_u32()?;
            r.skip_longwords(n_longwords as usize)?;
        }
        other => unreachable!("skip_metadata_block called with non-metadata block type {other:#x}"),
    }
    Ok(())
}

/// Reads one `HUNK_DEBUG` block's raw payload and returns it verbatim,
/// given that the `HUNK_DEBUG` type word has already been consumed. Same
/// on-disk shape as `HUNK_NAME` (a longword count `n`, then `n * 4`
/// payload bytes -- see [`skip_metadata_block`]) and the same
/// `checked_mul`/`checked_add` bounds-checking precedent as
/// [`Reader::skip_longwords`], but the bytes are kept rather than
/// discarded: they're what [`Hunk::line_info`] decodes lazily later.
fn read_debug_block_payload(r: &mut Reader<'_>) -> Result<Vec<u8>, LoadError> {
    let n_longwords = r.read_u32()? as usize;
    let n_bytes = n_longwords.checked_mul(4).ok_or(LoadError::UnexpectedEof)?;
    r.read_bytes(n_bytes)
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
        // Skip (HUNK_NAME) or capture (HUNK_DEBUG/HUNK_SYMBOL) any number
        // of leading metadata blocks before the hunk's real body -- a
        // real assembler build with source-line debug info left in can
        // open a hunk with one or more HUNK_DEBUG blocks ahead of its
        // HUNK_CODE (see the module docs). Memory-flag bits never apply
        // to these metadata type words (only to CODE/DATA/BSS), so check
        // the raw word directly.
        let mut debug_blocks: Vec<Vec<u8>> = Vec::new();
        let mut symbol_blocks: Vec<Vec<u8>> = Vec::new();
        let raw_body_type = loop {
            let candidate = r.read_u32()?;
            if candidate == HUNK_DEBUG {
                debug_blocks.push(read_debug_block_payload(r)?);
                continue;
            }
            if candidate == HUNK_SYMBOL {
                symbol_blocks.push(read_symbol_block_raw(r)?);
                continue;
            }
            if is_metadata_block(candidate) {
                skip_metadata_block(r, candidate)?;
                continue;
            }
            break candidate;
        };
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
                HUNK_DEBUG => {
                    debug_blocks.push(read_debug_block_payload(r)?);
                }
                HUNK_SYMBOL => {
                    symbol_blocks.push(read_symbol_block_raw(r)?);
                }
                HUNK_NAME => {
                    skip_metadata_block(r, block_type)?;
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
            debug_blocks,
            symbol_blocks,
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

    /// Appends a `HUNK_DEBUG` block with `n_longwords` longwords of
    /// arbitrary payload.
    fn push_debug_block(buf: &mut Vec<u8>, n_longwords: u32) {
        push_u32(buf, HUNK_DEBUG);
        push_u32(buf, n_longwords);
        for i in 0..n_longwords {
            push_u32(buf, 0xD0D0_0000 | i);
        }
    }

    /// Appends a `HUNK_NAME` block with `n_longwords` longwords of
    /// arbitrary payload (same on-disk shape as `HUNK_DEBUG`).
    fn push_name_block(buf: &mut Vec<u8>, n_longwords: u32) {
        push_u32(buf, HUNK_NAME);
        push_u32(buf, n_longwords);
        for i in 0..n_longwords {
            push_u32(buf, 0x00A2_0000 | i);
        }
    }

    /// Appends a `HUNK_SYMBOL` block containing `names` as consecutive
    /// `{ name_length_longwords, name, value }` entries (values are
    /// synthesized as the entry's index), terminated by a zero
    /// `name_length`. `name` must be non-empty; it's padded with NUL
    /// bytes up to the next longword boundary the way a real assembler
    /// does.
    fn push_symbol_block(buf: &mut Vec<u8>, names: &[&str]) {
        push_u32(buf, HUNK_SYMBOL);
        for (i, name) in names.iter().enumerate() {
            let mut padded = name.as_bytes().to_vec();
            while !padded.len().is_multiple_of(4) {
                padded.push(0);
            }
            push_u32(buf, (padded.len() / 4) as u32);
            buf.extend_from_slice(&padded);
            push_u32(buf, i as u32); // symbol value
        }
        push_u32(buf, 0); // terminate symbol table
    }

    /// A `HUNK_DEBUG` block sitting *before* a hunk's `HUNK_CODE` body
    /// (the real-world shape found in `LawBreaker`, an ordinary assembler
    /// build with source-line debug info left in) must be skipped, not
    /// rejected as an unexpected hunk-body type -- see issue #70.
    #[test]
    fn skips_leading_debug_block_before_hunk_body() {
        let mut buf = Vec::new();
        push_u32(&mut buf, HUNK_HEADER);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 1);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 1); // hunk 0: 1 longword

        push_debug_block(&mut buf, 3);

        push_u32(&mut buf, HUNK_CODE);
        push_u32(&mut buf, 1);
        push_u32(&mut buf, 0x4E71_4E71);
        push_u32(&mut buf, HUNK_END);

        let file = parse(&buf).expect("leading HUNK_DEBUG should be skipped");
        assert_eq!(file.hunks.len(), 1);
        assert_eq!(file.hunks[0].data, 0x4E71_4E71u32.to_be_bytes());
    }

    /// Same as above but with a leading `HUNK_SYMBOL` block containing
    /// real named entries (not just the empty/terminator-only case),
    /// exercising the NUL-terminated-list parsing rather than a single
    /// count-prefixed block.
    #[test]
    fn skips_leading_symbol_block_with_named_entries_before_hunk_body() {
        let mut buf = Vec::new();
        push_u32(&mut buf, HUNK_HEADER);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 1);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 1);

        push_symbol_block(&mut buf, &["_main", "someLabel"]);

        push_u32(&mut buf, HUNK_CODE);
        push_u32(&mut buf, 1);
        push_u32(&mut buf, 0x4E71_4E71);
        push_u32(&mut buf, HUNK_END);

        let file = parse(&buf).expect("leading HUNK_SYMBOL should be skipped");
        assert_eq!(file.hunks.len(), 1);
        assert_eq!(file.hunks[0].data, 0x4E71_4E71u32.to_be_bytes());
    }

    /// Same as above but with a leading `HUNK_NAME` block.
    #[test]
    fn skips_leading_name_block_before_hunk_body() {
        let mut buf = Vec::new();
        push_u32(&mut buf, HUNK_HEADER);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 1);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 1);

        push_name_block(&mut buf, 2);

        push_u32(&mut buf, HUNK_CODE);
        push_u32(&mut buf, 1);
        push_u32(&mut buf, 0x4E71_4E71);
        push_u32(&mut buf, HUNK_END);

        let file = parse(&buf).expect("leading HUNK_NAME should be skipped");
        assert_eq!(file.hunks.len(), 1);
        assert_eq!(file.hunks[0].data, 0x4E71_4E71u32.to_be_bytes());
    }

    /// Several leading metadata blocks back to back (of all three kinds,
    /// in a mixed order) must all be skipped before the real body is
    /// found -- a single `if` (rather than a loop) would only handle one.
    #[test]
    fn skips_multiple_leading_metadata_blocks_in_a_row() {
        let mut buf = Vec::new();
        push_u32(&mut buf, HUNK_HEADER);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 1);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 1);

        push_debug_block(&mut buf, 1);
        push_name_block(&mut buf, 1);
        push_symbol_block(&mut buf, &["foo", "bar"]);
        push_debug_block(&mut buf, 2);

        push_u32(&mut buf, HUNK_CODE);
        push_u32(&mut buf, 1);
        push_u32(&mut buf, 0x4E71_4E71);
        push_u32(&mut buf, HUNK_END);

        let file =
            parse(&buf).expect("multiple leading metadata blocks in a row should be skipped");
        assert_eq!(file.hunks.len(), 1);
        assert_eq!(file.hunks[0].data, 0x4E71_4E71u32.to_be_bytes());
    }

    /// `HUNK_NAME` in the trailing position (after a body, alongside its
    /// long-standing `HUNK_SYMBOL`/`HUNK_DEBUG` siblings) must also be
    /// skipped -- it wasn't recognized there at all before this fix.
    #[test]
    fn skips_trailing_name_block_after_hunk_body() {
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

        push_name_block(&mut buf, 2);

        push_u32(&mut buf, HUNK_END);

        let file = parse(&buf).expect("trailing HUNK_NAME should be skipped");
        assert_eq!(file.hunks.len(), 1);
        assert_eq!(file.hunks[0].data, 0x4E71_4E71u32.to_be_bytes());
    }

    /// Several trailing metadata blocks in a row, interleaved with a real
    /// `HUNK_RELOC32` block, must all be skipped -- matching the leading
    /// case's "more than one in a row" coverage but after both a body and
    /// its relocations.
    #[test]
    fn skips_multiple_trailing_metadata_blocks_after_relocations() {
        let mut buf = Vec::new();
        push_u32(&mut buf, HUNK_HEADER);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 2);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 1);
        push_u32(&mut buf, 1); // hunk 0 size: 1 longword
        push_u32(&mut buf, 1); // hunk 1 size: 1 longword

        push_u32(&mut buf, HUNK_CODE);
        push_u32(&mut buf, 1);
        push_u32(&mut buf, 0); // addend placeholder
        push_u32(&mut buf, HUNK_RELOC32);
        push_u32(&mut buf, 1);
        push_u32(&mut buf, 1); // target hunk 1
        push_u32(&mut buf, 0); // offset 0
        push_u32(&mut buf, 0); // terminate reloc groups

        push_debug_block(&mut buf, 1);
        push_symbol_block(&mut buf, &["one", "two"]);
        push_name_block(&mut buf, 1);

        push_u32(&mut buf, HUNK_END);

        push_u32(&mut buf, HUNK_DATA);
        push_u32(&mut buf, 1);
        push_u32(&mut buf, 0xCAFE_BABE);
        push_u32(&mut buf, HUNK_END);

        let file = parse(&buf).expect("trailing metadata blocks after relocations should skip");
        assert_eq!(file.hunks.len(), 2);
        assert_eq!(file.hunks[0].relocs.len(), 1);
        assert_eq!(file.hunks[0].relocs[0].target_hunk, 1);
        assert_eq!(file.hunks[1].data, 0xCAFE_BABEu32.to_be_bytes());
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

    // --- HUNK_DEBUG / LINE source-line info (issue #74) ---

    /// A `Hunk` with the given raw `HUNK_DEBUG` payloads and nothing
    /// else -- for tests that exercise [`Hunk::line_info`] directly
    /// without going through a full file parse.
    fn make_hunk(debug_blocks: Vec<Vec<u8>>) -> Hunk {
        make_hunk_full(debug_blocks, Vec::new(), 0x1000)
    }

    /// Same as `make_hunk` but with `HUNK_SYMBOL` blocks too, for tests
    /// that exercise [`Hunk::symbol_table`]/[`Hunk::locate`].
    fn make_hunk_with_symbols(symbol_blocks: Vec<Vec<u8>>) -> Hunk {
        make_hunk_full(Vec::new(), symbol_blocks, 0x1000)
    }

    /// Fully-parameterized `Hunk` builder for tests, including
    /// `reserved_size` -- needed by tests that exercise
    /// [`SymbolTable::lookup`]'s hunk-end distance cap, which only bites
    /// when the hunk isn't the default helper's generous 0x1000 bytes.
    fn make_hunk_full(
        debug_blocks: Vec<Vec<u8>>,
        symbol_blocks: Vec<Vec<u8>>,
        reserved_size: usize,
    ) -> Hunk {
        Hunk {
            kind: HunkKind::Code,
            data: Vec::new(),
            reserved_size,
            relocs: Vec::new(),
            debug_blocks,
            symbol_blocks,
        }
    }

    /// Builds a `LINE`-format `HUNK_DEBUG` *payload* (the bytes that end
    /// up in `Hunk::debug_blocks`, i.e. after the block's own type and
    /// length longwords): base offset, `"LINE"` magic, longword-counted
    /// NUL-padded filename, then `(line, offset)` pairs -- the layout
    /// documented in issue #74 and verified against LawBreaker/SAS/C.
    fn build_line_payload(base_offset: u32, filename: &str, pairs: &[(u32, u32)]) -> Vec<u8> {
        let mut buf = Vec::new();
        push_u32(&mut buf, base_offset);
        buf.extend_from_slice(b"LINE");
        let mut name_bytes = filename.as_bytes().to_vec();
        while !name_bytes.len().is_multiple_of(4) {
            name_bytes.push(0);
        }
        push_u32(&mut buf, (name_bytes.len() / 4) as u32);
        buf.extend_from_slice(&name_bytes);
        for &(line, offset) in pairs {
            push_u32(&mut buf, line);
            push_u32(&mut buf, offset);
        }
        buf
    }

    /// Appends a full on-disk `HUNK_DEBUG` block (type longword + length
    /// longword + payload) to `buf`, for tests that exercise the full
    /// [`parse`] path rather than constructing a [`Hunk`] directly.
    fn push_debug_block_raw(buf: &mut Vec<u8>, payload: &[u8]) {
        assert_eq!(
            payload.len() % 4,
            0,
            "test helper requires a longword-aligned debug payload"
        );
        push_u32(buf, HUNK_DEBUG);
        push_u32(buf, (payload.len() / 4) as u32);
        buf.extend_from_slice(payload);
    }

    #[test]
    fn line_debug_block_parses_into_line_info() {
        // The LawBreaker pairs from issue #74, verified against real
        // instruction boundaries in that binary.
        let payload = build_line_payload(
            0,
            "LawBreaker.asm",
            &[
                (133, 0x0000),
                (134, 0x0004),
                (135, 0x0006),
                (136, 0x000a),
                (137, 0x000e),
                (141, 0x0010),
            ],
        );
        let hunk = make_hunk(vec![payload]);
        let info = hunk.line_info();
        assert!(!info.is_empty());
        assert_eq!(info.lookup(0x0000), Some(("LawBreaker.asm", 133)));
        assert_eq!(info.lookup(0x0004), Some(("LawBreaker.asm", 134)));
        assert_eq!(info.lookup(0x000a), Some(("LawBreaker.asm", 136)));
        assert_eq!(info.lookup(0x0010), Some(("LawBreaker.asm", 141)));
    }

    /// The pairs are sparse (one per source line, not per instruction),
    /// so a lookup for an offset strictly between two recorded offsets
    /// must return the *preceding* entry, not `None` and not the next
    /// one -- an exact-match lookup would report nothing for most
    /// addresses, which is the whole reason this is a "greatest offset
    /// <= target" search.
    #[test]
    fn lookup_returns_preceding_entry_for_offset_between_pairs() {
        let payload = build_line_payload(0, "x.asm", &[(10, 0x00), (20, 0x10), (30, 0x20)]);
        let info = make_hunk(vec![payload]).line_info();

        assert_eq!(info.lookup(0x08), Some(("x.asm", 10)));
        assert_eq!(info.lookup(0x0f), Some(("x.asm", 10)));
        assert_eq!(info.lookup(0x1f), Some(("x.asm", 20)));
    }

    #[test]
    fn lookup_before_first_entry_is_none() {
        let payload = build_line_payload(0, "x.asm", &[(10, 0x10), (20, 0x20)]);
        let info = make_hunk(vec![payload]).line_info();

        assert_eq!(info.lookup(0x00), None);
        assert_eq!(info.lookup(0x0f), None);
    }

    #[test]
    fn lookup_past_last_entry_returns_the_last_entry() {
        let payload = build_line_payload(0, "x.asm", &[(10, 0x00), (20, 0x10)]);
        let info = make_hunk(vec![payload]).line_info();

        assert_eq!(info.lookup(0x10), Some(("x.asm", 20)));
        assert_eq!(info.lookup(0xFFFF_FFFF), Some(("x.asm", 20)));
    }

    #[test]
    fn empty_line_info_has_no_entries_and_no_lookups() {
        let info = make_hunk(Vec::new()).line_info();
        assert!(info.is_empty());
        assert_eq!(info.lookup(0), None);
    }

    /// `OPTS`/`SRC6` (SAS/C 6.58) and a `HEAD`-magic block (the leading
    /// four bytes of a `HEADDBGV01` directory) must be recognized as
    /// *not* `LINE` and contribute nothing, without disturbing the
    /// parse of a real `LINE` block alongside them -- both the leading
    /// and trailing `HUNK_DEBUG` positions are exercised.
    #[test]
    fn skips_unrecognized_debug_magics_without_disturbing_line_info() {
        let mut opts_payload = Vec::new();
        push_u32(&mut opts_payload, 0);
        opts_payload.extend_from_slice(b"OPTS");
        push_u32(&mut opts_payload, 0xAAAA_AAAA);

        let mut src6_payload = Vec::new();
        push_u32(&mut src6_payload, 0);
        src6_payload.extend_from_slice(b"SRC6");

        let mut head_payload = Vec::new();
        push_u32(&mut head_payload, 0);
        head_payload.extend_from_slice(b"HEAD");
        push_u32(&mut head_payload, 0xBBBB_BBBB);

        let line_payload = build_line_payload(0, "hello.c", &[(2, 0x0000), (3, 0x000c)]);

        let mut buf = Vec::new();
        push_u32(&mut buf, HUNK_HEADER);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 1);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 1); // hunk 0: 1 longword

        // Leading position: OPTS then HEAD.
        push_debug_block_raw(&mut buf, &opts_payload);
        push_debug_block_raw(&mut buf, &head_payload);

        push_u32(&mut buf, HUNK_CODE);
        push_u32(&mut buf, 1);
        push_u32(&mut buf, 0x4E71_4E71);

        // Trailing position: SRC6 then the real LINE block.
        push_debug_block_raw(&mut buf, &src6_payload);
        push_debug_block_raw(&mut buf, &line_payload);

        push_u32(&mut buf, HUNK_END);

        let file = parse(&buf).expect("OPTS/SRC6/HEAD debug blocks should be skipped, not error");
        assert_eq!(file.hunks.len(), 1);
        assert_eq!(file.hunks[0].debug_blocks.len(), 4, "all four are captured");

        let info = file.hunks[0].line_info();
        assert_eq!(info.lookup(0x0000), Some(("hello.c", 2)));
        assert_eq!(info.lookup(0x000c), Some(("hello.c", 3)));
    }

    /// A hunk can carry more than one `LINE` block for different source
    /// files (e.g. one per `#include`d header); [`Hunk::line_info`]
    /// merges them into a single lookup table.
    #[test]
    fn merges_two_line_blocks_with_different_filenames() {
        let main_c = build_line_payload(0, "main.c", &[(5, 0x00), (6, 0x08)]);
        let included_h = build_line_payload(0, "included.h", &[(1, 0x04), (2, 0x0c)]);
        let info = make_hunk(vec![main_c, included_h]).line_info();

        assert_eq!(info.lookup(0x00), Some(("main.c", 5)));
        assert_eq!(info.lookup(0x04), Some(("included.h", 1)));
        assert_eq!(info.lookup(0x08), Some(("main.c", 6)));
        assert_eq!(info.lookup(0x0c), Some(("included.h", 2)));
    }

    /// When two blocks record an entry at the *exact* same offset, the
    /// later-encountered block wins (see [`HunkLineInfo`]'s doc on the
    /// tie-break) -- not a panic, not an arbitrary pick.
    #[test]
    fn overlapping_offset_across_two_blocks_prefers_the_later_block() {
        let first = build_line_payload(0, "old.c", &[(1, 0x00)]);
        let second = build_line_payload(0, "new.c", &[(99, 0x00)]);
        let info = make_hunk(vec![first, second]).line_info();

        assert_eq!(info.lookup(0x00), Some(("new.c", 99)));
    }

    // --- Real-world non-monotonic LINE pairs (PhxAss LINEDEBUG) ---

    /// A real PhxAss `LINEDEBUG` build (`fixtures/linetest`, see that
    /// test below) emits a `LINE` block whose `(line, offset)` pairs are
    /// **not** offset-monotonic and **not** in line-number order either
    /// -- e.g. `(33, 0x0e), (36, 0x00), (37, 0x15)` verbatim, in that
    /// file order. This is a synthetic reproduction of exactly that
    /// shape, so the "always re-sort, never assume file order" property
    /// documented on [`Hunk::line_info`] has a regression test that
    /// doesn't depend on the fixture file: if sorting were ever dropped
    /// (or replaced with an "already sorted, trust it" fast path), this
    /// fails with a *wrong* line number, not a missing one -- lookups at
    /// offsets between out-of-order entries would silently pick up
    /// whichever pair happened to be read last rather than the one with
    /// the truly greatest offset `<=` target.
    #[test]
    fn non_monotonic_pairs_are_sorted_before_lookup() {
        let payload = build_line_payload(0, "x.asm", &[(33, 0x0e), (36, 0x00), (37, 0x15)]);
        let info = make_hunk(vec![payload]).line_info();

        // After sorting by offset: (0x00, 36), (0x0e, 33), (0x15, 37).
        assert_eq!(info.lookup(0x00), Some(("x.asm", 36)));
        assert_eq!(info.lookup(0x0e), Some(("x.asm", 33)));
        assert_eq!(info.lookup(0x15), Some(("x.asm", 37)));
        // An offset that several out-of-order-by-line-number entries
        // could plausibly "claim" (it's numerically between the 0x0e
        // and 0x15 entries) resolves purely by offset, per the
        // documented rule -- not by line number, and not by which
        // pair appeared first/last in the file. The answer is "line
        // 33's code is still the most recent thing before this
        // address", even though line 33 is numerically lower than the
        // line (36) whose offset precedes it.
        assert_eq!(info.lookup(0x10), Some(("x.asm", 33)));
    }

    /// End-to-end, real-artifact confirmation of the same property,
    /// against a real PhxAss-built binary committed to the repo (not a
    /// scratch file or an unvendorable third-party fixture -- see
    /// [`warn_real_artifact_missing`]'s doc on why the LawBreaker/SAS-C
    /// tests below are corroboration only, and this one is the durable
    /// coverage). `fixtures/linetest.s` is a trivial two-section (code +
    /// data) program assembled with PhxAss's `LINEDEBUG` option, which
    /// emits one `LINE` block per section/hunk.
    ///
    /// The data hunk's real pairs, verified against a raw hex dump of
    /// the file (not just this parser's own output): `(33, 0x0e), (36,
    /// 0x00), (37, 0x15)`. Per the assembler's own author (not
    /// established further, and not something this loader needs to
    /// understand): line 33 is the `section data,data` directive itself,
    /// and PhxAss records its offset as `0x0e` -- inside the section's
    /// message string, not at a boundary -- rather than `0x00`, which
    /// line 36 gets instead. The code hunk's pairs are ordinary
    /// (offset-monotonic); the data hunk's are the real counter-example
    /// this loader must not assume away.
    #[test]
    fn real_linetest_fixture_matches_known_pairs() {
        const LINETEST: &[u8] = include_bytes!("../../../fixtures/linetest");

        let file = parse(LINETEST).expect("fixtures/linetest should be a well-formed hunk file");
        assert_eq!(file.hunks.len(), 2, "one code hunk, one data hunk");

        let code_info = file.hunks[0].line_info();
        assert_eq!(code_info.lookup(0x00), Some(("work:linetest.s", 20)));
        assert_eq!(code_info.lookup(0x06), Some(("work:linetest.s", 25)));
        assert_eq!(code_info.lookup(0x0a), Some(("work:linetest.s", 30)));
        assert_eq!(code_info.lookup(0x0c), Some(("work:linetest.s", 31)));

        let data_info = file.hunks[1].line_info();
        // Real, non-monotonic pairs as emitted (see this test's doc):
        // (33, 0x0e), (36, 0x00), (37, 0x15) -- sorted by offset that's
        // (0x00, 36), (0x0e, 33), (0x15, 37).
        assert_eq!(data_info.lookup(0x00), Some(("work:linetest.s", 36)));
        assert_eq!(data_info.lookup(0x0e), Some(("work:linetest.s", 33)));
        assert_eq!(data_info.lookup(0x15), Some(("work:linetest.s", 37)));
        // Between the line-33 and line-37 entries: still resolves by
        // offset alone, landing on line 33 despite line 36's entry
        // having a *lower* offset than line 33's -- exactly the
        // property `non_monotonic_pairs_are_sorted_before_lookup`
        // isolates synthetically.
        assert_eq!(data_info.lookup(0x10), Some(("work:linetest.s", 33)));
    }

    // --- HUNK_SYMBOL / symbol+offset attribution (issue #74 follow-up) ---

    /// Builds a `HUNK_SYMBOL` block's raw bytes (the on-disk shape --
    /// see [`read_symbol_block_raw`]/[`parse_symbol_block`]): repeating
    /// `{ name_length_longwords, name (NUL-padded), value }` entries,
    /// terminated by a zero name length.
    fn build_symbol_block_payload(entries: &[(&str, u32)]) -> Vec<u8> {
        let mut buf = Vec::new();
        for &(name, value) in entries {
            let mut name_bytes = name.as_bytes().to_vec();
            while !name_bytes.len().is_multiple_of(4) {
                name_bytes.push(0);
            }
            push_u32(&mut buf, (name_bytes.len() / 4) as u32);
            buf.extend_from_slice(&name_bytes);
            push_u32(&mut buf, value);
        }
        push_u32(&mut buf, 0); // terminator
        buf
    }

    #[test]
    fn symbol_lookup_returns_preceding_symbol_for_offset_between_two() {
        let payload = build_symbol_block_payload(&[("foo", 0x00), ("bar", 0x20)]);
        let table = make_hunk_with_symbols(vec![payload]).symbol_table();

        assert_eq!(table.lookup(0x10, 0x100), Some(("foo", 0x10)));
        assert_eq!(table.lookup(0x1f, 0x100), Some(("foo", 0x1f)));
        assert_eq!(table.lookup(0x20, 0x100), Some(("bar", 0x00)));
    }

    #[test]
    fn symbol_lookup_before_first_symbol_is_none() {
        let payload = build_symbol_block_payload(&[("foo", 0x10)]);
        let table = make_hunk_with_symbols(vec![payload]).symbol_table();

        assert_eq!(table.lookup(0x00, 0x100), None);
        assert_eq!(table.lookup(0x0f, 0x100), None);
    }

    /// A small symbol table must not "match" an offset arbitrarily far
    /// past its last entry -- the concrete failure mode this guards
    /// against (`Done+0x9c40`, from a three-symbol table matching 40 KB
    /// past its last entry) is exactly what prompted issue #74's
    /// coordinator follow-up. The bound is the owning hunk's own size:
    /// an offset within it but far past the last symbol reports
    /// nothing; one right at the boundary or beyond does too (there's
    /// nothing past a hunk's end to attribute to anything).
    #[test]
    fn symbol_lookup_caps_distance_at_the_hunk_end() {
        let payload = build_symbol_block_payload(&[("start", 0x00)]);
        let table = make_hunk_with_symbols(vec![payload]).symbol_table();

        // hunk_size = 0x20: offset 0x1f is the last in-bounds byte.
        assert_eq!(table.lookup(0x1f, 0x20), Some(("start", 0x1f)));
        assert_eq!(table.lookup(0x20, 0x20), None, "at the hunk's own end");
        assert_eq!(
            table.lookup(0x9c40, 0x20),
            None,
            "40 KB past the last symbol must not report Done+0x9c40-style nonsense"
        );
    }

    /// Symbol names aren't guaranteed valid UTF-8 (same caveat as `LINE`
    /// filenames); a non-UTF-8 name must decode lossily rather than
    /// drop the entry or panic.
    #[test]
    fn symbol_name_non_utf8_decodes_lossily_without_panicking() {
        let mut payload = Vec::new();
        push_u32(&mut payload, 1); // name: 1 longword (4 bytes)
        payload.extend_from_slice(&[0xFF, 0xFE, b'z', 0]); // invalid UTF-8 + 'z' + NUL pad
        push_u32(&mut payload, 0x40);
        push_u32(&mut payload, 0); // terminator

        let table = make_hunk_with_symbols(vec![payload]).symbol_table();
        let (name, delta) = table
            .lookup(0x40, 0x1000)
            .expect("entry should be recovered");
        assert_eq!(delta, 0);
        assert!(
            name.contains('z'),
            "the one valid byte should survive lossy decoding, got {name:?}"
        );
    }

    /// [`Hunk::locate`]'s precedence: `file:line` wins over
    /// `symbol+offset` whenever `LINE` covers the offset, even though a
    /// symbol covers the same offset too.
    #[test]
    fn locate_prefers_line_over_symbol_when_both_cover_the_offset() {
        let line_payload = build_line_payload(0, "main.c", &[(10, 0x00), (11, 0x10)]);
        let symbol_payload = build_symbol_block_payload(&[("_main", 0x00)]);
        let hunk = make_hunk_full(vec![line_payload], vec![symbol_payload], 0x100);

        // Offset 0x08 is covered by both the LINE entry at 0x00 and the
        // _main symbol at 0x00 -- LINE must win.
        assert_eq!(
            hunk.locate(0x08),
            Some(Location::Line {
                file: "main.c".to_string(),
                line: 10,
            })
        );
    }

    /// With no `LINE` coverage at all, [`Hunk::locate`] falls back to
    /// the symbol table.
    #[test]
    fn locate_falls_back_to_symbol_when_there_is_no_line_info() {
        let symbol_payload = build_symbol_block_payload(&[("_main", 0x00), ("_helper", 0x20)]);
        let hunk = make_hunk_full(Vec::new(), vec![symbol_payload], 0x100);

        assert_eq!(
            hunk.locate(0x05),
            Some(Location::Symbol {
                name: "_main".to_string(),
                offset: 5,
            })
        );
        assert_eq!(
            hunk.locate(0x00),
            Some(Location::Symbol {
                name: "_main".to_string(),
                offset: 0,
            }),
            "exactly at the first symbol's own value"
        );
    }

    /// End-to-end evidence against a real `m68k-amigaos-gcc -g -O0
    /// -noixemul` executable: it carries real, per-hunk `HUNK_SYMBOL`
    /// data (47/15/19 symbols across its code/data/bss hunks) but no
    /// `LINE` data this loader attaches to any hunk -- its one
    /// `HUNK_DEBUG` block (the untagged stabs blob covered by
    /// `real_gcc_untagged_stabs_block_is_skipped_cleanly`) sits in the
    /// file *after* the last hunk the header declares, so it's outside
    /// any hunk's boundary and this loader (matching real `LoadSeg`,
    /// which also never reads past the declared hunk range) never
    /// captures it into any `Hunk::debug_blocks` at all. So `_free` gets
    /// no `file:line`, but does get `_free+0x5` via the symbol fallback
    /// -- exactly the case this whole follow-up exists for. Skips
    /// loudly if the fixture isn't present (see
    /// [`warn_real_artifact_missing`]); corroboration only, same
    /// reasoning as the other two real-artifact tests.
    #[test]
    fn real_gcc_binary_falls_back_to_symbols_with_no_line_info() {
        let path = "/private/tmp/claude-501/-Users-simond-src-volamos/25505440-09a1-4588-b085-ae3c886e6132/scratchpad/gccd/t";
        let Ok(bytes) = std::fs::read(path) else {
            warn_real_artifact_missing(
                "real_gcc_binary_falls_back_to_symbols_with_no_line_info",
                path,
            );
            return;
        };

        let file = parse(&bytes).expect("real gcc binary should parse");
        assert_eq!(file.hunks.len(), 3, "code, data, bss");
        let code = &file.hunks[0];
        assert!(
            code.line_info().is_empty(),
            "no LINE data is attached to any hunk in this binary"
        );
        assert!(!code.symbol_table().is_empty());

        // _free is at 0x226e in the real binary, with no closer symbol
        // until 0x2318 -- 0x2273 (0x226e + 5) should resolve to
        // _free+0x5 via the symbol fallback.
        match code.locate(0x2273) {
            Some(Location::Symbol { name, offset }) => {
                assert_eq!(name, "_free");
                assert_eq!(offset, 5);
            }
            other => panic!("expected a symbol fallback for _free+5, got {other:?}"),
        }
    }

    // --- Malformed HUNK_DEBUG payloads: must degrade to "no info", never panic ---

    #[test]
    fn malformed_too_short_for_a_magic_yields_no_entries() {
        // Only 6 bytes: not even a full base-offset-plus-magic header.
        let payload = vec![0, 0, 0, 0, b'L', b'I'];
        let info = make_hunk(vec![payload]).line_info();
        assert!(info.is_empty());
    }

    #[test]
    fn malformed_absurd_filename_length_yields_no_entries() {
        let mut payload = Vec::new();
        push_u32(&mut payload, 0);
        payload.extend_from_slice(b"LINE");
        push_u32(&mut payload, u32::MAX); // absurd: claims ~16GB of filename
        let info = make_hunk(vec![payload]).line_info();
        assert!(info.is_empty());
    }

    #[test]
    fn malformed_filename_length_overruns_block_yields_no_entries() {
        let mut payload = Vec::new();
        push_u32(&mut payload, 0);
        payload.extend_from_slice(b"LINE");
        push_u32(&mut payload, 100); // claims 400 bytes; payload has none
        let info = make_hunk(vec![payload]).line_info();
        assert!(info.is_empty());
    }

    /// A non-UTF-8 filename must decode (lossily) rather than drop the
    /// whole block or panic -- the entries themselves are still valid
    /// and must still be reachable.
    #[test]
    fn malformed_non_utf8_filename_decodes_lossily_without_panicking() {
        let mut payload = Vec::new();
        push_u32(&mut payload, 0);
        payload.extend_from_slice(b"LINE");
        push_u32(&mut payload, 1); // filename: 1 longword (4 bytes)
        payload.extend_from_slice(&[0xFF, 0xFE, b'a', 0]); // invalid UTF-8 + 'a' + NUL pad
        push_u32(&mut payload, 42);
        push_u32(&mut payload, 0x10);

        let info = make_hunk(vec![payload]).line_info();
        let (filename, line) = info.lookup(0x10).expect("entry should still be recovered");
        assert_eq!(line, 42);
        assert!(
            filename.contains('a'),
            "the one valid byte should survive lossy decoding, got {filename:?}"
        );
    }

    /// A dangling partial pair (a line number with no following offset)
    /// at the end of a block must be dropped silently, not turned into
    /// a bogus entry and not treated as an error for the whole block.
    #[test]
    fn malformed_dangling_partial_pair_is_ignored() {
        let mut payload = build_line_payload(0, "x.asm", &[(10, 0x00)]);
        push_u32(&mut payload, 99); // a line number with no offset to follow it
        let hunk = make_hunk(vec![payload]);
        let info = hunk.line_info();

        assert_eq!(info.lookup(0x00), Some(("x.asm", 10)));
        assert_eq!(
            info.entries.len(),
            1,
            "the dangling partial pair must not have contributed an entry"
        );
    }

    /// The exact untagged bytes reported from a real local
    /// `m68k-amigaos-gcc -g -O0 -noixemul` build's single `HUNK_DEBUG`
    /// block (stabs-format, no ASCII magic in the position `LINE`/
    /// `OPTS`/`SRC6`/`HEAD` use -- see the module docs' "gcc is not
    /// covered" note). Must classify as unrecognized and contribute no
    /// entries, without panicking or being mistaken for `LINE`.
    #[test]
    fn real_gcc_untagged_stabs_block_is_skipped_cleanly() {
        let payload: Vec<u8> = vec![
            0x00, 0x00, 0x00, 0x4c, 0x00, 0x00, 0x00, 0x10, 0xff, 0xff, 0xff, 0xff, 0x03, 0x00,
            0x01, 0x7e,
        ];
        let info = make_hunk(vec![payload]).line_info();
        assert!(info.is_empty());
    }

    /// The same real gcc payload, but exercised through a full leading-
    /// position [`parse`] (rather than a direct [`Hunk::line_info`]
    /// call), confirming it doesn't derail parsing of the hunk it
    /// precedes -- matching the untagged-stabs shape found in a real
    /// `m68k-amigaos-gcc` build.
    #[test]
    fn real_gcc_untagged_stabs_block_does_not_disturb_parse() {
        let gcc_payload: Vec<u8> = vec![
            0x00, 0x00, 0x00, 0x4c, 0x00, 0x00, 0x00, 0x10, 0xff, 0xff, 0xff, 0xff, 0x03, 0x00,
            0x01, 0x7e,
        ];

        let mut buf = Vec::new();
        push_u32(&mut buf, HUNK_HEADER);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 1);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 1);

        push_debug_block_raw(&mut buf, &gcc_payload);

        push_u32(&mut buf, HUNK_CODE);
        push_u32(&mut buf, 1);
        push_u32(&mut buf, 0x4E71_4E71);
        push_u32(&mut buf, HUNK_END);

        let file = parse(&buf).expect("the untagged gcc debug block should be skipped, not error");
        assert_eq!(file.hunks.len(), 1);
        assert_eq!(file.hunks[0].data, 0x4E71_4E71u32.to_be_bytes());
        assert!(file.hunks[0].line_info().is_empty());
    }

    #[test]
    fn lookup_line_translates_a_loaded_address_via_the_owning_hunks_load_offset() {
        let line_payload = build_line_payload(0, "prog.asm", &[(1, 0x00), (2, 0x04)]);
        let mut buf = Vec::new();
        push_u32(&mut buf, HUNK_HEADER);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 1);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 2); // hunk 0: 2 longwords

        push_u32(&mut buf, HUNK_CODE);
        push_u32(&mut buf, 2);
        push_u32(&mut buf, 0x4E71_4E71);
        push_u32(&mut buf, 0x4E71_4E71);
        push_debug_block_raw(&mut buf, &line_payload);
        push_u32(&mut buf, HUNK_END);

        let file = parse(&buf).unwrap();
        let mut mem = FlatMemory::new(0x1000);
        let result = load(&file, &mut mem, 0x400).unwrap();

        assert_eq!(
            result.lookup_line(&file, 0x404),
            Some(("prog.asm".to_string(), 2))
        );
        assert_eq!(
            result.lookup_line(&file, 0x406),
            Some(("prog.asm".to_string(), 2)),
            "sparse lookup: still the line-2 entry, the greatest offset <= target"
        );
        // Before the hunk's load address entirely.
        assert_eq!(result.lookup_line(&file, 0x100), None);
        // Past the end of the hunk's reserved size.
        assert_eq!(result.lookup_line(&file, 0x500), None);
    }

    /// Prints a hard-to-miss banner (not a quiet one-liner) when a
    /// real-artifact test can't find its fixture and is about to return
    /// early without running its assertions. `cargo test`'s default
    /// output capture swallows `eprintln!` on a *passing* test, so this
    /// can't force visibility on every run -- but under `--nocapture`,
    /// or if the artifact vanishing ever coincides with an unrelated
    /// failure that flips capture on, this makes it unmistakable that
    /// "test passed" here means "test didn't run", not "assertions
    /// held". These two tests are corroboration for
    /// `fixtures/linetest`'s committed, always-present coverage (see
    /// [`real_linetest_fixture_matches_known_pairs`]) -- not something
    /// this loader's correctness depends on, since neither artifact can
    /// be vendored into the repo (LawBreaker ships under Enforcer's
    /// non-commercial/no-modification terms; the SAS/C object lives in a
    /// session-scoped scratch directory).
    fn warn_real_artifact_missing(test_name: &str, path: &str) {
        eprintln!(
            "\n\
             ============================================================\n\
             SKIPPED (not a failure, but NOT a pass either): {test_name}\n\
             Real-artifact fixture not found at: {path}\n\
             This test's assertions did NOT run. It is local corroboration\n\
             only -- fixtures/linetest's committed-fixture tests are the\n\
             durable coverage this loader's correctness actually relies on.\n\
             ============================================================\n"
        );
    }

    /// End-to-end evidence against the real `LawBreaker` binary (issue
    /// #74's primary source): three real `HUNK_DEBUG` blocks (two
    /// `HEADDBGV01` directory blocks this loader doesn't interpret, and
    /// one real `LINE` block for `LawBreaker.asm`), parsed through the
    /// full [`parse`] entry point exactly as a caller would use it.
    /// Skips (loudly -- see [`warn_real_artifact_missing`]) rather than
    /// fails if the fixture isn't present on this machine: it lives
    /// outside the repo (LawBreaker can't be vendored in -- Enforcer's
    /// non-commercial, no-modification distribution terms), so this is
    /// corroboration, not something CI (or any other machine) can rely
    /// on. `fixtures/linetest` (see
    /// [`real_linetest_fixture_matches_known_pairs`]) is the committed,
    /// always-present equivalent this loader's tested correctness
    /// actually depends on.
    #[test]
    fn real_lawbreaker_binary_line_info_matches_known_pairs() {
        let path = "/Users/simond/.claude/uploads/25505440-09a1-4588-b085-ae3c886e6132/9d77395a-LawBreaker";
        let Ok(bytes) = std::fs::read(path) else {
            warn_real_artifact_missing(
                "real_lawbreaker_binary_line_info_matches_known_pairs",
                path,
            );
            return;
        };

        let file = parse(&bytes).expect("real LawBreaker binary should parse");
        assert_eq!(file.hunks.len(), 1);
        // Two HEADDBGV01 directory blocks plus the one real LINE block.
        assert_eq!(file.hunks[0].debug_blocks.len(), 3);

        let info = file.hunks[0].line_info();
        assert_eq!(info.lookup(0x0000), Some(("LawBreaker.asm", 133)));
        assert_eq!(info.lookup(0x0004), Some(("LawBreaker.asm", 134)));
        assert_eq!(info.lookup(0x0006), Some(("LawBreaker.asm", 135)));
        assert_eq!(info.lookup(0x000a), Some(("LawBreaker.asm", 136)));
        assert_eq!(info.lookup(0x000e), Some(("LawBreaker.asm", 137)));
        assert_eq!(info.lookup(0x0010), Some(("LawBreaker.asm", 141)));
        // Sparse: an offset strictly between two recorded entries
        // (0x0006 and 0x000a) must return the preceding one.
        assert_eq!(info.lookup(0x0008), Some(("LawBreaker.asm", 135)));

        // LawBreaker also carries a real HUNK_SYMBOL block: 3 named
        // entries (LawBreaker@0x0, Do_Law@0x22, Done@0xa6). Offset 0 is
        // covered by both the LawBreaker@0x0 symbol *and* the LINE
        // entry for line 133 -- Hunk::locate's precedence (issue #74's
        // coordinator follow-up) must prefer file:line.
        let symbols = file.hunks[0].symbol_table();
        assert_eq!(symbols.lookup(0x00, 0xcc), Some(("LawBreaker", 0)));
        assert_eq!(symbols.lookup(0x25, 0xcc), Some(("Do_Law", 3)));
        assert_eq!(symbols.lookup(0xa8, 0xcc), Some(("Done", 2)));
        assert_eq!(
            file.hunks[0].locate(0x0000),
            Some(Location::Line {
                file: "LawBreaker.asm".to_string(),
                line: 133,
            }),
            "file:line must win over the LawBreaker@0x0 symbol at the same offset"
        );
    }

    /// End-to-end evidence against a real SAS/C 6.58 object file
    /// (`sc DEBUG=LINE hello.c`), which is *not* loadable through
    /// [`parse`] (it's a `HUNK_UNIT` object module, not a `HUNK_HEADER`
    /// executable) -- so this tests the block-level decoder
    /// (`Hunk::line_info` via a directly-constructed `Hunk`) against the
    /// object file's real, unmodified `HUNK_DEBUG` payload bytes
    /// instead, sliced out at the byte offsets its real `LINE` block
    /// occupies (found by inspection: type/length longwords at file
    /// offset 212, 56-byte payload immediately after). Skips loudly if
    /// the fixture isn't present (see [`warn_real_artifact_missing`]),
    /// same reasoning as the LawBreaker test: this session's scratch
    /// directory doesn't survive, so this is corroboration, not durable
    /// coverage -- see [`real_linetest_fixture_matches_known_pairs`] for
    /// that.
    #[test]
    fn real_sasc_object_line_block_parses() {
        let path = "/private/tmp/claude-501/-Users-simond-src-volamos/25505440-09a1-4588-b085-ae3c886e6132/scratchpad/dbgd/hello.o";
        let Ok(bytes) = std::fs::read(path) else {
            warn_real_artifact_missing("real_sasc_object_line_block_parses", path);
            return;
        };

        // The real file's HUNK_DEBUG/LINE block: type longword (0x3F1)
        // and length longword (0xe = 14 longwords) at file offset 212,
        // payload (56 bytes) immediately after.
        assert_eq!(
            u32::from_be_bytes(bytes[212..216].try_into().unwrap()),
            HUNK_DEBUG,
            "fixture layout assumption: HUNK_DEBUG type word at offset 212"
        );
        let n_longwords = u32::from_be_bytes(bytes[216..220].try_into().unwrap()) as usize;
        let payload = &bytes[220..220 + n_longwords * 4];

        let (filename, entries) =
            parse_line_debug_block(payload).expect("real SAS/C LINE block should parse");
        assert_eq!(filename, "work:hello.c");
        assert_eq!(
            entries,
            vec![
                LineEntry { line: 2, offset: 0 },
                LineEntry {
                    line: 3,
                    offset: 0xc
                },
                LineEntry {
                    line: 4,
                    offset: 22
                },
                LineEntry {
                    line: 5,
                    offset: 24
                },
            ]
        );
    }
}
