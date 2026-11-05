//! A minimal abstraction over an m68k CPU core.
//!
//! `volamos` does not implement its own m68k instruction interpreter; it
//! delegates to a third-party emulator crate. The [`Cpu`] trait exists so
//! that choice is an implementation detail: swapping the underlying
//! emulator later should mean writing a new trait impl, not rewriting the
//! callers (trap dispatch, process setup, etc.) that only need to read and
//! write registers, step instructions, and touch guest memory.
//!
//! No concrete emulator crate is wired up yet. This module only defines
//! the shape of the abstraction.

use crate::memory::AddressSpace;

/// The eight m68k data registers, D0-D7.
pub const NUM_DATA_REGISTERS: usize = 8;

/// The eight m68k address registers, A0-A7 (A7 is the active stack
/// pointer, USP or SSP depending on supervisor state).
pub const NUM_ADDRESS_REGISTERS: usize = 8;

/// Identifies one of the eight data registers (D0-D7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DataRegister(pub u8);

/// Identifies one of the eight address registers (A0-A7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AddressRegister(pub u8);

/// Which flavor of m68k trap/exception a [`StopReason::Trap`] represents.
///
/// This mirrors the interception points a host runtime needs to hook for
/// high-level emulation of library calls: AmigaOS library jump tables are
/// conventionally implemented with A-line traps, but TRAP #n and illegal
/// instructions are useful hooks too (e.g. for a syscall-style ABI or for
/// catching guest bugs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrapKind {
    /// An A-line trap (an 0xAxxx opcode word, conventionally reserved for
    /// library call dispatch on AmigaOS).
    ALine {
        /// The trapping opcode word.
        opcode: u16,
    },
    /// An F-line trap (an 0xFxxx opcode word, reserved for
    /// coprocessor/line-1111 emulation).
    FLine {
        /// The trapping opcode word.
        opcode: u16,
    },
    /// A `TRAP #n` instruction (n in 0..=15).
    Trap {
        /// The trap number encoded in the instruction.
        trap_num: u8,
    },
    /// A `BKPT #n` instruction (n in 0..=7).
    Breakpoint {
        /// The breakpoint number encoded in the instruction.
        bp_num: u8,
    },
    /// An illegal (undecodable, or explicitly the ILLEGAL opcode)
    /// instruction.
    Illegal {
        /// The illegal opcode word.
        opcode: u16,
    },
}

/// Details of a trap surfaced by [`StopReason::Trap`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrapInfo {
    /// Which kind of trap/exception occurred.
    pub kind: TrapKind,
    /// The address of the instruction that caused the trap (i.e. the PC
    /// value at the start of the trapping instruction, not the PC after
    /// it). A later stage uses this to decode which library call was
    /// being made (e.g. by looking at the jump-table slot the guest
    /// branched through to reach this address).
    pub pc: u32,
}

/// Why a call to [`Cpu::run`] returned control to the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// A single instruction was executed (used by [`Cpu::step`]).
    Step,
    /// The CPU hit a trap/exception that the host runtime needs to
    /// service (e.g. an A-line trap used for library call dispatch). See
    /// [`TrapInfo`] for which trap and where it happened.
    Trap(TrapInfo),
    /// The CPU halted itself (e.g. executed a STOP instruction, or hit an
    /// illegal instruction with no handler).
    Halted,
    /// The program counter ran off the end of guest memory (e.g. a `JSR`/
    /// `JMP` through a bogus/uninitialized address register). Without
    /// this check, [`crate::memory::AddressSpace`]'s "out-of-range reads
    /// return `0`" convention means the CPU would silently decode an
    /// endless stream of zero-word instructions and keep marching
    /// forward (`u32` address wraparound included) until it happened to
    /// land back on something that traps -- turning a guest bug into a
    /// wildly misleading diagnostic pointing at the wrong address,
    /// possibly thousands of instructions later. Caught eagerly instead
    /// (see [`Cpu::run`]'s default implementation), so the reported `pc`
    /// is the real faulting jump target.
    PcOutOfBounds {
        /// The out-of-range program counter value.
        pc: u32,
    },
}

/// A minimal abstraction over an m68k CPU core.
///
/// Implementations own their register file and are given access to guest
/// memory via an [`AddressSpace`]. Everything here is deliberately narrow:
/// just enough surface for trap plumbing and process setup to work
/// against, so a concrete emulator crate can be dropped in behind this
/// trait later.
pub trait Cpu {
    /// The concrete [`AddressSpace`] this CPU reads/writes guest memory
    /// through.
    type Memory: AddressSpace;

    /// Executes a single instruction and returns why execution stopped
    /// (normally [`StopReason::Step`], but a trap or halt can also occur
    /// on the very first instruction).
    fn step(&mut self, mem: &mut Self::Memory) -> StopReason;

    /// Runs instructions until a trap or halt occurs. A naive
    /// implementation may simply loop over [`Cpu::step`]; a faster
    /// backend may run a tight native loop instead.
    fn run(&mut self, mem: &mut Self::Memory) -> StopReason {
        loop {
            let pc = self.pc();
            if pc as usize >= mem.len() {
                return StopReason::PcOutOfBounds { pc };
            }
            match self.step(mem) {
                StopReason::Step => continue,
                other => return other,
            }
        }
    }

    /// Reads a data register (D0-D7).
    fn data_register(&self, reg: DataRegister) -> u32;

    /// Writes a data register (D0-D7).
    fn set_data_register(&mut self, reg: DataRegister, value: u32);

    /// Reads an address register (A0-A7).
    fn address_register(&self, reg: AddressRegister) -> u32;

    /// Writes an address register (A0-A7).
    fn set_address_register(&mut self, reg: AddressRegister, value: u32);

    /// Reads the program counter.
    fn pc(&self) -> u32;

    /// Writes the program counter.
    fn set_pc(&mut self, value: u32);

    /// Reads the status register (condition codes + system byte).
    fn sr(&self) -> u16;

    /// Writes the status register.
    fn set_sr(&mut self, value: u16);

    /// Delivers the real m68k hardware exception for a trap [`Cpu::step`]/
    /// [`Cpu::run`] just reported, other than [`TrapKind::ALine`] (this
    /// runtime's own library-call dispatch convention, always handled by
    /// the caller itself, never routed here): reads the guest's real
    /// exception vector table (at `vector * 4`, matching a plain 68000
    /// with `VBR` fixed at `0`), and if the guest has installed a
    /// handler there (a non-`0` entry -- real AmigaOS/well-behaved guest
    /// programs commonly do this for hardware feature detection, e.g.
    /// probing for an FPU by executing a real F-line instruction and
    /// catching the resulting exception), pushes a real exception stack
    /// frame (`SR` then `PC`) and jumps to it -- exactly what real
    /// hardware does, letting the guest's *own* handler run and
    /// eventually `RTE` back.
    ///
    /// Returns `false` (does nothing to CPU state) if the vector table
    /// entry is `0` (no handler installed) -- the caller should treat
    /// this the same as before this method existed (report an
    /// unhandled/unexpected trap) rather than blindly jumping to a
    /// garbage `0` address.
    ///
    /// # The pushed return `PC` is the *trapping* instruction's own
    ///   address, not the next one
    ///
    /// For every exception this delivers (F-line, illegal instruction,
    /// BKPT), real 68000 hardware stacks the address of the instruction
    /// that *couldn't* execute, not the one after it -- unlike `TRAP
    /// #n`, which is a deliberate, always-executable instruction that
    /// stacks its successor. This is real, standard 68000 behavior (not
    /// a simplification this runtime introduced): the exception handler
    /// is expected to either software-emulate the trapping instruction
    /// and advance the stacked `PC` itself before `RTE`, or decide it
    /// isn't going to resume normal execution at all. A guest handler
    /// that just `RTE`s immediately without adjusting the stack will
    /// re-trap on the same instruction forever -- this is a property of
    /// real hardware semantics, faithfully reproduced, not a bug in
    /// this method.
    fn take_hardware_exception(&mut self, mem: &mut Self::Memory, kind: TrapKind) -> bool;

    /// Enters supervisor mode the way the CPU enters an exception, with
    /// a frame that returns to `return_pc` in the current `SR`: switches
    /// to the supervisor stack (clearing the trace bits), then pushes the
    /// frame the emulated model's own `RTE` pops -- `SR` and `PC` on a
    /// 68000, plus a format 0 vector word (`$20`, privilege violation) on
    /// a 68010 and later. `exec.library`'s `Supervisor` enters its routine
    /// through this, see `crate::execfmt::supervisor_handler`.
    fn push_exception_frame(&mut self, mem: &mut Self::Memory, return_pc: u32);

    /// Accumulated real m68k CPU cycles consumed so far, for a backend
    /// that tracks them at all -- see
    /// [`crate::backend::M68kCpu::set_clock_mhz`] (the CLI's
    /// `--clock-mhz` flag, issue #102) for the one implementation that
    /// does.
    ///
    /// **Only [`Cpu::run`] accumulates cycles, never [`Cpu::step`].**
    /// `M68kCpu::run_via_cycles` (the method backing this) is reached
    /// exclusively through `Cpu::run`; `M68kCpu::step` always goes
    /// through the plain interpreter (`core.step`), which this crate's
    /// backend never wires up to any cycle counter. This matters
    /// because a handful of callers reach guest code through `step`
    /// directly rather than `run`: `execfmt.rs`'s `RawDoFmt` calls a
    /// guest-supplied `PutChProc` callback one instruction at a time
    /// (see its step loop, `CALLOUT_STEP_BUDGET`), and its `Supervisor`
    /// handler single-steps a guest routine the same way (see its own
    /// step loop, `SUPERVISOR_STEP_BUDGET`). Guest instructions executed
    /// through either of those cost *zero* emulated cycles under
    /// `--clock-mhz`, exactly like time spent inside any other
    /// native-Rust library handler -- consistent with, not a special
    /// case of, the general "native handlers are free" behavior (see
    /// `crate::exectask::read_eclock_handler`'s and the CLI's
    /// `--clock-mhz` doc for the user-facing version of this caveat).
    ///
    /// Defaults to `0` so adding this method is non-breaking: every
    /// caller that predates issue #102 already assumes execution carries
    /// no cycle count at all (the normal `run_batch` path this runtime
    /// has used since Phase 1 never surfaced one -- see `backend.rs`'s
    /// module docs on why `m68k::BatchResult` has no `cycles` field),
    /// and `0` is indistinguishable from "never asked". Only meaningful
    /// together with [`Cpu::clock_hz`]; a caller that cares about actual
    /// elapsed emulated time should check `clock_hz().is_some()` first
    /// rather than trying to interpret a `0` cycle count on its own.
    fn emulated_cycles(&self) -> u64 {
        0
    }

    /// Instructions retired over the same span [`Cpu::emulated_cycles`]
    /// counts cycles over -- `0` by default, for the same reason and with
    /// the same caveat: only [`Cpu::run`] accumulates, never
    /// [`Cpu::step`].
    ///
    /// Reported next to the cycle count so a run's *memory intensity* is
    /// visible from the run itself. Cycles alone cannot distinguish a
    /// workload whose time is arithmetic from one whose time is bus
    /// traffic, and that distinction decides whether this runtime's
    /// timing means anything for a given workload: it bills no bus wait
    /// states, so a memory-bound workload's emulated time is optimistic
    /// by a wide and workload-dependent margin (volamos/issues/105
    /// measured 1.02x to 25x against cycle-paced hardware, monotonic in
    /// memory intensity). Cycles-per-instruction and accesses-per-
    /// instruction are what let a caller place a workload on that scale
    /// without a second runtime to compare against.
    fn emulated_instructions(&self) -> u64 {
        0
    }

    /// The clock rate ([`Cpu::emulated_cycles`] is being counted at (in
    /// Hz), if this backend is currently deriving guest-visible elapsed
    /// time from emulated cycles rather than the host wall clock --
    /// `Some` exactly when [`crate::backend::M68kCpu::set_clock_mhz`]
    /// installed a rate. `None` (the default for every backend, and for
    /// [`crate::backend::M68kCpu`] itself unless `--clock-mhz` was
    /// given) means "no such mode active"; callers like
    /// [`crate::exectask::read_eclock_handler`] use that to decide
    /// whether to fall back to the host clock.
    fn clock_hz(&self) -> Option<f64> {
        None
    }
}

/// A complete snapshot of one task's architectural register state --
/// everything a cooperative task switch (issue #121) needs to suspend a
/// task and later resume it exactly where it left off.
///
/// Deliberately *not* a raw snapshot of a concrete backend's whole CPU
/// core (e.g. `m68k::CpuCore`, which [`crate::backend::M68kCpu`] wraps):
/// that type bundles non-architectural state alongside the real
/// registers (a lazily-built per-core opcode decode cache, transient
/// bookkeeping captured only during a batch run) and isn't `Clone`
/// besides. This instead holds exactly the state [`Cpu`]'s own
/// getters/setters expose, captured/restored one register at a time via
/// [`capture`]/[`restore`] -- 18 calls each way, cheap, and correct for
/// any `Cpu` implementation rather than tied to one backend's internals.
///
/// Deliberately **not** FPU state (`FP0`-`FP7`/`FPSR`/`FPCR`/`FPIAR`):
/// nothing in scope for the first cooperative-switching slice runs
/// FPU-using code across a switch point, and the [`Cpu`] trait doesn't
/// expose FPU registers at all today. A task that switches away mid-FPU
/// computation would lose that state -- a known gap, not yet a problem
/// for anything this runtime's switch points (`Wait`/`Permit`+`SFB_SAR`/
/// same-priority `SetTaskPri`) actually hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegisterSnapshot {
    /// D0-D7.
    pub d: [u32; NUM_DATA_REGISTERS],
    /// A0-A7 (A7 is whichever stack -- user or supervisor -- was active
    /// when captured; this runtime never models supervisor/user stack
    /// separation, so there is only ever the one `A7` [`Cpu::
    /// address_register`] already reports).
    pub a: [u32; NUM_ADDRESS_REGISTERS],
    /// The program counter.
    pub pc: u32,
    /// The full status register (condition codes + system byte).
    pub sr: u16,
}

/// Captures `cpu`'s current architectural registers into a
/// [`RegisterSnapshot`]. See that type's doc for what this deliberately
/// does and doesn't cover.
pub fn capture<C: Cpu>(cpu: &C) -> RegisterSnapshot {
    let mut d = [0u32; NUM_DATA_REGISTERS];
    let mut a = [0u32; NUM_ADDRESS_REGISTERS];
    for (i, slot) in d.iter_mut().enumerate() {
        *slot = cpu.data_register(DataRegister(i as u8));
    }
    for (i, slot) in a.iter_mut().enumerate() {
        *slot = cpu.address_register(AddressRegister(i as u8));
    }
    RegisterSnapshot {
        d,
        a,
        pc: cpu.pc(),
        sr: cpu.sr(),
    }
}

/// Writes every register in `snapshot` back into `cpu` -- the inverse of
/// [`capture`], used to resume a task a switch previously suspended.
///
/// `sr` is restored *first*, deliberately before `a[7]`: the `m68k`
/// crate's `set_sr` banks the stack pointer when the S bit changes
/// (`backup_sp`s the live `A7` into the *old* mode's hidden USP/SSP
/// slot, then `restore_sp`s from the *new* mode's slot) -- restoring
/// `a[7]` first and `sr` after would have the SR write silently
/// overwrite the just-restored `A7` with whatever stale value sits in
/// the new mode's bank (`0` for a mode this task has never used
/// before), undoing the restore. Found by a `CreateNewProc` child
/// task's very first `JSR` pushing its return address at `A7 =
/// 0xFFFFFFFC` instead of its real stack -- the child's initial
/// snapshot's `sr` differed from the parent's live `sr` at the moment
/// of the switch, and the old write-order let that SR write clobber
/// the stack pointer this function had just set.
pub fn restore<C: Cpu>(cpu: &mut C, snapshot: &RegisterSnapshot) {
    cpu.set_sr(snapshot.sr);
    for (i, &value) in snapshot.d.iter().enumerate() {
        cpu.set_data_register(DataRegister(i as u8), value);
    }
    for (i, &value) in snapshot.a.iter().enumerate() {
        cpu.set_address_register(AddressRegister(i as u8), value);
    }
    cpu.set_pc(snapshot.pc);
}
