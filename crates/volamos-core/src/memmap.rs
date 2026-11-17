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
    pub fn new(mut regions: Vec<MemoryRegion>) -> Result<Self, LayoutError> {
        if regions.is_empty() {
            return Err(LayoutError::Empty);
        }
        regions.sort_by_key(|r| (r.base, r.kind));

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
        }

        let chips = regions
            .iter()
            .filter(|r| r.kind == RegionKind::Chip)
            .count();
        if regions[0].kind != RegionKind::Chip || regions[0].base != 0 {
            return Err(LayoutError::MustStartWithChipAtZero);
        }
        if chips > 1 {
            return Err(LayoutError::DuplicateChip);
        }

        for pair in regions.windows(2) {
            if pair[0].end() > u64::from(pair[1].base) {
                return Err(LayoutError::Overlap {
                    a: pair[0].kind,
                    b: pair[1].kind,
                });
            }
        }
        Ok(Self { regions })
    }

    /// The layout every run had before #332: chip RAM for the first
    /// 2 MiB (or all of `total` if smaller) and, if `total` is larger,
    /// one contiguous block above it, labelled
    /// [`Accelerator`](RegionKind::Accelerator) -- see the module docs
    /// for why not `Fast`. `total` is rounded down to
    /// [`REGION_GRANULE`] and clamped to at least one granule.
    pub fn legacy_flat(total: u32) -> Self {
        let total = (total & !(REGION_GRANULE - 1)).max(REGION_GRANULE);
        let chip = total.min(MAX_CHIP_SIZE);
        let mut regions = vec![MemoryRegion::new(RegionKind::Chip, 0, chip)];
        if total > chip {
            regions.push(MemoryRegion::new(
                RegionKind::Accelerator,
                chip,
                total - chip,
            ));
        }
        Self::new(regions).expect("the legacy flat layout is valid by construction")
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

    /// Whether the regions form one gap-free block starting at address
    /// 0 -- true of [`Self::legacy_flat`], and what lets the memory
    /// backing keep its single-`Vec` fast path.
    pub fn is_contiguous_from_zero(&self) -> bool {
        self.regions
            .windows(2)
            .all(|p| p[0].end() == u64::from(p[1].base))
    }

    /// Fails if any region reaches beyond what an `address_bits`-wide
    /// bus can address (24 for a 68000/68010/EC 68020/EC 68030, 32
    /// otherwise).
    pub fn check_address_bits(&self, address_bits: u32) -> Result<(), LayoutError> {
        let limit = 1u64 << address_bits.min(32);
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
