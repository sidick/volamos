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
}
