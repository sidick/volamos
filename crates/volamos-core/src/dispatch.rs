//! Fake AmigaOS library jump tables, trap dispatch, and the top-level
//! [`Runtime`] that ties CPU + memory + dispatch together to run a loaded
//! guest program to completion.
//!
//! # Jump-table mechanics
//!
//! Real AmigaOS libraries are called through a base pointer (conventionally
//! held in `A6`) plus a negative offset, the "LVO" (library vector offset):
//! `jsr LVOFoo(a6)`. The library's jump table lives just *below* its base
//! pointer, one `JMP` instruction per vector, walking downward as offsets
//! get more negative.
//!
//! `volamos` doesn't implement real AmigaOS libraries; instead it fakes
//! just enough of the jump table to intercept the LVOs a guest program
//! actually uses. [`LibraryTable::register`] writes a single A-line opcode
//! word (`0xA000 | slot`, where `slot` is a small integer identifying which
//! handler to run) at `base + lvo` in guest memory. Because the `m68k`
//! backend surfaces A-line opcodes as [`StopReason::Trap`] rather than
//! taking a hardware exception (see `backend.rs`), the flow at run time is:
//!
//! 1. Guest does `jsr -948(a6)` (e.g. `PutStr`). This is an ordinary `JSR`
//!    instruction: the CPU core pushes the return address onto the guest
//!    stack (`A7`) and jumps to `a6 - 948`, all in guest-visible state,
//!    with no host involvement.
//! 2. The word at that address is the A-line opcode [`LibraryTable`]
//!    planted there. The CPU core can't execute it, so `step`/`run`
//!    returns `StopReason::Trap(TrapInfo { kind: ALine { opcode }, pc })`,
//!    where `pc` is the address of the trapping word (i.e. `a6 - 948`
//!    itself).
//! 3. The host ([`Runtime::run`]) decodes `slot` from the low 12 bits of
//!    `opcode`, looks up the registered handler, and calls it. The handler
//!    reads arguments from guest registers/memory, does its (host-side)
//!    work, and writes a result back to `D0` — exactly like a real library
//!    function would on return.
//! 4. The host then performs the `RTS` *itself*, in host code: it pops the
//!    return address the guest's original `JSR` pushed off of `A7`, sets
//!    `PC` to it (through [`Cpu::set_pc`], which also invalidates the
//!    backend's prefetch queue), and bumps `A7` by 4. Execution resumes in
//!    the guest exactly as if the library call had really run and
//!    returned.
//!
//! No real code ever executes at the trapped address; the A-line word is
//! purely a signal to the host. Adding another library call later is just
//! another [`LibraryTable::register`] call with a different base/LVO/
//! handler — no new mechanism required.
//!
//! # Clean exit
//!
//! [`Runtime::new`] arranges the initial guest stack so the very first
//! return address on it (the one the program's outermost `rts` will pop)
//! is [`EXIT_STUB_ADDR`], a sentinel inside the reserved low region that
//! also holds a trapping opcode. When a trap's `pc` equals
//! `EXIT_STUB_ADDR`, [`Runtime::run`] treats it as program termination
//! rather than a library call: it reads `D0` as the guest's process exit
//! code and stops, without trying to dispatch or perform a synthetic RTS
//! (there's nothing to return to).

use std::collections::HashMap;
use std::fmt;
use std::io::Write;

use crate::backend::{TRAP_TABLE_BASE, TRAP_TABLE_END};
use crate::cpu::{AddressRegister, Cpu, DataRegister, StopReason, TrapKind};
use crate::lvos::{LvoEntry, find_by_lvo, find_by_name};
use crate::memory::AddressSpace;

/// Number of distinct handler slots representable in an A-line opcode's
/// low 12 bits (`0xA000`..=`0xAFFF`). One slot is reserved for the exit
/// sentinel, so [`LibraryTable::register`] can hand out at most
/// `MAX_LIBRARY_SLOTS - 1` library-call slots.
pub const MAX_LIBRARY_SLOTS: u16 = 0x1000;

/// The slot index (and low 12 bits of the opcode word) reserved for the
/// exit stub. Not available to [`LibraryTable::register`].
const EXIT_SLOT: u16 = MAX_LIBRARY_SLOTS - 1;

/// The slot index reserved as the "no handler registered" sentinel.
/// [`Runtime::new`] prefills the *entire* reserved jump-table region with
/// this opcode before registering any real handlers and before writing
/// the exit stub, so a `jsr`/`jmp` through any LVO nobody registered
/// still traps cleanly (as [`DispatchError::UnknownCall`]) instead of
/// falling through to whatever garbage bytes happen to be at that
/// address. Not available to [`LibraryTable::register`].
const UNKNOWN_SLOT: u16 = 0;

/// Guest address of the exit sentinel: the last word inside the reserved
/// jump-table region, deliberately kept clear of any library base's
/// negative-offset range (all real library bases used here sit at or
/// below [`DOS_LIBRARY_BASE`], well short of this address).
pub const EXIT_STUB_ADDR: u32 = TRAP_TABLE_END - 4;

/// Fake `dos.library` base address. Chosen so that every LVO this runtime
/// currently emulates (`PutStr` at -948) lands inside the reserved
/// low-memory region `[`TRAP_TABLE_BASE`, `TRAP_TABLE_END`)`, comfortably
/// below [`EXIT_STUB_ADDR`].
pub const DOS_LIBRARY_BASE: u32 = 0x0800;

/// `dos.library`'s `PutStr` LVO (library vector offset): -948, i.e.
/// `_LVOPutStr`.
pub const LVO_PUTSTR: i32 = -948;

/// What a host-side library call handler is given to do its work: mutable
/// access to the CPU (registers), guest memory, and an output sink for
/// anything the call writes to "stdout" (e.g. `PutStr`).
pub struct HandlerContext<'a, C: Cpu> {
    pub cpu: &'a mut C,
    pub mem: &'a mut C::Memory,
    pub out: &'a mut dyn Write,
}

/// A host-side implementation of one AmigaOS library call.
///
/// Implementations read arguments from `ctx.cpu`/`ctx.mem` (per the real
/// call's register-based calling convention), do their work, and write a
/// result back to `D0` (again, matching real AmigaOS convention) before
/// returning `Ok(())`. Returning `Err` aborts the run with a
/// [`RuntimeError`].
pub trait LibraryHandler<C: Cpu> {
    /// Calls the emulated library function.
    fn call(&mut self, ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError>;
}

impl<C: Cpu, F> LibraryHandler<C> for F
where
    F: FnMut(&mut HandlerContext<'_, C>) -> Result<(), DispatchError>,
{
    fn call(&mut self, ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
        self(ctx)
    }
}

/// One registered library-call slot: enough metadata to log it (for
/// `--verbose`) plus the handler itself.
struct Slot<C: Cpu> {
    library: String,
    lvo: i32,
    handler_name: String,
    handler: Box<dyn LibraryHandler<C>>,
}

/// Details of a dispatched (or attempted) library call, used both for
/// `--verbose` logging and for error reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallInfo {
    pub library: String,
    pub lvo: i32,
    pub handler_name: String,
}

impl fmt::Display for CallInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}({:+}) -> {}",
            self.library, self.lvo, self.handler_name
        )
    }
}

/// Errors a [`LibraryHandler`] can report, or that dispatch itself can hit
/// before ever reaching a handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchError {
    /// The trapping A-line opcode's slot has no registered handler (a
    /// `jsr`/`jmp` landed somewhere inside the jump-table region that
    /// [`LibraryTable::register`] never wrote an entry for).
    UnknownCall {
        /// Address of the trapping instruction.
        pc: u32,
        /// The full trapping opcode word.
        opcode: u16,
        /// Best-effort guess at which library base this call was relative
        /// to, and the LVO offset from it, computed against every base
        /// that has at least one registered slot.
        candidates: Vec<(String, i32)>,
    },
    /// A handler reported its own failure (e.g. malformed guest input).
    HandlerFailed {
        library: String,
        lvo: i32,
        handler_name: String,
        message: String,
    },
    /// [`LibraryTable::register_by_name`] was asked to register a function
    /// name that isn't in the supplied [`LvoEntry`] table.
    UnknownLibraryFunction { library: String, name: String },
}

impl fmt::Display for DispatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DispatchError::UnknownCall {
                pc,
                opcode,
                candidates,
            } => {
                write!(
                    f,
                    "unhandled library call: opcode {opcode:#06x} at {pc:#010x}"
                )?;
                if candidates.is_empty() {
                    write!(f, " (no known library base nearby)")
                } else {
                    write!(f, " (candidates: ")?;
                    for (i, (lib, offset)) in candidates.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{lib} ({offset:+})")?;
                    }
                    write!(f, ")")
                }
            }
            DispatchError::HandlerFailed {
                library,
                lvo,
                handler_name,
                message,
            } => write!(f, "{library}({lvo:+}) [{handler_name}] failed: {message}"),
            DispatchError::UnknownLibraryFunction { library, name } => {
                write!(f, "{library}: no LVO metadata for function {name:?}")
            }
        }
    }
}

impl std::error::Error for DispatchError {}

/// A registry of fake library jump-table entries and the handlers they
/// dispatch to. See the module docs for the full trap flow.
pub struct LibraryTable<C: Cpu> {
    slots: HashMap<u16, Slot<C>>,
    /// Next slot number [`LibraryTable::register`] will hand out. Starts
    /// at 1 since slot 0 is reserved for [`UNKNOWN_SLOT`].
    next_slot: u16,
    /// Every distinct `(library name, base address)` a slot has been
    /// registered against, used to produce helpful diagnostics for
    /// unhandled calls.
    bases: HashMap<String, u32>,
    /// LVO metadata table associated with each library base that's been
    /// registered through [`LibraryTable::register_by_name`] (bare
    /// [`LibraryTable::register`] calls don't populate this -- they have
    /// no table to record). Used to resolve unknown-call diagnostics for
    /// that base down to a function name instead of a raw offset.
    tables: HashMap<u32, &'static [LvoEntry]>,
}

impl<C: Cpu> Default for LibraryTable<C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C: Cpu> LibraryTable<C> {
    /// Creates an empty table (no jump-table entries registered yet).
    pub fn new() -> Self {
        Self {
            slots: HashMap::new(),
            next_slot: UNKNOWN_SLOT + 1,
            bases: HashMap::new(),
            tables: HashMap::new(),
        }
    }

    /// Registers a handler for `library`'s LVO `lvo` at `base`, writing
    /// the trapping A-line opcode into `mem` at `base + lvo`. Returns the
    /// slot index assigned (mostly useful for tests).
    ///
    /// # Panics
    ///
    /// Panics if more than [`MAX_LIBRARY_SLOTS`] `- 2` handlers have
    /// already been registered (slot 0 is reserved for "unknown call" and
    /// the top slot is reserved for the exit sentinel).
    pub fn register(
        &mut self,
        mem: &mut C::Memory,
        base: u32,
        lvo: i32,
        library: &str,
        handler_name: &str,
        handler: impl LibraryHandler<C> + 'static,
    ) -> u16 {
        let slot = self.next_slot;
        assert!(
            slot < EXIT_SLOT,
            "LibraryTable: too many registered handlers (max {})",
            EXIT_SLOT - 1
        );
        self.next_slot += 1;

        let addr = base.wrapping_add(lvo as u32);
        let opcode = 0xA000 | slot;
        mem.write_u16(addr, opcode);

        self.bases.insert(library.to_string(), base);
        self.slots.insert(
            slot,
            Slot {
                library: library.to_string(),
                lvo,
                handler_name: handler_name.to_string(),
                handler: Box::new(handler),
            },
        );
        slot
    }

    /// Convenience wrapper around [`LibraryTable::register`] that looks up
    /// `name`'s LVO in `table` (an [`LvoEntry`] table, e.g.
    /// [`crate::lvos::dos::DOS_LVOS`]) instead of requiring the caller to
    /// know the raw offset. Also records `table` against `base` so
    /// [`DispatchError::UnknownCall`] can resolve *other*, unregistered
    /// LVOs on this same base down to a function name (see
    /// [`LibraryTable::dispatch`]).
    ///
    /// Returns [`DispatchError::UnknownLibraryFunction`] if `name` isn't in
    /// `table`, rather than panicking -- `table` may be incomplete (a
    /// handwritten table for a library `tools/gen_lvos.py` hasn't been run
    /// against yet) and callers should be able to handle that as an
    /// ordinary error, e.g. surfacing "unimplemented library function" at
    /// startup instead of aborting.
    pub fn register_by_name(
        &mut self,
        mem: &mut C::Memory,
        base: u32,
        table: &'static [LvoEntry],
        library: &str,
        name: &str,
        handler: impl LibraryHandler<C> + 'static,
    ) -> Result<u16, DispatchError> {
        let entry =
            find_by_name(table, name).ok_or_else(|| DispatchError::UnknownLibraryFunction {
                library: library.to_string(),
                name: name.to_string(),
            })?;
        let lvo = entry.lvo;
        let entry_name = entry.name;
        self.tables.insert(base, table);
        Ok(self.register(mem, base, lvo, library, entry_name, handler))
    }

    /// Dispatches a trapped A-line `opcode` encountered at `pc`. On
    /// success, returns the [`CallInfo`] describing what was called (for
    /// `--verbose` logging).
    fn dispatch(
        &mut self,
        opcode: u16,
        pc: u32,
        cpu: &mut C,
        mem: &mut C::Memory,
        out: &mut dyn Write,
    ) -> Result<CallInfo, DispatchError> {
        let slot = opcode & 0x0FFF;
        let Some(entry) = self.slots.get_mut(&slot) else {
            let candidates = self
                .bases
                .iter()
                .map(|(lib, &base)| {
                    let offset = pc as i64 as i32 - base as i32;
                    // Resolve through this base's LVO table, if it has one,
                    // so the diagnostic can name the function the guest
                    // was trying to call instead of just an offset.
                    let label = self
                        .tables
                        .get(&base)
                        .and_then(|table| find_by_lvo(table, offset))
                        .map(|found| format!("{lib}/{}", found.name))
                        .unwrap_or_else(|| lib.clone());
                    (label, offset)
                })
                .collect();
            return Err(DispatchError::UnknownCall {
                pc,
                opcode,
                candidates,
            });
        };

        let info = CallInfo {
            library: entry.library.clone(),
            lvo: entry.lvo,
            handler_name: entry.handler_name.clone(),
        };

        let mut ctx = HandlerContext { cpu, mem, out };
        entry.handler.call(&mut ctx).map_err(|e| match e {
            DispatchError::HandlerFailed { message, .. } => DispatchError::HandlerFailed {
                library: info.library.clone(),
                lvo: info.lvo,
                handler_name: info.handler_name.clone(),
                message,
            },
            other => other,
        })?;

        Ok(info)
    }
}

/// Reads a NUL-terminated string starting at `addr` out of guest memory.
/// The terminator is not included in the returned bytes.
fn read_c_string(mem: &dyn AddressSpace, addr: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut a = addr;
    loop {
        let b = mem.read_u8(a);
        if b == 0 {
            break;
        }
        bytes.push(b);
        a = a.wrapping_add(1);
    }
    bytes
}

/// The `dos.library` `PutStr` handler: writes the NUL-terminated string
/// pointed to by `D1` to `ctx.out` verbatim (no newline added; the guest
/// string already contains one if it wants one), and returns 0 (success)
/// in `D0`, matching real `PutStr`'s "0 on success, -1 (`EOF`) on
/// failure" contract. Never fails on the host side (a write error would
/// only happen if the sink itself errors, which we don't expect for an
/// in-memory buffer or stdout); it maps such a failure to `HandlerFailed`
/// rather than panicking.
fn putstr_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let ptr = ctx.cpu.data_register(DataRegister(1));
    let bytes = read_c_string(ctx.mem, ptr);
    ctx.out
        .write_all(&bytes)
        .map_err(|e| DispatchError::HandlerFailed {
            library: "dos.library".to_string(),
            lvo: LVO_PUTSTR,
            handler_name: "PutStr".to_string(),
            message: e.to_string(),
        })?;
    ctx.cpu.set_data_register(DataRegister(0), 0);
    Ok(())
}

/// Errors [`Runtime::run`] can report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeError {
    /// A library call couldn't be serviced; see [`DispatchError`].
    Dispatch(DispatchError),
    /// The CPU stopped for a reason a running guest program shouldn't
    /// trigger via the fake jump table (an F-line trap, `TRAP #n`,
    /// `BKPT #n`, or an illegal instruction executed outside the jump
    /// table, or the backend halting itself, e.g. via `STOP`).
    UnexpectedStop { reason: String, pc: u32 },
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuntimeError::Dispatch(e) => write!(f, "{e}"),
            RuntimeError::UnexpectedStop { reason, pc } => {
                write!(f, "unexpected CPU stop at {pc:#010x}: {reason}")
            }
        }
    }
}

impl std::error::Error for RuntimeError {}

impl From<DispatchError> for RuntimeError {
    fn from(e: DispatchError) -> Self {
        RuntimeError::Dispatch(e)
    }
}

/// Ties a [`Cpu`] backend, its guest memory, and a [`LibraryTable`]
/// together to run a loaded program to completion.
///
/// Construction sets up:
/// - `A6` = [`DOS_LIBRARY_BASE`] (the fixtures this runtime targets
///   assume a library base is already in `A6` at program start; Phase 2
///   would replace this with a real `OpenLibrary`-driven base).
/// - `A7` (the stack pointer) at the top of guest memory, with
///   [`EXIT_STUB_ADDR`] pre-pushed as the return address so the guest's
///   own final `rts` lands on the exit sentinel (see module docs).
/// - `PC` = the program's entry point.
pub struct Runtime<C: Cpu> {
    cpu: C,
    mem: C::Memory,
    table: LibraryTable<C>,
}

/// A single trapped library call, reported to an optional trace callback
/// so a CLI's `--verbose` flag can log it.
pub type TraceEvent = CallInfo;

impl<C: Cpu + 'static> Runtime<C> {
    /// Builds a runtime around an already-constructed CPU and loaded
    /// guest memory. `entry` is the guest address to start executing at
    /// (typically [`crate::loader::LoadResult::entry`]).
    ///
    /// The fake `dos.library` jump table (currently just `PutStr`) is
    /// registered automatically; see [`Runtime::library_table_mut`] to
    /// register additional handlers before calling [`Runtime::run`].
    pub fn new(mut cpu: C, mut mem: C::Memory, entry: u32) -> Self {
        // Prefill the whole reserved jump-table region with the "unknown
        // call" sentinel opcode before registering anything real, so any
        // `jsr`/`jmp` into an LVO nobody's registered a handler for still
        // traps cleanly (as an `UnknownCall`) instead of falling through
        // to whatever raw (zeroed) bytes happen to sit there.
        let mut addr = TRAP_TABLE_BASE;
        while addr < TRAP_TABLE_END {
            mem.write_u16(addr, 0xA000 | UNKNOWN_SLOT);
            addr = addr.wrapping_add(2);
        }

        let mut table = LibraryTable::new();
        table.register(
            &mut mem,
            DOS_LIBRARY_BASE,
            LVO_PUTSTR,
            "dos.library",
            "PutStr",
            putstr_handler::<C>,
        );

        // Exit sentinel: any A-line word works (we never decode it; the
        // exit path is short-circuited on address, not opcode), but using
        // a real slot number keeps the trap table self-consistent to
        // read/disassemble.
        mem.write_u16(EXIT_STUB_ADDR, 0xA000 | EXIT_SLOT);

        // Stack: top of guest memory, 4-byte aligned, with the exit
        // sentinel pre-pushed as the return address for the program's
        // outermost `rts`.
        let top = (mem.len() as u32) & !3;
        let sp = top.wrapping_sub(4);
        mem.write_u32(sp, EXIT_STUB_ADDR);

        cpu.set_address_register(AddressRegister(7), sp);
        cpu.set_address_register(AddressRegister(6), DOS_LIBRARY_BASE);
        cpu.set_pc(entry);

        Self { cpu, mem, table }
    }

    /// Mutable access to the library table, so callers can register
    /// additional handlers (or override the default `PutStr`) before
    /// [`Runtime::run`].
    pub fn library_table_mut(&mut self) -> &mut LibraryTable<C> {
        &mut self.table
    }

    /// Direct access to guest memory (e.g. for tests that want to inspect
    /// state after a run).
    pub fn memory(&self) -> &C::Memory {
        &self.mem
    }

    /// Runs the guest program to completion, writing anything it prints
    /// (currently just `PutStr` output) to `out`. If `trace` is set, it's
    /// called once per dispatched library call (for `--verbose` logging)
    /// before the call's result is applied.
    ///
    /// Returns the guest's exit code (the value of `D0` when it reached
    /// the exit sentinel) on success.
    pub fn run(
        &mut self,
        out: &mut dyn Write,
        mut trace: Option<&mut dyn FnMut(&TraceEvent)>,
    ) -> Result<i32, RuntimeError> {
        loop {
            match self.cpu.run(&mut self.mem) {
                StopReason::Step => unreachable!(
                    "Cpu::run only returns once execution stops (Trap or Halted), never Step"
                ),
                StopReason::Halted => {
                    return Err(RuntimeError::UnexpectedStop {
                        reason: "CPU halted".to_string(),
                        pc: self.cpu.pc(),
                    });
                }
                StopReason::Trap(info) => {
                    if info.pc == EXIT_STUB_ADDR {
                        let code = self.cpu.data_register(DataRegister(0)) as i32;
                        return Ok(code);
                    }

                    let TrapKind::ALine { opcode } = info.kind else {
                        return Err(RuntimeError::UnexpectedStop {
                            reason: format!("{:?}", info.kind),
                            pc: info.pc,
                        });
                    };

                    let call_info =
                        self.table
                            .dispatch(opcode, info.pc, &mut self.cpu, &mut self.mem, out)?;
                    if let Some(trace) = trace.as_deref_mut() {
                        trace(&call_info);
                    }

                    // Perform the RTS ourselves: pop the return address
                    // the guest's JSR pushed, and resume there.
                    let sp = self.cpu.address_register(AddressRegister(7));
                    let return_addr = self.mem.read_u32(sp);
                    self.cpu
                        .set_address_register(AddressRegister(7), sp.wrapping_add(4));
                    self.cpu.set_pc(return_addr);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::M68kCpu;
    use crate::cpu::DataRegister;
    use crate::memory::FlatMemory;

    fn load_words(mem: &mut FlatMemory, addr: u32, words: &[u16]) {
        let mut offset = addr;
        for &w in words {
            mem.write_u16(offset, w);
            offset += 2;
        }
    }

    /// Builds a runtime with a fresh CPU/memory and a program consisting
    /// only of `words`, loaded at `TRAP_TABLE_END`.
    fn runtime_with_program(words: &[u16]) -> Runtime<M68kCpu> {
        let mut mem = FlatMemory::new(0x4000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, words);
        Runtime::new(M68kCpu::new(), mem, entry)
    }

    #[test]
    fn putstr_writes_string_and_exits_zero() {
        // Program: D1 = pointer to "hi\0" (stored right after the code),
        // jsr -948(a6), moveq #0,d0, rts.
        let entry = TRAP_TABLE_END;
        // 7 code words (14 bytes) precede the string: move.l #imm,d1 (3
        // words), jsr <disp16>(a6) (2 words), moveq (1 word), rts (1 word).
        let str_addr = entry + 14;

        let mut words = vec![
            0x223C, // move.l #imm,d1 (imm follows as 2 words)
        ];
        words.push((str_addr >> 16) as u16);
        words.push(str_addr as u16);
        words.push(0x4EAE); // jsr <disp16>(a6)
        words.push(0xFC4C); // -948
        words.push(0x7000); // moveq #0,d0
        words.push(0x4E75); // rts

        let mut rt = runtime_with_program(&words);
        // Write "hi\0" at str_addr (as raw bytes into the memory before running).
        {
            let mem = &mut rt.mem;
            mem.write_u8(str_addr, b'h');
            mem.write_u8(str_addr + 1, b'i');
            mem.write_u8(str_addr + 2, 0);
        }

        let mut out = Vec::new();
        let mut events = Vec::new();
        let code = rt
            .run(
                &mut out,
                Some(&mut |ev: &TraceEvent| events.push(ev.clone())),
            )
            .expect("run should succeed");

        assert_eq!(code, 0);
        assert_eq!(out, b"hi");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].library, "dos.library");
        assert_eq!(events[0].lvo, LVO_PUTSTR);
        assert_eq!(events[0].handler_name, "PutStr");
    }

    #[test]
    fn nonzero_exit_code_is_propagated() {
        // moveq #42,d0 ; rts
        let words = [0x702A, 0x4E75];
        let mut rt = runtime_with_program(&words);

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 42);
        assert!(out.is_empty());
    }

    #[test]
    fn unregistered_lvo_reports_unknown_call_error() {
        // jsr -100(a6) where a6 = DOS_LIBRARY_BASE but nothing is
        // registered at that offset; moveq/rts never reached.
        let words = [0x4EAE, (-100i16) as u16];
        let mut rt = runtime_with_program(&words);

        let mut out = Vec::new();
        let err = rt.run(&mut out, None).unwrap_err();
        match err {
            RuntimeError::Dispatch(DispatchError::UnknownCall { pc, .. }) => {
                assert_eq!(pc, DOS_LIBRARY_BASE.wrapping_sub(100));
            }
            other => panic!("expected UnknownCall, got {other:?}"),
        }
    }

    #[test]
    fn library_table_register_writes_aline_opcode() {
        let mut mem = FlatMemory::new(0x2000);
        let mut table: LibraryTable<M68kCpu> = LibraryTable::new();
        let slot = table.register(
            &mut mem,
            DOS_LIBRARY_BASE,
            LVO_PUTSTR,
            "dos.library",
            "PutStr",
            putstr_handler::<M68kCpu>,
        );
        let addr = DOS_LIBRARY_BASE.wrapping_add(LVO_PUTSTR as u32);
        assert_eq!(mem.read_u16(addr), 0xA000 | slot);
    }

    #[test]
    fn exit_stub_address_is_inside_reserved_region_and_traps() {
        // After Runtime::new, the exit sentinel word is a real A-line
        // opcode (not zeroed/garbage), so landing on it always traps.
        let rt = runtime_with_program(&[0x4E75]); // rts
        assert_eq!(rt.mem.read_u16(EXIT_STUB_ADDR) & 0xF000, 0xA000);
    }

    #[test]
    fn additional_handler_can_be_registered_before_run() {
        // Registers a second fake library call (an arbitrary made-up LVO
        // on the same fake dos base) and checks it gets dispatched and
        // its result (D0 = 7) survives to become the process exit code.
        let entry = TRAP_TABLE_END;
        let words = [0x4EAE, (-200i16) as u16, 0x4E75]; // jsr -200(a6) ; rts
        let mut mem = FlatMemory::new(0x4000);
        load_words(&mut mem, entry, &words);
        let mut rt = Runtime::new(M68kCpu::new(), mem, entry);

        // Disjoint private-field access (same module tree) lets us borrow
        // `table` and `mem` independently, which `library_table_mut()`
        // alone can't do since it needs `mem` too.
        rt.table.register(
            &mut rt.mem,
            DOS_LIBRARY_BASE,
            -200,
            "dos.library",
            "Fake",
            |ctx: &mut HandlerContext<'_, M68kCpu>| {
                ctx.cpu.set_data_register(DataRegister(0), 7);
                Ok(())
            },
        );

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 7);
    }

    #[test]
    fn register_by_name_looks_up_lvo_and_writes_opcode() {
        use crate::lvos::dos::DOS_LVOS;

        let mut mem = FlatMemory::new(0x2000);
        let mut table: LibraryTable<M68kCpu> = LibraryTable::new();
        let slot = table
            .register_by_name(
                &mut mem,
                DOS_LIBRARY_BASE,
                DOS_LVOS,
                "dos.library",
                "PutStr",
                putstr_handler::<M68kCpu>,
            )
            .expect("PutStr is in DOS_LVOS");

        // PutStr's real LVO is -948; register_by_name should have looked
        // that up and written the trapping opcode at base - 948.
        let addr = DOS_LIBRARY_BASE.wrapping_sub(948);
        assert_eq!(mem.read_u16(addr), 0xA000 | slot);
    }

    #[test]
    fn register_by_name_unknown_function_is_an_error_not_a_panic() {
        use crate::lvos::dos::DOS_LVOS;

        let mut mem = FlatMemory::new(0x2000);
        let mut table: LibraryTable<M68kCpu> = LibraryTable::new();
        let err = table
            .register_by_name(
                &mut mem,
                DOS_LIBRARY_BASE,
                DOS_LVOS,
                "dos.library",
                "TotallyNotARealFunction",
                putstr_handler::<M68kCpu>,
            )
            .unwrap_err();
        assert_eq!(
            err,
            DispatchError::UnknownLibraryFunction {
                library: "dos.library".to_string(),
                name: "TotallyNotARealFunction".to_string(),
            }
        );
    }

    #[test]
    fn unknown_call_diagnostic_names_the_function_when_table_is_known() {
        use crate::lvos::dos::DOS_LVOS;

        // Register PutStr by name (populates the base -> table map), then
        // jsr an unrelated, unregistered LVO on the same base: -84 is
        // Lock's real offset, but we never registered a handler for it.
        let entry = TRAP_TABLE_END;
        let words = [0x4EAE, (-84i16) as u16, 0x4E75]; // jsr -84(a6) ; rts
        let mut mem = FlatMemory::new(0x4000);
        load_words(&mut mem, entry, &words);
        let mut rt = Runtime::new(M68kCpu::new(), mem, entry);
        rt.table
            .register_by_name(
                &mut rt.mem,
                DOS_LIBRARY_BASE,
                DOS_LVOS,
                "dos.library",
                "PutStr",
                putstr_handler::<M68kCpu>,
            )
            .expect("PutStr is in DOS_LVOS");

        let mut out = Vec::new();
        let err = rt.run(&mut out, None).unwrap_err();
        match err {
            RuntimeError::Dispatch(DispatchError::UnknownCall { candidates, .. }) => {
                assert!(
                    candidates
                        .iter()
                        .any(|(lib, offset)| lib == "dos.library/Lock" && *offset == -84),
                    "expected a dos.library/Lock (-84) candidate, got {candidates:?}"
                );
            }
            other => panic!("expected UnknownCall, got {other:?}"),
        }
    }

    #[test]
    fn unknown_call_diagnostic_falls_back_to_raw_offset_without_a_table() {
        // Same shape as the PutStr-only default registration in
        // Runtime::new (bare `register`, no table recorded): an unknown
        // LVO on that base should still report the library name with a
        // plain numeric offset, not panic or silently omit the candidate.
        let words = [0x4EAE, (-100i16) as u16]; // jsr -100(a6)
        let mut rt = runtime_with_program(&words);

        let mut out = Vec::new();
        let err = rt.run(&mut out, None).unwrap_err();
        match err {
            RuntimeError::Dispatch(DispatchError::UnknownCall { candidates, .. }) => {
                assert!(
                    candidates
                        .iter()
                        .any(|(lib, offset)| lib == "dos.library" && *offset == -100),
                    "expected a dos.library (-100) candidate, got {candidates:?}"
                );
            }
            other => panic!("expected UnknownCall, got {other:?}"),
        }
    }
}
