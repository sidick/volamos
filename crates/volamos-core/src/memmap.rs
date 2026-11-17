//! The guest memory map: which address ranges are RAM, and of what
//! class. Issue #332 (the Amiga memory-map umbrella, which supersedes
//! #135).
//!
//! # Why a layout type, and why now
//!
//! Until this module the guest's RAM was "everything from address 0 to
//! `--ram`": one flat range, one allocator, a `TypeOfMem` that guessed
//! chip/fast by address. A real Amiga's RAM is a handful of separate
//! regions at fixed places in the address space -- chip RAM at 0,
//! Zorro II fast RAM from `$200000`, "slow" RAM at `$C00000`, an
//! accelerator board's local RAM, Zorro III RAM from `$10000000` --
//! and which of them exist depends on the machine and, above all, on
//! the CPU: a 68000/68010 or an EC 68020/68030 has only a 24-bit
//! address bus, so nothing above `$FFFFFF` can exist there. This type
//! is the description of that map. It is deliberately *just data and
//! validation*: no memory, no allocator. The region-backed
//! [`crate::memory::FlatMemory`], the per-region heaps and the
//! per-CPU defaults are built on top in later phases of #332.
//!
//! # Region kinds and their address windows
//!
//! Each [`RegionKind`] may only sit in the part of the address space
//! where that class of RAM really lives, so a layout that validates is
//! one a real machine could have:
//!
//! | Kind | Window | Why |
//! |---|---|---|
//! | [`Chip`](RegionKind::Chip) | `$000000-$1FFFFF`, must start at `0` | the custom chips' DMA reach; 2 MiB is the most any ECS/AGA machine has, and the runtime's own reserved low region (vectors, `ExecBase`, trap table) lives at 0 |
//! | [`Slow`](RegionKind::Slow) | `$C00000-$D7FFFF` | the "ranger" window (trapdoor expansion on an A500/A2000), ending where `$D80000` motherboard devices begin |
//! | [`Fast`](RegionKind::Fast) | `$200000-$9FFFFF` | the Zorro II autoconfig space |
//! | [`Accelerator`](RegionKind::Accelerator) | `$200000` upward | CPU-local RAM on an accelerator board or A3000/A4000 motherboard; it decodes wherever the board says, so only the lower bound is fixed |
//! | [`Zorro3`](RegionKind::Zorro3) | `$10000000-$7FFFFFFF` | the Zorro III autoconfig space |
//!
//! Chip's `$200000` upper bound is also the lower bound of every other
//! kind, so "chip" and "everything else" can never overlap.
//!
//! Accelerator RAM is the one kind whose own window is wide (it decodes
//! wherever the board says), so [`MemoryLayout::new`] additionally
//! refuses any region that covers the reserved I/O, device and ROM
//! ranges inside the 24-bit space (`$A00000-$BFFFFF` and
//! `$D80000-$FFFFFF`): on a real machine those addresses are never RAM.
//!
//! # The 24-bit constraint
//!
//! [`MemoryLayout::check_address_bits`] rejects a layout with any byte
//! above what the CPU can address. It is a separate call, not part of
//! [`MemoryLayout::new`], because the layout alone doesn't know the CPU
//! (the caller does -- see `crate::backend`'s 24-bit handling, issue
//! #98).
//!
//! # The legacy flat layout
//!
//! [`MemoryLayout::legacy_flat`] is the layout every run had before
//! this module existed: chip RAM for the first 2 MiB (or all of RAM if
//! that is smaller) and one contiguous block above it up to `--ram`.
//! That block is deliberately labelled [`Accelerator`](RegionKind::Accelerator),
//! not `Fast`: Zorro II space ends at `$9FFFFF`, so a 14 MiB contiguous
//! block can only be an accelerator board's own RAM, and the label
//! keeps the validation honest. It exists so that the first phase of
//! #332 can introduce this type without changing any run's behaviour.

use std::fmt;

/// Most chip RAM any ECS/AGA machine has, and the upper bound of the
/// chip window (see the module docs).
pub const MAX_CHIP_SIZE: u32 = 0x20_0000;

/// First address of the Zorro II fast-RAM window.
const FAST_WINDOW: (u64, u64) = (0x20_0000, 0xA0_0000);
/// The "slow" (ranger) RAM window.
const SLOW_WINDOW: (u64, u64) = (0xC0_0000, 0xD8_0000);
/// The Zorro III autoconfig window.
const ZORRO3_WINDOW: (u64, u64) = (0x1000_0000, 0x8000_0000);
/// Lowest address an accelerator board's RAM may start at.
const ACCELERATOR_MIN: u64 = 0x20_0000;
/// Address ranges inside the 24-bit space that real machines never
/// decode as RAM: Zorro II I/O and the CIAs (`$A00000-$BFFFFF`) and the
/// motherboard devices, custom chips and ROM (`$D80000-$FFFFFF`).
/// Slow RAM sits between them and is covered by its own window.
const RESERVED_WINDOWS: [(u64, u64); 2] = [(0xA0_0000, 0xC0_0000), (0xD8_0000, 0x100_0000)];
/// End (exclusive) of the 32-bit address space.
const ADDRESS_SPACE_END: u64 = 0x1_0000_0000;

/// Granularity of a region's base and size. Real boards are far
/// coarser (Zorro II boards come in 64 KiB multiples at least); 4 KiB
/// keeps the layout expressible in whole pages without inventing a
/// restriction no real machine needs to be tested against.
pub const REGION_GRANULE: u32 = 0x1000;

/// A class of RAM, i.e. what an exec memory list would call it. See the
/// module docs for each kind's address window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RegionKind {
    /// Chip RAM: reachable by the custom chips' DMA.
    Chip,
    /// "Slow" RAM at `$C00000`: counted as fast RAM by exec, but on the
    /// chip bus.
    Slow,
    /// Zorro II fast RAM.
    Fast,
    /// CPU-local RAM on an accelerator board or 32-bit motherboard.
    Accelerator,
    /// Zorro III fast RAM.
    Zorro3,
}

impl RegionKind {
    /// Short lowercase name, as used in diagnostics and (later) on the
    /// command line.
    pub fn name(self) -> &'static str {
        match self {
            RegionKind::Chip => "chip",
            RegionKind::Slow => "slow",
            RegionKind::Fast => "fast",
            RegionKind::Accelerator => "accel",
            RegionKind::Zorro3 => "z3",
        }
    }

    /// The `[lo, hi)` window this kind of RAM may occupy.
    fn window(self) -> (u64, u64) {
        match self {
            RegionKind::Chip => (0, u64::from(MAX_CHIP_SIZE)),
            RegionKind::Slow => SLOW_WINDOW,
            RegionKind::Fast => FAST_WINDOW,
            RegionKind::Accelerator => (ACCELERATOR_MIN, ADDRESS_SPACE_END),
            RegionKind::Zorro3 => ZORRO3_WINDOW,
        }
    }
}

/// `MEMF_KICK` (bit 10): memory a Kickstart-resident module may use.
/// Not in the NDK 1.3 headers; the value (0x400) is what every real
/// `MemHeader` below carries in `mh_Attributes`.
pub const MEMF_KICK: u32 = 1 << 10;

impl RegionKind {
    /// The `ln_Pri` real exec gives this class's `MemHeader`: allocation
    /// tries the highest priority first, so `MEMF_ANY` prefers
    /// accelerator RAM, then Zorro II/III fast RAM, then slow, then chip.
    ///
    /// Chip (-10), slow (-5) and Zorro II fast (0) and the A4000-style
    /// 32-bit CPU RAM (30, used for [`RegionKind::Accelerator`]) were
    /// measured on Kickstart 3.1 r40.68 via Copperline; Zorro III is
    /// assumed to match Zorro II (0) and is *not* hardware-verified.
    pub fn priority(self) -> i8 {
        match self {
            RegionKind::Chip => -10,
            RegionKind::Slow => -5,
            RegionKind::Fast | RegionKind::Zorro3 => 0,
            RegionKind::Accelerator => 30,
        }
    }

    /// The `mh_Name` real exec gives this class's `MemHeader`.
    pub fn header_name(self) -> &'static str {
        match self {
            RegionKind::Chip => "chip memory",
            RegionKind::Slow => "memory",
            RegionKind::Fast | RegionKind::Zorro3 | RegionKind::Accelerator => "expansion memory",
        }
    }
}

/// One contiguous block of RAM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryRegion {
    pub kind: RegionKind,
    /// First guest address of the block.
    pub base: u32,
    /// Length in bytes.
    pub size: u32,
}

impl MemoryRegion {
    pub fn new(kind: RegionKind, base: u32, size: u32) -> Self {
        Self { kind, base, size }
    }

    /// One past the last byte. A `u64` so a region ending exactly at
    /// the top of the 32-bit space doesn't wrap.
    pub fn end(&self) -> u64 {
        u64::from(self.base) + u64::from(self.size)
    }

    /// The `mh_Attributes` real exec gives this region's `MemHeader`,
    /// i.e. what `TypeOfMem` reports for an address in it and what an
    /// `AllocMem` requirement has to be a subset of. Measured on
    /// Kickstart 3.1 r40.68 (Copperline):
    ///
    /// | kind | attributes |
    /// |---|---|
    /// | chip | `0x703` public, chip, local, 24-bit DMA, kick |
    /// | slow | `0x705` public, fast, local, 24-bit DMA, kick |
    /// | Zorro II fast | `0x605` public, fast, 24-bit DMA, kick |
    /// | 32-bit CPU RAM (A4000 motherboard) | `0x505` public, fast, local, kick |
    ///
    /// Accelerator RAM takes the last row, plus 24-bit DMA when it lies
    /// wholly within the 24-bit space (as the legacy flat layout's does).
    /// Zorro III is assumed to be Zorro II's without 24-bit DMA
    /// (`0x405`); not hardware-verified.
    pub fn attributes(&self) -> u32 {
        use crate::execmem::{MEMF_24BITDMA, MEMF_CHIP, MEMF_FAST, MEMF_LOCAL, MEMF_PUBLIC};
        let within_24_bits = self.end() <= 0x100_0000;
        let dma = if within_24_bits { MEMF_24BITDMA } else { 0 };
        let base = MEMF_PUBLIC | MEMF_KICK;
        match self.kind {
            RegionKind::Chip => base | MEMF_CHIP | MEMF_LOCAL | MEMF_24BITDMA,
            RegionKind::Slow => base | MEMF_FAST | MEMF_LOCAL | MEMF_24BITDMA,
            RegionKind::Fast => base | MEMF_FAST | MEMF_24BITDMA,
            RegionKind::Accelerator => base | MEMF_FAST | MEMF_LOCAL | dma,
            RegionKind::Zorro3 => base | MEMF_FAST,
        }
    }

    /// Whether `addr` is inside this region.
    pub fn contains(&self, addr: u32) -> bool {
        addr >= self.base && u64::from(addr) < self.end()
    }
}

/// Why a set of regions is not a valid [`MemoryLayout`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutError {
    /// No regions at all: the runtime's reserved low region needs RAM.
    Empty,
    /// The first region is not chip RAM at address 0.
    MustStartWithChipAtZero,
    /// A second chip region, or one that doesn't start at 0.
    DuplicateChip,
    /// A region of zero length.
    ZeroSize(RegionKind),
    /// A base or size that is not a multiple of [`REGION_GRANULE`].
    Misaligned {
        kind: RegionKind,
        base: u32,
        size: u32,
    },
    /// A region reaching outside the address window its kind lives in.
    OutsideWindow {
        kind: RegionKind,
        base: u32,
        end: u64,
        window: (u64, u64),
    },
    /// Two regions sharing an address.
    Overlap { a: RegionKind, b: RegionKind },
    /// A region covering addresses a real machine decodes as I/O,
    /// motherboard devices or ROM, never RAM.
    OverlapsReserved {
        kind: RegionKind,
        base: u32,
        end: u64,
    },
    /// A region extending past what the CPU's address bus can reach
    /// (see [`MemoryLayout::check_address_bits`]).
    BeyondAddressBus {
        kind: RegionKind,
        end: u64,
        address_bits: u32,
    },
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LayoutError::Empty => write!(f, "memory layout has no RAM"),
            LayoutError::MustStartWithChipAtZero => {
                write!(f, "memory layout must start with chip RAM at address 0")
            }
            LayoutError::DuplicateChip => {
                write!(f, "memory layout has more than one chip RAM region")
            }
            LayoutError::ZeroSize(k) => write!(f, "{} RAM region has zero size", k.name()),
            LayoutError::Misaligned { kind, base, size } => write!(
                f,
                "{} RAM region at ${base:X} size ${size:X} is not a multiple of ${REGION_GRANULE:X}",
                kind.name()
            ),
            LayoutError::OutsideWindow {
                kind,
                base,
                end,
                window,
            } => write!(
                f,
                "{} RAM at ${base:X}-${:X} is outside its address window ${:X}-${:X}",
                kind.name(),
                end - 1,
                window.0,
                window.1 - 1
            ),
            LayoutError::OverlapsReserved { kind, base, end } => write!(
                f,
                "{} RAM at ${base:X}-${:X} covers addresses that are I/O, devices or ROM on a real machine",
                kind.name(),
                end - 1
            ),
            LayoutError::Overlap { a, b } => {
                write!(f, "{} and {} RAM regions overlap", a.name(), b.name())
            }
            LayoutError::BeyondAddressBus {
                kind,
                end,
                address_bits,
            } => write!(
                f,
                "{} RAM ends at ${:X}, beyond the {address_bits}-bit address bus of the selected CPU",
                kind.name(),
                end - 1
            ),
        }
    }
}

impl std::error::Error for LayoutError {}

/// A validated set of non-overlapping RAM regions, sorted by base
/// address, the first of which is chip RAM at address 0. See the module
/// docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryLayout {
    regions: Vec<MemoryRegion>,
}

impl MemoryLayout {
    /// Validates and sorts `regions`.
    pub fn new(regions: Vec<MemoryRegion>) -> Result<Self, LayoutError> {
        Self::build(regions, true)
    }

    /// [`Self::new`], with `enforce_reserved` choosing whether regions
    /// may cover the reserved I/O/device/ROM ranges. Only
    /// [`Self::legacy_flat`] passes `false`.
    fn build(mut regions: Vec<MemoryRegion>, enforce_reserved: bool) -> Result<Self, LayoutError> {
        if regions.is_empty() {
            return Err(LayoutError::Empty);
        }
        regions.sort_by_key(|r| (r.base, r.kind));

        if regions[0].kind != RegionKind::Chip || regions[0].base != 0 {
            return Err(LayoutError::MustStartWithChipAtZero);
        }

        for r in &regions {
            if r.size == 0 {
                return Err(LayoutError::ZeroSize(r.kind));
            }
            if r.base % REGION_GRANULE != 0 || r.size % REGION_GRANULE != 0 {
                return Err(LayoutError::Misaligned {
                    kind: r.kind,
                    base: r.base,
                    size: r.size,
                });
            }
            let window = r.kind.window();
            if u64::from(r.base) < window.0 || r.end() > window.1 {
                return Err(LayoutError::OutsideWindow {
                    kind: r.kind,
                    base: r.base,
                    end: r.end(),
                    window,
                });
            }
            if enforce_reserved
                && RESERVED_WINDOWS
                    .iter()
                    .any(|&(lo, hi)| u64::from(r.base) < hi && r.end() > lo)
            {
                return Err(LayoutError::OverlapsReserved {
                    kind: r.kind,
                    base: r.base,
                    end: r.end(),
                });
            }
        }

        // Overlap before the chip count, so two overlapping chip blocks
        // are reported as an overlap rather than as "duplicate".
        for pair in regions.windows(2) {
            if pair[0].end() > u64::from(pair[1].base) {
                return Err(LayoutError::Overlap {
                    a: pair[0].kind,
                    b: pair[1].kind,
                });
            }
        }
        if regions
            .iter()
            .filter(|r| r.kind == RegionKind::Chip)
            .count()
            > 1
        {
            return Err(LayoutError::DuplicateChip);
        }
        Ok(Self { regions })
    }

    /// The layout every run had before #332: chip RAM for the first
    /// 2 MiB (or all of `total` if smaller) and, if `total` is larger,
    /// one contiguous block above it, labelled
    /// [`Accelerator`](RegionKind::Accelerator) -- see the module docs
    /// for why not `Fast`. `total` is rounded down to
    /// [`REGION_GRANULE`] and clamped to at least one granule, so a
    /// caller sizing real memory from this layout must use
    /// [`Self::total_size`], not the `total` it passed in.
    ///
    /// This is the one layout that knowingly covers the reserved
    /// I/O/device/ROM ranges (a 16 MiB block does, by construction):
    /// it is a compatibility layout, not a machine, and is what the
    /// later per-CPU defaults replace.
    pub fn legacy_flat(total: u32) -> Self {
        let total = (total & !(REGION_GRANULE - 1)).max(REGION_GRANULE);
        Self::build(Self::legacy_regions(total), false)
            .expect("the legacy flat layout is valid by construction")
    }

    /// The layout every run had before #332, built from an exact byte
    /// count: chip RAM for the first 2 MiB (or all of `total`), one
    /// contiguous accelerator-labelled block above. Unlike
    /// [`Self::legacy_flat`] it neither rounds nor validates, so it can
    /// describe whatever size an existing caller already built its
    /// memory with (tests use sizes below one granule).
    pub fn legacy_exact(total: u32) -> Self {
        Self {
            regions: Self::legacy_regions(total),
        }
    }

    /// Chip, then accelerator RAM up to `total`, split at the 24-bit
    /// boundary: real exec gives the part below 16 MiB (which DMA can
    /// reach) its own `MemHeader` with `MEMF_24BITDMA`, and one block
    /// straddling the line would lose the bit for all of it.
    fn legacy_regions(total: u32) -> Vec<MemoryRegion> {
        const BOUNDARY: u32 = 0x100_0000;
        let chip = total.min(MAX_CHIP_SIZE);
        let mut regions = vec![MemoryRegion::new(RegionKind::Chip, 0, chip)];
        if total > chip {
            let low_end = total.min(BOUNDARY);
            regions.push(MemoryRegion::new(
                RegionKind::Accelerator,
                chip,
                low_end - chip,
            ));
            if total > BOUNDARY {
                regions.push(MemoryRegion::new(
                    RegionKind::Accelerator,
                    BOUNDARY,
                    total - BOUNDARY,
                ));
            }
        }
        regions
    }

    /// Index into [`Self::regions`] of the region containing `addr`.
    pub fn region_index_at(&self, addr: u32) -> Option<usize> {
        self.regions.iter().position(|r| r.contains(addr))
    }

    /// Total bytes of RAM in regions whose attributes include every bit
    /// of `required` -- what `AvailMem(... | MEMF_TOTAL)` reports.
    pub fn total_capacity(&self, required: u32) -> u64 {
        self.regions
            .iter()
            .filter(|r| r.attributes() & required == required)
            .map(|r| u64::from(r.size))
            .sum()
    }

    /// The regions, sorted by base address.
    pub fn regions(&self) -> &[MemoryRegion] {
        &self.regions
    }

    /// The region containing `addr`, if any.
    pub fn region_at(&self, addr: u32) -> Option<&MemoryRegion> {
        self.regions.iter().find(|r| r.contains(addr))
    }

    /// Total bytes of RAM across all regions.
    pub fn total_size(&self) -> u64 {
        self.regions.iter().map(|r| u64::from(r.size)).sum()
    }

    /// One past the highest RAM address.
    pub fn highest_end(&self) -> u64 {
        self.regions.last().map_or(0, MemoryRegion::end)
    }

    /// `(end, count)` of the longest run of regions touching address 0
    /// without gaps: the first `count` regions cover `[0, end)`. This
    /// is what the memory backing keeps in its single primary `Vec`.
    pub fn contiguous_prefix(&self) -> (u64, usize) {
        let mut end = 0u64;
        let mut count = 0;
        for r in &self.regions {
            if u64::from(r.base) != end {
                break;
            }
            end = r.end();
            count += 1;
        }
        (end, count)
    }

    /// Whether the regions form one gap-free block starting at address
    /// 0 -- true of [`Self::legacy_flat`], and what lets the memory
    /// backing keep its single-`Vec` fast path.
    pub fn is_contiguous_from_zero(&self) -> bool {
        self.contiguous_prefix().1 == self.regions.len()
    }

    /// Fails if any region reaches beyond what an `address_bits`-wide
    /// bus can address (24 for a 68000/68010/EC 68020/EC 68030, 32
    /// otherwise).
    pub fn check_address_bits(&self, address_bits: u32) -> Result<(), LayoutError> {
        // A bus is between 1 and 32 bits wide; report the width used.
        let address_bits = address_bits.clamp(1, 32);
        let limit = 1u64 << address_bits;
        match self.regions.iter().find(|r| r.end() > limit) {
            Some(r) => Err(LayoutError::BeyondAddressBus {
                kind: r.kind,
                end: r.end(),
                address_bits,
            }),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u32 = 0x10_0000;

    fn r(kind: RegionKind, base: u32, size: u32) -> MemoryRegion {
        MemoryRegion::new(kind, base, size)
    }

    #[test]
    fn a_stock_a500_style_layout_validates() {
        let layout = MemoryLayout::new(vec![
            r(RegionKind::Slow, 0xC0_0000, MIB / 2),
            r(RegionKind::Chip, 0, 512 * 1024),
        ])
        .expect("512K chip + 512K slow is a real A500 configuration");
        // Sorted by base regardless of the order given.
        assert_eq!(layout.regions()[0].kind, RegionKind::Chip);
        assert_eq!(layout.regions()[1].kind, RegionKind::Slow);
        assert_eq!(layout.total_size(), 1024 * 1024);
        assert!(!layout.is_contiguous_from_zero());
    }

    #[test]
    fn a_32_bit_machine_with_every_region_kind_validates() {
        let layout = MemoryLayout::new(vec![
            r(RegionKind::Chip, 0, 2 * MIB),
            r(RegionKind::Fast, 2 * MIB, 8 * MIB),
            r(RegionKind::Slow, 0xC0_0000, MIB),
            r(RegionKind::Accelerator, 0x0800_0000, 64 * MIB),
            r(RegionKind::Zorro3, 0x4000_0000, 128 * MIB),
        ])
        .unwrap();
        assert_eq!(layout.regions().len(), 5);
        assert_eq!(layout.highest_end(), 0x4800_0000);
        assert_eq!(
            layout.region_at(0x0800_0004).map(|r| r.kind),
            Some(RegionKind::Accelerator)
        );
        assert_eq!(layout.region_at(0xB0_0000), None, "unmapped gap");
    }

    #[test]
    fn legacy_flat_reproduces_the_old_single_range() {
        let l = MemoryLayout::legacy_flat(16 * MIB);
        assert!(l.is_contiguous_from_zero());
        assert_eq!(l.total_size(), 16 * u64::from(MIB));
        assert_eq!(l.regions()[0], r(RegionKind::Chip, 0, 2 * MIB));
        assert_eq!(
            l.regions()[1],
            r(RegionKind::Accelerator, 2 * MIB, 14 * MIB)
        );
        assert_eq!(l.regions().len(), 2, "16 MiB is exactly the 24-bit space");
    }

    #[test]
    fn region_attributes_and_priorities_match_real_kickstart_headers() {
        let chip = r(RegionKind::Chip, 0, 2 * MIB);
        let slow = r(RegionKind::Slow, 0xC0_0000, MIB / 2);
        let z2 = r(RegionKind::Fast, 2 * MIB, 8 * MIB);
        let mb = r(RegionKind::Accelerator, 0x0780_0000, 8 * MIB);
        // Measured with a probe walking SysBase->MemList on KS 3.1 r40.68.
        assert_eq!(chip.attributes(), 0x703);
        assert_eq!(slow.attributes(), 0x705);
        assert_eq!(z2.attributes(), 0x605);
        assert_eq!(mb.attributes(), 0x505);
        assert_eq!(
            [chip.kind, slow.kind, z2.kind, mb.kind].map(RegionKind::priority),
            [-10, -5, 0, 30]
        );
        // Wholly inside the 24-bit space an accelerator gains 24-bit DMA.
        let legacy = MemoryLayout::legacy_flat(16 * MIB);
        assert_eq!(legacy.regions()[1].attributes(), 0x705);
        assert_eq!(r(RegionKind::Zorro3, 0x1000_0000, MIB).attributes(), 0x405);
    }

    #[test]
    fn total_capacity_sums_the_regions_with_every_required_bit() {
        let l = MemoryLayout::new(vec![
            r(RegionKind::Chip, 0, 2 * MIB),
            r(RegionKind::Fast, 2 * MIB, 4 * MIB),
            r(RegionKind::Accelerator, 0x0780_0000, 8 * MIB),
        ])
        .unwrap();
        let (chip, fast) = (crate::execmem::MEMF_CHIP, crate::execmem::MEMF_FAST);
        assert_eq!(l.total_capacity(chip), u64::from(2 * MIB));
        assert_eq!(l.total_capacity(fast), u64::from(12 * MIB));
        assert_eq!(l.total_capacity(0), u64::from(14 * MIB));
        assert_eq!(l.total_capacity(chip | fast), 0, "no header has both");
    }

    #[test]
    fn legacy_exact_does_not_round_or_validate() {
        let l = MemoryLayout::legacy_exact(0x2_0000 + 6);
        assert_eq!(l.total_size(), 0x2_0006);
        assert_eq!(l.regions().len(), 1, "all chip below 2 MiB");
        assert_eq!(l.region_index_at(0x2_0005), Some(0));
        assert_eq!(l.region_index_at(0x2_0006), None);
    }

    #[test]
    fn legacy_layouts_split_accelerator_ram_at_the_24_bit_boundary() {
        // Beyond 16 MiB, the part DMA can reach keeps MEMF_24BITDMA.
        for l in [
            MemoryLayout::legacy_flat(64 * MIB),
            MemoryLayout::legacy_exact(64 * MIB),
        ] {
            assert_eq!(l.regions().len(), 3);
            assert_eq!(
                l.regions()[1],
                r(RegionKind::Accelerator, 2 * MIB, 14 * MIB)
            );
            assert_eq!(
                l.regions()[2],
                r(RegionKind::Accelerator, 16 * MIB, 48 * MIB)
            );
            assert_eq!(l.regions()[1].attributes(), 0x705);
            assert_eq!(l.regions()[2].attributes(), 0x505);
            assert!(l.is_contiguous_from_zero());
            assert_eq!(l.total_size(), 64 * u64::from(MIB));
        }
    }

    #[test]
    fn legacy_flat_below_two_mib_is_all_chip() {
        let l = MemoryLayout::legacy_flat(MIB);
        assert_eq!(l.regions(), &[r(RegionKind::Chip, 0, MIB)]);
    }

    #[test]
    fn legacy_flat_rounds_odd_sizes_to_the_granule() {
        let l = MemoryLayout::legacy_flat(3 * MIB + 123);
        assert_eq!(l.total_size(), 3 * u64::from(MIB));
        // And never produces an empty layout.
        assert_eq!(MemoryLayout::legacy_flat(0).total_size(), 4096);
    }

    #[test]
    fn legacy_flat_beyond_sixteen_mib_is_still_a_valid_layout() {
        // --ram 64M: allowed (32-bit CPUs); only the 24-bit check objects.
        let l = MemoryLayout::legacy_flat(64 * MIB);
        assert_eq!(
            l.check_address_bits(24),
            Err(LayoutError::BeyondAddressBus {
                kind: RegionKind::Accelerator,
                end: 64 * u64::from(MIB),
                address_bits: 24
            })
        );
        assert_eq!(l.check_address_bits(32), Ok(()));
    }

    #[test]
    fn exactly_sixteen_mib_fits_a_24_bit_bus() {
        assert_eq!(
            MemoryLayout::legacy_flat(16 * MIB).check_address_bits(24),
            Ok(())
        );
    }

    #[test]
    fn a_region_ending_at_the_top_of_the_address_space_does_not_wrap() {
        // Accelerator RAM may run to $FFFFFFFF.
        let l = MemoryLayout::new(vec![
            r(RegionKind::Chip, 0, 2 * MIB),
            r(RegionKind::Accelerator, 0xFF00_0000, 0x100_0000),
        ])
        .unwrap();
        assert_eq!(l.highest_end(), 0x1_0000_0000);
        assert!(l.region_at(0xFFFF_FFFC).is_some());
    }

    #[test]
    fn empty_and_chipless_layouts_are_rejected() {
        assert_eq!(MemoryLayout::new(vec![]), Err(LayoutError::Empty));
        assert_eq!(
            MemoryLayout::new(vec![r(RegionKind::Fast, 2 * MIB, MIB)]),
            Err(LayoutError::MustStartWithChipAtZero)
        );
    }

    #[test]
    fn chip_must_start_at_zero_and_there_can_only_be_one() {
        assert_eq!(
            MemoryLayout::new(vec![r(RegionKind::Chip, 0x1000, MIB)]),
            Err(LayoutError::MustStartWithChipAtZero)
        );
        // A second chip block, even one inside the chip window.
        assert_eq!(
            MemoryLayout::new(vec![
                r(RegionKind::Chip, 0, MIB / 2),
                r(RegionKind::Chip, MIB, MIB / 2),
            ]),
            Err(LayoutError::DuplicateChip)
        );
    }

    #[test]
    fn chip_larger_than_two_mib_is_rejected() {
        assert!(matches!(
            MemoryLayout::new(vec![r(RegionKind::Chip, 0, 4 * MIB)]),
            Err(LayoutError::OutsideWindow {
                kind: RegionKind::Chip,
                ..
            })
        ));
    }

    #[test]
    fn each_kind_is_confined_to_its_address_window() {
        let chip = r(RegionKind::Chip, 0, 2 * MIB);
        for bad in [
            r(RegionKind::Fast, 0x10_0000 * 9, 2 * MIB), // spills past $9FFFFF
            r(RegionKind::Slow, 0xB0_0000, MIB),         // starts below $C00000
            r(RegionKind::Slow, 0xD0_0000, MIB),         // runs into $D80000
            r(RegionKind::Zorro3, 0x0100_0000, MIB),     // below the Z3 window
            r(RegionKind::Accelerator, 0x10_0000, MIB),  // overlaps chip's space
        ] {
            assert!(
                matches!(
                    MemoryLayout::new(vec![chip, bad]),
                    Err(LayoutError::OutsideWindow { .. })
                ),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn regions_covering_io_devices_or_rom_are_rejected() {
        let chip = r(RegionKind::Chip, 0, 2 * MIB);
        for (base, size) in [
            (2 * MIB, 14 * MIB),  // the old flat 16 MiB block
            (0xB0_0000, MIB),     // Zorro II I/O / CIA range
            (0xF0_0000, MIB / 2), // ROM
        ] {
            assert!(
                matches!(
                    MemoryLayout::new(vec![chip, r(RegionKind::Accelerator, base, size)]),
                    Err(LayoutError::OverlapsReserved { .. })
                ),
                "${base:X}+${size:X} must be refused"
            );
        }
        // Accelerator RAM below the I/O range, or above 16 MiB, is fine.
        MemoryLayout::new(vec![chip, r(RegionKind::Accelerator, 2 * MIB, 8 * MIB)]).unwrap();
        MemoryLayout::new(vec![chip, r(RegionKind::Accelerator, 0x100_0000, 16 * MIB)]).unwrap();
    }

    #[test]
    fn overlapping_chip_blocks_are_reported_as_an_overlap() {
        assert_eq!(
            MemoryLayout::new(vec![
                r(RegionKind::Chip, 0, MIB),
                r(RegionKind::Chip, MIB / 2, MIB / 2),
            ]),
            Err(LayoutError::Overlap {
                a: RegionKind::Chip,
                b: RegionKind::Chip
            })
        );
    }

    #[test]
    fn address_bits_outside_one_to_thirty_two_are_clamped_and_reported_as_used() {
        let l = MemoryLayout::legacy_flat(16 * MIB);
        assert_eq!(l.check_address_bits(40), Ok(()));
        assert!(matches!(
            l.check_address_bits(0),
            Err(LayoutError::BeyondAddressBus {
                address_bits: 1,
                ..
            })
        ));
    }

    #[test]
    fn overlapping_regions_are_rejected() {
        assert_eq!(
            MemoryLayout::new(vec![
                r(RegionKind::Chip, 0, 2 * MIB),
                r(RegionKind::Accelerator, 2 * MIB, 8 * MIB),
                r(RegionKind::Fast, 4 * MIB, 2 * MIB),
            ]),
            Err(LayoutError::Overlap {
                a: RegionKind::Accelerator,
                b: RegionKind::Fast
            })
        );
    }

    #[test]
    fn zero_sized_and_misaligned_regions_are_rejected() {
        assert_eq!(
            MemoryLayout::new(vec![r(RegionKind::Chip, 0, 0)]),
            Err(LayoutError::ZeroSize(RegionKind::Chip))
        );
        assert!(matches!(
            MemoryLayout::new(vec![r(RegionKind::Chip, 0, 0x1800)]),
            Err(LayoutError::Misaligned { .. })
        ));
    }

    #[test]
    fn errors_read_as_plain_sentences() {
        let e = MemoryLayout::legacy_flat(64 * MIB)
            .check_address_bits(24)
            .unwrap_err();
        assert_eq!(
            e.to_string(),
            "accel RAM ends at $3FFFFFF, beyond the 24-bit address bus of the selected CPU"
        );
    }
}
