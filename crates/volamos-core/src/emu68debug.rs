//! Emu68's debug byte port at `0xDEADBEEF` (`--emu68-debug`, issue #140).
//!
//! Emu68 (the PiStorm JIT, `michalsc/Emu68`) treats a **byte-sized store
//! to the absolute address `0xDEADBEEF`** as "print this character" on
//! its debug console (`src/aarch64/vectors.c`: `if (far == 0xdeadbeef &&
//! size == 1) kprintf("%c", value)`; its own `kprintf` is built on it, see
//! `putByte` in `src/ppc/support.c`). This is that port: the byte is
//! written to the sink exactly as stored -- no newline, no prefix.
//!
//! - Word and long stores to the address are **not** special in Emu68, so
//!   they are not claimed here either: they fall through to ordinary
//!   memory exactly as with the flag off.
//! - Loads are not intercepted.
//! - The address is fixed; Emu68 publishes it in its device tree, which
//!   volamos has none of.
//! - **32-bit CPU models only.** The 68000/68010 cores present the
//!   24-bit-masked address (`0x00ADBEEF`) to the bus, so there the port
//!   never triggers. The CLI warns about that at startup.
//!
//! It is a second [`StorePort`] beside [`crate::uaedebug`]'s; the
//! interception path ([`crate::memory::FlatMemory`]) is shared.

use crate::memory::{AddressSpace, StorePort, StoreSize};
use crate::uaedebug::DebugSink;

/// The port's address.
pub const EMU68_DEBUG_ADDRESS: u32 = 0xDEAD_BEEF;

/// The port: a byte store at [`EMU68_DEBUG_ADDRESS`] emits that byte.
pub struct Emu68Debug {
    sink: Box<dyn DebugSink>,
}

impl Emu68Debug {
    pub fn new(sink: Box<dyn DebugSink>) -> Self {
        Self { sink }
    }
}

impl StorePort for Emu68Debug {
    fn claims(&self, addr: u32) -> bool {
        addr == EMU68_DEBUG_ADDRESS
    }

    fn claims_store(&self, addr: u32, size: StoreSize) -> bool {
        size == StoreSize::Byte && self.claims(addr)
    }

    fn store(&mut self, _mem: &dyn AddressSpace, _addr: u32, value: u32, _size: StoreSize) {
        self.sink.text(&[value as u8]);
    }
}

/// Installs the port on `mem`, writing to `sink`. Pair with
/// [`crate::dispatch::Runtime::reserve_emu68_debug_port`] on the runtime.
pub fn install(mem: &mut crate::memory::FlatMemory, sink: Box<dyn DebugSink>) {
    mem.add_store_port(Box::new(Emu68Debug::new(sink)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{CpuType, M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{Runtime, StartConfig};
    use crate::memory::FlatMemory;
    use crate::uaedebug::{CaptureSink, DEBUG_SPRINTF_ADDRESS};
    use m68k::AddressBus;

    const RAM: usize = 0x2_0000;
    /// A NUL-terminated format string planted for the both-ports test.
    const FMT: u32 = 0x1_0000;

    fn hi(v: u32) -> u16 {
        (v >> 16) as u16
    }
    fn lo(v: u32) -> u16 {
        v as u16
    }
    /// `move.b #imm,abs.l`
    fn mov_b(imm: u8, addr: u32) -> Vec<u16> {
        vec![0x13FC, u16::from(imm), hi(addr), lo(addr)]
    }
    fn finish(mut words: Vec<Vec<u16>>) -> Vec<u16> {
        words.push(vec![0x7000, 0x4E75]); // moveq #0,d0 ; rts
        words.concat()
    }
    fn emit(bytes: &[u8]) -> Vec<u16> {
        finish(
            bytes
                .iter()
                .map(|b| mov_b(*b, EMU68_DEBUG_ADDRESS))
                .collect(),
        )
    }

    struct Guest {
        cpu: CpuType,
        emu68: bool,
        uae: bool,
        sanitize: bool,
    }
    impl Guest {
        fn new(cpu: CpuType) -> Self {
            Self {
                cpu,
                emu68: true,
                uae: false,
                sanitize: false,
            }
        }
        /// Returns (emu68 output, uae output, the runtime after the run).
        fn run(self, program: &[u16]) -> (CaptureSink, CaptureSink, Runtime<M68kCpu>) {
            self.run_with(program, false)
        }
        fn run_with(
            self,
            program: &[u16],
            jit: bool,
        ) -> (CaptureSink, CaptureSink, Runtime<M68kCpu>) {
            let (e, u) = (CaptureSink::default(), CaptureSink::default());
            let mut mem = FlatMemory::new(RAM);
            if self.sanitize {
                mem.enable_sanitizer();
            }
            if self.emu68 {
                install(&mut mem, Box::new(e.clone()));
            }
            if self.uae {
                crate::uaedebug::install(&mut mem, self.cpu, Box::new(u.clone()));
            }
            for (i, b) in b"uae\0".iter().enumerate() {
                mem.write_u8(FMT + i as u32, *b);
            }
            for (i, w) in program.iter().enumerate() {
                mem.write_u16(TRAP_TABLE_END + 2 * i as u32, *w);
            }
            let mut cpu = M68kCpu::with_config(self.cpu, false);
            cpu.set_jit(jit);
            let mut rt = Runtime::new(
                cpu,
                mem,
                StartConfig {
                    entry: TRAP_TABLE_END,
                    load_end: TRAP_TABLE_END + 0x400,
                    ..StartConfig::default()
                },
            );
            rt.reserve_emu68_debug_port().expect("reserve");
            rt.run(&mut Vec::new(), None).expect("guest run");
            (e, u, rt)
        }
    }

    fn port() -> (Emu68Debug, CaptureSink, FlatMemory) {
        let cap = CaptureSink::default();
        (
            Emu68Debug::new(Box::new(cap.clone())),
            cap,
            FlatMemory::new(16),
        )
    }

    #[test]
    fn claims_only_byte_stores_at_exactly_the_address() {
        let (p, _, _) = port();
        let a = EMU68_DEBUG_ADDRESS;
        assert!(p.claims_store(a, StoreSize::Byte));
        assert!(!p.claims_store(a, StoreSize::Word));
        assert!(!p.claims_store(a, StoreSize::Long));
        for near in [a - 1, a + 1, a ^ 0x0100_0000, a & 0x00FF_FFFF] {
            assert!(!p.claims_store(near, StoreSize::Byte), "{near:#x}");
        }
    }

    #[test]
    fn bytes_reach_the_sink_raw_and_in_order() {
        let (mut p, cap, mem) = port();
        let bytes = [b'H', b'i', b'\n', 0x00, 0x80, 0xFF, 0x1B, b'\r'];
        for b in bytes {
            p.store(&mem, EMU68_DEBUG_ADDRESS, u32::from(b), StoreSize::Byte);
        }
        assert_eq!(cap.0.borrow().text, bytes);
        assert!(cap.warnings().is_empty());
    }

    #[test]
    fn only_the_low_byte_of_the_value_is_used() {
        let (mut p, cap, mem) = port();
        p.store(&mem, EMU68_DEBUG_ADDRESS, 0x1234_5641, StoreSize::Byte);
        assert_eq!(cap.0.borrow().text, [0x41]);
    }

    #[test]
    fn guest_prints_hi_on_a_68020() {
        let (e, _, _) = Guest::new(CpuType::M68020).run(&emit(b"Hi\n"));
        assert_eq!(e.0.borrow().text, b"Hi\n");
    }

    #[test]
    fn a_loop_with_the_jit_requested_prints_every_byte() {
        // moveq #2,d2 ; L: move.b #'x',$DEADBEEF ; dbra d2,L
        let mut prog = vec![0x7402];
        let body = mov_b(b'x', EMU68_DEBUG_ADDRESS);
        prog.extend(&body);
        let back = -((body.len() as i32 + 1) * 2) as i16;
        prog.extend([0x51CA, back as u16, 0x7000, 0x4E75]);
        let (e, _, _) = Guest::new(CpuType::M68020).run_with(&prog, true);
        assert_eq!(e.text(), "xxx");
    }

    #[test]
    fn word_and_long_stores_are_not_claimed() {
        let a = EMU68_DEBUG_ADDRESS;
        let prog = finish(vec![
            vec![0x33FC, 0x4141, hi(a), lo(a)],         // move.w #$4141,abs
            vec![0x23FC, 0x4141, 0x4141, hi(a), lo(a)], // move.l #$41414141,abs
            mov_b(b'!', a),
        ]);
        let (e, _, _) = Guest::new(CpuType::M68020).run(&prog);
        assert_eq!(e.text(), "!");
    }

    #[test]
    fn the_24_bit_cpu_models_never_trigger_it() {
        // Their cores present 0x00ADBEEF (see uaedebug's masking test).
        for cpu in [CpuType::M68000, CpuType::M68010] {
            let (e, _, _) = Guest::new(cpu).run(&emit(b"Hi\n"));
            assert_eq!(e.text(), "", "{cpu:?}");
        }
        for cpu in [
            CpuType::M68020,
            CpuType::M68030,
            CpuType::M68040,
            CpuType::M68060,
        ] {
            let (e, _, _) = Guest::new(cpu).run(&emit(b"Hi\n"));
            assert_eq!(e.text(), "Hi\n", "{cpu:?}");
        }
    }

    #[test]
    fn without_the_port_the_store_is_a_silent_out_of_range_write() {
        let mut g = Guest::new(CpuType::M68020);
        g.emu68 = false;
        let (e, _, mut rt) = g.run(&emit(b"Hi\n"));
        assert_eq!(e.text(), "");
        assert_eq!(rt.memory_mut().read_byte(EMU68_DEBUG_ADDRESS), 0);
    }

    #[test]
    fn jit_is_not_used_when_the_port_is_installed() {
        let mut mem = FlatMemory::new(RAM);
        assert!(mem.fast_mem().is_some(), "control: fast path normally on");
        install(&mut mem, Box::new(CaptureSink::default()));
        assert!(mem.fast_mem().is_none());
    }

    #[test]
    fn works_alongside_the_uae_debug_port() {
        let a = DEBUG_SPRINTF_ADDRESS;
        let prog = finish(vec![
            mov_b(b'a', EMU68_DEBUG_ADDRESS),
            // move.l #FMT,$BFFF04 -- UAE: print the format string
            vec![0x23FC, hi(FMT), lo(FMT), hi(a + 4), lo(a + 4)],
            mov_b(b'b', EMU68_DEBUG_ADDRESS),
        ]);
        let mut g = Guest::new(CpuType::M68020);
        g.uae = true;
        let (e, u, _) = g.run(&prog);
        assert_eq!(e.text(), "ab");
        assert_eq!(u.text(), "uae");
    }

    #[test]
    fn port_stores_are_not_sanitizer_violations() {
        let mut g = Guest::new(CpuType::M68020);
        g.sanitize = true;
        let (e, _, rt) = g.run(&emit(b"ok"));
        assert_eq!(e.text(), "ok");
        assert_eq!(rt.memory().shadow().unwrap().violation_count(), 0);
    }

    #[test]
    fn flag_off_under_sanitize_is_unchanged_by_the_port_existing() {
        // Control for the test above: with no port, the out-of-range
        // store is whatever the sanitizer has always made of it. Record
        // it, and check it is the same as for an unrelated out-of-range
        // address (i.e. 0xDEADBEEF has no special status without the flag).
        let violations = |addr: u32| {
            let mut g = Guest::new(CpuType::M68020);
            g.sanitize = true;
            g.emu68 = false;
            let prog = finish(vec![mov_b(b'x', addr)]);
            g.run(&prog).2.memory().shadow().unwrap().violation_count()
        };
        assert_eq!(violations(EMU68_DEBUG_ADDRESS), violations(0xDEAD_BEE0));
    }

    #[test]
    fn the_byte_is_kept_out_of_the_heap_when_ram_could_cover_it() {
        // A real run would need ~3.5 GiB of RAM, so exercise the heap
        // reservation directly on a heap spanning the address.
        use crate::guestmem::GuestHeap;
        let mut heap = GuestHeap::new(0xD000_0000, 0xE000_0000);
        let before = heap.total_free();
        assert!(heap.reserve_range(EMU68_DEBUG_ADDRESS, 1));
        assert_eq!(heap.total_free(), before - 1);
    }
}
