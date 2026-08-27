//! The [`m68k`] crate backend: a concrete [`Cpu`] implementation.
//!
//! `volamos` doesn't implement its own m68k interpreter; this module wires
//! the third-party [`m68k`](https://docs.rs/m68k) crate's `CpuCore` behind
//! the [`Cpu`] trait defined in [`crate::cpu`].
//!
//! # Choice of backend
//!
//! The `m68k` crate (not to be confused with any similarly-named crate) is
//! a safe, embeddable M68000-family interpreter with:
//!
//! - an [`AddressBus`] trait for host-provided memory/devices, matching
//!   the shape of our own [`AddressSpace`] closely enough that
//!   [`FlatMemory`] can implement both;
//! - a `CpuCore::step` that surfaces A-line traps, F-line traps, `TRAP
//!   #n`, `BKPT #n`, and illegal instructions as distinct
//!   [`m68k::StepResult`] variants *without* taking the corresponding
//!   hardware exception, which is exactly the hook AmigaOS library-call
//!   dispatch (traditionally implemented via A-line opcodes in a jump
//!   table) needs.
//!
//! # Trait fit
//!
//! [`crate::cpu::Cpu::Memory`] is a single associated type, so the guest
//! memory implementation needs to satisfy both our [`AddressSpace`] trait
//! and `m68k`'s [`AddressBus`] trait. Rather than introduce a wrapper
//! type, [`AddressBus`] is implemented directly for [`FlatMemory`] in this
//! module (in terms of the [`AddressSpace`] methods it already has), so
//! `M68kCpu::Memory = FlatMemory`.
//!
//! No changes were needed to the `Cpu` trait's method signatures; only
//! `StopReason` gained a payload (see [`crate::cpu::TrapInfo`]) so callers
//! can tell which trap fired and where.

use m68k::{AddressBus, CpuCore, StepResult};

use crate::cpu::{AddressRegister, Cpu, DataRegister, StopReason, TrapInfo, TrapKind};
use crate::memory::{AddressSpace, FlatMemory};

/// Re-exported so callers (the CLI's `--cpu` flag) can name a model
/// without depending on the `m68k` crate directly -- see
/// [`M68kCpu::with_config`].
pub use m68k::CpuType;

/// Start of the low guest-memory region reserved for a fake AmigaOS
/// library jump table.
///
/// A later stage populates this region with trap-triggering entries (one
/// A-line opcode per library vector, conventionally at negative offsets
/// from a library base pointer) so that guest `JSR`/`JMP` instructions
/// through a library base land here and surface as [`StopReason::Trap`].
/// Guest code and data must be loaded above [`TRAP_TABLE_END`].
pub const TRAP_TABLE_BASE: u32 = 0x0000;

/// Size in bytes of the reserved trap table region.
///
/// 6 KiB is far more than any single classic AmigaOS library's jump
/// table needs (even `exec.library`'s is under 1 KiB), leaving headroom
/// for several libraries' worth of fake vectors before guest code/data
/// must start -- plus, above [`crate::dispatch::EXEC_LIBRARY_BASE`],
/// enough positive-offset room for a real (if partial) `struct ExecBase`
/// including its `LibList` at the real NDK-documented offset (378), see
/// [`crate::dispatch::EXEC_BASE_LIBLIST_OFFSET`]'s docs. Grown from the
/// original `0x1000` specifically to fit that, then again from `0x1200`
/// to fit three more real library bases (the standard Workbench math
/// libraries -- see `crate::mathlibs`'s module docs) each needing their
/// own negative-offset jump table plus positive-offset `struct Library`
/// header room, then once more from `0x1800` for `timer.device`'s real
/// device base (see `crate::dispatch::TIMER_DEVICE_BASE`), then once
/// more from `0x1A00` for `mathffp.library`'s real base (see
/// `crate::dispatch::MATHFFP_LIBRARY_BASE`), then once more from
/// `0x1C00` for `locale.library`'s real base (see
/// `crate::dispatch::LOCALE_LIBRARY_BASE`), then once more (this time
/// a *double*-size, `0x400` chunk -- see
/// `crate::dispatch::INTUITION_LIBRARY_BASE`'s doc for why) from
/// `0x1E00` for `intuition.library`’s real base, then once more from
/// `0x2200` for `bsdsocket.library`’s real base (see
/// `crate::dispatch::BSDSOCKET_LIBRARY_BASE`), then once more from
/// `0x2400` for `graphics.library`’s real base (see
/// `crate::dispatch::GRAPHICS_LIBRARY_BASE`) -- same reasoning each
/// time.
pub const TRAP_TABLE_SIZE: u32 = 0x2600;

/// First guest address *after* the reserved trap table region
/// (exclusive). Guest code, data, and stack should live at or above this
/// address.
pub const TRAP_TABLE_END: u32 = TRAP_TABLE_BASE + TRAP_TABLE_SIZE;

impl AddressBus for FlatMemory {
    fn read_byte(&mut self, address: u32) -> u8 {
        AddressSpace::read_u8(self, address)
    }

    fn read_word(&mut self, address: u32) -> u16 {
        AddressSpace::read_u16(self, address)
    }

    fn read_long(&mut self, address: u32) -> u32 {
        AddressSpace::read_u32(self, address)
    }

    fn write_byte(&mut self, address: u32, value: u8) {
        AddressSpace::write_u8(self, address, value);
    }

    fn write_word(&mut self, address: u32, value: u16) {
        AddressSpace::write_u16(self, address, value);
    }

    fn write_long(&mut self, address: u32, value: u32) {
        AddressSpace::write_u32(self, address, value);
    }

    /// The whole guest address space is one plain, side-effect-free
    /// `Vec<u8>` (see [`FlatMemory`]'s doc comment) -- exactly what the
    /// `jit` feature's [`m68k::CpuCore::run_batch`] fast path needs, and
    /// harmless to expose unconditionally since `step`/`execute` never
    /// call this hook regardless of feature flags (see
    /// [`m68k::AddressBus::fast_mem`]'s doc comment).
    fn fast_mem(&mut self) -> Option<m68k::FastMem> {
        let len = AddressSpace::len(self) as u32;
        Some(m68k::FastMem {
            ptr: self.as_mut_slice().as_mut_ptr(),
            base: 0,
            len,
        })
    }
}

/// A [`Cpu`] implementation backed by the `m68k` crate's `CpuCore`.
///
/// [`M68kCpu::new`] emulates a plain M68000 with no FPU (`fpu_present =
/// false`) -- the lowest common denominator every real Kickstart 3.1
/// machine shares, and AmigaOS CLI binaries (this project's target)
/// generally don't need anything past a 68000-level instruction set.
/// [`M68kCpu::with_config`] picks a different [`CpuType`]/FPU presence
/// for the rare binary that does (the CLI's `--cpu`/`--fpu` flags).
pub struct M68kCpu {
    core: CpuCore,
    /// Whether [`Cpu::run`] should batch-execute via
    /// [`m68k::CpuCore::run_batch`] (the crate's trace JIT) instead of
    /// stepping one instruction at a time. Defaults to `false` -- the
    /// plain interpreter remains this runtime's correctness reference
    /// (see the CLI's `--jit`/`--no-jit` flags); set with
    /// [`Self::set_jit`].
    jit: bool,
}

impl M68kCpu {
    /// Creates a new M68000 core with no FPU -- shorthand for
    /// [`Self::with_config`]`(CpuType::M68000, false)`. See this
    /// struct's doc comment for why that's the default.
    pub fn new() -> Self {
        Self::with_config(CpuType::M68000, false)
    }

    /// Enables or disables the batch-execution (`run_batch`/trace JIT)
    /// path for [`Cpu::run`] -- see [`Self::jit`]'s field doc and the
    /// CLI's `--jit`/`--no-jit` flags. Off by default.
    pub fn set_jit(&mut self, jit: bool) {
        self.jit = jit;
    }

    /// Creates a new core for `cpu_type`, with `fpu_present` controlling
    /// whether F-line (coprocessor ID 1) opcodes execute as real FPU
    /// instructions or trap out to [`Cpu::take_hardware_exception`] --
    /// see that method's doc comment for the guest-visible difference
    /// (a real, well-behaved AmigaOS program probes for an FPU exactly
    /// this way, expecting the trap when one isn't fitted).
    ///
    /// `fpu_present` only matters for `cpu_type` `M68020` and later: the
    /// `m68k` crate models pre-68020 CPUs as having no coprocessor
    /// interface at all (matching real 68000/68010 hardware), so F-line
    /// always traps on those regardless of this flag.
    ///
    /// Registers and the program counter start at `0`; callers are
    /// expected to set up the initial PC and stack pointer (typically via
    /// [`Cpu::set_pc`] and [`Cpu::set_address_register`] on A7) before
    /// running guest code; see this module's docs for the reserved
    /// low-memory trap table region guest code must be loaded above.
    ///
    /// This deliberately does *not* perform the m68k hardware reset
    /// sequence (which reads the initial SSP/PC from guest addresses 0
    /// and 4): those addresses are reserved for the fake library jump
    /// table, not a real reset vector.
    pub fn with_config(cpu_type: CpuType, fpu_present: bool) -> Self {
        let mut core = CpuCore::new();
        core.set_cpu_type(cpu_type);
        core.fpu_present = fpu_present;
        core.reset_soft();
        Self { core, jit: false }
    }
}

impl Default for M68kCpu {
    fn default() -> Self {
        Self::new()
    }
}

impl Cpu for M68kCpu {
    type Memory = FlatMemory;

    fn step(&mut self, mem: &mut Self::Memory) -> StopReason {
        match self.core.step(mem) {
            StepResult::Ok { .. } => StopReason::Step,
            StepResult::Stopped => StopReason::Halted,
            StepResult::AlineTrap { opcode } => StopReason::Trap(TrapInfo {
                kind: TrapKind::ALine { opcode },
                pc: self.core.ppc,
            }),
            StepResult::FlineTrap { opcode } => StopReason::Trap(TrapInfo {
                kind: TrapKind::FLine { opcode },
                pc: self.core.ppc,
            }),
            StepResult::TrapInstruction { trap_num } => StopReason::Trap(TrapInfo {
                kind: TrapKind::Trap { trap_num },
                pc: self.core.ppc,
            }),
            StepResult::Breakpoint { bp_num } => StopReason::Trap(TrapInfo {
                kind: TrapKind::Breakpoint { bp_num },
                pc: self.core.ppc,
            }),
            StepResult::IllegalInstruction { opcode } => StopReason::Trap(TrapInfo {
                kind: TrapKind::Illegal { opcode },
                pc: self.core.ppc,
            }),
        }
    }

    /// When [`Self::jit`] is set, runs a whole batch of instructions via
    /// [`m68k::CpuCore::run_batch`] instead of stepping one at a time --
    /// the crate's trace JIT compiles hot backward-branch loops under
    /// the hood, but every trap/halt this runtime cares about is still
    /// surfaced at exactly the same boundary [`Cpu::step`]'s default
    /// `run` loop would stop at (see [`m68k::BatchExit`]'s doc comment:
    /// traps are reported, never taken as hardware exceptions, matching
    /// [`StepResult`] one-for-one). `max_instructions` is unbounded
    /// (`u32::MAX`) since this runtime has no use for budget-based
    /// preemption; a `BudgetExhausted` exit (astronomically unlikely in
    /// practice, since AmigaOS guest code traps out to library calls
    /// constantly) just resumes the batch loop rather than returning
    /// early. When unset, falls back to the plain step loop (the same
    /// logic as [`Cpu::run`]'s own default implementation, duplicated
    /// here since overriding `run` at all requires handling both
    /// branches in one method).
    fn run(&mut self, mem: &mut Self::Memory) -> StopReason {
        use m68k::BatchExit;

        if !self.jit {
            loop {
                let pc = self.pc();
                if pc as usize >= AddressSpace::len(mem) {
                    return StopReason::PcOutOfBounds { pc };
                }
                match self.step(mem) {
                    StopReason::Step => continue,
                    other => return other,
                }
            }
        }

        loop {
            let pc = self.pc();
            if pc as usize >= AddressSpace::len(mem) {
                return StopReason::PcOutOfBounds { pc };
            }
            let result = self.core.run_batch(mem, u32::MAX, &[]);
            match result.exit {
                BatchExit::BudgetExhausted => continue,
                BatchExit::Stopped => return StopReason::Halted,
                BatchExit::WatchedPc { .. } => {
                    unreachable!("no watch_pcs are ever passed to run_batch")
                }
                BatchExit::AlineTrap { opcode } => {
                    return StopReason::Trap(TrapInfo {
                        kind: TrapKind::ALine { opcode },
                        pc: self.core.ppc,
                    });
                }
                BatchExit::FlineTrap { opcode } => {
                    return StopReason::Trap(TrapInfo {
                        kind: TrapKind::FLine { opcode },
                        pc: self.core.ppc,
                    });
                }
                BatchExit::TrapInstruction { trap_num } => {
                    return StopReason::Trap(TrapInfo {
                        kind: TrapKind::Trap { trap_num },
                        pc: self.core.ppc,
                    });
                }
                BatchExit::Breakpoint { bp_num } => {
                    return StopReason::Trap(TrapInfo {
                        kind: TrapKind::Breakpoint { bp_num },
                        pc: self.core.ppc,
                    });
                }
                BatchExit::IllegalInstruction { opcode } => {
                    return StopReason::Trap(TrapInfo {
                        kind: TrapKind::Illegal { opcode },
                        pc: self.core.ppc,
                    });
                }
            }
        }
    }

    fn data_register(&self, reg: DataRegister) -> u32 {
        self.core.d(reg.0 as usize)
    }

    fn set_data_register(&mut self, reg: DataRegister, value: u32) {
        self.core.set_d(reg.0 as usize, value);
    }

    fn address_register(&self, reg: AddressRegister) -> u32 {
        self.core.a(reg.0 as usize)
    }

    fn set_address_register(&mut self, reg: AddressRegister, value: u32) {
        self.core.set_a(reg.0 as usize, value);
    }

    fn pc(&self) -> u32 {
        self.core.pc
    }

    fn set_pc(&mut self, value: u32) {
        self.core.pc = value;
        // The prefetch queue may hold words fetched relative to the old
        // PC; drop them so the next `step` refetches from the new PC.
        self.core.invalidate_prefetch();
    }

    fn sr(&self) -> u16 {
        self.core.get_sr()
    }

    fn set_sr(&mut self, value: u16) {
        self.core.set_sr(value);
    }

    fn take_hardware_exception(&mut self, mem: &mut Self::Memory, kind: TrapKind) -> bool {
        use m68k::core::exceptions::vector;

        let vec_num = match kind {
            TrapKind::ALine { .. } => {
                debug_assert!(
                    false,
                    "ALine traps are never routed through take_hardware_exception"
                );
                return false;
            }
            TrapKind::FLine { .. } => vector::LINE_1111,
            TrapKind::Illegal { .. } | TrapKind::Breakpoint { .. } => vector::ILLEGAL_INSTRUCTION,
            TrapKind::Trap { trap_num } => vector::TRAP_BASE + u32::from(trap_num),
        };

        // A `0` entry means the guest never installed a handler for this
        // vector; jumping there would just run off into whatever
        // (probably zeroed) memory sits at address 0, so decline instead
        // -- see this method's doc comment on `crate::cpu::Cpu`.
        let handler = AddressSpace::read_u32(mem, vec_num * 4);
        if handler == 0 {
            return false;
        }

        match kind {
            TrapKind::FLine { .. } => {
                self.core.take_fline_exception(mem);
            }
            TrapKind::Illegal { .. } => {
                self.core.take_illegal_exception(mem);
            }
            TrapKind::Breakpoint { .. } => {
                self.core.take_bkpt_exception(mem);
            }
            TrapKind::Trap { trap_num } => {
                self.core.take_trap_exception(mem, trap_num);
            }
            TrapKind::ALine { .. } => unreachable!("handled above"),
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Loads `words` (big-endian opcode/operand words) into `mem` starting
    /// at `addr`.
    fn load_words(mem: &mut FlatMemory, addr: u32, words: &[u16]) {
        let mut offset = addr;
        for &w in words {
            mem.write_u16(offset, w);
            offset += 2;
        }
    }

    /// A fresh CPU with its PC set just past the reserved trap table, and
    /// A7 set to the top of memory.
    fn new_cpu_with_memory(size: usize) -> (M68kCpu, FlatMemory) {
        let mem = FlatMemory::new(size);
        let mut cpu = M68kCpu::new();
        cpu.set_pc(TRAP_TABLE_END);
        cpu.set_address_register(AddressRegister(7), size as u32);
        (cpu, mem)
    }

    #[test]
    fn moveq_sets_data_register_and_advances_pc() {
        let (mut cpu, mut mem) = new_cpu_with_memory(0x3000);
        let start = cpu.pc();
        // 0x7005: MOVEQ.L #5, D0
        load_words(&mut mem, start, &[0x7005]);

        let reason = cpu.step(&mut mem);

        assert_eq!(reason, StopReason::Step);
        assert_eq!(cpu.data_register(DataRegister(0)), 5);
        assert_eq!(cpu.pc(), start + 2);
    }

    #[test]
    fn nop_advances_pc_without_changing_registers() {
        let (mut cpu, mut mem) = new_cpu_with_memory(0x3000);
        let start = cpu.pc();
        // 0x4E71: NOP
        load_words(&mut mem, start, &[0x4E71]);
        cpu.set_data_register(DataRegister(1), 0xABCD_1234);

        let reason = cpu.step(&mut mem);

        assert_eq!(reason, StopReason::Step);
        assert_eq!(cpu.data_register(DataRegister(1)), 0xABCD_1234);
        assert_eq!(cpu.pc(), start + 2);
    }

    #[test]
    fn trap_instruction_surfaces_as_stop_reason_trap() {
        let (mut cpu, mut mem) = new_cpu_with_memory(0x3000);
        let start = cpu.pc();
        // 0x4E40: TRAP #0
        load_words(&mut mem, start, &[0x4E40]);

        let reason = cpu.step(&mut mem);

        assert_eq!(
            reason,
            StopReason::Trap(TrapInfo {
                kind: TrapKind::Trap { trap_num: 0 },
                pc: start,
            })
        );
        // The m68k core still advances PC past the trapping word even
        // though the trap is intercepted rather than taken as a hardware
        // exception.
        assert_eq!(cpu.pc(), start + 2);
    }

    #[test]
    fn with_config_pre_68020_traps_fline_regardless_of_fpu_present() {
        // A real coprocessor-ID-1 F-line opcode (the generic FPU
        // instruction word format); pre-68020 CPUs have no coprocessor
        // interface at all, so this always traps -- see
        // M68kCpu::with_config's doc comment.
        let mut cpu = M68kCpu::with_config(CpuType::M68000, true);
        cpu.set_pc(TRAP_TABLE_END);
        let mut mem = FlatMemory::new(0x3000);
        load_words(&mut mem, TRAP_TABLE_END, &[0xF200, 0x0000]);

        let reason = cpu.step(&mut mem);

        assert!(
            matches!(
                reason,
                StopReason::Trap(TrapInfo {
                    kind: TrapKind::FLine { .. },
                    ..
                })
            ),
            "expected an FLine trap, got {reason:?}"
        );
    }

    #[test]
    fn with_config_68020_with_no_fpu_traps_fline() {
        let mut cpu = M68kCpu::with_config(CpuType::M68020, false);
        cpu.set_pc(TRAP_TABLE_END);
        let mut mem = FlatMemory::new(0x3000);
        load_words(&mut mem, TRAP_TABLE_END, &[0xF200, 0x0000]);

        let reason = cpu.step(&mut mem);

        assert!(
            matches!(
                reason,
                StopReason::Trap(TrapInfo {
                    kind: TrapKind::FLine { .. },
                    ..
                })
            ),
            "expected an FLine trap (no FPU fitted), got {reason:?}"
        );
    }

    #[test]
    fn with_config_68020_with_fpu_does_not_trap_fline() {
        let mut cpu = M68kCpu::with_config(CpuType::M68020, true);
        cpu.set_pc(TRAP_TABLE_END);
        let mut mem = FlatMemory::new(0x3000);
        load_words(&mut mem, TRAP_TABLE_END, &[0xF200, 0x0000]);

        let reason = cpu.step(&mut mem);

        assert!(
            !matches!(
                reason,
                StopReason::Trap(TrapInfo {
                    kind: TrapKind::FLine { .. },
                    ..
                })
            ),
            "a fitted FPU should decode this as a real instruction, not trap: {reason:?}"
        );
    }

    #[test]
    fn aline_opcode_surfaces_as_stop_reason_trap_with_pc_of_trapping_instruction() {
        let (mut cpu, mut mem) = new_cpu_with_memory(0x3000);
        // 0x7007: MOVEQ.L #7, D0 (step 1, just to move PC off the reset value)
        // 0xA000: A-line trap opcode (library jump-table style vector)
        let start = cpu.pc();
        load_words(&mut mem, start, &[0x7007, 0xA000]);

        let first = cpu.step(&mut mem);
        assert_eq!(first, StopReason::Step);
        let aline_pc = cpu.pc();

        let reason = cpu.step(&mut mem);

        assert_eq!(
            reason,
            StopReason::Trap(TrapInfo {
                kind: TrapKind::ALine { opcode: 0xA000 },
                pc: aline_pc,
            })
        );
        assert_eq!(cpu.pc(), aline_pc + 2);
    }

    #[test]
    fn address_register_roundtrip() {
        let (mut cpu, _mem) = new_cpu_with_memory(0x3000);
        cpu.set_address_register(AddressRegister(3), 0xDEAD_BEEF);
        assert_eq!(cpu.address_register(AddressRegister(3)), 0xDEAD_BEEF);
    }

    #[test]
    fn status_register_roundtrip() {
        let (mut cpu, _mem) = new_cpu_with_memory(0x3000);
        cpu.set_sr(0x2700);
        assert_eq!(cpu.sr(), 0x2700);
    }

    #[test]
    fn run_reports_pc_out_of_bounds_instead_of_silently_reading_zeros_forever() {
        // A JSR/JMP through a bad address register (e.g. a guest bug --
        // found via the real PhxAss assembler jumping through an
        // uninitialized/garbage value) can send PC to a wildly
        // out-of-range address. Without an eager bounds check,
        // AddressSpace's "out-of-range reads are 0" convention means the
        // CPU would decode an endless stream of zero-word instructions,
        // walk forward (with u32 wraparound) potentially forever, and
        // only stop if it happened to wrap back around onto something
        // that traps -- reporting a misleading address far from the real
        // bug. `Cpu::run` must catch this immediately instead.
        let (mut cpu, mut mem) = new_cpu_with_memory(0x3000);
        cpu.set_pc(0xFFFF_FFD1);

        let reason = cpu.run(&mut mem);

        assert_eq!(reason, StopReason::PcOutOfBounds { pc: 0xFFFF_FFD1 });
    }

    #[test]
    fn jit_mode_also_reports_pc_out_of_bounds() {
        let (mut cpu, mut mem) = new_cpu_with_memory(0x3000);
        cpu.set_jit(true);
        cpu.set_pc(0xFFFF_FFD1);

        let reason = cpu.run(&mut mem);

        assert_eq!(reason, StopReason::PcOutOfBounds { pc: 0xFFFF_FFD1 });
    }

    #[test]
    fn jit_batch_execution_matches_interpreter_for_a_backward_branch_loop() {
        // A DBRA-based backward-branch loop -- deliberately chosen since
        // it's the specific pattern the trace JIT compiles (see
        // `M68kCpu::run`'s doc comment), so this exercises the actual
        // native-code path rather than just trap/budget plumbing. Both
        // modes must agree exactly: the interpreter is this runtime's
        // correctness reference (see the CLI's `--jit`/`--no-jit`
        // flags' doc), so any divergence here would be a real bug.
        let words: &[u16] = &[
            0x7004, // MOVEQ #4, D0
            0x4E71, // [loop] NOP
            0x51C8, 0xFFFC, // DBRA D0, loop (disp = -4)
            0xA000, // A-line trap: stop here
        ];

        let mut interp_mem = FlatMemory::new(0x3000);
        load_words(&mut interp_mem, TRAP_TABLE_END, words);
        let mut interp_cpu = M68kCpu::new();
        interp_cpu.set_pc(TRAP_TABLE_END);
        let interp_reason = interp_cpu.run(&mut interp_mem);

        let mut jit_mem = FlatMemory::new(0x3000);
        load_words(&mut jit_mem, TRAP_TABLE_END, words);
        let mut jit_cpu = M68kCpu::new();
        jit_cpu.set_jit(true);
        jit_cpu.set_pc(TRAP_TABLE_END);
        let jit_reason = jit_cpu.run(&mut jit_mem);

        assert_eq!(interp_reason, jit_reason);
        assert_eq!(
            interp_cpu.data_register(DataRegister(0)),
            jit_cpu.data_register(DataRegister(0)),
        );
        assert_eq!(interp_cpu.pc(), jit_cpu.pc());
    }
}
