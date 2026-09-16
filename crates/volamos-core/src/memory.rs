//! A minimal, flat guest address space.
//!
//! The m68k guest CPU is big-endian, so all multi-byte accesses here use
//! big-endian byte order regardless of the host's native endianness.
//!
//! # Optional sanitizer
//!
//! [`FlatMemory`] can optionally carry a [`crate::sanitize::ShadowMap`]
//! (installed with [`FlatMemory::enable_sanitizer`], off by default --
//! see the CLI's `--sanitize` flag). When installed, every access path
//! below (`read_u8`/`write_u8` and all four overridden multi-byte fast
//! paths) checks/updates it first; when not installed, every access is
//! exactly as fast and exactly as total as before this feature existed
//! -- one `Option` check (via [`FlatMemory::shadow_mut`]/[`FlatMemory::
//! shadow`]) per access, not per byte.
//!
//! The shadow map lives *inside* `FlatMemory` rather than as a separate
//! wrapper type implementing [`AddressSpace`]: [`crate::cpu::Cpu::
//! Memory`] is a single concrete associated type (`FlatMemory`, see
//! `crate::backend`'s module doc on why), so a generic wrapper would
//! need threading a type parameter through the entire runtime for a
//! feature that's off by default. An `Option` field is far simpler and
//! costs nothing when unused.

use crate::sanitize::ShadowMap;

/// A byte-addressable memory space that a [`Cpu`](crate::cpu::Cpu)
/// implementation can read from and write to.
///
/// Multi-byte reads/writes use m68k (big-endian) byte order.
///
/// Out-of-range behavior is deliberately simple and total (no `Result`,
/// no panics): reads past the end of the backing store return `0`, and
/// writes past the end of the backing store are silently ignored. This
/// keeps the trait ergonomic for a CPU core's hot path; callers that need
/// to detect bad guest addresses (e.g. to raise a bus error) should check
/// bounds themselves via [`AddressSpace::len`] before accessing.
pub trait AddressSpace {
    /// The size in bytes of this address space.
    fn len(&self) -> usize;

    /// Returns `true` if this address space has zero bytes.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Reads a single byte. Returns `0` if `addr` is out of range.
    fn read_u8(&self, addr: u32) -> u8;

    /// Writes a single byte. Silently ignored if `addr` is out of range.
    fn write_u8(&mut self, addr: u32, value: u8);

    /// Reads a big-endian 16-bit value. Any byte that falls out of range
    /// reads as `0`.
    fn read_u16(&self, addr: u32) -> u16 {
        let hi = self.read_u8(addr) as u16;
        let lo = self.read_u8(addr.wrapping_add(1)) as u16;
        (hi << 8) | lo
    }

    /// Writes a big-endian 16-bit value. Any byte that falls out of range
    /// is silently dropped.
    fn write_u16(&mut self, addr: u32, value: u16) {
        self.write_u8(addr, (value >> 8) as u8);
        self.write_u8(addr.wrapping_add(1), value as u8);
    }

    /// Reads a big-endian 32-bit value. Any byte that falls out of range
    /// reads as `0`.
    fn read_u32(&self, addr: u32) -> u32 {
        let hi = self.read_u16(addr) as u32;
        let lo = self.read_u16(addr.wrapping_add(2)) as u32;
        (hi << 16) | lo
    }

    /// Writes a big-endian 32-bit value. Any byte that falls out of range
    /// is silently dropped.
    fn write_u32(&mut self, addr: u32, value: u32) {
        self.write_u16(addr, (value >> 16) as u16);
        self.write_u16(addr.wrapping_add(2), value as u16);
    }

    /// The sanitizer shadow map this address space carries, if any.
    ///
    /// This lives on the trait (defaulting to `None`) rather than only
    /// as an inherent method on [`FlatMemory`] because the library-call
    /// handlers that need to poison and un-poison ranges --
    /// `crate::execmem`'s `AllocMem`/`FreeMem` above all -- are generic
    /// over `C: Cpu` and so only ever see their memory as
    /// `C::Memory: AddressSpace`, never as the concrete `FlatMemory`.
    /// Without these two methods the shadow map would be installed but
    /// unreachable from precisely the code that knows where allocations
    /// begin and end.
    fn shadow(&self) -> Option<&crate::sanitize::ShadowMap> {
        None
    }

    /// Mutable counterpart to [`AddressSpace::shadow`]. Defaults to
    /// `None` so an address space with no sanitizer support needs no
    /// boilerplate.
    fn shadow_mut(&mut self) -> Option<&mut crate::sanitize::ShadowMap> {
        None
    }
}

/// A simple flat, contiguous [`AddressSpace`] backed by a `Vec<u8>`.
///
/// This is a placeholder implementation good enough for early CPU/trap
/// plumbing work. It has no notion of memory-mapped regions, protection,
/// or sparse allocation; guest address `0` maps to byte `0` of the
/// backing `Vec`.
#[derive(Debug, Clone)]
pub struct FlatMemory {
    bytes: Vec<u8>,
    /// The optional sanitizer shadow map -- see this module's doc.
    /// Boxed so the common (disabled) case doesn't pay for
    /// [`ShadowMap`]'s own `Vec`/`HashMap` fields inline in every
    /// `FlatMemory`, and so `FlatMemory` stays cheap to move around
    /// (e.g. into a nested [`crate::dispatch::Runtime`]).
    shadow: Option<Box<ShadowMap>>,
}

impl FlatMemory {
    /// Creates a new zero-initialized [`FlatMemory`] of `size` bytes,
    /// with the sanitizer disabled (see [`Self::enable_sanitizer`]).
    pub fn new(size: usize) -> Self {
        Self {
            bytes: vec![0u8; size],
            shadow: None,
        }
    }

    /// Returns a read-only view of the backing bytes.
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns a mutable view of the backing bytes.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.bytes
    }

    /// Installs a fresh [`ShadowMap`] sized to this memory's length, so
    /// every subsequent access is checked against it -- see this
    /// module's doc and the CLI's `--sanitize` flag.
    ///
    /// It's harmless to call this before or after
    /// [`crate::loader::load`] populates guest memory: the shadow map's
    /// default state is [`crate::sanitize::ShadowState::Valid`] (see
    /// that type's doc for why), so the program image being written
    /// through [`AddressSpace`] either before or after this call is
    /// checked against `Valid` bytes either way and never flags a
    /// violation. Enabling it right after construction (as the CLI
    /// does) is simply the more obvious place to put the call, not a
    /// correctness requirement.
    ///
    /// Also see [`crate::backend`]'s `AddressBus::fast_mem` impl: once
    /// this is called, `fast_mem` starts returning `None`, which is
    /// what forces the m68k crate's trace JIT off the raw-pointer fast
    /// path that would otherwise bypass every check installed here.
    pub fn enable_sanitizer(&mut self) {
        self.shadow = Some(Box::new(ShadowMap::new(self.bytes.len())));
    }

    /// The installed [`ShadowMap`], if [`Self::enable_sanitizer`] was
    /// called.
    pub fn shadow(&self) -> Option<&ShadowMap> {
        self.shadow.as_deref()
    }

    /// Mutable access to the installed [`ShadowMap`], if any -- for
    /// callers that mark ranges valid/uninit/unaddressable (heap
    /// allocators, stack-pointer tracking, ...) or that publish the
    /// current PC (the CPU run loop, see [`ShadowMap::set_current_pc`]'s
    /// doc).
    pub fn shadow_mut(&mut self) -> Option<&mut ShadowMap> {
        self.shadow.as_deref_mut()
    }
}

impl AddressSpace for FlatMemory {
    fn len(&self) -> usize {
        self.bytes.len()
    }

    fn read_u8(&self, addr: u32) -> u8 {
        if let Some(shadow) = &self.shadow {
            shadow.check_read(addr, 1);
        }
        self.bytes.get(addr as usize).copied().unwrap_or(0)
    }

    fn write_u8(&mut self, addr: u32, value: u8) {
        if let Some(shadow) = &mut self.shadow {
            shadow.check_write(addr, 1);
        }
        if let Some(slot) = self.bytes.get_mut(addr as usize) {
            *slot = value;
        }
    }

    // Overridden (rather than relying on the default trait methods'
    // byte-by-byte composition) so the common in-range case is a single
    // bounds check plus a native big-endian load/store, instead of two or
    // four separate bounds-checked `read_u8`/`write_u8` calls. This is a
    // hot path: every instruction fetch and most operand reads/writes go
    // through here.
    //
    // Each override below still checks/updates the sanitizer shadow map
    // (when installed) for *every* byte of the access, not just the
    // first -- a 2- or 4-byte access whose last byte lands in a
    // redzone is just as much a violation as one whose first byte
    // does, and delegating to `read_u8`/`write_u8` here (which would
    // get that for free) is exactly the fast path these overrides
    // exist to avoid. The shadow check itself is one `Option` check
    // per *access* (not per byte) before the byte-granular loop inside
    // `ShadowMap::check_read`/`check_write` -- see `memory.rs`'s module
    // doc.

    fn read_u16(&self, addr: u32) -> u16 {
        if let Some(shadow) = &self.shadow {
            shadow.check_read(addr, 2);
        }
        let addr = addr as usize;
        match self.bytes.get(addr..addr + 2) {
            Some(slice) => u16::from_be_bytes(slice.try_into().unwrap()),
            None => {
                let hi = self.bytes.get(addr).copied().unwrap_or(0) as u16;
                let lo = self.bytes.get(addr + 1).copied().unwrap_or(0) as u16;
                (hi << 8) | lo
            }
        }
    }

    fn write_u16(&mut self, addr: u32, value: u16) {
        if let Some(shadow) = &mut self.shadow {
            shadow.check_write(addr, 2);
        }
        let addr = addr as usize;
        match self.bytes.get_mut(addr..addr + 2) {
            Some(slice) => slice.copy_from_slice(&value.to_be_bytes()),
            None => {
                if let Some(slot) = self.bytes.get_mut(addr) {
                    *slot = (value >> 8) as u8;
                }
                if let Some(slot) = self.bytes.get_mut(addr + 1) {
                    *slot = value as u8;
                }
            }
        }
    }

    fn read_u32(&self, addr: u32) -> u32 {
        if let Some(shadow) = &self.shadow {
            shadow.check_read(addr, 4);
        }
        let addr = addr as usize;
        match self.bytes.get(addr..addr + 4) {
            Some(slice) => u32::from_be_bytes(slice.try_into().unwrap()),
            None => {
                let mut value = 0u32;
                for i in 0..4 {
                    let byte = self.bytes.get(addr + i).copied().unwrap_or(0);
                    value = (value << 8) | u32::from(byte);
                }
                value
            }
        }
    }

    fn write_u32(&mut self, addr: u32, value: u32) {
        if let Some(shadow) = &mut self.shadow {
            shadow.check_write(addr, 4);
        }
        let addr = addr as usize;
        match self.bytes.get_mut(addr..addr + 4) {
            Some(slice) => slice.copy_from_slice(&value.to_be_bytes()),
            None => {
                for (i, byte) in value.to_be_bytes().into_iter().enumerate() {
                    if let Some(slot) = self.bytes.get_mut(addr + i) {
                        *slot = byte;
                    }
                }
            }
        }
    }

    // These two forward to the inherent methods of the same name above.
    // Both spellings exist deliberately: the inherent ones keep working
    // for code holding a concrete `FlatMemory` without importing the
    // trait, while these make the shadow map reachable from the handler
    // code that is generic over `C::Memory: AddressSpace` (see the trait
    // methods' own docs). Inherent methods win name resolution on a
    // concrete `FlatMemory`, so the two can never disagree.

    fn shadow(&self) -> Option<&ShadowMap> {
        self.shadow.as_deref()
    }

    fn shadow_mut(&mut self) -> Option<&mut ShadowMap> {
        self.shadow.as_deref_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sanitize::PoisonReason;

    #[test]
    fn without_enable_sanitizer_shadow_is_none_and_fast_mem_stays_available() {
        let mem = FlatMemory::new(16);
        assert!(mem.shadow().is_none());
    }

    #[test]
    fn enable_sanitizer_installs_a_shadow_map_sized_to_memory_len() {
        let mut mem = FlatMemory::new(16);
        mem.enable_sanitizer();
        assert_eq!(mem.shadow().unwrap().len(), 16);
    }

    #[test]
    fn one_byte_redzone_read_through_flatmemory_is_a_violation() {
        let mut mem = FlatMemory::new(16);
        mem.enable_sanitizer();
        mem.shadow_mut()
            .unwrap()
            .mark_unaddressable(4, 1, PoisonReason::Redzone);

        let _ = mem.read_u8(4);

        assert_eq!(mem.shadow().unwrap().violation_count(), 1);
    }

    #[test]
    fn four_byte_write_straddling_into_a_redzone_is_a_violation_through_flatmemory() {
        let mut mem = FlatMemory::new(16);
        mem.enable_sanitizer();
        // Bytes 6,7 valid; 8,9 poisoned. A 4-byte write at addr 6 covers
        // both -- must be caught even though the access started clean.
        mem.shadow_mut()
            .unwrap()
            .mark_unaddressable(8, 2, PoisonReason::Redzone);

        mem.write_u32(6, 0xAABB_CCDD);

        assert_eq!(mem.shadow().unwrap().violation_count(), 2);
        // The write still proceeds (detector, not enforcer) -- see
        // crate::sanitize's module doc.
        assert_eq!(mem.read_u8(6), 0xAA);
        assert_eq!(mem.read_u8(7), 0xBB);
    }

    #[test]
    fn write_promotes_uninit_to_valid_through_flatmemory() {
        let mut mem = FlatMemory::new(16);
        mem.enable_sanitizer();
        mem.shadow_mut().unwrap().mark_uninit(0, 4);

        mem.write_u16(0, 0x1234);

        assert_eq!(
            mem.shadow().unwrap().state(0),
            crate::sanitize::ShadowState::Valid
        );
        assert_eq!(
            mem.shadow().unwrap().state(1),
            crate::sanitize::ShadowState::Valid
        );
        // Bytes 2,3 weren't part of the write, still Uninit.
        assert_eq!(
            mem.shadow().unwrap().state(2),
            crate::sanitize::ShadowState::Uninit
        );
    }

    #[test]
    fn u16_roundtrip_is_big_endian() {
        let mut mem = FlatMemory::new(16);
        mem.write_u16(0, 0x1234);
        // Big-endian: high byte first.
        assert_eq!(mem.read_u8(0), 0x12);
        assert_eq!(mem.read_u8(1), 0x34);
        assert_eq!(mem.read_u16(0), 0x1234);
    }

    #[test]
    fn u32_roundtrip_is_big_endian() {
        let mut mem = FlatMemory::new(16);
        mem.write_u32(4, 0xDEAD_BEEF);
        assert_eq!(mem.read_u8(4), 0xDE);
        assert_eq!(mem.read_u8(5), 0xAD);
        assert_eq!(mem.read_u8(6), 0xBE);
        assert_eq!(mem.read_u8(7), 0xEF);
        assert_eq!(mem.read_u32(4), 0xDEAD_BEEF);
    }

    #[test]
    fn out_of_range_read_returns_zero() {
        let mem = FlatMemory::new(4);
        assert_eq!(mem.read_u8(100), 0);
        assert_eq!(mem.read_u16(100), 0);
        assert_eq!(mem.read_u32(100), 0);
    }

    #[test]
    fn out_of_range_write_is_ignored() {
        let mut mem = FlatMemory::new(4);
        mem.write_u8(100, 0xFF);
        mem.write_u32(1000, 0xFFFF_FFFF);
        assert_eq!(mem.as_slice(), &[0, 0, 0, 0]);
    }

    #[test]
    fn straddling_out_of_range_write_partially_applies() {
        // A multi-byte access that starts in range but crosses the end
        // writes the in-range bytes and drops the rest.
        let mut mem = FlatMemory::new(4);
        mem.write_u32(2, 0xAABB_CCDD);
        assert_eq!(mem.read_u8(2), 0xAA);
        assert_eq!(mem.read_u8(3), 0xBB);
        // Bytes 4 and 5 don't exist; reading them back gives 0.
        assert_eq!(mem.read_u8(4), 0);
    }
}
