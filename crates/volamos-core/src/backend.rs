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
use crate::m68kops::{self, ControlFlowOp};
use crate::memory::{AddressSpace, FlatMemory, StoreSize};

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
/// `crate::dispatch::GRAPHICS_LIBRARY_BASE`), then once more (this time
/// two `0x200` chunks at once) from `0x2600` for
/// `mathieeesingbas.library`/`mathieeesingtrans.library`'s real bases
/// (see `crate::dispatch::MATHIEEESINGBAS_LIBRARY_BASE`/
/// `MATHIEEESINGTRANS_LIBRARY_BASE`) -- same reasoning each time, then
/// once more from `0x2A00` for `SetFunction`'s stub pool (see
/// `crate::execsetfunc::STUB_POOL_BASE`) -- not a library base at all,
/// just scratch space for synthesized "old function" stubs -- then once
/// more (this time just 4 bytes -- one `ILLEGAL` opcode word plus one
/// padding word, rounded up to keep [`TRAP_TABLE_END`] 4-byte aligned,
/// which the heap-start rounding in `crate::dispatch::Runtime::new`'s
/// own doc relies on -- no library involved at all) from `0x2C00` for
/// [`crate::dispatch::FPU_DISABLED_060_DECLINE_STUB_ADDR`] (issue #167's
/// second half): the fixed-address stub `--cpu 68060 --no-fpu` pre-seeds
/// into vector 11's slot, so the `m68k` crate's internal 68060
/// FPU-disabled exception delivery (which bypasses
/// [`Cpu::take_hardware_exception`] entirely -- see that constant's doc
/// for the full mechanism) lands on a clean trap instead of falling
/// through the deliberately-zeroed exception-vector-table span into
/// [`crate::dispatch::CONTINUATION_STUB_ADDR`] by accident.
pub const TRAP_TABLE_SIZE: u32 = 0x2C04;

/// First guest address *after* the reserved trap table region
/// (exclusive). Guest code, data, and stack should live at or above this
/// address.
pub const TRAP_TABLE_END: u32 = TRAP_TABLE_BASE + TRAP_TABLE_SIZE;

impl AddressBus for FlatMemory {
    fn read_byte(&mut self, address: u32) -> u8 {
        self.count_read();
        AddressSpace::read_u8(self, address)
    }

    fn read_word(&mut self, address: u32) -> u16 {
        self.count_read();
        AddressSpace::read_u16(self, address)
    }

    fn read_long(&mut self, address: u32) -> u32 {
        self.count_read();
        AddressSpace::read_u32(self, address)
    }

    fn write_byte(&mut self, address: u32, value: u8) {
        self.count_write();
        if self.intercept_store(address, u32::from(value), StoreSize::Byte) {
            return;
        }
        AddressSpace::write_u8(self, address, value);
    }

    fn write_word(&mut self, address: u32, value: u16) {
        self.count_write();
        if self.intercept_store(address, u32::from(value), StoreSize::Word) {
            return;
        }
        AddressSpace::write_u16(self, address, value);
    }

    fn write_long(&mut self, address: u32, value: u32) {
        self.count_write();
        if self.intercept_store(address, value, StoreSize::Long) {
            return;
        }
        AddressSpace::write_u32(self, address, value);
    }

    /// Instruction-stream word read -- distinct from [`Self::
    /// read_word`] so `--cache`'s modeled instruction cache (issue
    /// #148, [`crate::memory::FlatMemory::enable_icache`]) only ever
    /// sees real opcode/extension-word fetches, never ordinary data
    /// reads. `count_read` still runs here exactly as it would have
    /// under the default (which delegates to `read_word`), so
    /// `--clock-mhz`'s access-count reporting is unaffected by this
    /// override existing at all.
    fn read_immediate_word(&mut self, address: u32) -> u16 {
        self.count_read();
        self.icache_note_instruction_fetch(address, 2);
        AddressSpace::read_u16(self, address)
    }

    /// As [`Self::read_immediate_word`], for a long instruction-stream
    /// read (a 4-byte immediate operand).
    fn read_immediate_long(&mut self, address: u32) -> u32 {
        self.count_read();
        self.icache_note_instruction_fetch(address, 4);
        AddressSpace::read_u32(self, address)
    }

    /// See [`m68k::AddressBus::last_fetch_was_cached`]'s doc; backed by
    /// [`FlatMemory::icache_last_fetch_was_cached`].
    fn last_fetch_was_cached(&self) -> bool {
        self.icache_last_fetch_was_cached()
    }

    /// See [`m68k::AddressBus::begin_instruction_fetches`]'s doc;
    /// backed by [`FlatMemory::icache_begin_instruction_fetches`].
    fn begin_instruction_fetches(&mut self) {
        self.icache_begin_instruction_fetches();
    }

    /// See [`m68k::AddressBus::instruction_fetches_were_cached`]'s
    /// doc; backed by [`FlatMemory::icache_fetches_were_cached`].
    fn instruction_fetches_were_cached(&self) -> bool {
        self.icache_fetches_were_cached()
    }

    /// The whole guest address space is one plain, side-effect-free
    /// `Vec<u8>` (see [`FlatMemory`]'s doc comment) -- exactly what the
    /// `jit` feature's [`m68k::CpuCore::run_batch`] fast path needs, and
    /// harmless to expose unconditionally since `step`/`execute` never
    /// call this hook regardless of feature flags (see
    /// [`m68k::AddressBus::fast_mem`]'s doc comment).
    ///
    /// **Except** when a sanitizer shadow map is installed
    /// ([`FlatMemory::shadow`] is `Some`, see the CLI's `--sanitize`
    /// flag and `crate::sanitize`'s module doc): this returns `None` in
    /// that case, unconditionally, and this is the single most
    /// important correctness detail in the whole sanitizer feature.
    /// `fast_mem` hands the `m68k` crate a raw pointer straight into the
    /// backing `Vec<u8>`; `run_batch`'s trace-JIT fast path reads and
    /// writes guest memory through that pointer directly, completely
    /// bypassing `read_byte`/`write_byte`/`read_word`/... above -- which
    /// is exactly where every shadow-map check lives. If `fast_mem` kept
    /// returning `Some` once sanitizing was turned on, the JIT would
    /// silently defeat every single check this feature exists to
    /// perform, and a `--sanitize` run would look clean no matter what
    /// the guest actually did. `crate::memory`'s `--sanitize` wiring
    /// also forces the JIT off outright (`M68kCpu::set_jit(false)`) for
    /// the same reason, belt-and-braces; this `None` is what makes that
    /// actually load-bearing rather than merely a hint the JIT could
    /// ignore.
    // The same reasoning applies to an installed `StorePort` (the
    // `--uae-debug` port): a raw-pointer store would skip the
    // interception in `write_byte`/`write_word`/`write_long` above.
    fn fast_mem(&mut self) -> Option<m68k::FastMem> {
        if self.shadow().is_some() || self.has_store_ports() {
            return None;
        }
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
    /// The clock rate (in Hz) [`Self::run_via_cycles`] is deriving
    /// emulated time from, if `--clock-mhz` was given -- `Some` makes
    /// [`Cpu::run`] switch from `run_batch` to
    /// [`m68k::CpuCore::run_for_cycles`] entirely, ignoring [`Self::jit`]
    /// (see [`Self::set_clock_mhz`]'s doc for why the two are mutually
    /// exclusive at the CLI layer). `None` (the default) is this
    /// backend's behavior before issue #102: no cycle counting at all,
    /// `run_batch` as always.
    clock_hz: Option<f64>,
    /// Real m68k cycles consumed so far by [`Self::run_via_cycles`] --
    /// see [`Cpu::emulated_cycles`]. Only advanced while
    /// [`Self::clock_hz`] is `Some`; stays `0` otherwise, matching
    /// [`Cpu::emulated_cycles`]'s documented "0 means never asked"
    /// default.
    cycles: u64,
    /// Instructions retired so far by [`Self::run_via_cycles`] -- see
    /// [`Cpu::emulated_instructions`]. Advanced under exactly the same
    /// condition as [`Self::cycles`], and from the same
    /// `CycleBatchResult`, so the two are always counted over the same
    /// span of execution and their ratio is meaningful.
    instructions: u64,
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

    /// Installs (`Some(mhz)`) or clears (`None`) an emulated clock rate
    /// for `timer.device`'s `ReadEClock` -- the CLI's `--clock-mhz`
    /// flag (issue #102). `mhz` is a clock rate in megahertz (fractional
    /// values allowed, e.g. Copperline's own A600/Gayle configuration
    /// models a 25 MHz 68000 as `clock_mhz = 25.0`); stored internally
    /// as a plain Hz rate so [`Self::run_via_cycles`] and
    /// [`crate::exectask::read_eclock_handler`] don't need to repeat the
    /// `* 1_000_000.0` conversion.
    ///
    /// The point of this mode is reproducible, host-load-independent
    /// compiler A/B benchmarks: [`Cpu::run`]'s normal
    /// [`m68k::CpuCore::run_batch`] path (`--jit` or not) never surfaces
    /// a cycle count at all (see this module's docs on why
    /// `m68k::BatchResult` has no `cycles` field), so with `Some` here
    /// [`Cpu::run`] switches to [`Self::run_via_cycles`] instead, which
    /// calls [`m68k::CpuCore::run_for_cycles`] -- the crate's
    /// transaction-exact, `precise_bus`-stepping path -- and accumulates
    /// the real cycle counts it returns into [`Self::cycles`]
    /// ([`Cpu::emulated_cycles`]). `read_eclock_handler` then reports
    /// `cycles / hz` seconds of *emulated* time instead of host
    /// wall-clock time.
    ///
    /// This is unconditional and mutually exclusive with the JIT at the
    /// CLI layer (`main.rs` refuses `--clock-mhz` together with an
    /// explicit `--jit`), not something this method itself arbitrates:
    /// `run_batch`'s trace JIT has nothing a cycle count could be
    /// derived from, so there is no sensible way to honor both at once.
    /// Note this is *unlike* `--sanitize` forcing the JIT off on its own
    /// (see [`AddressBus::fast_mem`]'s doc on `FlatMemory`) -- there,
    /// forcing is safe because `--no-jit` is an exact, not merely
    /// approximate, substitute for what `--sanitize` needs. Here there
    /// is no such fallback (`run_batch` has literally no cycle count to
    /// hand back under any configuration), so the CLI refuses the
    /// combination outright rather than silently picking a winner.
    ///
    /// `--clock-mhz` is *also* refused together with `--sanitize`, for
    /// a related but distinct reason: [`Self::run_via_cycles`] (the path
    /// this switches [`Cpu::run`] to) never calls the sanitizer's
    /// per-instruction hooks at all, so combining the two wouldn't just
    /// be imprecise, it would leave the shadow call stack and below-`A7`
    /// tracking silently stale while `--sanitize` still reports as if
    /// everything had been checked -- see `main.rs`'s
    /// `check_clock_mhz_sanitize` for the CLI-layer refusal and
    /// [`Self::run_via_cycles`]'s own doc for the mechanical reason.
    ///
    /// Roughly 2.2x slower than `--no-jit` and 7x slower than `--jit` in
    /// measurement (CoreMark 1.0, `~/src/external/coremark/
    /// coremark.amiga`, `--cpu 68020`, host wall-clock throughput:
    /// ~217 iterations/sec for `--no-jit`, ~97 for `--clock-mhz 25`,
    /// ~680 for `--jit` -- see [`Self::run_via_cycles`]'s doc for the
    /// full measurement methodology), which is expected and not a bug
    /// to chase -- `run_for_cycles` tracks cycle-accurate bus/prefetch
    /// state that this otherwise non-cycle-accurate runtime never
    /// needed before. Opt-in, for benchmarking only.
    pub fn set_clock_mhz(&mut self, mhz: Option<f64>) {
        self.clock_hz = mhz.map(|mhz| mhz * 1_000_000.0);
    }

    /// Flips the `m68k` crate's `CpuCore::emulate_unimplemented_060` flag
    /// -- the CLI's `--fpsp-emulate` (issue #129). `false` (this struct's
    /// default, via [`Self::new`]/[`Self::with_config`]) matches real
    /// 68060 hardware: the instructions a real 68060 removed from
    /// silicon -- most FPU transcendentals (FSIN/FCOS/FATAN/FETOX/FLOGN/
    /// FMOD/FSCALE/FINT/FGETEXP/FGETMAN/FSINCOS/...) plus a handful of
    /// integer ops (MOVEP, CHK2/CMP2, misaligned CAS/CAS2, 64-bit
    /// MUL/DIV) -- take an "unimplemented instruction" exception instead
    /// of executing, exactly as on real silicon; a real AmigaOS machine
    /// only keeps 68040-FP-targeted binaries working because
    /// `68060.library`'s FPSP/ISP kernels install exception-vector
    /// handlers at boot (via `SetPatch`) to emulate them in software --
    /// something volamos, with no persistent cross-process boot state,
    /// has no equivalent of (see issue #129's discussion of why loading
    /// a real `68060.library` isn't viable here).
    ///
    /// `true` instead asks the crate to execute those same instructions
    /// natively, using its own built-in reference implementation
    /// (`m68k::fpu::transcendental` backing the FPU subset) rather than
    /// trapping -- an opt-in convenience for running an unmodified
    /// 68040-FP binary under `--cpu 68060` without needing any real
    /// `68060.library`/`fpsp.library` file or persistent boot state.
    /// Deliberately **not** the default: real-hardware-faithful trapping
    /// is what every other `--cpu` model already does here, and this
    /// flag only exists to override that, one run at a time.
    pub fn set_emulate_unimplemented_060(&mut self, on: bool) {
        self.core.emulate_unimplemented_060 = on;
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
        Self {
            core,
            jit: false,
            clock_hz: None,
            cycles: 0,
            instructions: 0,
        }
    }
}

/// How many bytes of guest address space `cpu_type` can actually reach
/// (issue #98): `0x100_0000` (16 MiB) for the models with a 24-bit
/// address bus -- the 68000 and 68010 -- and the full `0x1_0000_0000`
/// for the 32-bit ones.
///
/// This exists because an address space *larger* than this is not merely
/// wasteful, it is broken: the guest stack sits at the top of the
/// address space, so a 32 MiB space on a 68000 puts `A7` somewhere the
/// CPU cannot express. The address wraps to 24 bits, a `JSR` into a
/// library pushes its return address into low memory instead, and the
/// `RTS` pops whatever happened to be there -- which is faithful to the
/// hardware, and useless. The CLI rejects that combination up front
/// rather than letting a program die several calls later with a
/// baffling "continuation stub trapped" message; see the `--ram` doc in
/// `main.rs`.
///
/// **Asked of the `m68k` crate rather than hardcoded here.** The crate
/// owns `CpuCore::address_mask` and sets it per model, so the one
/// authority on which models wrap is the emulator that does the
/// wrapping. A table duplicated here would be a second source of truth
/// that silently goes stale the next time the crate refines a model --
/// the 68EC020, for instance, has a 24-bit *external* bus on real
/// silicon, which the crate does not currently model, and volamos
/// should follow the crate's behavior rather than reject a
/// configuration that demonstrably works today.
pub fn addressable_bytes(cpu_type: CpuType) -> u64 {
    let mut core = CpuCore::new();
    core.set_cpu_type(cpu_type);
    u64::from(core.address_mask) + 1
}

/// Whether `cpu_type` performs a long store as two word bus cycles
/// (68000, 68010 and the 68070 -- 16-bit data bus), which is what UAE's
/// debug sprintf port sees and latches (see [`crate::uaedebug`]).
pub fn splits_long_stores(cpu_type: CpuType) -> bool {
    matches!(
        cpu_type,
        CpuType::M68000 | CpuType::M68010 | CpuType::SCC68070
    )
}

impl Default for M68kCpu {
    fn default() -> Self {
        Self::new()
    }
}

impl M68kCpu {
    /// Pre-instruction sanitizer hook: classifies the instruction about
    /// to execute at `pc` and, if it is a subroutine return, validates
    /// the return address still sitting in its stack slot against the
    /// shadow call stack. Returns the classification so
    /// [`Self::sanitize_after_instruction`] knows whether a call just
    /// pushed a frame. A no-op returning [`ControlFlowOp::Other`] when
    /// no shadow map is installed.
    ///
    /// The return address must be checked *before* the instruction
    /// executes, while `A7` still points at the slot holding it -- once
    /// the return has run, the stack pointer has moved past it and the
    /// evidence of corruption is gone.
    ///
    /// The opcode word and the slot are read with
    /// [`FlatMemory::peek_u16`]/[`FlatMemory::peek_u32`], never through
    /// [`AddressSpace`]: these are the sanitizer inspecting memory on
    /// its own behalf, and routing them through the checked path would
    /// both invent violations and heal `Uninit` bytes the guest never
    /// wrote. See those methods' docs.
    fn sanitize_before_instruction(&mut self, mem: &mut FlatMemory, pc: u32) -> ControlFlowOp {
        if mem.shadow().is_none() {
            return ControlFlowOp::Other;
        }

        let op = m68kops::classify(mem.peek_u16(pc));
        // Where the return address sits relative to A7 differs per
        // instruction, and getting it wrong would compare the wrong
        // bytes and invent corruption reports:
        //
        // - `RTS`/`RTD` pop the PC straight off the top of the stack.
        // - `RTR` pops the condition-code register first, so the return
        //   address is one word further up.
        // - `RTE` returns from an exception, not a subroutine: its
        //   frame is a status word plus PC (and, on 68010+, a
        //   format/vector word), and nothing put it there via `JSR`.
        //   Deliberately not checked -- the shadow call stack's own
        //   reconciliation discards the frames it unwinds.
        let slot_offset = match op {
            ControlFlowOp::Rts | ControlFlowOp::Rtd => Some(0),
            ControlFlowOp::Rtr => Some(2),
            _ => None,
        };
        if let Some(offset) = slot_offset {
            let sp = self
                .address_register(AddressRegister(7))
                .wrapping_add(offset);
            let actual = mem.peek_u32(sp);
            if let Some(shadow) = mem.shadow_mut() {
                shadow.check_return(sp, actual);
            }
        }
        op
    }

    /// Post-instruction sanitizer hook: records a freshly-pushed return
    /// address if `op` was a call, then republishes the stack pointer so
    /// the below-SP poisoning tracks the frame that just appeared or
    /// disappeared.
    ///
    /// Reading the pushed return address back off the stack here --
    /// rather than deriving it from the instruction -- is deliberate,
    /// and is why `crate::m68kops` needs no effective-address decoding
    /// at all. A `JSR`'s target can be any control addressing mode,
    /// with extension words whose length varies (and on 68020+ can nest
    /// through memory indirection), so computing the return address
    /// from the encoding means reimplementing a chunk of the CPU. The
    /// CPU has just done it for us: whatever `A7` now points at *is*
    /// the return address it pushed.
    fn sanitize_after_instruction(&mut self, mem: &mut FlatMemory, op: ControlFlowOp) {
        if mem.shadow().is_none() {
            return;
        }

        let sp = self.address_register(AddressRegister(7));
        if matches!(op, ControlFlowOp::Jsr | ControlFlowOp::Bsr) {
            let pushed = mem.peek_u32(sp);
            if let Some(shadow) = mem.shadow_mut() {
                shadow.record_call(sp, pushed);
            }
        }
        if let Some(shadow) = mem.shadow_mut() {
            shadow.update_stack_pointer(sp);
        }
    }

    /// The `--clock-mhz` execution path: runs via
    /// [`m68k::CpuCore::run_for_cycles`] instead of `run_batch`,
    /// accumulating real emulated cycles into [`Self::cycles`] as it
    /// goes -- see [`Self::set_clock_mhz`]'s doc for why this path
    /// exists and what it costs. Only called from [`Cpu::run`], and only
    /// once [`Self::clock_hz`] is `Some`.
    ///
    /// Every trap/halt exit this runtime cares about is surfaced
    /// identically to the `run_batch` path in [`Cpu::run`] below --
    /// [`m68k::CycleBatchExit`] matches [`m68k::BatchExit`] one-for-one
    /// plus one extra variant, `BoundaryRequested` (see below).
    ///
    /// `run_for_cycles`'s budget parameter is a plain `i32`, unlike
    /// `run_batch`'s `u32::MAX`-as-"unbounded" convention, so there is
    /// no single call that can express "run until something interesting
    /// happens" the way the JIT path's batch size does. Each call is
    /// simply capped at `i32::MAX` cycles instead (a hair over three and
    /// a half minutes of emulated time even at a generous 10 MHz, far
    /// longer than any single guest instruction sequence between two
    /// library-call traps takes in practice) and re-issued on
    /// `BudgetExhausted`, exactly mirroring how [`Cpu::run`]'s
    /// `run_batch` loop already treats its own `BudgetExhausted` exit as
    /// "nothing happened yet, keep going".
    ///
    /// This runtime never installs the sanitizer's per-instruction hooks
    /// ([`Self::sanitize_before_instruction`]/
    /// [`Self::sanitize_after_instruction`]) here, which is why the CLI
    /// (`main.rs`'s `check_clock_mhz_sanitize`) refuses `--clock-mhz`
    /// together with `--sanitize` outright rather than letting a caller
    /// combine them: the shadow map's own byte-level checks (routed
    /// through ordinary [`AddressSpace`] reads/writes, which
    /// `run_for_cycles` always uses -- it has no `fast_mem` raw-pointer
    /// fast path to bypass them) would still run, but the return-address
    /// and below-`A7` bookkeeping that needs strict one-instruction
    /// granularity would see many instructions retire between
    /// publications -- a stale shadow call stack and PC-attributed
    /// violations pinned to whatever address happened to be current
    /// several cycle-batches ago, while `--sanitize` still prints a
    /// normal-looking summary as if every instruction had been checked.
    /// A caller that reaches this method directly (bypassing the CLI)
    /// with a shadow map installed gets exactly that silently degraded
    /// behavior -- this method itself does not (and, short of adopting
    /// `run_batch`'s own shadow-map-forces-batches-of-one treatment,
    /// cannot cheaply) guard against it; only the CLI's explicit refusal
    /// does.
    ///
    /// # Measurement methodology (for [`Self::set_clock_mhz`]'s
    ///   perf numbers)
    ///
    /// [CoreMark 1.0](https://github.com/eembc/coremark)
    /// (`~/src/external/coremark/coremark.amiga`, `-O2 -m68020
    /// -msoft-float`), run under `--cpu 68020`. CoreMark self-calibrates
    /// its own iteration count against `ReadEClock`, and self-reports
    /// "iterations/sec" computed from the same clock -- both entirely
    /// valid for `--no-jit`/`--jit` (host wall-clock `ReadEClock`), but
    /// meaningless for `--clock-mhz` (which redefines what `ReadEClock`
    /// measures). So every figure here is host wall-clock throughput
    /// instead: completed iterations (as CoreMark reports them) divided
    /// by real elapsed seconds around the whole process (`time`), three
    /// runs per mode, averaged.
    fn run_via_cycles(&mut self, mem: &mut FlatMemory) -> StopReason {
        use m68k::CycleBatchExit;

        const CYCLE_BUDGET: i32 = i32::MAX;

        loop {
            let pc = self.pc();
            if pc as usize >= AddressSpace::len(mem) {
                return StopReason::PcOutOfBounds { pc };
            }
            let result = self.core.run_for_cycles(mem, CYCLE_BUDGET);
            // `cycles` is documented as "actual CPU cycles consumed" for
            // a positive budget, so this is never negative in practice;
            // `max(0)` is just cheap insurance against ever underflowing
            // the u64 accumulator if that documented behavior changes.
            self.cycles = self.cycles.saturating_add(result.cycles.max(0) as u64);
            self.instructions = self
                .instructions
                .saturating_add(u64::from(result.instructions));
            match result.exit {
                CycleBatchExit::BudgetExhausted => continue,
                // volamos's `FlatMemory` implements no `sync`/bus-
                // boundary-request mechanism at all (see this module's
                // `AddressBus` impl), so nothing volamos ever hands the
                // `m68k` crate as a bus can actually produce this exit.
                // If a future crate version ever raised one anyway, the
                // only sane response is the same as `BudgetExhausted`:
                // there is no trap/halt to report, so just keep running.
                CycleBatchExit::BoundaryRequested => continue,
                CycleBatchExit::Stopped => return StopReason::Halted,
                CycleBatchExit::AlineTrap { opcode } => {
                    return StopReason::Trap(TrapInfo {
                        kind: TrapKind::ALine { opcode },
                        pc: self.core.ppc,
                    });
                }
                CycleBatchExit::FlineTrap { opcode } => {
                    return StopReason::Trap(TrapInfo {
                        kind: TrapKind::FLine { opcode },
                        pc: self.core.ppc,
                    });
                }
                CycleBatchExit::TrapInstruction { trap_num } => {
                    return StopReason::Trap(TrapInfo {
                        kind: TrapKind::Trap { trap_num },
                        pc: self.core.ppc,
                    });
                }
                CycleBatchExit::Breakpoint { bp_num } => {
                    return StopReason::Trap(TrapInfo {
                        kind: TrapKind::Breakpoint { bp_num },
                        pc: self.core.ppc,
                    });
                }
                CycleBatchExit::IllegalInstruction { opcode } => {
                    return StopReason::Trap(TrapInfo {
                        kind: TrapKind::Illegal { opcode },
                        pc: self.core.ppc,
                    });
                }
            }
        }
    }
}

impl Cpu for M68kCpu {
    type Memory = FlatMemory;

    fn step(&mut self, mem: &mut Self::Memory) -> StopReason {
        // Publish the PC about to execute into the sanitizer shadow map
        // (a no-op when no shadow map is installed) -- see
        // `crate::sanitize::ShadowMap::set_current_pc`'s doc for why
        // this is the run loop's job: this is the one place that knows
        // "this PC is about to execute" *before* the instruction (and
        // any host-side library call it traps into) makes its memory
        // accesses, so every violation from here until the next step
        // is attributed to it.
        if let Some(shadow) = mem.shadow_mut() {
            shadow.set_current_pc(self.pc());
        }
        let op = self.sanitize_before_instruction(mem, self.pc());
        let result = self.core.step(mem);
        self.sanitize_after_instruction(mem, op);
        match result {
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

    /// Runs via [`m68k::CpuCore::run_batch`] rather than stepping one
    /// instruction at a time through [`Cpu::step`]/`core.step` -- every
    /// trap/halt this runtime cares about is still surfaced at exactly
    /// the same boundary the old per-instruction loop stopped at (see
    /// [`m68k::BatchExit`]'s doc comment: traps are reported, never
    /// taken as hardware exceptions, matching [`StepResult`]
    /// one-for-one).
    ///
    /// The batch size is the only difference [`Self::jit`] makes here:
    /// when set, `max_instructions` is unbounded (`u32::MAX`), letting
    /// the crate's trace JIT compile hot backward-branch loops; when
    /// unset, it's `1`, so this still executes and reports one
    /// instruction at a time (preserving `--no-jit`'s per-instruction
    /// granularity as a correctness reference against `--jit`) while
    /// still getting `run_batch`'s other, independent speedup: it runs
    /// with `precise_bus` off and raw-pointer `fast_mem` access, unlike
    /// `core.step`, which always tracks cycle-accurate bus/fetch-cache
    /// state that this non-cycle-accurate runtime never uses. A
    /// `BudgetExhausted` exit (the batch ending without a trap/halt --
    /// always, in `--no-jit` mode's batch-of-1) just resumes the batch
    /// loop rather than returning early.
    fn run(&mut self, mem: &mut Self::Memory) -> StopReason {
        // `--clock-mhz` (issue #102) takes over the whole run loop: see
        // `Self::run_via_cycles`'s doc for why `run_batch` (`self.jit`
        // either way) can't participate in cycle-derived `ReadEClock`
        // timing at all, and why the CLI refuses to let both be
        // requested at once rather than picking a winner here.
        if self.clock_hz.is_some() {
            return self.run_via_cycles(mem);
        }

        use m68k::BatchExit;

        // An installed shadow map forces batches of one instruction even
        // with the JIT requested. `fast_mem` returning `None` already
        // keeps the *checks* correct under a large batch, but the PC
        // published below is only republished once per batch, so a
        // multi-instruction batch would attribute every violation in it
        // to the batch's first PC -- a silently misleading report, which
        // for a debugging tool is worse than a slow one. The CLI also
        // forces `--sanitize` to turn the JIT off, but deriving it here
        // too means a caller that installs a shadow map directly
        // (bypassing the CLI) still gets precise attribution rather
        // than depending on remembering to pair the two flags.
        let max_instructions = if self.jit && mem.shadow().is_none() && !mem.has_store_ports() {
            u32::MAX
        } else {
            1
        };

        loop {
            let pc = self.pc();
            if pc as usize >= AddressSpace::len(mem) {
                return StopReason::PcOutOfBounds { pc };
            }
            // See `Cpu::step`'s matching comment -- same "publish before
            // executing" reasoning applies per batch here.
            if let Some(shadow) = mem.shadow_mut() {
                shadow.set_current_pc(pc);
            }
            // With a shadow map installed `max_instructions` is 1 (see
            // above), so these per-instruction hooks really do bracket
            // exactly one instruction here, same as in `step`. They are
            // no-ops when no shadow map is installed, which is what
            // keeps the unsanitized batch path untouched.
            let op = self.sanitize_before_instruction(mem, pc);
            let result = self.core.run_batch(mem, max_instructions, &[]);
            self.sanitize_after_instruction(mem, op);
            match result.exit {
                BatchExit::BudgetExhausted => continue,
                BatchExit::Stopped => return StopReason::Halted,
                BatchExit::WatchedPc { .. } => {
                    unreachable!("no watch_pcs are ever passed to run_batch")
                }
                BatchExit::BoundaryRequested => {
                    unreachable!(
                        "FlatMemory never overrides batch_boundary, whose default \
                         implementation always returns false"
                    )
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

    fn push_exception_frame(&mut self, mem: &mut Self::Memory, return_pc: u32) {
        let old_sr = self.core.get_sr();
        // Supervisor mode, on the supervisor stack, without trace: what
        // exception entry does before it writes the frame.
        self.core.set_sr((old_sr | 0x2000) & !0xC000);
        let top = self.address_register(AddressRegister(7));
        let size = if self.core.cpu_type == CpuType::M68000 {
            6
        } else {
            8
        };
        let sp = top.wrapping_sub(size);
        self.set_address_register(AddressRegister(7), sp);
        // Publish the push to the sanitizer before writing the frame,
        // as its own per-instruction hook does for a guest push.
        if let Some(shadow) = mem.shadow_mut() {
            shadow.update_stack_pointer(sp);
        }
        AddressSpace::write_u16(mem, sp, old_sr);
        AddressSpace::write_u32(mem, sp.wrapping_add(2), return_pc);
        if size == 8 {
            AddressSpace::write_u16(mem, sp.wrapping_add(6), 0x0020);
        }
    }

    fn emulated_cycles(&self) -> u64 {
        self.cycles
    }

    fn emulated_instructions(&self) -> u64 {
        self.instructions
    }

    fn clock_hz(&self) -> Option<f64> {
        self.clock_hz
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
    fn fast_mem_returns_none_once_the_sanitizer_is_enabled() {
        // The single most important correctness detail in the sanitizer
        // feature: fast_mem must stop handing out a raw pointer once a
        // shadow map is installed, or the JIT's fast path would bypass
        // every check -- see AddressBus::fast_mem's doc comment on
        // FlatMemory.
        let mut mem = FlatMemory::new(0x100);
        assert!(AddressBus::fast_mem(&mut mem).is_some());

        mem.enable_sanitizer();

        assert!(AddressBus::fast_mem(&mut mem).is_none());
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

    // --- --fpsp-emulate (issue #129) ---
    //
    // MOVEP is one of the instructions a real 68060 removed from
    // silicon (see `M68kCpu::set_emulate_unimplemented_060`'s doc);
    // unlike the F-line/A-line traps this `backend` module routes
    // through `Cpu::take_hardware_exception` itself, the `m68k` crate
    // takes this one as a genuine CPU-level hardware exception directly
    // inside `step` (reading the real vector 61 -- `UNIMPLEMENTED_
    // INTEGER` -- from guest memory and jumping `PC` there), the same
    // way real silicon does. volamos never populates that vector table
    // slot (there is no `68060.library`/`SetPatch` here to do it), so
    // on a fresh `FlatMemory` it reads back `0` -- a deterministic,
    // distinctive "the exception was taken" signature these tests use
    // instead of decoding `m68k`'s own internal step-result variants.

    /// A `MOVEP.L (d16,A0), D0` opcode (mem-to-register, long): one of
    /// the instructions `trap_unimpl_060` traps for real 68060 fidelity
    /// -- see `m68k::core::decode`'s dispatch_group_0, which checks this
    /// *before* consuming the following displacement word, so it's
    /// enough to test standalone.
    const MOVEP_L_MEM_TO_D0: u16 = 0x0148;

    #[test]
    fn movep_traps_on_m68060_by_default() {
        // Default `emulate_unimplemented_060 = false` -- real-hardware
        // fidelity -- must make MOVEP take the real vector-61 hardware
        // exception instead of executing, exactly like real 68060
        // silicon.
        let mut cpu = M68kCpu::with_config(CpuType::M68060, false);
        cpu.set_pc(TRAP_TABLE_END);
        let mut mem = FlatMemory::new(0x3000);
        cpu.set_address_register(AddressRegister(7), 0x3000);
        // Displacement word (0) immediately follows the opcode; MOVEP
        // traps before ever reading it, so its value doesn't matter.
        load_words(&mut mem, TRAP_TABLE_END, &[MOVEP_L_MEM_TO_D0, 0x0000]);
        cpu.set_data_register(DataRegister(0), 0xDEAD_BEEF);

        cpu.step(&mut mem);

        // `take_exception` jumps PC to whatever address vector 61 holds
        // in guest memory; volamos never installs a real handler there
        // (no SetPatch equivalent -- see issue #129), so a fresh,
        // zero-filled `FlatMemory` reads back exactly 0. That's the
        // real-hardware-exception signature this test is checking for
        // -- the opcode's own fallthrough PC would be
        // `TRAP_TABLE_END + 4`, never 0.
        assert_eq!(
            cpu.pc(),
            0,
            "expected the real vector-61 hardware exception to fire (PC jumped to the vector \
             table's -- empty -- contents), but MOVEP executed instead"
        );
        assert_eq!(
            cpu.data_register(DataRegister(0)),
            0xDEAD_BEEF,
            "D0 must be untouched -- the trap fires before MOVEP ever reads memory into it"
        );
    }

    #[test]
    fn movep_executes_natively_on_m68060_with_emulate_unimplemented_060() {
        // Opt-in `--fpsp-emulate`: the same instruction now executes
        // for real instead of trapping.
        let mut cpu = M68kCpu::with_config(CpuType::M68060, false);
        cpu.set_emulate_unimplemented_060(true);
        cpu.set_pc(TRAP_TABLE_END);
        let mut mem = FlatMemory::new(0x3000);
        cpu.set_address_register(AddressRegister(7), 0x3000);
        let start = cpu.pc();
        load_words(&mut mem, start, &[MOVEP_L_MEM_TO_D0, 0x0000]);
        // A0 points at a scratch area well past the loaded opcode/
        // displacement words; MOVEP.L reads one byte each at a 2-byte
        // stride (addr, addr+2, addr+4, addr+6) into D0 -- i.e. just the
        // high byte of each 16-bit word written below (big-endian, see
        // FlatMemory::write_u16), not the whole word.
        let scratch = start + 0x100;
        cpu.set_address_register(AddressRegister(0), scratch);
        mem.write_u16(scratch, 0x1200);
        mem.write_u16(scratch + 2, 0x5600);
        mem.write_u16(scratch + 4, 0x9A00);
        mem.write_u16(scratch + 6, 0xDE00);

        cpu.step(&mut mem);

        assert_eq!(
            cpu.pc(),
            start + 4,
            "MOVEP should execute and advance PC past the opcode + displacement word, not trap"
        );
        assert_eq!(
            cpu.data_register(DataRegister(0)),
            0x1256_9ADE,
            "MOVEP.L should have assembled D0 from the high byte of each word at the 2-byte \
             stride"
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

    /// Test: guest-CPU bus accesses are counted through the
    /// `m68k::AddressBus` impl, and volamos's *own* access to guest memory
    /// through `AddressSpace` is not.
    ///
    /// That separation is the whole meaning of the number: the CLI reports
    /// it as the traffic the emulated CPU put on the bus, so if a native
    /// library handler's reads and writes leaked into it the figure would
    /// be neither guest bus traffic nor anything else useful.
    #[test]
    fn bus_access_counts_track_the_cpu_not_the_runtimes_own_memory_access() {
        use m68k::AddressBus;

        let mut mem = FlatMemory::new(0x1_0000);
        assert_eq!(mem.access_counts(), (0, 0), "nothing has touched the bus");

        // What volamos's own handlers do: AddressSpace, not AddressBus.
        AddressSpace::write_u32(&mut mem, 0x100, 0xDEAD_BEEF);
        let _ = AddressSpace::read_u32(&mem, 0x100);
        assert_eq!(
            mem.access_counts(),
            (0, 0),
            "the runtime's own AddressSpace access is not guest bus traffic"
        );

        // What the emulated CPU does: AddressBus.
        let _ = AddressBus::read_byte(&mut mem, 0x100);
        let _ = AddressBus::read_word(&mut mem, 0x100);
        let _ = AddressBus::read_long(&mut mem, 0x100);
        AddressBus::write_byte(&mut mem, 0x200, 1);
        AddressBus::write_word(&mut mem, 0x200, 2);
        assert_eq!(
            mem.access_counts(),
            (3, 2),
            "each CPU-side access counts once, regardless of width"
        );
        assert_eq!(
            AddressSpace::bus_access_counts(&mem),
            (3, 2),
            "the trait method reports the same counts"
        );
    }

    #[test]
    fn emulated_cycles_and_clock_hz_default_to_zero_and_none() {
        // Every backend's default posture, per Cpu::emulated_cycles'/
        // Cpu::clock_hz's own doc comments -- these defaults are what
        // makes adding both trait methods non-breaking for any other
        // Cpu implementation that predates issue #102.
        let cpu = M68kCpu::new();
        assert_eq!(Cpu::emulated_cycles(&cpu), 0);
        assert_eq!(Cpu::clock_hz(&cpu), None);
    }

    #[test]
    fn set_clock_mhz_none_leaves_run_on_the_ordinary_run_batch_path() {
        // With no clock-mhz mode installed, `Cpu::run` must take exactly
        // the same run_batch path it always has -- this is the "strictly
        // additive, zero behaviour change when the flag is absent"
        // requirement from issue #102. set_clock_mhz(None) (the same
        // thing M68kCpu::new()/with_config() already leave it at) must
        // not, on its own, start counting cycles.
        let (mut cpu, mut mem) = new_cpu_with_memory(0x3000);
        cpu.set_clock_mhz(None);
        let start = cpu.pc();
        // MOVEQ.L #5, D0, then an A-line trap so `run` (which executes
        // until a trap/halt, unlike `step`) has somewhere to stop.
        load_words(&mut mem, start, &[0x7005, 0xA000]);

        let reason = cpu.run(&mut mem);

        assert!(matches!(
            reason,
            StopReason::Trap(TrapInfo {
                kind: TrapKind::ALine { opcode: 0xA000 },
                ..
            })
        ));
        assert_eq!(cpu.data_register(DataRegister(0)), 5);
        assert_eq!(cpu.emulated_cycles(), 0);
        assert_eq!(cpu.clock_hz(), None);
    }

    #[test]
    fn clock_mhz_mode_accumulates_real_cycles_and_still_reports_traps() {
        // A DBRA-based loop, same shape as
        // jit_batch_execution_matches_interpreter_for_a_backward_branch_loop
        // above, run through the run_for_cycles path instead. Two
        // things this pins down: (1) the trap the loop ends on is still
        // reported exactly like every other execution mode, and (2)
        // Cpu::emulated_cycles() actually goes up -- a MOVEQ/NOP/DBRA
        // sequence executed 5 times plus the trapping opcode fetch is
        // dozens of real 68000 cycles, so any plausible lower bound
        // catches "never accumulated anything" without pinning this
        // test to the m68k crate's exact per-instruction cycle counts
        // (an internal detail of that crate, not this one).
        let words: &[u16] = &[
            0x7004, // MOVEQ #4, D0
            0x4E71, // [loop] NOP
            0x51C8, 0xFFFC, // DBRA D0, loop (disp = -4)
            0xA000, // A-line trap: stop here
        ];
        let mut mem = FlatMemory::new(0x3000);
        load_words(&mut mem, TRAP_TABLE_END, words);
        let mut cpu = M68kCpu::new();
        cpu.set_clock_mhz(Some(25.0));
        cpu.set_pc(TRAP_TABLE_END);

        let reason = cpu.run(&mut mem);

        assert!(matches!(
            reason,
            StopReason::Trap(TrapInfo {
                kind: TrapKind::ALine { opcode: 0xA000 },
                ..
            })
        ));
        // DBRA is a word-sized decrement: D0 started at 4 (from MOVEQ,
        // zero-extended in the high word), looped 5 times down to -1 as
        // a 16-bit value (0xFFFF), leaving the high word untouched.
        assert_eq!(cpu.data_register(DataRegister(0)), 0x0000_FFFF);
        assert!(
            cpu.emulated_cycles() >= 20,
            "expected at least 20 real 68000 cycles for a 5-iteration NOP/DBRA loop, got {}",
            cpu.emulated_cycles()
        );
        assert_eq!(cpu.clock_hz(), Some(25_000_000.0));
    }

    #[test]
    fn clock_mhz_mode_also_reports_pc_out_of_bounds() {
        // Same guarantee as jit_mode_also_reports_pc_out_of_bounds above,
        // for the third execution path Cpu::run can now take.
        let (mut cpu, mut mem) = new_cpu_with_memory(0x3000);
        cpu.set_clock_mhz(Some(7.0));
        cpu.set_pc(0xFFFF_FFD1);

        let reason = cpu.run(&mut mem);

        assert_eq!(reason, StopReason::PcOutOfBounds { pc: 0xFFFF_FFD1 });
    }

    // --- --cache (issue #148): instruction cache model, end to end ---

    /// Builds a loop (`MOVEQ #iterations,D0`, then `nop_count` NOPs,
    /// then `DBRA D0,<back to the first NOP>`, then an A-line trap to
    /// stop) at [`TRAP_TABLE_END`] in a fresh `--cpu 68020` memory --
    /// `--cache`'s model only feeds the `m68k` crate's 68020/68030/68060
    /// timing tables (see the CLI's `--cache` doc), so a 68000 loop
    /// here would prove nothing about whether the model actually
    /// reaches real cycle counts.
    fn icache_loop_cpu_and_mem(nop_count: usize, iterations: i16) -> (M68kCpu, FlatMemory) {
        let mut words = vec![0x7000 | (iterations as u16 & 0xFF)]; // MOVEQ #iterations, D0
        words.extend(std::iter::repeat_n(0x4E71u16, nop_count)); // NOP * nop_count
        // DBRA D0, loop -- branch displacement is relative to the
        // extension word itself (PC+2 at execution time), back to the
        // first NOP (one word after the MOVEQ).
        let disp = -(2 * nop_count as i32 + 2);
        words.push(0x51C8);
        words.push(disp as u16);
        words.push(0xA000); // A-line trap: stop here

        let mut mem = FlatMemory::new(0x4000);
        load_words(&mut mem, TRAP_TABLE_END, &words);
        let mut cpu = M68kCpu::with_config(CpuType::M68020, false);
        cpu.set_pc(TRAP_TABLE_END);
        cpu.set_clock_mhz(Some(25.0));
        (cpu, mem)
    }

    #[test]
    fn enable_icache_makes_a_cache_thrashing_loop_cost_more_cycles_under_clock_mhz() {
        // 200 NOPs (400 bytes = 100 4-byte lines) is comfortably more
        // than a 256-byte/64-line --cpu 68020 cache can hold, so every
        // iteration evicts the lines the *previous* iteration needs --
        // real thrashing, not a model that happens to never miss.
        const NOP_COUNT: usize = 200;
        const ICACHE_SIZE_68020: u32 = 256;

        let (mut cpu, mut mem) = icache_loop_cpu_and_mem(NOP_COUNT, 2);
        let reason = cpu.run(&mut mem);
        assert!(matches!(
            reason,
            StopReason::Trap(TrapInfo {
                kind: TrapKind::ALine { opcode: 0xA000 },
                ..
            })
        ));
        let cycles_without_cache = cpu.emulated_cycles();

        let (mut cpu, mut mem) = icache_loop_cpu_and_mem(NOP_COUNT, 2);
        mem.enable_icache(ICACHE_SIZE_68020);
        let reason = cpu.run(&mut mem);
        assert!(matches!(
            reason,
            StopReason::Trap(TrapInfo {
                kind: TrapKind::ALine { opcode: 0xA000 },
                ..
            })
        ));
        let cycles_with_cache = cpu.emulated_cycles();

        assert!(
            cycles_with_cache > cycles_without_cache,
            "a thrashing loop should cost strictly more cycles once --cache's model can \
             actually report a miss (without it, every fetch is an unconditional hit, the \
             68020 timing tables' best case) -- got {cycles_without_cache} without --cache, \
             {cycles_with_cache} with it"
        );
    }

    #[test]
    fn enable_icache_a_loop_that_fits_entirely_in_the_cache_warms_up_once_then_costs_nothing_more()
    {
        // 20 NOPs (40 bytes = 10 lines) comfortably fits in a 256-byte
        // cache with room to spare, so it costs a one-time miss
        // penalty filling the cache on the loop's first pass, then
        // settles into all-hits -- unlike the thrashing case above,
        // where every single pass re-misses. Proved by comparing the
        // cost of two *additional* iterations once already warmed up
        // (iterations 4 vs. 6) against the same delta without --cache
        // at all: if those match exactly, nothing past the first pass
        // cost anything extra.
        const NOP_COUNT: usize = 20;
        const ICACHE_SIZE_68020: u32 = 256;

        let cycles_without_cache = |iterations| {
            let (mut cpu, mut mem) = icache_loop_cpu_and_mem(NOP_COUNT, iterations);
            cpu.run(&mut mem);
            cpu.emulated_cycles()
        };
        let cycles_with_cache = |iterations| {
            let (mut cpu, mut mem) = icache_loop_cpu_and_mem(NOP_COUNT, iterations);
            mem.enable_icache(ICACHE_SIZE_68020);
            cpu.run(&mut mem);
            cpu.emulated_cycles()
        };

        let delta_without_cache = cycles_without_cache(6) - cycles_without_cache(4);
        let delta_with_cache = cycles_with_cache(6) - cycles_with_cache(4);
        assert_eq!(
            delta_with_cache, delta_without_cache,
            "once warmed up, two more iterations of a loop that fits entirely in the cache \
             must cost exactly what they'd cost with no cache model at all (all hits)"
        );

        // And the model genuinely did something: the *first* iteration
        // (a cold cache) really did cost more with --cache than
        // without, or this test would prove nothing about the model
        // actually being consulted.
        assert!(
            cycles_with_cache(1) > cycles_without_cache(1),
            "the very first pass through a cold cache should still record real misses"
        );
    }
}
