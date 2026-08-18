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
use crate::dosfile::DosState;
use crate::guestmem::{GuestHeap, STACK_SIZE, read_c_string};
use crate::lvos::{LvoEntry, find_by_lvo, find_by_name};
use crate::memory::AddressSpace;
use crate::vfs::Vfs;

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

/// The slot index reserved for the shared fake-library-vector handler
/// (see [`fake_lib_vector_handler`] and the "vamos escape hatch" module
/// docs). Exactly one handler instance serves *every* auto-created fake
/// library's entire jump table: [`open_library_handler`] writes this
/// same opcode word (`0xA000 | FAKE_LIB_SLOT`) across the whole reserved
/// block it carves out of the guest heap for a new fake library, and the
/// handler resolves which library/offset a given call landed on from
/// [`HandlerContext::pc`] via [`LibraryRegistry::resolve_fake`]. Not
/// available to [`LibraryTable::register`].
const FAKE_LIB_SLOT: u16 = MAX_LIBRARY_SLOTS - 2;

/// Size in bytes of the jump-table block [`open_library_handler`] carves
/// out of the guest heap for each newly auto-created fake library.
/// Generous (matches [`crate::backend::TRAP_TABLE_SIZE`]) so any
/// plausible LVO offset a real program might use lands inside it; a
/// `jsr`/`jmp` at an offset beyond this falls outside the prefilled
/// block and reads whatever the heap allocator happened to put there
/// next -- a known limitation worth revisiting if a future fixture ever
/// hits it.
const FAKE_LIB_JUMP_TABLE_SIZE: u32 = 0x1000;

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

/// Fake `exec.library` base address (T12). AmigaOS convention puts the
/// running system's `SysBase`/`ExecBase` pointer at absolute guest
/// address 4 ([`ABS_EXEC_BASE_ADDR`]); this is the value stored there.
///
/// # Reserved-region memory map
///
/// The whole reserved region is `[`TRAP_TABLE_BASE`] (`0x0000`),
/// [`crate::backend::TRAP_TABLE_END`]) (`0x1000`). Within it:
///
/// ```text
/// 0x0000 .. 0x0004   (unused; kept clear so AbsExecBase's own pointer,
///                      one word below the lowest thing anyone reads at
///                      absolute 0, is unambiguous)
/// 0x0004 .. 0x0008   AbsExecBase: holds EXEC_LIBRARY_BASE (u32), the
///                     value a real program reads via `move.l 4,a6`.
///                     Not a jump-table entry -- never executed.
/// 0x0008 .. 0x02CC   unused headroom
/// 0x02CC .. 0x0800   dos.library's registered LVOs live here (T10/T11
///                     handlers register LVOs as negative as -1356 off
///                     DOS_LIBRARY_BASE; 0x0800 - 1356 = 0x02CC)
/// 0x0800             DOS_LIBRARY_BASE
/// 0x0800 .. 0x0CC8   headroom above dos.library's jump table, below
///                     exec.library's
/// 0x0CC8 .. 0x0F00   exec.library's registered LVOs (OpenLibrary at
///                     -552 = 0x0CC8, OldOpenLibrary at -408 = 0x0D68,
///                     CloseLibrary at -414 = 0x0D62)
/// 0x0F00             EXEC_LIBRARY_BASE
/// 0x0F00 .. 0x0FFC   headroom
/// 0x0FFC .. 0x1000   EXIT_STUB_ADDR (the last word of the region)
/// ```
///
/// Every real (non-fake) library base and its currently-implemented
/// LVOs therefore sits inside the reserved region, with room to spare
/// before the two bases' jump tables would ever collide. Fake libraries
/// (see the "vamos escape hatch" docs on [`open_library_handler`]) are
/// carved from the guest *heap* instead, well above this region, since
/// their number and jump-table extent aren't known up front.
pub const EXEC_LIBRARY_BASE: u32 = 0x0F00;

/// Guest address holding the running "system's" `ExecBase` pointer --
/// `AbsExecBase`, read by real AmigaOS startup code via `move.l 4,a6`
/// (or `move.l 4.w,a6`). [`Runtime::new`] writes [`EXEC_LIBRARY_BASE`]
/// here.
pub const ABS_EXEC_BASE_ADDR: u32 = 4;

/// What a host-side library call handler is given to do its work: mutable
/// access to the CPU (registers), guest memory, an output sink for
/// anything the call writes to "stdout" (e.g. `PutStr`), and the guest
/// heap (for handlers that need to allocate guest-visible structures,
/// e.g. T10/T11's `FileHandle`/`FileInfoBlock`).
pub struct HandlerContext<'a, C: Cpu> {
    pub cpu: &'a mut C,
    pub mem: &'a mut C::Memory,
    pub out: &'a mut dyn Write,
    pub heap: &'a mut GuestHeap,
    /// The registry of known library bases (real and vamos-style
    /// auto-created fakes; see [`LibraryRegistry`]). `exec.library`'s
    /// `OpenLibrary`/`OldOpenLibrary` consult and grow this; the shared
    /// fake-library-vector handler reads it (via [`HandlerContext::pc`])
    /// to name which fake library a trapped call belongs to.
    pub registry: &'a mut LibraryRegistry,
    /// Guest address of the trapping A-line opcode that led to this
    /// call (i.e. the LVO address, `base + lvo`). Most handlers don't
    /// need this (they already know their own LVO), but the shared
    /// fake-library-vector handler does, since one handler instance
    /// serves every auto-created fake library's entire jump table.
    pub pc: u32,
    /// Host-side `dos.library` state (T10): the file-handle registry,
    /// current `IoErr()` value, and (optionally) a [`Vfs`] for path
    /// resolution. See [`crate::dosfile`]'s module docs.
    pub dos: &'a mut DosState,
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
            slot < FAKE_LIB_SLOT,
            "LibraryTable: too many registered handlers (max {})",
            FAKE_LIB_SLOT - 1
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

    /// Binds `slot` directly to `handler` without writing any jump-table
    /// opcode word or consuming a `next_slot` counter value (unlike
    /// [`LibraryTable::register`]). Used exactly once, at
    /// [`Runtime::new`] time, to bind [`FAKE_LIB_SLOT`] to the shared
    /// [`fake_lib_vector_handler`]: every word [`open_library_handler`]
    /// later writes into a fake library's jump-table block reuses this
    /// same slot number, so one handler instance serves every
    /// auto-created fake library, keyed at call time by [`HandlerContext::pc`]
    /// rather than by slot.
    fn register_fixed_slot(&mut self, slot: u16, handler: impl LibraryHandler<C> + 'static) {
        self.slots.insert(
            slot,
            Slot {
                library: "<fake>".to_string(),
                lvo: 0,
                handler_name: "<fake-library-vector>".to_string(),
                handler: Box::new(handler),
            },
        );
    }

    /// Dispatches a trapped A-line `opcode` encountered at `pc`. On
    /// success, returns the [`CallInfo`] describing what was called (for
    /// `--verbose` logging).
    ///
    /// `ctx.pc` must already be set to the trapping opcode's address
    /// (this is also where `ctx.cpu`/`ctx.mem`/etc. get handed to
    /// whichever handler is dispatched to, per [`HandlerContext`]'s
    /// docs).
    fn dispatch(
        &mut self,
        opcode: u16,
        ctx: &mut HandlerContext<'_, C>,
    ) -> Result<CallInfo, DispatchError> {
        let pc = ctx.pc;
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

        // Handlers construct their own `DispatchError::HandlerFailed`
        // with accurate `library`/`lvo`/`handler_name` fields (see e.g.
        // `putstr_handler`), so unlike an earlier version of this code,
        // nothing here rewrites those fields from the slot's registered
        // metadata: the shared fake-library-vector handler (see
        // `fake_lib_vector_handler`) reports a *different* library/LVO
        // per call (resolved from `pc` against the fake library that
        // owns this slot's opcode), which a blanket rewrite from the
        // slot's own (generic, shared) metadata would clobber.
        entry.handler.call(ctx)?;

        Ok(info)
    }
}

/// Whether a [`LibraryRegistry`] entry is backed by a real emulated jump
/// table, or was auto-created by [`open_library_handler`]'s vamos-style
/// escape hatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibraryKind {
    /// A library base this runtime actually emulates (has real,
    /// individually-registered LVO handlers), e.g. `dos.library`.
    Real,
    /// A library base auto-created because `OpenLibrary`/
    /// `OldOpenLibrary` was asked for a name this runtime doesn't
    /// implement. `OpenLibrary` itself never fails for these -- only
    /// calling one of the fake library's vectors does, with a
    /// diagnostic naming the library (see [`fake_lib_vector_handler`]).
    /// Shaped so a future real-library-backed base (Phase 3's `LoadSeg`
    /// passthrough for e.g. the math libraries) can slot in as a third
    /// kind without disturbing this one.
    Faked,
}

/// One auto-created fake library's jump-table block, recorded so
/// [`fake_lib_vector_handler`] can resolve a trapped call's `pc` back to
/// a library name + offset (see [`LibraryRegistry::resolve_fake`]).
#[derive(Debug, Clone)]
struct FakeLibrary {
    name: String,
    /// The fake library's base address (the value returned in `D0`);
    /// the jump-table block occupies `[base - size, base)`.
    base: u32,
    size: u32,
}

/// Registry of known library bases by name -- both the runtime's real,
/// individually-emulated libraries ([`LibraryKind::Real`], registered
/// once at [`Runtime::new`] time) and any fake libraries
/// [`open_library_handler`] auto-creates on demand ([`LibraryKind::
/// Faked`]). `exec.library`'s `OpenLibrary`/`OldOpenLibrary` consult
/// this to avoid creating duplicate fake bases for repeat `OpenLibrary`
/// calls of the same unimplemented name.
///
/// Deliberately a separate type from [`LibraryTable`] (rather than a
/// field on it): [`LibraryTable::dispatch`] already holds a mutable
/// borrow of one registered handler slot while calling it, and
/// `OpenLibrary`'s handler needs to *insert* a new library base while
/// running as one of those very slots -- keeping this registry as an
/// independent value on [`Runtime`] (passed into [`HandlerContext`]
/// alongside, not through, the table) sidesteps that aliasing
/// conflict entirely.
#[derive(Debug, Clone, Default)]
pub struct LibraryRegistry {
    known: HashMap<String, (u32, LibraryKind)>,
    fakes: Vec<FakeLibrary>,
}

impl LibraryRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a real, emulated library base (e.g. `dos.library` ->
    /// [`DOS_LIBRARY_BASE`]).
    pub fn register_real(&mut self, name: &str, base: u32) {
        self.known
            .insert(name.to_string(), (base, LibraryKind::Real));
    }

    /// Looks up a previously-registered (real or fake) library base by
    /// name.
    pub fn lookup(&self, name: &str) -> Option<(u32, LibraryKind)> {
        self.known.get(name).copied()
    }

    /// Records a newly auto-created fake library base and its
    /// jump-table block size, so future `OpenLibrary` calls for the
    /// same name reuse it, and [`Self::resolve_fake`] can name it.
    fn register_fake(&mut self, name: &str, base: u32, size: u32) {
        self.known
            .insert(name.to_string(), (base, LibraryKind::Faked));
        self.fakes.push(FakeLibrary {
            name: name.to_string(),
            base,
            size,
        });
    }

    /// Given the guest address a trapped call landed at, finds the fake
    /// library (if any) whose jump-table block contains it, returning
    /// its name and the LVO offset (`pc - base`, always `<= 0`) within
    /// it.
    fn resolve_fake(&self, pc: u32) -> Option<(&str, i32)> {
        self.fakes
            .iter()
            .find(|f| pc >= f.base.wrapping_sub(f.size) && pc < f.base)
            .map(|f| (f.name.as_str(), pc as i64 as i32 - f.base as i32))
    }
}

/// `exec.library`'s `OpenLibrary` handler (LVO -552): `A1` = pointer to
/// the library name (C string), `D0` = requested minimum version
/// (ignored -- this runtime doesn't track library versions). Returns
/// the library's base in `D0`.
///
/// # vamos escape hatch
///
/// Ported from vamos's own behavior (see `docs/plan.md`'s "vamos escape
/// hatches" note): opening a library this runtime doesn't implement
/// never fails here. Instead, a fake base is auto-created on first
/// request (and reused on repeat requests for the same name, via
/// [`LibraryRegistry`]) -- a block of [`FAKE_LIB_JUMP_TABLE_SIZE`] bytes
/// carved from the guest heap, entirely prefilled with the shared
/// [`FAKE_LIB_SLOT`] opcode, with the base set to the end of that block
/// (so every plausible negative LVO offset lands inside it and traps).
/// A run therefore never fails at `OpenLibrary` time for an unknown
/// library -- only if/when the guest actually calls one of its vectors,
/// which [`fake_lib_vector_handler`] turns into a diagnostic naming the
/// library. This mirrors real (and vamos) behavior: many programs
/// `OpenLibrary` a handful of libraries speculatively and only call
/// into the ones that actually opened successfully.
fn open_library_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let name_ptr = ctx.cpu.address_register(AddressRegister(1));
    let name = String::from_utf8_lossy(&read_c_string(ctx.mem, name_ptr)).into_owned();
    open_library_common(ctx, &name)
}

/// `exec.library`'s `OldOpenLibrary` handler (LVO -408): the pre-V36
/// single-argument form of `OpenLibrary` (`A1` = library name, no
/// version). Shares [`open_library_common`] with [`open_library_handler`].
fn old_open_library_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let name_ptr = ctx.cpu.address_register(AddressRegister(1));
    let name = String::from_utf8_lossy(&read_c_string(ctx.mem, name_ptr)).into_owned();
    open_library_common(ctx, &name)
}

/// Shared `OpenLibrary`/`OldOpenLibrary` implementation: looks `name` up
/// in [`HandlerContext::registry`], auto-creating a fake base per the
/// vamos escape hatch (see [`open_library_handler`]) if it isn't there,
/// and writes the resulting base into `D0`.
fn open_library_common<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
    name: &str,
) -> Result<(), DispatchError> {
    if let Some((base, _kind)) = ctx.registry.lookup(name) {
        ctx.cpu.set_data_register(DataRegister(0), base);
        return Ok(());
    }

    let size = FAKE_LIB_JUMP_TABLE_SIZE;
    let block = ctx
        .heap
        .alloc(size)
        .map_err(|e| DispatchError::HandlerFailed {
            library: "exec.library".to_string(),
            lvo: -552,
            handler_name: "OpenLibrary".to_string(),
            message: format!("couldn't auto-create fake library {name:?}: {e}"),
        })?;
    let base = block.wrapping_add(size);

    let mut addr = block;
    while addr < base {
        ctx.mem.write_u16(addr, 0xA000 | FAKE_LIB_SLOT);
        addr = addr.wrapping_add(2);
    }

    ctx.registry.register_fake(name, base, size);
    ctx.cpu.set_data_register(DataRegister(0), base);
    Ok(())
}

/// `exec.library`'s `CloseLibrary` handler (LVO -414): `A1` = library
/// base. A no-op -- this runtime doesn't refcount library opens/closes,
/// and real `CloseLibrary` only returns a meaningful (non-`NULL`) `D0`
/// (a `BPTR` segList) when the close actually expunges the library from
/// memory, which never applies to anything faked or fixed-address here.
fn close_library_handler<C: Cpu>(_ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    Ok(())
}

/// The shared handler bound to [`FAKE_LIB_SLOT`]: every vector of every
/// auto-created fake library (see [`open_library_handler`]) traps here.
/// Resolves [`HandlerContext::pc`] back to the owning fake library's
/// name and LVO offset via [`LibraryRegistry::resolve_fake`], then fails
/// the call with a diagnostic naming it -- this is where the vamos
/// escape hatch's "clear diagnostic on first real use" guarantee is
/// actually produced.
fn fake_lib_vector_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let (library, lvo) = match ctx.registry.resolve_fake(ctx.pc) {
        Some((name, lvo)) => (name.to_string(), lvo),
        None => ("<unresolved fake library>".to_string(), 0),
    };
    Err(DispatchError::HandlerFailed {
        library,
        lvo,
        handler_name: "<unimplemented>".to_string(),
        message: "call into an auto-created fake library (OpenLibrary of a library name this \
                  runtime doesn't implement); this vector isn't emulated"
            .to_string(),
    })
}

/// The `dos.library` `PutStr` handler: writes the NUL-terminated string
/// pointed to by `D1` to `ctx.out` verbatim (no newline added; the guest
/// string already contains one if it wants one), and returns 0 (success)
/// in `D0`, matching real `PutStr`'s "0 on success, -1 (`EOF`) on
/// failure" contract. Never fails on the host side (a write error would
/// only happen if the sink itself errors, which we don't expect for an
/// in-memory buffer or stdout); it maps such a failure to `HandlerFailed`
/// rather than panicking.
///
/// Superseded in [`Runtime::new`]'s actual registration by T10's
/// [`crate::dosfile`]-based `PutStr` (written through `Output()`, per
/// the T7 table); kept here (`#[cfg(test)]`) because this module's own
/// tests still use it directly to exercise [`LibraryTable::register`]/
/// [`LibraryTable::register_by_name`] mechanics independent of
/// `DosState`.
#[cfg(test)]
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

/// Start-up configuration for [`Runtime::new`]: everything about a guest
/// program invocation that isn't already implied by the loaded hunks
/// themselves (the CPU and memory are supplied separately).
///
/// Introduced in T12 to replace the fixed `new(cpu, mem, entry)`
/// signature Phase 1 had -- the one deliberately cross-cutting API
/// change T12 owns (see `docs/plan.md`'s T12 entry): threading real
/// guest command-line arguments through, and deriving the heap's start
/// address from where the loaded program actually ends (see the
/// `guestmem` module docs) instead of Phase 1's fixed placeholder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartConfig {
    /// Guest address to start executing at (typically
    /// [`crate::loader::LoadResult::entry`]).
    pub entry: u32,
    /// The first guest address *after* every loaded hunk (typically
    /// [`crate::loader::LoadResult::end`]). The guest heap starts here
    /// (rounded up to a 4-byte boundary), so it never overlaps the
    /// loaded program image.
    pub load_end: u32,
    /// The guest program's command-line arguments (not including the
    /// program name itself, matching `argv[1..]` convention), passed
    /// per AmigaOS startup convention: joined with spaces into a single
    /// buffer allocated on the guest heap, `A0` = pointer to it, `D0` =
    /// its length (see [`Runtime::new`]'s doc for the exact framing).
    pub args: Vec<String>,
}

/// Ties a [`Cpu`] backend, its guest memory, and a [`LibraryTable`]
/// together to run a loaded program to completion.
///
/// Construction sets up:
/// - `A6` = [`DOS_LIBRARY_BASE`]. This is a compatibility seed, *not*
///   how a real AmigaOS program finds a library base: real startup code
///   reads [`EXEC_LIBRARY_BASE`] from [`ABS_EXEC_BASE_ADDR`] (guest
///   address 4) and calls `OpenLibrary("dos.library", 0)` itself, both
///   of which are also fully wired up (see [`open_library_handler`]).
///   `A6` is seeded anyway because the Phase 1 `hello` fixture (and its
///   tests) call straight into `-948(a6)` without ever calling
///   `OpenLibrary` first; T14 updates the fixtures to use the real
///   `OpenLibrary` flow, at which point this seed becomes redundant but
///   still harmless (real programs overwrite `A6` themselves before
///   using it).
/// - Guest address 4 ([`ABS_EXEC_BASE_ADDR`]) = [`EXEC_LIBRARY_BASE`].
/// - `A7` (the stack pointer) at the top of guest memory, with
///   [`EXIT_STUB_ADDR`] pre-pushed as the return address so the guest's
///   own final `rts` lands on the exit sentinel (see module docs).
/// - `A0`/`D0` = the guest command-line buffer/length (see
///   [`Runtime::new`]).
/// - `PC` = the program's entry point.
pub struct Runtime<C: Cpu> {
    cpu: C,
    mem: C::Memory,
    table: LibraryTable<C>,
    heap: GuestHeap,
    registry: LibraryRegistry,
    dos: DosState,
}

/// A single trapped library call, reported to an optional trace callback
/// so a CLI's `--verbose` flag can log it.
pub type TraceEvent = CallInfo;

impl<C: Cpu + 'static> Runtime<C> {
    /// Builds a runtime around an already-constructed CPU and loaded
    /// guest memory, per `config` (see [`StartConfig`]).
    ///
    /// The fake `dos.library` (`PutStr`) and `exec.library`
    /// (`OpenLibrary`/`OldOpenLibrary`/`CloseLibrary`) jump tables are
    /// registered automatically; see [`Runtime::library_table_mut`] to
    /// register additional handlers before calling [`Runtime::run`].
    ///
    /// # Guest command-line convention
    ///
    /// `config.args` is joined with single spaces and a trailing `'\n'`
    /// (the AmigaOS CLI command-line buffer convention: a program that
    /// parses its own arguments out of `A0`/`D0`, e.g. via `ReadArgs`,
    /// expects exactly this framing) into a buffer allocated on the
    /// guest heap, with one extra `NUL` byte written after the `'\n'`
    /// as a defensive terminator for anything that scans for one
    /// instead of trusting the length. `A0` is set to that buffer's
    /// address; `D0` is set to the buffer's length *including* the
    /// `'\n'` but *not* the extra `NUL`, matching the real convention.
    pub fn new(mut cpu: C, mut mem: C::Memory, config: StartConfig) -> Self {
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

        // dos.library file I/O (T10): Open/Close/Read/Write/Seek,
        // Input/Output, IoErr/SetIoErr, and PutStr (reimplemented on top
        // of Output() -- see crate::dosfile's module docs). Registered
        // unconditionally, by name through the T7 DOS_LVOS table; these
        // handlers work even before a Vfs is installed (see
        // Runtime::set_vfs) for everything except path-based calls.
        crate::dosfile::register_dos_handlers(&mut table, &mut mem);

        // exec.library: only the three LVOs T12 needs (OpenLibrary /
        // OldOpenLibrary / CloseLibrary) -- see EXEC_LIBRARY_BASE's doc
        // for the full reserved-region memory map. Looked up by name
        // through the generated EXEC_LVOS table (T7-style), same as
        // dos.library, so unknown-call diagnostics on this base resolve
        // to real function names too.
        table
            .register_by_name(
                &mut mem,
                EXEC_LIBRARY_BASE,
                crate::lvos::exec::EXEC_LVOS,
                "exec.library",
                "OpenLibrary",
                open_library_handler::<C>,
            )
            .expect("OpenLibrary is in EXEC_LVOS");
        table
            .register_by_name(
                &mut mem,
                EXEC_LIBRARY_BASE,
                crate::lvos::exec::EXEC_LVOS,
                "exec.library",
                "OldOpenLibrary",
                old_open_library_handler::<C>,
            )
            .expect("OldOpenLibrary is in EXEC_LVOS");
        table
            .register_by_name(
                &mut mem,
                EXEC_LIBRARY_BASE,
                crate::lvos::exec::EXEC_LVOS,
                "exec.library",
                "CloseLibrary",
                close_library_handler::<C>,
            )
            .expect("CloseLibrary is in EXEC_LVOS");

        // The shared fake-library-vector handler: bound once, to a slot
        // number every auto-created fake library's jump table reuses
        // (see FAKE_LIB_SLOT's docs). No opcode word is written here --
        // open_library_handler writes them, per fake library, on demand.
        table.register_fixed_slot(FAKE_LIB_SLOT, fake_lib_vector_handler::<C>);

        // AbsExecBase: guest address 4 holds EXEC_LIBRARY_BASE, the
        // pointer real startup code reads via `move.l 4,a6`. Written
        // after the sentinel prefill above so it isn't overwritten by
        // it (this is a plain data word, never a jump-table entry).
        mem.write_u32(ABS_EXEC_BASE_ADDR, EXEC_LIBRARY_BASE);

        let mut registry = LibraryRegistry::new();
        registry.register_real("dos.library", DOS_LIBRARY_BASE);
        registry.register_real("exec.library", EXEC_LIBRARY_BASE);

        // Exit sentinel: any A-line word works (we never decode it; the
        // exit path is short-circuited on address, not opcode), but using
        // a real slot number keeps the trap table self-consistent to
        // read/disassemble.
        mem.write_u16(EXIT_STUB_ADDR, 0xA000 | EXIT_SLOT);

        // Stack: a fixed-size region (see `guestmem::STACK_SIZE`) at the
        // top of guest memory, 4-byte aligned, with the exit sentinel
        // pre-pushed as the return address for the program's outermost
        // `rts`.
        let top = (mem.len() as u32) & !3;
        let sp = top.wrapping_sub(4);
        mem.write_u32(sp, EXIT_STUB_ADDR);

        // Heap: from the loaded program's end (rounded up) to the base
        // of the reserved stack region, so it never overlaps either.
        let stack_base = top.saturating_sub(STACK_SIZE) & !3;
        let mut heap = GuestHeap::new(config.load_end, stack_base);

        // Command-line buffer: args joined with spaces, '\n'-terminated
        // (the length reported in D0 includes this '\n'), plus one extra
        // NUL byte as a defensive terminator for code that scans instead
        // of trusting D0. Allocated on the heap built just above.
        let mut line = config.args.join(" ").into_bytes();
        line.push(b'\n');
        let line_len = line.len() as u32;
        let args_addr = heap
            .alloc(line_len + 1)
            .expect("guest heap has room for the command-line buffer");
        {
            let mut a = args_addr;
            for &b in &line {
                mem.write_u8(a, b);
                a = a.wrapping_add(1);
            }
            mem.write_u8(a, 0);
        }

        cpu.set_address_register(AddressRegister(7), sp);
        cpu.set_address_register(AddressRegister(6), DOS_LIBRARY_BASE);
        cpu.set_address_register(AddressRegister(0), args_addr);
        cpu.set_data_register(DataRegister(0), line_len);
        cpu.set_pc(config.entry);

        Self {
            cpu,
            mem,
            table,
            heap,
            registry,
            dos: DosState::new(None),
        }
    }

    /// Installs (or replaces) the [`Vfs`] used by `dos.library` path-based
    /// calls (`Open`, and T11's `Lock`/`Examine`/...). Without this,
    /// those calls fail cleanly with an `IoErr()` of
    /// [`crate::dosfile::ERROR_OBJECT_NOT_FOUND`] (see
    /// [`crate::dosfile`]'s module docs); `Input`/`Output`/`PutStr`/
    /// `IoErr`/`SetIoErr` work either way.
    pub fn set_vfs(&mut self, vfs: Vfs) {
        self.dos.vfs = Some(vfs);
    }

    /// Mutable access to the library table, so callers can register
    /// additional handlers (or override the default `PutStr`) before
    /// [`Runtime::run`].
    pub fn library_table_mut(&mut self) -> &mut LibraryTable<C> {
        &mut self.table
    }

    /// Mutable access to the guest heap, so handlers/tests can allocate
    /// guest-visible structures directly.
    pub fn heap_mut(&mut self) -> &mut GuestHeap {
        &mut self.heap
    }

    /// Replaces the runtime's guest heap wholesale -- e.g. to shrink or
    /// grow it relative to what [`StartConfig::load_end`] implied, or to
    /// swap in a heap with pre-existing allocations for a test. Note
    /// this does *not* move the already-allocated command-line buffer
    /// [`Runtime::new`] placed on the *old* heap; callers replacing the
    /// heap should generally do so before relying on `A0`/`D0`.
    pub fn set_heap(&mut self, heap: GuestHeap) {
        self.heap = heap;
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

                    let mut ctx = HandlerContext {
                        cpu: &mut self.cpu,
                        mem: &mut self.mem,
                        out,
                        heap: &mut self.heap,
                        registry: &mut self.registry,
                        pc: info.pc,
                        dos: &mut self.dos,
                    };
                    let call_info = self.table.dispatch(opcode, &mut ctx)?;
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
    /// only of `words`, loaded at `TRAP_TABLE_END`. Memory is sized
    /// generously (128 KiB) so the fixed 64 KiB stack region and a
    /// working heap both fit comfortably above the tiny test programs
    /// this helper loads (see `guestmem::STACK_SIZE`).
    fn runtime_with_program(words: &[u16]) -> Runtime<M68kCpu> {
        runtime_with_program_and_args(words, Vec::new())
    }

    /// As [`runtime_with_program`], but with explicit guest command-line
    /// arguments.
    fn runtime_with_program_and_args(words: &[u16], args: Vec<String>) -> Runtime<M68kCpu> {
        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, words);
        // Comfortably past any test program's code + inline data.
        let load_end = entry + 0x100;
        Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end,
                args,
            },
        )
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
        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: entry + 0x100,
                args: Vec::new(),
            },
        );

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
        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: entry + 0x100,
                args: Vec::new(),
            },
        );
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

    // --- T12: exec.library OpenLibrary/OldOpenLibrary/CloseLibrary,
    // process startup (A0/D0 args, heap placement, AbsExecBase) ---

    /// `movea.l #imm32,An` opcode (source: immediate long, destination:
    /// address register direct). The immediate follows as two words.
    fn movea_imm(n: u16) -> u16 {
        0x207C | (n << 9)
    }

    /// `movea.l Dx,An` opcode (source: data register direct,
    /// destination: address register direct).
    fn movea_dn(an: u16, dn: u16) -> u16 {
        0x2040 | (an << 9) | dn
    }

    /// `jsr <disp16>(An)` opcode. The 16-bit displacement follows as one
    /// word.
    fn jsr_disp16(an: u16) -> u16 {
        0x4EA8 | an
    }

    const MOVEQ_D0_0: u16 = 0x7000; // moveq #0,d0
    const RTS: u16 = 0x4E75;

    /// Appends a `movea.l #imm32,An` (3 words: opcode + hi + lo) to
    /// `words`.
    fn push_movea_imm(words: &mut Vec<u16>, an: u16, imm: u32) {
        words.push(movea_imm(an));
        words.push((imm >> 16) as u16);
        words.push(imm as u16);
    }

    /// Appends a `jsr <disp16>(An)` (2 words) to `words`.
    fn push_jsr(words: &mut Vec<u16>, an: u16, disp: i32) {
        words.push(jsr_disp16(an));
        words.push(disp as u16);
    }

    #[test]
    fn open_library_of_dos_returns_dos_base() {
        // A1 = "dos.library"\0, A6 = EXEC_LIBRARY_BASE, jsr OpenLibrary
        // (-552(a6)), rts -- D0 (and hence the exit code) is whatever
        // OpenLibrary put there.
        let entry = TRAP_TABLE_END;
        let name = b"dos.library\0";

        let mut words = Vec::new();
        push_movea_imm(&mut words, 1, 0); // A1 placeholder, patched below
        push_movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        push_jsr(&mut words, 6, -552); // OpenLibrary
        words.push(RTS);
        let str_addr = entry + (words.len() as u32) * 2;
        // Patch A1's immediate (the two words right after the movea
        // opcode at index 0) now that str_addr is known.
        words[1] = (str_addr >> 16) as u16;
        words[2] = str_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        crate::guestmem::write_c_string(&mut mem, str_addr, name);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: str_addr + name.len() as u32 + 4,
                args: Vec::new(),
            },
        );

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code as u32, DOS_LIBRARY_BASE,
            "OpenLibrary(\"dos.library\") should return DOS_LIBRARY_BASE in D0"
        );
    }

    #[test]
    fn open_library_of_unknown_name_auto_creates_fake_and_succeeds() {
        // OpenLibrary("xyz.library") must succeed (never fails at
        // OpenLibrary time, per the vamos escape hatch), returning some
        // fake base in D0; only calling a vector on that base fails.
        let entry = TRAP_TABLE_END;
        let name = b"xyz.library\0";

        let mut words = Vec::new();
        push_movea_imm(&mut words, 1, 0); // A1 placeholder
        push_movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        push_jsr(&mut words, 6, -552); // OpenLibrary("xyz.library") -> D0
        words.push(movea_dn(6, 0)); // A6 = D0 (the fake base)
        push_jsr(&mut words, 6, -6); // call an arbitrary vector on it
        words.push(RTS);
        let str_addr = entry + (words.len() as u32) * 2;
        words[1] = (str_addr >> 16) as u16;
        words[2] = str_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        crate::guestmem::write_c_string(&mut mem, str_addr, name);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: str_addr + name.len() as u32 + 4,
                args: Vec::new(),
            },
        );

        let mut out = Vec::new();
        let err = rt.run(&mut out, None).unwrap_err();
        match err {
            RuntimeError::Dispatch(DispatchError::HandlerFailed { library, lvo, .. }) => {
                assert_eq!(library, "xyz.library");
                assert_eq!(lvo, -6);
            }
            other => panic!("expected a HandlerFailed naming xyz.library, got {other:?}"),
        }
    }

    #[test]
    fn close_library_is_a_no_op() {
        // A1 = DOS_LIBRARY_BASE, A6 = EXEC_LIBRARY_BASE, jsr
        // CloseLibrary (-414(a6)); then explicitly zero D0 and exit, so
        // a clean 0 exit code proves CloseLibrary didn't error or crash
        // (it isn't expected to touch D0 itself).
        let mut words = Vec::new();
        push_movea_imm(&mut words, 1, DOS_LIBRARY_BASE);
        push_movea_imm(&mut words, 6, EXEC_LIBRARY_BASE);
        push_jsr(&mut words, 6, -414); // CloseLibrary
        words.push(MOVEQ_D0_0);
        words.push(RTS);

        let mut rt = runtime_with_program(&words);
        let mut out = Vec::new();
        let code = rt
            .run(&mut out, None)
            .expect("CloseLibrary should be a no-op, not error");
        assert_eq!(code, 0);
    }

    #[test]
    fn location_4_holds_exec_library_base() {
        let rt = runtime_with_program(&[RTS]);
        assert_eq!(
            rt.mem.read_u32(ABS_EXEC_BASE_ADDR),
            EXEC_LIBRARY_BASE,
            "guest address 4 (AbsExecBase) should hold EXEC_LIBRARY_BASE"
        );
    }

    #[test]
    fn a0_d0_hold_the_joined_newline_terminated_command_line() {
        let rt = runtime_with_program_and_args(&[RTS], vec!["foo".to_string(), "bar".to_string()]);
        let a0 = rt.cpu.address_register(AddressRegister(0));
        let d0 = rt.cpu.data_register(DataRegister(0));
        assert_eq!(d0, 8, "\"foo bar\\n\" is 8 bytes");
        let bytes: Vec<u8> = (0..d0).map(|i| rt.mem.read_u8(a0 + i)).collect();
        assert_eq!(bytes, b"foo bar\n");
        // A defensive NUL immediately follows, not counted in D0.
        assert_eq!(rt.mem.read_u8(a0 + d0), 0);
    }

    #[test]
    fn empty_args_still_produce_a_bare_newline_command_line() {
        let rt = runtime_with_program_and_args(&[RTS], Vec::new());
        let a0 = rt.cpu.address_register(AddressRegister(0));
        let d0 = rt.cpu.data_register(DataRegister(0));
        assert_eq!(d0, 1);
        assert_eq!(rt.mem.read_u8(a0), b'\n');
    }

    #[test]
    fn heap_starts_at_load_end() {
        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &[RTS]);
        let load_end = entry + 0x40; // already 4-byte aligned
        let rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end,
                args: Vec::new(),
            },
        );
        // The command-line buffer is the very first thing Runtime::new
        // allocates from the heap, so A0 doubles as a direct probe of
        // the heap's start address.
        let a0 = rt.cpu.address_register(AddressRegister(0));
        assert_eq!(a0, load_end);
    }
}
