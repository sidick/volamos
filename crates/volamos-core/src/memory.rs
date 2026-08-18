//! A minimal, flat guest address space.
//!
//! The m68k guest CPU is big-endian, so all multi-byte accesses here use
//! big-endian byte order regardless of the host's native endianness.

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
}

impl FlatMemory {
    /// Creates a new zero-initialized [`FlatMemory`] of `size` bytes.
    pub fn new(size: usize) -> Self {
        Self {
            bytes: vec![0u8; size],
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
}

impl AddressSpace for FlatMemory {
    fn len(&self) -> usize {
        self.bytes.len()
    }

    fn read_u8(&self, addr: u32) -> u8 {
        self.bytes.get(addr as usize).copied().unwrap_or(0)
    }

    fn write_u8(&mut self, addr: u32, value: u8) {
        if let Some(slot) = self.bytes.get_mut(addr as usize) {
            *slot = value;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
