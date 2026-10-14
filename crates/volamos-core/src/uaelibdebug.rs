//! UAE's `uaelib` debug-log trap at `0xF0FF60`, mode 86 (`--uae-debug`,
//! issue #140).
//!
//! Guest code reaches it as a plain C function, as declared in amiberry's
//! `src/include/uae/uae_pragmas.h`:
//!
//! ```c
//! static int (*calltrap)(int arg0, ...) = (int (*)(int arg0, ...))0xF0FF60;
//! calltrap(86, "message");
//! ```
//!
//! # Calling convention (from `uaelib_demux2()` in amiberry's `uaelib.cpp`)
//!
//! Plain C varargs, stack only: on entry `(sp)` is the return address and
//! the arguments are the longs at `4(sp)` (the mode, `ARG0`), `8(sp)`
//! (`ARG1`), ... `24(sp)`. The result comes back in `D0`; the caller
//! cleans the stack. UAE plants a trap opcode followed by `RTS` there;
//! here the A-line word is the whole stub, the host performs the return
//! (see the module docs of [`crate::dispatch`]).
//!
//! # Scope
//!
//! Only mode 86 is implemented: if `ARG1` is a valid address, the C
//! string there is written as `DBG: <string>\n` and `D0 = 1`, otherwise
//! `D0 = 0`. Every other mode returns 0 -- what "unsupported" looks like
//! to a probing program -- and, once per distinct mode, a warning goes
//! to the sink so the probe is visible to the user.
//!
//! # Deviations from UAE
//!
//! - The string is capped at [`MAX_DPATH`]` - 1` bytes, like the sprintf
//!   port's.

use crate::cpu::{AddressRegister, Cpu, DataRegister};
use crate::dispatch::{DispatchError, HandlerContext};
use crate::guestmem::GuestHeap;
use crate::memory::AddressSpace;
use crate::uaedebug::{DebugSink, MAX_DPATH, read_string};
use std::collections::HashSet;

/// The `calltrap` address (`rtarea_base + 0xFF60`).
pub const UAELIB_ADDRESS: u32 = 0x00F0_FF60;

/// Bytes planted there: one A-line opcode word.
pub const UAELIB_STUB_SIZE: u32 = 2;

/// The only supported mode: log a string.
pub const MODE_DBG_LOG: u32 = 86;

/// Whether the stub fits in a RAM of `ram_len` bytes.
pub fn fits_in_ram(ram_len: usize) -> bool {
    u64::from(UAELIB_ADDRESS) + u64::from(UAELIB_STUB_SIZE) <= ram_len as u64
}

/// Reserves the stub's bytes out of `heap`. `Ok(false)` if `ram_len`
/// doesn't reach them (nothing to do), `Err` if the program or stack
/// already covers them.
pub fn reserve(heap: &mut GuestHeap, ram_len: usize) -> Result<bool, String> {
    if !fits_in_ram(ram_len) {
        return Ok(false);
    }
    if heap.reserve_range(UAELIB_ADDRESS, UAELIB_STUB_SIZE) {
        Ok(true)
    } else {
        Err(format!(
            "the UAE debug-log trap at {UAELIB_ADDRESS:#x} falls inside the loaded program \
             or the guest stack; use a larger --ram or a smaller --stack"
        ))
    }
}

/// Builds the trap handler writing to `sink`.
pub fn handler<C: Cpu>(
    mut sink: Box<dyn DebugSink>,
) -> impl FnMut(&mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let mut warned: HashSet<u32> = HashSet::new();
    move |ctx| {
        let sp = ctx.cpu.address_register(AddressRegister(7));
        let mode = ctx.mem.read_u32(sp.wrapping_add(4));
        let result = if mode == MODE_DBG_LOG {
            let ptr = ctx.mem.read_u32(sp.wrapping_add(8));
            log_string(&*ctx.mem, ptr, sink.as_mut())
        } else {
            if warned.insert(mode) {
                sink.warning(&format!(
                    "unsupported uaelib mode {mode} called via {UAELIB_ADDRESS:#x} \
                     (only mode {MODE_DBG_LOG} is implemented); returning 0"
                ));
            }
            0
        };
        ctx.cpu.set_data_register(DataRegister(0), result);
        Ok(())
    }
}

fn log_string(mem: &dyn AddressSpace, ptr: u32, sink: &mut dyn DebugSink) -> u32 {
    if u64::from(ptr) >= mem.len() as u64 {
        return 0;
    }
    let mut line = b"DBG: ".to_vec();
    line.extend(read_string(mem, ptr, MAX_DPATH - 1));
    line.push(b'\n');
    sink.text(&line);
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{CpuType, M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{Runtime, StartConfig};
    use crate::memory::FlatMemory;
    use crate::sanitize::PoisonReason;
    use crate::uaedebug::CaptureSink;

    const DEFAULT_RAM: usize = 16 * 1024 * 1024;
    const STR: u32 = 0x1_0000;
    /// Result cells the guest stores `D0` into, one per call.
    const CELLS: u32 = 0x1_1000;

    fn hi(v: u32) -> u16 {
        (v >> 16) as u16
    }
    fn lo(v: u32) -> u16 {
        v as u16
    }
    /// `pea ptr.l ; move.l #mode,-(sp) ; jsr $F0FF60.l ; addq.l #8,sp ;
    /// move.l d0,cell.l` -- `calltrap(mode, ptr)` in C stack order.
    fn call(mode: u32, ptr: u32, cell: u32) -> Vec<u16> {
        vec![
            0x4879,
            hi(ptr),
            lo(ptr),
            0x2F3C,
            hi(mode),
            lo(mode),
            0x4EB9,
            hi(UAELIB_ADDRESS),
            lo(UAELIB_ADDRESS),
            0x508F,
            0x23C0,
            hi(CELLS + cell * 4),
            lo(CELLS + cell * 4),
        ]
    }
    fn finish(calls: Vec<Vec<u16>>) -> Vec<u16> {
        let mut words = calls.concat();
        words.extend([0x7000, 0x4E75]); // moveq #0,d0 ; rts
        words
    }

    fn config() -> StartConfig {
        StartConfig {
            entry: TRAP_TABLE_END,
            load_end: TRAP_TABLE_END + 0x400,
            ..StartConfig::default()
        }
    }

    struct Guest {
        cpu: CpuType,
        enable: bool,
        sanitize: bool,
    }
    impl Guest {
        fn new(cpu: CpuType) -> Self {
            Self {
                cpu,
                enable: true,
                sanitize: false,
            }
        }
        fn run(
            self,
            program: &[u16],
            setup: impl FnOnce(&mut FlatMemory),
        ) -> (CaptureSink, Runtime<M68kCpu>) {
            let cap = CaptureSink::default();
            let mut mem = FlatMemory::new(DEFAULT_RAM);
            if self.sanitize {
                mem.enable_sanitizer();
            }
            for (i, w) in program.iter().enumerate() {
                mem.write_u16(TRAP_TABLE_END + 2 * i as u32, *w);
            }
            for (i, b) in b"hello\0".iter().enumerate() {
                mem.write_u8(STR + i as u32, *b);
            }
            setup(&mut mem);
            let mut rt = Runtime::new(M68kCpu::with_config(self.cpu, false), mem, config());
            if self.enable {
                assert_eq!(rt.enable_uaelib_debug(Box::new(cap.clone())), Ok(true));
            }
            let mut out = Vec::new();
            rt.run(&mut out, None).expect("guest run");
            (cap, rt)
        }
    }

    fn cell(rt: &Runtime<M68kCpu>, n: u32) -> u32 {
        rt.memory().read_u32(CELLS + n * 4)
    }

    fn bare_runtime(ram: usize, config: StartConfig) -> Runtime<M68kCpu> {
        Runtime::new(
            M68kCpu::with_config(CpuType::M68020, false),
            FlatMemory::new(ram),
            config,
        )
    }

    #[test]
    fn mode_86_logs_the_string_and_returns_1_on_either_cpu_family() {
        for cpu in [CpuType::M68000, CpuType::M68020] {
            let (cap, rt) = Guest::new(cpu).run(&finish(vec![call(86, STR, 0)]), |_| {});
            assert_eq!(cap.text(), "DBG: hello\n", "{cpu:?}");
            assert_eq!(cell(&rt, 0), 1);
            assert!(cap.warnings().is_empty());
        }
    }

    #[test]
    fn the_caller_cleans_the_stack_and_returns_to_the_next_instruction() {
        // Two calls in a row only work if SP and the return address were
        // both handled like a real `rts`.
        let prog = finish(vec![call(86, STR, 0), call(86, STR, 1)]);
        let (cap, rt) = Guest::new(CpuType::M68020).run(&prog, |_| {});
        assert_eq!(cap.text(), "DBG: hello\nDBG: hello\n");
        assert_eq!((cell(&rt, 0), cell(&rt, 1)), (1, 1));
    }

    #[test]
    fn bad_pointers_return_0_and_log_nothing() {
        for ptr in [DEFAULT_RAM as u32, 0x7000_0000, 0xFFFF_FFFF] {
            let (cap, rt) =
                Guest::new(CpuType::M68020).run(&finish(vec![call(86, ptr, 0)]), |_| {});
            assert_eq!(cap.text(), "", "{ptr:#x}");
            assert_eq!(cell(&rt, 0), 0, "{ptr:#x}");
        }
    }

    #[test]
    fn a_null_pointer_is_accepted_like_uae() {
        // UAE's `valid_address(ARG1, 1)` accepts address 0 (chip RAM
        // there), so mode 86 logs whatever string sits at 0. Here that is
        // the start of the trap table: not meaningful text, but the call
        // succeeds and the output is still one well-formed `DBG: ...\n`
        // line.
        let (cap, rt) = Guest::new(CpuType::M68020).run(&finish(vec![call(86, 0, 0)]), |_| {});
        assert_eq!(cell(&rt, 0), 1);
        let text = cap.text();
        assert!(
            text.starts_with("DBG: ") && text.ends_with('\n'),
            "{text:?}"
        );
        assert_eq!(text.matches('\n').count(), 1, "{text:?}");
    }

    #[test]
    fn an_overlong_string_is_capped() {
        let (cap, _) = Guest::new(CpuType::M68020).run(&finish(vec![call(86, STR, 0)]), |m| {
            for i in 0..(MAX_DPATH as u32 + 50) {
                m.write_u8(STR + i, b'x');
            }
            m.write_u8(STR + MAX_DPATH as u32 + 50, 0);
        });
        assert_eq!(cap.text().len(), "DBG: ".len() + MAX_DPATH - 1 + 1);
    }

    #[test]
    fn other_modes_return_0_and_warn_once_per_distinct_mode() {
        let prog = finish(vec![
            call(7, STR, 0),
            call(7, STR, 1),
            call(1, STR, 2),
            call(7, STR, 3),
        ]);
        let (cap, rt) = Guest::new(CpuType::M68020).run(&prog, |_| {});
        assert_eq!(cap.text(), "");
        assert_eq!((0..4).map(|n| cell(&rt, n)).collect::<Vec<_>>(), [0; 4]);
        let w = cap.warnings();
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(w[0].contains("mode 7") && w[1].contains("mode 1"), "{w:?}");
    }

    #[test]
    fn flag_off_plants_nothing() {
        let rt = bare_runtime(DEFAULT_RAM, config());
        assert_eq!(rt.memory().read_u16(UAELIB_ADDRESS), 0);
    }

    #[test]
    fn the_heap_never_hands_out_the_stub() {
        let mut rt = bare_runtime(DEFAULT_RAM, config());
        assert_eq!(
            rt.enable_uaelib_debug(Box::new(CaptureSink::default())),
            Ok(true)
        );
        let mut scratch = FlatMemory::new(DEFAULT_RAM);
        let mut seen = 0;
        while let Ok(a) = rt.heap_mut().alloc(&mut scratch, 0x1000) {
            assert!(
                a + 0x1000 <= UAELIB_ADDRESS || a >= UAELIB_ADDRESS + UAELIB_STUB_SIZE,
                "{a:#x} overlaps the stub"
            );
            seen += 1;
        }
        assert!(seen > 3000, "only {seen} allocations");
    }

    #[test]
    fn a_ram_that_stops_short_plants_nothing() {
        let mut rt = bare_runtime(1 << 20, config());
        assert!(!fits_in_ram(1 << 20));
        assert_eq!(
            rt.enable_uaelib_debug(Box::new(CaptureSink::default())),
            Ok(false)
        );
    }

    #[test]
    fn the_ram_boundary_is_exact() {
        assert!(fits_in_ram(UAELIB_ADDRESS as usize + 2));
        assert!(!fits_in_ram(UAELIB_ADDRESS as usize + 1));
    }

    #[test]
    fn a_stack_covering_the_stub_is_an_error() {
        let mut rt = bare_runtime(
            DEFAULT_RAM,
            StartConfig {
                stack_size: 2 * 1024 * 1024,
                ..config()
            },
        );
        let err = rt
            .enable_uaelib_debug(Box::new(CaptureSink::default()))
            .unwrap_err();
        assert!(err.contains("0xf0ff60"), "{err}");
    }

    #[test]
    fn the_stub_fetch_is_not_a_sanitizer_violation_but_a_bad_string_read_is() {
        let mut g = Guest::new(CpuType::M68020);
        g.sanitize = true;
        let (cap, rt) = g.run(&finish(vec![call(86, STR, 0)]), |_| {});
        assert_eq!(cap.text(), "DBG: hello\n");
        assert_eq!(rt.memory().shadow().unwrap().violation_count(), 0);

        let mut g = Guest::new(CpuType::M68020);
        g.sanitize = true;
        let (_, rt) = g.run(&finish(vec![call(86, STR, 0)]), |m| {
            m.shadow_mut()
                .unwrap()
                .mark_unaddressable(STR, 6, PoisonReason::Freed);
        });
        assert!(rt.memory().shadow().unwrap().violation_count() > 0);
    }
}
