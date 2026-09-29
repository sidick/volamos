//! `volamos` command-line entry point.
//!
//! Loads an AmigaOS "hunk" CLI executable, runs it against
//! `volamos-core`'s fake-library dispatch runtime, and exits with the
//! guest program's own exit code.
//!
//! ```text
//! volamos [-v|--verbose] [-s|--snoop] [-V NAME:hostdir]... [-a NAME:target[+target...]]...
//!         [--cwd AMIGAPATH] [--auto-assign HOSTDIR] <program> [args...]
//! ```
//!
//! `[args...]` is passed through to the guest program per AmigaOS
//! startup convention (joined with spaces into a command-line buffer,
//! `A0`/`D0` -- see `volamos_core::dispatch::Runtime::new`); a program
//! that parses its own arguments (e.g. via `ReadArgs`) can read them
//! from there. `-v`/`--verbose` logs each trapped library call (library
//! name, LVO, and handler name) to stderr as it happens; `-s`/`--snoop`
//! is a `SnoopDos`-style lighter-weight alternative that logs only
//! resource-opening calls (`OpenLibrary`/`OldOpenLibrary`, `Open`) --
//! what was requested and whether it resolved to a real/unimplemented
//! library or succeeded/failed for a file (see
//! [`volamos_core::dispatch::CallInfo::detail`]). Both can be given
//! together, in which case `--verbose` wins (its per-call output
//! already includes the same detail inline).
//!
//! `-V`/`--volume`, `-a`/`--assign`, `--cwd`, and `--auto-assign` set up
//! a [`volamos_core::vfs::Vfs`] for `dos.library`'s path-based calls
//! (`Open`, `Lock`, `Examine`, ...) -- see [`print_usage`] for the exact
//! grammar and the `--cwd` defaulting rule. Since issue #43, a built-in
//! standard-volume defaults layer (`SYS:`/`RAM:` and the standard
//! `C:`/`S:`/`LIBS:`/`DEVS:`/`ENVARC:`/`T:`/`ENV:` assigns onto them,
//! all backed by host directories created lazily on first actual use)
//! is active by default, so these names resolve out of the box even
//! with none of those flags given -- `--no-defaults`/`DEFAULTS=false`
//! restores the original pre-#43 behavior of installing no `Vfs` at all
//! in that case, so path-based dos.library calls fail cleanly with an
//! IoErr (`Input`/`Output`/`PutStr`/... still work either way) -- see
//! [`config::built_in_defaults`] for the full design.
//!
//! `--stack SIZE` (Phase 3 stage 6) overrides the guest stack region's
//! size (default [`volamos_core::DEFAULT_STACK_SIZE`], 64 KiB); `SIZE`
//! is a plain byte count, optionally suffixed `K`/`k` (KiB) or `M`/`m`
//! (MiB) -- see [`parse_byte_size`]. Values below
//! [`volamos_core::MIN_STACK_SIZE`] are silently clamped up to it by
//! [`volamos_core::dispatch::Runtime::new`], mirroring real AmigaOS's
//! own stack-size clamp.
//!
//! `--ram SIZE` overrides the total guest address space (default
//! [`DEFAULT_RAM_SIZE`], 16 MiB), same `K`/`M`-suffixed syntax as
//! `--stack`. `--stack` must leave real room within it for the loaded
//! program and the runtime's own guest heap -- [`run`]/
//! [`run_nested_program`] check this upfront and fail with a clear
//! error (rather than letting [`volamos_core::dispatch::Runtime::new`]
//! panic deep inside guest-heap setup) if `--stack` is too close to or
//! exceeds `--ram`. `--ram` must also be an address space the `--cpu`
//! model can actually reach: a 68000/68010 has a 24-bit address bus, so
//! anything above 16 MiB puts the guest stack at an address the CPU
//! cannot express, and [`check_ram_addressable`] refuses that
//! combination up front too.
//!
//! `--cpu MODEL` picks the emulated [`CpuType`] (default `68000`, the
//! lowest common denominator every Kickstart 3.1 machine shares -- see
//! [`volamos_core::backend::M68kCpu`]'s doc comment); `--fpu`/`--no-fpu`
//! (default: no FPU) sets whether a coprocessor FPU is fitted, only
//! meaningful for `--cpu 68020` and later -- see
//! [`volamos_core::backend::M68kCpu::with_config`]. A nested `System()`/
//! `Execute()` run (see [`run_nested_program`]) reuses the same CPU
//! configuration as the top-level run, same as `--stack`.
//!
//! `~/.volamos`, a `.volamos` next to the launched binary, and a
//! `.volamos` in the current directory supply default values for all
//! of the above (except `<program>`/`[args...]` themselves) so a
//! repeated-use project or self-contained toolchain doesn't need to
//! retype them -- explicit flags on the command line always win, then
//! the cwd file, then the program-directory file, then the global one
//! -- see [`config`]'s module doc for the exact grammar and merge
//! semantics.

mod config;

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use volamos_core::backend::{CpuType, M68kCpu, TRAP_TABLE_END, addressable_bytes};
use volamos_core::dispatch::{Runtime, StartConfig, TraceEvent};
use volamos_core::exectask::install_host_break_handler;
use volamos_core::loader::Location;
use volamos_core::memory::FlatMemory;
use volamos_core::vfs::{Vfs, VfsConfig};
use volamos_core::{DEFAULT_STACK_SIZE, LoadError, loader};

/// Default guest address space size, overridable with `--ram`. 16 MiB
/// comfortably covers the tiny CLI binaries this runtime currently
/// targets (with plenty of headroom for a much larger `--stack` than
/// the previous fixed 1 MiB ceiling allowed) while staying trivial for
/// any modern host to allocate.
const DEFAULT_RAM_SIZE: u32 = 16 * 1024 * 1024;

/// Minimum bytes of address space [`run`]/[`run_nested_program`]
/// require to remain between the loaded program's end and the top of
/// the guest stack region, beyond `--stack` itself -- real room for
/// [`Runtime::new`]'s own guest heap setup (the fake current task's
/// `struct Process`, its `pr_CLI`, the command-line buffer, ...) plus
/// headroom for the guest program's own `AllocMem`/etc. calls. Not
/// tied precisely to those internal structures' exact sizes (a few
/// hundred bytes today) -- a generous, stable margin that doesn't need
/// to change every time something inside `Runtime::new` grows by a few
/// bytes.
const MIN_HEAP_HEADROOM: u32 = 4096;

/// The CLI-only instrumentation family, kept together -- see
/// [`Options::sanitize`]'s doc for why instrumentation flags
/// deliberately aren't part of `config::Overrides` (a stale config file
/// silently enabling a debugging mode is a surprise nobody wants).
///
/// Grouped into a struct rather than threaded as more positional
/// arguments: `parse_args_raw`/`resolve` already carry a four-element
/// tuple, and growing it to a handful of interchangeable `bool`s is
/// exactly how arguments end up swapped at a call site.
///
/// Note `dirty_heap` living here is a *packaging* decision, not a
/// behavioural one: it is deliberately independent of `enabled` and
/// works with no shadow map installed at all (see its own doc). These
/// flags share a home because they share a lifecycle -- all CLI-only,
/// all applied to a freshly-built `Runtime` -- not because one implies
/// another.
#[derive(Debug, Default, Clone)]
struct InstrumentationOptions {
    /// `--sanitize`: install the shadow map and check every guest access.
    enabled: bool,
    /// `--sanitize-uninit`: additionally report reads of memory that was
    /// allocated but never written. Off by default even with
    /// `--sanitize`, because uninitialized-read detection is the
    /// noisiest class in any sanitizer -- see `crate::sanitize`'s docs
    /// and issue #68.
    uninit: bool,
    /// `--sanitize-ignore-pc`: guest PCs whose violations are
    /// suppressed, for silencing a site that has already been triaged
    /// without needing a suppression file.
    ignore_pcs: Vec<u32>,
    /// `--dirty-heap` (issue #80): fill every non-`MEMF_CLEAR`
    /// allocation with a poison pattern instead of leaving it as the
    /// zeros volamos's memory happens to start as, so a guest relying
    /// on uncleared memory being zero fails here the way it can on real
    /// hardware.
    ///
    /// Independent of [`Self::enabled`] in both directions: this one
    /// *changes what the guest sees* rather than observing it, which is
    /// why it is not folded into `--sanitize` (whose
    /// never-perturb-the-program property is worth protecting), and it
    /// is useful on its own with no shadow map and no slowdown.
    dirty_heap: bool,
}

impl InstrumentationOptions {
    /// Applies these options to a freshly-installed shadow map. A no-op
    /// when sanitizing is off (there is no shadow map to configure).
    fn apply(&self, mem: &mut FlatMemory) {
        let Some(shadow) = mem.shadow_mut() else {
            return;
        };
        shadow.report_uninit = self.uninit;
        for &pc in &self.ignore_pcs {
            shadow.ignore_pc(pc);
        }
    }
}

/// Where an `opts.jit == true` actually came from -- an explicit
/// `--jit`/`--no-jit` on this command line, or a `JIT=true`/`JIT=false`
/// line in a `~/.volamos`/`.volamos` config file (see
/// [`crate::config`]'s module doc for the full precedence chain). Exists
/// solely so [`check_clock_mhz_jit`]'s error message can name the flag
/// the user actually needs to change, rather than always saying "drop
/// --jit" to someone who never typed it -- a `JIT=true` sitting in a
/// config file they may not even remember exists is a much easier
/// mistake to make than a stray `--jit` on the command line right in
/// front of the `--clock-mhz` that conflicts with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JitSource {
    /// `jit` is `false` (the default, or an explicit `--no-jit`/
    /// `JIT=false`) -- never actually consulted by
    /// [`check_clock_mhz_jit`], since there's nothing to complain about,
    /// but kept as the [`Options::jit_source`] default so every
    /// `Options` value has one.
    Default,
    /// `jit` is `true` because of `--jit` on this command line.
    CommandLine,
    /// `jit` is `true` because of `JIT=true` in a config file, with no
    /// overriding `--jit`/`--no-jit` on the command line itself.
    ConfigFile,
}

#[derive(Debug)]
struct Options {
    verbose: bool,
    snoop: bool,
    program: String,
    guest_args: Vec<String>,
    volumes: Vec<(String, PathBuf)>,
    assigns: Vec<(String, Vec<String>)>,
    cwd: Option<String>,
    auto_assign_root: Option<PathBuf>,
    stack_size: u32,
    ram_size: u32,
    cpu_type: CpuType,
    fpu: bool,
    jit: bool,
    /// See [`JitSource`]. `Default` whenever `jit` is `false`; otherwise
    /// names which source (`--jit` or a config file's `JIT=true`) is
    /// responsible, for [`check_clock_mhz_jit`]'s error message.
    jit_source: JitSource,
    /// `--sanitize`: installs a [`volamos_core::sanitize::ShadowMap`] on
    /// the guest [`FlatMemory`] and forces the JIT off (see [`run`]) --
    /// see `volamos_core::sanitize`'s module doc for the detector
    /// itself. Off by default; CLI-only, like `net` below (not a
    /// `~/.volamos`/`.volamos` config key) -- kept out of
    /// `config::Overrides` deliberately, so enabling extra diagnostic
    /// overhead/behavior-changing instrumentation is always an explicit,
    /// per-invocation choice rather than something a stale config file
    /// silently turns on. See [`InstrumentationOptions`] for the family.
    sanitize: InstrumentationOptions,
    /// `--clock-mhz N` (issue #102): install an emulated clock rate of
    /// `N` MHz and switch `timer.device`'s `ReadEClock` from host
    /// wall-clock time to time derived from the CPU's own emulated
    /// cycle count at that rate -- see
    /// [`volamos_core::backend::M68kCpu::set_clock_mhz`]'s doc for the
    /// full mechanism and [`volamos_core::exectask::read_eclock_handler`]
    /// for why the *rate* `ReadEClock` reports in `D0` is unaffected by
    /// this. `None` (the default) is unchanged host-clock behavior.
    ///
    /// CLI-only, like `sanitize`/`net` -- kept out of
    /// `config::Overrides` deliberately: this changes what a guest
    /// program's own timing measurements *mean* (emulated time instead
    /// of real time), which is exactly the kind of thing a stale
    /// `~/.volamos`/`.volamos` shouldn't be able to silently flip on for
    /// every future run in that directory.
    ///
    /// Mutually exclusive with `jit` and with `sanitize.enabled`: see
    /// [`check_clock_mhz_jit`]/[`check_clock_mhz_sanitize`].
    clock_mhz: Option<f64>,
    net: bool,
    /// The built-in defaults layer's own lazy-creation bookkeeping
    /// (issue #43), if that layer is active -- passed straight through
    /// to [`vfs_config_from_opts`]'s `VfsConfig`. Empty whenever
    /// `--no-defaults`/`DEFAULTS=false` disabled it, or (in tests) when
    /// `Options` was built via [`parse_args`] rather than real config
    /// loading.
    lazy_volumes: Vec<volamos_core::vfs::LazyVolume>,
    /// Host directories to remove once this run (and any nested runs
    /// sharing its `VfsConfig`) is over -- the defaults layer's
    /// ephemeral `RAM:` root. `main` reads this directly after `run`
    /// returns; see [`volamos_core::vfs::VfsConfig::ephemeral_dirs`]'s
    /// doc for why cleanup can't just be a `Drop` impl.
    ephemeral_dirs: Vec<PathBuf>,
}

impl Options {
    /// Whether any VFS-related flag or setting was given at all -- if
    /// not, `run` doesn't install a [`Vfs`] on the [`Runtime`]. Since
    /// issue #43, `main` unconditionally merges in
    /// `config::built_in_defaults`'s `SYS:`/`RAM:` layer before
    /// `resolve` ever builds an `Options` (unless `--no-defaults`/
    /// `DEFAULTS=false` disabled it), so `self.volumes` is non-empty --
    /// and this returns `true` -- for an ordinary zero-flag invocation
    /// too now; pre-T13's original "nothing at all was configured"
    /// behavior is what `--no-defaults` restores.
    fn wants_vfs(&self) -> bool {
        !self.volumes.is_empty()
            || !self.assigns.is_empty()
            || self.cwd.is_some()
            || self.auto_assign_root.is_some()
    }
}

fn print_usage(program_name: &str) {
    eprintln!(
        "usage: {program_name} [-v|--verbose] [-s|--snoop] [-V NAME:hostdir]... \
         [-a NAME:target[+target...]]... [--cwd AMIGAPATH] \
         [--auto-assign HOSTDIR] [--defaults|--no-defaults] [--volumes-dir HOSTDIR] \
         [--stack SIZE] [--ram SIZE] [--cpu MODEL] \
         [--fpu|--no-fpu] [--jit|--no-jit] [--clock-mhz N] [--sanitize] [--sanitize-uninit] \
         [--sanitize-ignore-pc ADDR] [--dirty-heap] [--net] <program> [args...]"
    );
    eprintln!();
    eprintln!("Runs an AmigaOS CLI hunk executable under volamos.");
    eprintln!();
    eprintln!("options:");
    eprintln!("  -v, --verbose             log each emulated library call to stderr");
    eprintln!(
        "  -s, --snoop               SnoopDos-style: log every opened library/file to stderr"
    );
    eprintln!(
        "                            (name, and whether it resolved to a real or unimplemented"
    );
    eprintln!("                            library, or succeeded/failed for a file)");
    eprintln!("  -V, --volume NAME:hostdir map an Amiga volume NAME: onto a host directory");
    eprintln!("                            (repeatable)");
    eprintln!("  -a, --assign NAME:target[+target...]");
    eprintln!("                            assign NAME: to one or more Amiga path targets,");
    eprintln!("                            joined with '+' for a multi-assign search order");
    eprintln!("                            (repeatable)");
    eprintln!("  --cwd AMIGAPATH           initial guest current directory. Default: the");
    eprintln!("                            first -V volume's root if any -V was given,");
    eprintln!("                            else the first -a assign's root, else \"root:\"");
    eprintln!("                            (relying on --auto-assign to resolve it)");
    eprintln!("  --auto-assign HOSTDIR     fall back to <HOSTDIR>/NAME for any otherwise");
    eprintln!("                            unknown volume/assign NAME:");
    eprintln!("  --defaults / --no-defaults");
    eprintln!(
        "                            whether the built-in standard-volume defaults (SYS:/RAM:"
    );
    eprintln!(
        "                            and the standard C:/S:/LIBS:/DEVS:/ENVARC:/T:/ENV: assigns"
    );
    eprintln!(
        "                            onto them) apply (default: on). An explicit -V/-a for the"
    );
    eprintln!(
        "                            same NAME: always overrides the matching default, exactly"
    );
    eprintln!("                            like any other higher-precedence source");
    eprintln!("  --volumes-dir HOSTDIR     where the default SYS: volume lives on the host");
    eprintln!(
        "                            (default ~/.volamos.d/volumes); ignored with --no-defaults"
    );
    eprintln!(
        "  --stack SIZE              guest stack size in bytes (default {DEFAULT_STACK_SIZE});"
    );
    eprintln!("                            SIZE may be suffixed K (KiB) or M (MiB), e.g. 256K");
    eprintln!(
        "  --ram SIZE                total guest address space in bytes (default \
         {DEFAULT_RAM_SIZE});"
    );
    eprintln!("                            same K/M suffix syntax as --stack. --stack must leave");
    eprintln!("                            real room within this for the loaded program and the");
    eprintln!("                            runtime's own guest heap. Above 16M needs --cpu 68020");
    eprintln!("                            or later: a 68000/68010 cannot address more than that");
    eprintln!("  --cpu MODEL               emulated CPU (default 68000): 68000, 68010, 68020,");
    eprintln!("                            68ec020, 68030, 68ec030, 68040, 68ec040, 68lc040,");
    eprintln!("                            68060, or scc68070");
    eprintln!("  --fpu / --no-fpu          whether a coprocessor FPU is fitted (default: no FPU);");
    eprintln!("                            only meaningful for --cpu 68020 and later -- earlier");
    eprintln!("                            models have no coprocessor interface at all, so F-line");
    eprintln!("                            (FPU) instructions always trap on them regardless");
    eprintln!(
        "  --jit / --no-jit          batch-execute guest code via the m68k crate's trace JIT"
    );
    eprintln!(
        "                            instead of stepping one instruction at a time (default:"
    );
    eprintln!(
        "                            no JIT -- the interpreter is this runtime's correctness"
    );
    eprintln!(
        "                            reference); every library-call trap boundary is identical"
    );
    eprintln!("                            either way");
    eprintln!(
        "  --clock-mhz N             report timer.device's ReadEClock as emulated time derived"
    );
    eprintln!(
        "                            from the CPU's own emulated cycle count at N MHz (fractional"
    );
    eprintln!(
        "                            values allowed, e.g. 25 or 7.14), instead of host wall-clock"
    );
    eprintln!("                            time -- for reproducible, host-load-independent A/B");
    eprintln!(
        "                            benchmarking. Off by default. Cannot be combined with an"
    );
    eprintln!(
        "                            explicit --jit (run_batch's trace JIT never tracks a cycle"
    );
    eprintln!(
        "                            count, so there'd be nothing to derive emulated time from)"
    );
    eprintln!(
        "                            or with --sanitize (the cycle-counted execution path skips"
    );
    eprintln!("                            the sanitizer's per-instruction hooks entirely, so its");
    eprintln!("                            checks would be silently incomplete rather than merely");
    eprintln!(
        "                            slow). This is the slowest execution mode: measured ~2.2x"
    );
    eprintln!(
        "                            slower than --no-jit and ~7x slower than --jit (CoreMark 1.0"
    );
    eprintln!("                            on --cpu 68020, host wall-clock throughput). volamos's");
    eprintln!(
        "                            memory bus has no wait states at all, so memory-bound guest"
    );
    eprintln!(
        "                            code still won't match real hardware timing, and time spent"
    );
    eprintln!(
        "                            inside volamos's own native-Rust library handlers (e.g."
    );
    eprintln!("                            CopyMem) or single-stepped guest callbacks (RawDoFmt's");
    eprintln!(
        "                            PutChProc, Supervisor's routine) costs zero emulated cycles"
    );
    eprintln!("                            and is invisible in the reported total");
    eprintln!("  --sanitize                enable shadow-memory checking of guest accesses (heap");
    eprintln!(
        "                            redzones, freed blocks, below-stack-pointer reads/writes);"
    );
    eprintln!(
        "                            reports violations to stderr after the run. Off by default;"
    );
    eprintln!(
        "                            forces --no-jit regardless of --jit/--no-jit, since the"
    );
    eprintln!(
        "                            JIT's fast memory path would otherwise bypass every check"
    );
    eprintln!(
        "  --sanitize-uninit         additionally report reads of memory that was allocated but"
    );
    eprintln!("                            never written. Implies --sanitize. Separate and off by");
    eprintln!(
        "                            default because uninitialized-read detection is the noisiest"
    );
    eprintln!(
        "                            class in any sanitizer -- a whole-struct copy that includes"
    );
    eprintln!(
        "                            padding, or a table scan touching unused slots, can report"
    );
    eprintln!("                            legitimately");
    eprintln!(
        "  --sanitize-ignore-pc ADDR suppress violations reported at guest PC ADDR (decimal, or"
    );
    eprintln!(
        "                            hex with a 0x prefix). Repeatable. For silencing a site you"
    );
    eprintln!(
        "                            have already triaged, without needing a suppression file"
    );
    eprintln!(
        "  --dirty-heap              fill every AllocMem/AllocVec/AllocPooled block made without"
    );
    eprintln!(
        "                            MEMF_CLEAR with 0xA5 instead of leaving it zeroed, so a guest"
    );
    eprintln!(
        "                            relying on uncleared memory being zero fails here the way it"
    );
    eprintln!(
        "                            can on real hardware (where AllocMem returns whatever debris"
    );
    eprintln!(
        "                            was there). Independent of --sanitize: this one changes what"
    );
    eprintln!("                            the guest sees, rather than just observing it");
    eprintln!("  --net                     enable bsdsocket.library: real host network access for");
    eprintln!("                            the guest (socket/connect/send/recv/... via real host");
    eprintln!("                            sockets). Off by default and CLI-only -- not settable");
    eprintln!("                            via ~/.volamos/.volamos");
    eprintln!();
    eprintln!("[args...] is passed to the guest program's command line (A0/D0).");
    eprintln!();
    eprintln!(
        "By default (see --defaults above), SYS:, C:, S:, LIBS:, DEVS:, ENVARC:, RAM:, T:, and \
         ENV: all resolve out of the box, backed by empty host directories created only on \
         first actual use (SYS: persists across runs under --volumes-dir; RAM:/T:/ENV: are a \
         fresh, per-process temp directory, removed when this run ends). Any other name still \
         fails cleanly with an IoErr -- a typo isn't silently treated as a new empty volume. \
         With --no-defaults (or if none of -V/-a/--cwd/--auto-assign/the defaults apply), no \
         volume/assign filesystem is installed at all: dos.library path-based calls (Open, \
         Lock, Examine, ...) fail cleanly with an IoErr; Input/Output/PutStr/IoErr/SetIoErr \
         work either way."
    );
    eprintln!();
    eprintln!(
        "~/.volamos supplies default values for the flags above (KEY=value lines, e.g. \
         STACK=256K, DEFAULTS=false, VOLUMES_DIR=/path); a .volamos next to <program> (in its \
         own directory) overrides it; a .volamos in the current directory overrides both; \
         explicit flags on this command line win over all three. Relative VOLUME/AUTO_ASSIGN/ \
         VOLUMES_DIR paths in a config file resolve against that file's own directory. See the \
         Configuration page in the docs."
    );
}

/// Parses a `SIZE` value shared by `--stack` and `--ram`: a plain
/// non-negative byte count, or the same followed by a single `K`/`k`
/// (KiB, `* 1024`) or `M`/`m` (MiB, `* 1024 * 1024`) suffix -- e.g.
/// `"65536"`, `"64K"`, `"1M"`. Rejects empty input, non-digit content
/// before the optional suffix, more than one suffix character, and
/// multiplications that would overflow `u32` (a guest address space is
/// at most 4 GiB, so an overflowing request is never satisfiable
/// anyway). `flag` names the flag in the error message (`"--stack"` or
/// `"--ram"`).
fn parse_byte_size(flag: &str, s: &str) -> Result<u32, String> {
    let (digits, multiplier) = match s.as_bytes().last() {
        Some(b'K') | Some(b'k') => (&s[..s.len() - 1], 1024u32),
        Some(b'M') | Some(b'm') => (&s[..s.len() - 1], 1024 * 1024u32),
        _ => (s, 1u32),
    };
    let value: u32 = digits
        .parse()
        .map_err(|_| format!("{flag} expects a byte count (optionally K/M-suffixed), got {s:?}"))?;
    value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("{flag} value {s:?} overflows"))
}

/// An upper bound `--clock-mhz` refuses outright (issue #102, see
/// [`parse_clock_mhz`]). Exists purely to turn a fat-fingered entry (an
/// extra digit, a misplaced decimal point) into a clear error instead of
/// a silently-nonsensical "the CPU runs at a terahertz" benchmark run --
/// real classic-Amiga hardware, up to and including the fastest
/// Vampire-class FPGA accelerators on the market, never gets remotely
/// close to even a tenth of this.
const MAX_CLOCK_MHZ: f64 = 10_000.0;

/// A lower bound `--clock-mhz` refuses outright, for the same reason
/// [`MAX_CLOCK_MHZ`] exists but at the other end of the scale: without
/// one, something like `--clock-mhz 1e-30` parses as a perfectly
/// ordinary positive, finite `f64` and sails straight through the
/// `is_finite() && > 0.0` check, but drives every `ReadEClock` tick
/// count (`cycles / clock_hz * ECLOCK_PAL_HZ`, see
/// `read_eclock_handler`) to `f64::INFINITY`, which then saturates on
/// the `as u64` cast to `u64::MAX` -- harmless (no panic, no UB), but a
/// benchmark silently reporting the largest possible tick count instead
/// of a clear rejection is exactly the kind of "looks fine, isn't"
/// result `--clock-mhz` exists to avoid. `1 Hz` is generously below any
/// clock rate a real or realistically-slowed-down classic Amiga could
/// plausibly model (even a 1980s calculator's clock beats it), while
/// staying far away from the zero/underflow edge this guards against.
const MIN_CLOCK_MHZ: f64 = 0.000_001;

/// Parses a `--clock-mhz N` value: a positive clock rate in megahertz
/// for `ReadEClock`'s cycle-derived emulated-time mode (issue #102, see
/// [`volamos_core::backend::M68kCpu::set_clock_mhz`]). Fractional values
/// are accepted (`N.parse::<f64>()`) rather than requiring a whole
/// number -- Copperline, this project's own hardware-timing oracle,
/// models an A600's 68000 as `clock_mhz = 25.0`, and a real accelerator
/// board's rated speed is routinely a fraction (e.g. "14.28 MHz" for an
/// early NTSC-derived clock doubler), so an integer-only parser would
/// force a caller to round away the exact rate they're trying to model.
///
/// Rejects empty/non-numeric input, `0` and negative values (there is no
/// such thing as a non-positive clock rate), `NaN`/`inf`/`-inf` (which
/// `f64::parse` otherwise happily accepts and which would poison every
/// downstream division in `Cpu::run_via_cycles`/`read_eclock_handler`),
/// and anything outside [`MIN_CLOCK_MHZ`]..=[`MAX_CLOCK_MHZ`].
fn parse_clock_mhz(s: &str) -> Result<f64, String> {
    let value: f64 = s
        .parse()
        .map_err(|_| format!("--clock-mhz expects a positive number of MHz, got {s:?}"))?;
    if !value.is_finite() || value <= 0.0 {
        return Err(format!(
            "--clock-mhz expects a positive number of MHz, got {s:?}"
        ));
    }
    if value < MIN_CLOCK_MHZ {
        return Err(format!(
            "--clock-mhz {value} is implausibly low (under {MIN_CLOCK_MHZ} MHz) -- check for a \
             typo"
        ));
    }
    if value > MAX_CLOCK_MHZ {
        return Err(format!(
            "--clock-mhz {value} is implausibly high (over {MAX_CLOCK_MHZ} MHz) -- check for a \
             typo"
        ));
    }
    Ok(value)
}

/// Parses a `--cpu MODEL` value (case-insensitive) into a [`CpuType`].
/// Covers every real model the `m68k` crate models -- see
/// [`print_usage`] for the accepted spellings.
fn parse_cpu_type(s: &str) -> Result<CpuType, String> {
    match s.to_ascii_lowercase().as_str() {
        "68000" => Ok(CpuType::M68000),
        "68010" => Ok(CpuType::M68010),
        "68020" => Ok(CpuType::M68020),
        "68ec020" => Ok(CpuType::M68EC020),
        "68030" => Ok(CpuType::M68030),
        "68ec030" => Ok(CpuType::M68EC030),
        "68040" => Ok(CpuType::M68040),
        "68ec040" => Ok(CpuType::M68EC040),
        "68lc040" => Ok(CpuType::M68LC040),
        "68060" => Ok(CpuType::M68060),
        "scc68070" => Ok(CpuType::SCC68070),
        _ => Err(format!(
            "--cpu expects one of 68000, 68010, 68020, 68ec020, 68030, 68ec030, 68040, \
             68ec040, 68lc040, 68060, scc68070, got {s:?}"
        )),
    }
}

/// The `--cpu MODEL` spelling for `cpu_type` -- the inverse of
/// [`parse_cpu_type`], so a diagnostic can name the model the way the
/// user wrote it rather than as a Rust enum variant.
fn cpu_model_name(cpu_type: CpuType) -> &'static str {
    match cpu_type {
        CpuType::M68000 => "68000",
        CpuType::M68010 => "68010",
        CpuType::M68020 => "68020",
        CpuType::M68EC020 => "68ec020",
        CpuType::M68030 => "68030",
        CpuType::M68EC030 => "68ec030",
        CpuType::M68040 => "68040",
        CpuType::M68EC040 => "68ec040",
        CpuType::M68LC040 => "68lc040",
        CpuType::M68060 => "68060",
        CpuType::SCC68070 => "scc68070",
        // `CpuType` is the `m68k` crate's, so it can grow a variant (or
        // hand back its `Invalid` sentinel) without this match knowing.
        other => {
            debug_assert!(false, "unnamed CpuType {other:?}");
            "CPU"
        }
    }
}

/// Computes `ExecBase.AttnFlags` (`exec/execbase.h`'s `AFF_68010`/
/// `AFF_68020`/`AFF_68030`/`AFF_68040`/`AFF_68060`, plus `AFF_68881`/
/// `AFF_68882` for a coprocessor FPU or `AFF_FPU40` for the 68040's
/// on-die one) for `cpu_type`/`fpu`, matching what real Kickstart
/// startup code fills in for the machine it's actually running on --
/// see [`StartConfig::attn_flags`]'s doc for why the CLI computes this
/// rather than `Runtime`/`StartConfig` knowing about [`CpuType`]
/// directly. Each model's bit is documented as "also set for" every
/// later model (a real 68040 reports `AFF_68010`/`AFF_68020`/
/// `AFF_68030`/`AFF_68040` together, not just its own bit), so this
/// builds the flags cumulatively. `SCC68070` (a system-on-chip, not a
/// real desktop Amiga CPU) reports `0` -- no documented `AFF_*` bit
/// exists for it.
fn attn_flags_for(cpu_type: CpuType, fpu: bool) -> u16 {
    const AFF_68010: u16 = 1 << 0;
    const AFF_68020: u16 = 1 << 1;
    const AFF_68030: u16 = 1 << 2;
    const AFF_68040: u16 = 1 << 3;
    const AFF_68881: u16 = 1 << 4;
    const AFF_68882: u16 = 1 << 5;
    const AFF_FPU40: u16 = 1 << 6;
    const AFF_68060: u16 = 1 << 7;

    let mut flags = 0u16;
    if matches!(
        cpu_type,
        CpuType::M68010
            | CpuType::M68EC020
            | CpuType::M68020
            | CpuType::M68EC030
            | CpuType::M68030
            | CpuType::M68EC040
            | CpuType::M68LC040
            | CpuType::M68040
            | CpuType::M68060
    ) {
        flags |= AFF_68010;
    }
    if matches!(
        cpu_type,
        CpuType::M68EC020
            | CpuType::M68020
            | CpuType::M68EC030
            | CpuType::M68030
            | CpuType::M68EC040
            | CpuType::M68LC040
            | CpuType::M68040
            | CpuType::M68060
    ) {
        flags |= AFF_68020;
    }
    if matches!(
        cpu_type,
        CpuType::M68EC030
            | CpuType::M68030
            | CpuType::M68EC040
            | CpuType::M68LC040
            | CpuType::M68040
            | CpuType::M68060
    ) {
        flags |= AFF_68030;
    }
    if matches!(
        cpu_type,
        CpuType::M68EC040 | CpuType::M68LC040 | CpuType::M68040 | CpuType::M68060
    ) {
        flags |= AFF_68040;
    }
    if cpu_type == CpuType::M68060 {
        flags |= AFF_68060;
    }

    if fpu {
        match cpu_type {
            // The 68040/68060's on-die FPU (M68EC040/M68LC040 have no
            // FPU at all, so --fpu is a no-op there, matching real
            // hardware -- there's no external-68881-socket option on
            // those variants).
            CpuType::M68040 | CpuType::M68060 => flags |= AFF_FPU40,
            CpuType::M68EC020 | CpuType::M68020 | CpuType::M68EC030 | CpuType::M68030 => {
                flags |= AFF_68881 | AFF_68882;
            }
            _ => {}
        }
    }

    flags
}

/// Splits `NAME:rest` on the *first* `:` -- a volume/assign name can't
/// itself contain `:` (it's the Amiga path syntax's own separator), so
/// this is unambiguous even though the `rest` (a host directory, or an
/// Amiga path target) might rarely contain further `:` characters of
/// its own.
fn split_name_value<'a>(flag: &str, arg: &'a str) -> Result<(&'a str, &'a str), String> {
    match arg.split_once(':') {
        Some((name, rest)) if !name.is_empty() => Ok((name, rest)),
        _ => Err(format!("{flag} expects NAME:VALUE, got {arg:?}")),
    }
}

/// [`parse_args_raw`]'s return type, factored out purely to satisfy
/// clippy's `type_complexity` lint -- see that function's doc for what
/// each element means; `Option<f64>` is `--clock-mhz`'s CLI-only value
/// (issue #102), threaded alongside `InstrumentationOptions` for the
/// same reason (see `Options::clock_mhz`'s doc).
type RawParsedArgs = (
    config::Overrides,
    InstrumentationOptions,
    Option<f64>,
    String,
    Vec<String>,
);

/// Hand-rolled argument parsing: this CLI's surface is small enough that
/// pulling in an argument-parsing crate isn't worth the dependency.
///
/// Returns the *raw* [`config::Overrides`] rather than a fully-resolved
/// [`Options`] -- unlike a config file, `<program>`/`[args...]` aren't
/// part of that shared vocabulary, so they're returned alongside it
/// rather than folded in; and keeping built-in defaults out of this
/// function is what lets `main` tell "explicitly set on the CLI" apart
/// from "left at its default" when merging in `~/.volamos`/`.volamos`
/// (see `crate::config`'s module doc). [`parse_args`] is the
/// no-config-files convenience wrapper most callers (and every existing
/// test) actually want.
fn parse_args_raw(mut args: impl Iterator<Item = String>) -> Result<RawParsedArgs, String> {
    let mut overrides = config::Overrides::default();
    // CLI-only, unlike every other flag here -- see `Options::sanitize`'s
    // doc for why this deliberately isn't part of `config::Overrides`.
    let mut sanitize = InstrumentationOptions::default();
    // Also CLI-only, for the same reason -- see `Options::clock_mhz`'s
    // doc. Kept as its own local (rather than folded into
    // `InstrumentationOptions`) because it isn't part of that family's
    // shadow-map-instrumentation theme; it changes what `ReadEClock`
    // reports, not how guest memory accesses are checked.
    let mut clock_mhz: Option<f64> = None;
    let mut program = None;
    let mut guest_args = Vec::new();

    while let Some(arg) = args.next() {
        if program.is_some() {
            guest_args.push(arg);
            continue;
        }
        match arg.as_str() {
            "-v" | "--verbose" => overrides.verbose = Some(true),
            "-s" | "--snoop" => overrides.snoop = Some(true),
            "-h" | "--help" => return Err(String::new()), // caller prints usage and exits 0
            "-V" | "--volume" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("{arg} requires a NAME:hostdir argument"))?;
                let (name, hostdir) = split_name_value(&arg, &value)?;
                overrides
                    .volumes
                    .push((name.to_string(), PathBuf::from(hostdir)));
            }
            "-a" | "--assign" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("{arg} requires a NAME:target[+target...] argument"))?;
                let (name, targets) = split_name_value(&arg, &value)?;
                let targets: Vec<String> = targets.split('+').map(str::to_string).collect();
                overrides.assigns.push((name.to_string(), targets));
            }
            "--cwd" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--cwd requires an AMIGAPATH argument".to_string())?;
                overrides.cwd = Some(value);
            }
            "--auto-assign" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--auto-assign requires a HOSTDIR argument".to_string())?;
                overrides.auto_assign_root = Some(PathBuf::from(value));
            }
            "--stack" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--stack requires a SIZE argument".to_string())?;
                overrides.stack_size = Some(parse_byte_size("--stack", &value)?);
            }
            "--ram" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--ram requires a SIZE argument".to_string())?;
                overrides.ram_size = Some(parse_byte_size("--ram", &value)?);
            }
            "--cpu" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--cpu requires a MODEL argument".to_string())?;
                overrides.cpu_type = Some(parse_cpu_type(&value)?);
            }
            "--fpu" => overrides.fpu = Some(true),
            "--no-fpu" => overrides.fpu = Some(false),
            "--jit" => overrides.jit = Some(true),
            "--no-jit" => overrides.jit = Some(false),
            "--clock-mhz" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--clock-mhz requires an N argument".to_string())?;
                clock_mhz = Some(parse_clock_mhz(&value)?);
            }
            "--sanitize" => sanitize.enabled = true,
            "--dirty-heap" => sanitize.dirty_heap = true,
            // Implies --sanitize: asking for uninitialized-read
            // reporting without the shadow map installed could only be
            // a mistake, and silently doing nothing would be worse than
            // the implication.
            "--sanitize-uninit" => {
                sanitize.enabled = true;
                sanitize.uninit = true;
            }
            "--sanitize-ignore-pc" => {
                let raw = args.next().ok_or(
                    "--sanitize-ignore-pc needs an address, e.g. --sanitize-ignore-pc 0xe14a",
                )?;
                let hex = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X"));
                let pc = match hex {
                    Some(digits) => u32::from_str_radix(digits, 16),
                    None => raw.parse::<u32>(),
                }
                .map_err(|_| format!("--sanitize-ignore-pc: '{raw}' isn't a valid address"))?;
                sanitize.ignore_pcs.push(pc);
            }
            "--net" => overrides.net = Some(true),
            "--defaults" => overrides.standard_volumes = Some(true),
            "--no-defaults" => overrides.standard_volumes = Some(false),
            "--volumes-dir" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--volumes-dir requires a HOSTDIR argument".to_string())?;
                overrides.volumes_dir = Some(PathBuf::from(value));
            }
            _ => program = Some(arg),
        }
    }

    let program = program.ok_or_else(|| "missing <program> argument".to_string())?;
    Ok((overrides, sanitize, clock_mhz, program, guest_args))
}

/// Fills every unset field of `overrides` with its built-in default,
/// producing the final [`Options`] `run` consumes. Used both by
/// [`parse_args`] (CLI-only, no config files) and by `main` (after
/// merging CLI overrides with `~/.volamos`/`.volamos`).
fn resolve(
    overrides: config::Overrides,
    sanitize: InstrumentationOptions,
    clock_mhz: Option<f64>,
    cli_jit: Option<bool>,
    program: String,
    guest_args: Vec<String>,
) -> Options {
    let jit = overrides.jit.unwrap_or(false);
    // See JitSource's doc: only matters when `jit` is actually `true`,
    // in which case it's either straight from this command line's own
    // `--jit` (`cli_jit == Some(true)`), or `overrides.jit` only ended
    // up `true` because a config file's `JIT=true` won out over an
    // absent CLI flag (`config::merge`'s CLI-wins-when-present rule
    // means `cli_jit` can only be `Some(false)` or `None` here, never
    // `Some(true)` -- if it were, `overrides.jit` would already equal
    // it).
    let jit_source = if !jit {
        JitSource::Default
    } else if cli_jit == Some(true) {
        JitSource::CommandLine
    } else {
        JitSource::ConfigFile
    };
    Options {
        verbose: overrides.verbose.unwrap_or(false),
        snoop: overrides.snoop.unwrap_or(false),
        program,
        guest_args,
        volumes: overrides.volumes,
        assigns: overrides.assigns,
        cwd: overrides.cwd,
        auto_assign_root: overrides.auto_assign_root,
        stack_size: overrides.stack_size.unwrap_or(DEFAULT_STACK_SIZE),
        ram_size: overrides.ram_size.unwrap_or(DEFAULT_RAM_SIZE),
        cpu_type: overrides.cpu_type.unwrap_or(CpuType::M68000),
        fpu: overrides.fpu.unwrap_or(false),
        jit,
        jit_source,
        sanitize,
        clock_mhz,
        net: overrides.net.unwrap_or(false),
        // Only ever non-empty when `overrides` already includes
        // `config::built_in_defaults`'s own layer -- `main` merges that
        // in before calling `resolve`; `parse_args` (CLI-only, no
        // config files, no defaults -- see its own doc) never does, so
        // both are always empty there, matching its "just the flags"
        // scope.
        lazy_volumes: overrides.lazy_volumes,
        ephemeral_dirs: overrides.ephemeral_dirs,
    }
}

/// CLI-only argument parsing, ignoring `~/.volamos`/`.volamos` entirely.
/// `main` doesn't use this directly (it needs [`parse_args_raw`]'s CLI
/// overrides kept separate so it can merge in config-file values before
/// resolving defaults) -- this is the convenience every test in this
/// module wants instead.
#[cfg(test)]
fn parse_args(args: impl Iterator<Item = String>) -> Result<Options, String> {
    let (overrides, sanitize, clock_mhz, program, guest_args) = parse_args_raw(args)?;
    let cli_jit = overrides.jit;
    Ok(resolve(
        overrides, sanitize, clock_mhz, cli_jit, program, guest_args,
    ))
}

/// Works out the initial guest current directory per the defaulting rule
/// documented in [`print_usage`]: an explicit `--cwd` wins; otherwise
/// the first `-V` volume's root, else the first `-a` assign's root,
/// else `"root:"` (meaningful only in combination with `--auto-assign`,
/// which maps the otherwise-unknown `root:` onto `<auto-assign-root>/
/// root` -- if neither is configured, `Vfs::new` reports it as an
/// `UnknownVolume` error, same as any other unresolvable cwd).
fn default_cwd(opts: &Options) -> String {
    if let Some(cwd) = &opts.cwd {
        return cwd.clone();
    }
    if let Some((name, _)) = opts.volumes.first() {
        return format!("{name}:");
    }
    if let Some((name, _)) = opts.assigns.first() {
        return format!("{name}:");
    }
    "root:".to_string()
}

/// Builds the [`VfsConfig`] `opts` describes, or `None` if no VFS-related
/// flag was given at all (see [`Options::wants_vfs`]). `run` builds a
/// [`Vfs`] from this once for the top-level [`Runtime`], and clones it
/// into the `System()`/`Execute` nested-runner closure (see
/// [`run_nested_program`]) so a nested program gets an independently
/// constructed [`Vfs`] from the *same* configuration -- there's no way to
/// reach back into the parent's already-installed `Vfs` from there
/// anyway ([`crate::dispatch::Runtime`] owns it by value, not by any
/// handle a closure built before the `Runtime` exists could hold).
fn vfs_config_from_opts(opts: &Options) -> Option<VfsConfig> {
    if !opts.wants_vfs() {
        return None;
    }
    Some(VfsConfig {
        volumes: opts.volumes.clone(),
        assigns: opts.assigns.clone(),
        auto_assign_root: opts.auto_assign_root.clone(),
        cwd: default_cwd(opts),
        lazy_volumes: opts.lazy_volumes.clone(),
    })
}

/// The host-side `System()`/`Execute()` runner installed on the
/// top-level [`Runtime`] this CLI builds (see [`volamos_core::dosseg`]'s
/// module docs for the overall architecture): loads `host_path` through
/// the ordinary [`loader::load`] path into a *fresh* guest address space
/// (deliberately not `dosseg::build_seglist`'s seglist framing -- that's
/// a different in-guest-memory representation meant for `LoadSeg`
/// callers, not for actually executing a program) and runs it to
/// completion in a brand-new [`Runtime`], sharing `vfs_config` (the same
/// volumes/assigns the parent run was given) and `stack_size`.
///
/// Output goes to this process's own `std::io::stdout()`, opened fresh
/// here -- not threaded through from whatever `out` sink the *parent*
/// guest program's `Runtime::run` call was given -- since this closure
/// runs from inside a library-call handler with no access to that mid-run
/// borrow; see `volamos_core::dosseg`'s module docs for the consequence
/// this has for tests that capture output into an in-memory buffer.
///
/// **Scope cut**: the nested `Runtime` built here does *not* itself get
/// a `System()`/`Execute` runner installed, so a nested program's own
/// `System()`/`Execute` calls fail cleanly (see
/// [`volamos_core::dosseg::DosState::system`]/`execute`) rather than
/// recursing to a second level -- documented, not silent; revisit if a
/// corpus binary needs `System()`-calling-`System()`.
///
/// Returns the nested program's own exit code, or `-1` if it couldn't be
/// loaded/run at all (unreadable file, not a valid hunk executable, or a
/// [`volamos_core::RuntimeError`] during the nested run) -- `System()`'s
/// own "couldn't run it" sentinel, reused here since a load/run failure
/// deep inside a nested program is, from the parent guest's point of
/// view, indistinguishable from "the command couldn't be invoked".
/// The guest-visible program name for `pr_CLI`'s `cli_CommandName`
/// (`dos.library`'s `GetProgramName()`): the host path's own file name,
/// matching how a real AmigaOS Shell records just the command as typed
/// (not a full path) in `cli_CommandName`. Falls back to the whole path
/// string verbatim if it has no file-name component (e.g. `.` or `/`),
/// which should never happen for an actual loadable program path but
/// costs nothing to handle rather than panic.
fn program_name_from_path(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Prints a `--sanitize` run's shadow-map violations (if any) to
/// stderr, in the same "diagnostics go to stderr" style as `--verbose`/
/// `--snoop`'s per-call tracing (see [`run`]'s `trace` closure). A
/// no-op if `--sanitize` wasn't given (`runtime.memory().shadow()` is
/// `None`) or the run was clean (no violations recorded). Called once
/// per top-level or nested run, right after it finishes -- so a
/// `System()`/`Execute()`-spawned nested program's own violations are
/// reported too, not just the top-level program's.
/// Formats a resolved [`Location`] for a diagnostic message:
/// `"hello.c:42"` for source-line info, `"Do_Law+0x12"` for a symbol.
fn format_location(loc: &Location) -> String {
    match loc {
        Location::Line { file, line } => format!("{file}:{line}"),
        Location::Symbol { name, offset } if *offset == 0 => name.clone(),
        Location::Symbol { name, offset } => format!("{name}+{offset:#x}"),
    }
}

/// Prints a `--sanitize` run's violations, annotated with source
/// locations where the program's debug info (or failing that, its
/// symbol table) can supply them -- see `crate::loader`'s
/// `lookup_location` and issue #74.
///
/// `program` is the parsed executable and `load` where its hunks
/// landed; both are needed because the debug info records
/// *hunk-relative* offsets, so a PC has to have its hunk's load address
/// subtracted before it means anything. Passing `None` (the overlay
/// loading path, which doesn't produce a `LoadResult`) just prints
/// addresses alone, exactly as before.
fn report_sanitizer_violations(
    runtime: &Runtime<M68kCpu>,
    program: Option<(&loader::HunkFile, &loader::LoadResult)>,
) {
    if let Some(shadow) = runtime.memory().shadow()
        && shadow.violation_count() > 0
    {
        match program {
            Some((file, load)) => eprint!(
                "{}",
                shadow
                    .report_with(|pc| load.lookup_location(file, pc).as_ref().map(format_location))
            ),
            None => eprint!("{}", shadow.report()),
        }
    }
}

/// Checks that `ram_size` is an address space the configured CPU can
/// actually reach (issue #98).
///
/// The guest stack lives at the top of the address space, so a `--ram`
/// larger than the CPU's address bus can express puts `A7` at an
/// address that wraps: the `JSR` into a library pushes its return
/// address into low memory instead, and the `RTS` pops whatever was
/// there. That is what real hardware does, and it manifests as the
/// baffling "continuation stub trapped at 0x000000c4 with no pending
/// continuation" several calls into a run rather than as a complaint
/// about the flags -- so, like an oversized `--stack` (see
/// [`check_ram_fits`]), it is refused up front instead.
///
/// Only bites someone who raises `--ram` past 16 MiB without also
/// asking for a 32-bit CPU, since the default is exactly 16 MiB.
fn check_ram_addressable(ram_size: u32, cpu_type: CpuType) -> Result<(), String> {
    let addressable = addressable_bytes(cpu_type);
    if u64::from(ram_size) <= addressable {
        return Ok(());
    }
    let model = cpu_model_name(cpu_type);
    Err(format!(
        "--ram {ram_size} is more address space than a {model} can reach: that CPU has a \
         24-bit address bus, so it can only address {addressable} bytes, and the guest \
         stack at the top of a larger space would be at an address it cannot express -- \
         ask for a 32-bit address bus with --cpu 68020 (or any later model), or lower the \
         size to {addressable} or less (--ram on the command line, RAM= in a config file)"
    ))
}

/// Refuses `--clock-mhz` together with `jit` set (issue #102): `jit`
/// only ever becomes `true` via an explicit override -- `--jit` on the
/// command line, or `JIT=true` in a config file -- since
/// [`resolve`]'s own default is `false` (see [`print_usage`]'s `--jit`/
/// `--no-jit` doc). There is therefore no "default-on JIT" case this
/// could spuriously trip on: every `jit == true` this ever sees really
/// was asked for, by someone, somewhere. `jit_source` (see
/// [`JitSource`]) says which -- used only to phrase the error message
/// around whichever one it actually was, so a `JIT=true` sitting
/// forgotten in a config file doesn't get blamed on a `--jit` the user
/// never typed on this command line.
///
/// The combination is refused rather than one flag silently winning
/// (the way `--sanitize` silently forces the JIT off) because there is
/// no approximate answer to fall back to here: `run_batch` (the JIT's
/// execution path) never surfaces a cycle count at all -- see
/// `volamos_core::backend::M68kCpu::set_clock_mhz`'s doc -- so
/// `--clock-mhz` would have literally nothing to derive `ReadEClock`'s
/// reported time from. Reporting `0` or falling back to the host clock
/// without saying so would silently give a benchmark run numbers that
/// don't mean what `--clock-mhz` promised; an explicit error is the
/// honest answer.
fn check_clock_mhz_jit(
    clock_mhz: Option<f64>,
    jit: bool,
    jit_source: JitSource,
) -> Result<(), String> {
    if !(clock_mhz.is_some() && jit) {
        return Ok(());
    }
    let how_to_fix = match jit_source {
        JitSource::CommandLine => "drop --jit (or pass --no-jit, the default)",
        JitSource::ConfigFile => {
            "pass --no-jit on the command line to override it, or drop JIT=true from your \
             ~/.volamos/.volamos config file"
        }
        JitSource::Default => {
            unreachable!("jit_source is Default whenever jit is false, but jit is true here")
        }
    };
    Err(format!(
        "--clock-mhz cannot be combined with --jit: the trace JIT (run_batch) never tracks a \
         cycle count, so there is nothing for --clock-mhz to derive ReadEClock's emulated time \
         from -- {how_to_fix} to use --clock-mhz"
    ))
}

/// Refuses `--clock-mhz` together with `--sanitize` (issue #102 code
/// review): `M68kCpu::run_via_cycles` -- the execution path
/// `--clock-mhz` switches [`Cpu::run`](volamos_core::cpu::Cpu::run) to
/// -- never calls the per-instruction sanitizer hooks
/// (`sanitize_before_instruction`/`sanitize_after_instruction`) the
/// ordinary `run_batch` path calls on every single instruction when a
/// shadow map is installed: no `set_current_pc` publication, no
/// `check_return`/`record_call` shadow-call-stack bookkeeping, no
/// `update_stack_pointer` below-`A7` tracking. Running both flags
/// together wouldn't merely be slower or imprecise -- key parts of the
/// detector would be silently dead (a stale shadow call stack
/// comparing new returns against frames from far earlier in the run)
/// or attributing every violation in a whole cycle-budget's worth of
/// instructions to one stale PC, while `--sanitize` still prints its
/// normal-looking "no violations" or "N violations" summary as if
/// everything had been checked.
///
/// [`check_clock_mhz_jit`]'s own doc makes the applicable argument:
/// "an explicit error is the honest answer" beats a silently
/// compromised result, and that applies at least as strongly to a
/// safety/correctness tool quietly running with its instrumentation
/// half-disabled as it does to `--clock-mhz` having no cycle count to
/// read. Wiring the sanitizer hooks into `run_via_cycles` properly
/// (matching `run_batch`'s shadow-map-forces-batches-of-one treatment)
/// is future work, not something to fake with a doc-comment caveat.
fn check_clock_mhz_sanitize(clock_mhz: Option<f64>, sanitize_enabled: bool) -> Result<(), String> {
    if clock_mhz.is_some() && sanitize_enabled {
        return Err(
            "--clock-mhz cannot be combined with --sanitize: the cycle-counted execution path \
             --clock-mhz uses (M68kCpu::run_via_cycles) does not run the sanitizer's \
             per-instruction shadow-map hooks, so --sanitize's checks would be silently \
             incomplete rather than merely slow -- drop one of the two flags"
                .to_string(),
        );
    }
    Ok(())
}

/// Checks that `stack_size` plus [`MIN_HEAP_HEADROOM`] actually fits
/// between `load_end` (the loaded program's own end address) and
/// `ram_size` (the top of the guest address space) -- see
/// [`MIN_HEAP_HEADROOM`]'s doc for why this check exists: without it,
/// a `--stack` too close to or exceeding `--ram` leaves
/// [`Runtime::new`]'s own guest heap setup no room at all, which
/// panics deep inside guest-heap allocation instead of failing
/// cleanly.
fn check_ram_fits(load_end: u32, stack_size: u32, ram_size: u32) -> Result<(), String> {
    let required = load_end
        .checked_add(stack_size)
        .and_then(|v| v.checked_add(MIN_HEAP_HEADROOM));
    match required {
        Some(required) if required <= ram_size => Ok(()),
        _ => Err(format!(
            "--stack {stack_size} is too large for --ram {ram_size}: the loaded program ends \
             at {load_end:#x}, and there must be room for the stack plus at least \
             {MIN_HEAP_HEADROOM} bytes of guest heap after that -- increase --ram or decrease \
             --stack"
        )),
    }
}

#[allow(clippy::too_many_arguments)] // internal helper; one param per thing a nested run inherits from its parent
fn run_nested_program(
    host_path: &std::path::Path,
    args: &[String],
    raw_args: Option<&[u8]>,
    vfs_config: Option<VfsConfig>,
    stack_size: u32,
    ram_size: u32,
    cpu_type: CpuType,
    fpu: bool,
    jit: bool,
    sanitize: InstrumentationOptions,
    clock_mhz: Option<f64>,
    net: bool,
) -> i32 {
    let Ok(bytes) = std::fs::read(host_path) else {
        return -1;
    };
    let Ok(hunk_file) = loader::parse(&bytes) else {
        return -1;
    };
    let mut mem = FlatMemory::new(ram_size as usize);
    // Right after construction, before `loader::load` populates it --
    // harmless either way (see `FlatMemory::enable_sanitizer`'s doc for
    // why the default `Valid` shadow state makes the ordering a
    // non-issue), this is just the more obvious place to put the call.
    if sanitize.enabled {
        mem.enable_sanitizer();
        sanitize.apply(&mut mem);
    }
    let Ok(load_result) = loader::load(&hunk_file, &mut mem, TRAP_TABLE_END) else {
        return -1;
    };
    if check_ram_fits(load_result.end, stack_size, ram_size).is_err() {
        return -1;
    }

    let config = StartConfig {
        entry: load_result.entry,
        load_end: load_result.end,
        args: args.to_vec(),
        raw_command_line: raw_args.map(<[u8]>::to_vec),
        stack_size,
        attn_flags: attn_flags_for(cpu_type, fpu),
        program_name: program_name_from_path(host_path),
    };
    let mut cpu = M68kCpu::with_config(cpu_type, fpu);
    // The sanitizer's shadow-map checks only run through
    // `FlatMemory`'s own `AddressSpace` methods; the JIT's `fast_mem`
    // raw-pointer path bypasses them entirely, so `--sanitize` forces
    // the JIT off here regardless of `jit` -- see
    // `AddressBus::fast_mem`'s doc comment on `FlatMemory` for why this
    // is belt-and-braces (that impl already returns `None` once a
    // shadow map is installed, but making it explicit here means a
    // nested run never even attempts the JIT path in the first place).
    cpu.set_jit(jit && !sanitize.enabled);
    // --clock-mhz (issue #102): the top-level run already validated
    // this against `jit` (see `check_clock_mhz_jit`) before this
    // nested-run closure was ever installed, so `jit`/`clock_mhz` here
    // are the same already-consistent pair -- a nested System()/
    // Execute() run gets its own fresh cycle counter, starting at `0`
    // again, same as its own fresh CPU.
    cpu.set_clock_mhz(clock_mhz);
    let mut runtime = Runtime::new(cpu, mem, config);
    if sanitize.dirty_heap {
        runtime.enable_dirty_heap();
    }
    if sanitize.enabled {
        // The shadow map installed on `mem` above only records what it
        // is told to poison; this is what makes the heap actually
        // reserve and report redzones around guest allocations.
        runtime.enable_heap_sanitizer();
    }

    if let Some(vfs_config) = vfs_config {
        match Vfs::new(vfs_config) {
            Ok(vfs) => runtime.set_vfs(vfs),
            Err(_) => return -1,
        }
    }
    if let Some(dir) = host_path.parent() {
        runtime.set_program_dir(dir);
    }
    if net {
        runtime.enable_bsdsocket();
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let result = runtime.run(&mut out, None).unwrap_or(-1);
    // Nested runs parse their own executable locally and don't keep the
    // result around; source-location lookup is a top-level nicety, so
    // these report plain addresses.
    report_sanitizer_violations(&runtime, None);
    result
}

/// Builds a [`Runtime`] and installs its `Vfs`/`PROGDIR:` from `opts`
/// (shared by both of [`run`]'s loading strategies, see that function's
/// doc). `mem`/`config` are already fully built by the caller -- this
/// only handles the `Vfs`/program-dir wiring common to both.
fn build_runtime_with_vfs(
    opts: &Options,
    cpu: M68kCpu,
    mem: FlatMemory,
    config: StartConfig,
    vfs_config: Option<VfsConfig>,
) -> Result<Runtime<M68kCpu>, String> {
    let mut runtime = Runtime::new(cpu, mem, config);
    if let Some(vfs_config) = vfs_config {
        let vfs =
            Vfs::new(vfs_config).map_err(|e| format!("couldn't set up volumes/assigns: {e}"))?;
        runtime.set_vfs(vfs);
    }
    if let Some(dir) = std::path::Path::new(&opts.program).parent() {
        runtime.set_program_dir(dir);
    }
    if opts.net {
        runtime.enable_bsdsocket();
    }
    if opts.sanitize.enabled {
        // Pairs with the `mem.enable_sanitizer()` both of `run`'s
        // loading strategies already did: that installs the shadow map,
        // this makes the heap reserve redzones and quarantine freed
        // blocks so there is something for `crate::execmem`'s handlers
        // to poison. Done here rather than in each branch so the
        // overlay and flat loading paths can't drift apart.
        runtime.enable_heap_sanitizer();
    }
    if opts.sanitize.dirty_heap {
        runtime.enable_dirty_heap();
    }
    Ok(runtime)
}

fn run(opts: &Options) -> Result<i32, String> {
    // Before anything is read or loaded: these depend only on the
    // flags, and a run that violates any of them fails much later in a
    // way that does not point at them. --jit is checked before
    // --sanitize (matching the doc comments' own ordering) purely so
    // a run that somehow violates both reports the --jit conflict
    // first -- there's no other significance to the order.
    check_ram_addressable(opts.ram_size, opts.cpu_type)?;
    check_clock_mhz_jit(opts.clock_mhz, opts.jit, opts.jit_source)?;
    check_clock_mhz_sanitize(opts.clock_mhz, opts.sanitize.enabled)?;

    let bytes = std::fs::read(&opts.program)
        .map_err(|e| format!("couldn't read '{}': {e}", opts.program))?;

    let hunk_file = loader::parse(&bytes).map_err(|e: LoadError| {
        format!("'{}' is not a valid hunk executable: {e}", opts.program)
    })?;

    let mut cpu = M68kCpu::with_config(opts.cpu_type, opts.fpu);
    // See run_nested_program's matching comment: --sanitize always wins
    // over --jit, since the JIT's fast_mem path would otherwise bypass
    // every shadow-map check.
    cpu.set_jit(opts.jit && !opts.sanitize.enabled);
    // --clock-mhz (issue #102): already validated against --jit above,
    // so this is never fighting the JIT for control of Cpu::run -- see
    // M68kCpu::set_clock_mhz's doc for how the two would conflict if it
    // weren't.
    cpu.set_clock_mhz(opts.clock_mhz);
    let program_name = program_name_from_path(std::path::Path::new(&opts.program));
    let vfs_config = vfs_config_from_opts(opts);

    // Overlay executables (crate::loader's module docs) need the real
    // AmigaOS seglist framing (seg_length/next_seg immediately before
    // every hunk) -- their manager reads its own next_seg field to find
    // its second hunk before making a single library call, and its
    // OverlayHeader needs a real, open guest FileHandle. The ordinary
    // flat crate::loader::load placement doesn't provide either, and a
    // program built that way runs straight into a wild-PC crash --
    // found running a real overlay-linked binary. See
    // Runtime::load_top_level_program's doc for the full story.
    // `None` on the overlay path, which loads via
    // `load_top_level_program` and produces no `LoadResult` -- those
    // runs simply report addresses without source locations.
    let mut loaded: Option<loader::LoadResult> = None;
    let mut runtime = if hunk_file.overlay.is_some() {
        let mut mem = FlatMemory::new(opts.ram_size as usize);
        // Right after construction, before anything is loaded into it
        // -- harmless either way, see FlatMemory::enable_sanitizer's
        // doc on why the default Valid shadow state makes the ordering
        // a non-issue; this is simply the more obvious place to put it.
        if opts.sanitize.enabled {
            mem.enable_sanitizer();
            opts.sanitize.apply(&mut mem);
        }
        let config = StartConfig {
            entry: 0, // overridden by load_top_level_program below
            load_end: TRAP_TABLE_END,
            args: opts.guest_args.clone(),
            raw_command_line: None,
            stack_size: opts.stack_size,
            attn_flags: attn_flags_for(opts.cpu_type, opts.fpu),
            program_name,
        };
        let mut runtime = build_runtime_with_vfs(opts, cpu, mem, config, vfs_config.clone())?;
        runtime
            .load_top_level_program(std::path::Path::new(&opts.program))
            .map_err(|e| format!("couldn't LoadSeg '{}': IoErr {e}", opts.program))?;
        runtime
    } else {
        let mut mem = FlatMemory::new(opts.ram_size as usize);
        if opts.sanitize.enabled {
            mem.enable_sanitizer();
            opts.sanitize.apply(&mut mem);
        }
        let load_result = loader::load(&hunk_file, &mut mem, TRAP_TABLE_END)
            .map_err(|e| format!("couldn't load '{}': {e}", opts.program))?;
        check_ram_fits(load_result.end, opts.stack_size, opts.ram_size)?;
        // Kept for the sanitizer report's source-location lookup (issue
        // #74): the debug info records hunk-relative offsets, so a PC
        // needs its hunk's load address subtracted, which only this
        // result knows.
        loaded = Some(load_result.clone());
        let config = StartConfig {
            entry: load_result.entry,
            load_end: load_result.end,
            args: opts.guest_args.clone(),
            raw_command_line: None,
            stack_size: opts.stack_size,
            attn_flags: attn_flags_for(opts.cpu_type, opts.fpu),
            program_name,
        };
        build_runtime_with_vfs(opts, cpu, mem, config, vfs_config.clone())?
    };

    // System()/Execute/RunCommand (Phase 3 stage 7): a nested program is
    // loaded and run through run_nested_program, sharing this run's
    // volumes/assigns and --stack size -- see volamos_core::dosseg's
    // module docs. RunCommand's own explicit stack argument
    // (req.stack_size_override) takes priority when present; System()/
    // Execute() (which have no such argument) fall back to this run's
    // own --stack/default.
    let nested_stack_size = opts.stack_size;
    let nested_ram_size = opts.ram_size;
    let nested_cpu_type = opts.cpu_type;
    let nested_fpu = opts.fpu;
    let nested_jit = opts.jit;
    let nested_sanitize = opts.sanitize.clone();
    let nested_clock_mhz = opts.clock_mhz;

    let nested_net = opts.net;
    runtime.set_system_runner(move |req| {
        run_nested_program(
            &req.resolved_program_host_path,
            &req.args,
            req.raw_args.as_deref(),
            vfs_config.clone(),
            req.stack_size_override.unwrap_or(nested_stack_size),
            nested_ram_size,
            nested_cpu_type,
            nested_fpu,
            nested_jit,
            nested_sanitize.clone(),
            nested_clock_mhz,
            nested_net,
        )
    });

    let stdout = io::stdout();
    let mut out = stdout.lock();

    let verbose = opts.verbose;
    let snoop = opts.snoop;
    let mut trace = move |event: &TraceEvent| {
        if verbose {
            eprintln!("volamos: {event}");
        } else if snoop && let Some(detail) = &event.detail {
            eprintln!("snoop: {detail}");
        }
    };

    let result = runtime
        .run(&mut out, Some(&mut trace))
        .map_err(|e| format!("{}: {e}", opts.program));
    report_sanitizer_violations(&runtime, loaded.as_ref().map(|load| (&hunk_file, load)));
    report_emulated_cycles(&runtime);
    result
}

/// Prints the run's emulated cycle count and the wall time it
/// represents, once, at exit -- the whole point of `--clock-mhz` being a
/// *measurement* mode, and a no-op for every run without it (see
/// [`volamos_core::dispatch::Runtime::emulated_cycles`], which reports
/// `None` when no clock rate was configured).
///
/// Printed unconditionally rather than behind `-v`, and to stderr rather
/// than stdout. Unconditionally because a caller who passed an explicit
/// benchmarking flag asked for exactly this number, and making them pass
/// a second flag to see it would be a poor trade; to stderr because the
/// guest program's own output is on stdout and a benchmark harness
/// parsing that must not have this line spliced into it.
///
/// Reported even when the run ended in an error. A guest that crashed
/// part-way still burned the cycles it burned, and for a benchmark
/// that died half-way through, "how far did it get" is usually the
/// first question.
fn report_emulated_cycles<C: volamos_core::cpu::Cpu + 'static>(runtime: &Runtime<C>) {
    if let Some(line) = runtime
        .emulated_cycles()
        .and_then(|(cycles, clock_hz)| format_emulated_cycles(cycles, clock_hz))
    {
        eprintln!("volamos: {line}");
    }
    if let Some((cycles, _)) = runtime.emulated_cycles()
        && let Some((instructions, reads, writes)) =
            runtime.emulated_instruction_and_access_counts()
        && let Some(line) = format_emulated_work(cycles, instructions, reads, writes)
    {
        eprintln!("volamos: {line}");
    }
}

/// Formats the second report line: the work the run actually did, and the
/// two ratios that place it on the memory-intensity scale.
///
/// The ratios are the point. This runtime bills no bus wait states, so its
/// emulated time is close to real hardware for arithmetic-bound code and
/// very optimistic for bus-bound code -- measured at 1.02x to 25x against
/// cycle-paced hardware, monotonic in memory intensity (issue #105).
/// Cycles and seconds alone give a caller no way to tell which end of that
/// range a workload sits at; accesses per instruction does, from the
/// volamos run by itself, with no second runtime to compare against.
///
/// **`reads` includes instruction fetch**, which the `m68k::AddressBus`
/// methods cannot distinguish from a data read. So accesses-per-
/// instruction has a floor a little above 1.0 rather than 0, and it is the
/// margin above that floor, not the absolute value, that indicates data
/// traffic. Measured on two deliberately-opposite loops at `-O2`: a
/// byte-copy loop reads 2.67 accesses/instr, a register-only arithmetic
/// loop 1.27. The **write** count is the clean signal, carrying no fetch
/// component at all -- 819308 writes for that copy loop against 110 for
/// the arithmetic one. Both are reported rather than a single total for
/// exactly that reason.
///
/// `None` when no instruction was retired: every ratio would divide by
/// zero, and "0 instructions" is already evident from the cycle line
/// being 0 too.
fn format_emulated_work(cycles: u64, instructions: u64, reads: u64, writes: u64) -> Option<String> {
    if instructions == 0 {
        return None;
    }
    let accesses = reads.saturating_add(writes);
    let cpi = cycles as f64 / instructions as f64;
    let api = accesses as f64 / instructions as f64;
    let counts = format!(
        "{instructions} instructions, {accesses} bus accesses ({reads} read / {writes} write)"
    );
    Some(format!(
        "{counts}, {cpi:.2} cycles/instr, {api:.2} accesses/instr"
    ))
}

/// Formats [`report_emulated_cycles`]' one line, split out from the
/// printing so the wording and the arithmetic are testable without
/// capturing stderr.
///
/// `None` for a non-positive clock rate. [`parse_clock_mhz`] already
/// rejects any rate that low, so this is unreachable from the CLI --
/// but this function formats whatever it is handed, and emitting a
/// silent `inf` into a benchmark log would be worse than emitting
/// nothing.
fn format_emulated_cycles(cycles: u64, clock_hz: f64) -> Option<String> {
    // `is_nan` spelled out rather than `!(clock_hz > 0.0)`: the negated
    // form covers NaN too, but only incidentally, and reads as though a
    // NaN rate were an oversight rather than a case deliberately
    // excluded here.
    if clock_hz.is_nan() || clock_hz <= 0.0 {
        return None;
    }
    let seconds = cycles as f64 / clock_hz;
    Some(format!(
        "{cycles} emulated cycles, {seconds:.6} s at {} MHz",
        clock_hz / 1_000_000.0
    ))
}

fn main() -> ExitCode {
    // Host SIGINT/SIGTERM -> guest SIGBREAKF_CTRL_C (Phase 3 stage 5).
    // Installed here, once, at real CLI startup -- never from
    // `Runtime::new` itself, which would hijack the test runner's own
    // SIGINT handling for every unit test in the workspace. See
    // `volamos_core::exectask`'s module docs.
    install_host_break_handler();

    let mut args = std::env::args();
    let program_name = args.next().unwrap_or_else(|| "volamos".to_string());

    // -h/--help and any CLI parse error short-circuit here, before
    // ~/.volamos/.volamos are even read -- neither is relevant to
    // those paths (see parse_args_raw's doc).
    let (cli_overrides, sanitize, clock_mhz, program, guest_args) = match parse_args_raw(args) {
        Ok(v) => v,
        Err(msg) => {
            if !msg.is_empty() {
                eprintln!("volamos: {msg}");
            }
            print_usage(&program_name);
            return if msg.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            };
        }
    };

    let file_overrides = match config::load_all(&program) {
        Ok(overrides) => overrides,
        Err(msg) => {
            eprintln!("volamos: {msg}");
            return ExitCode::FAILURE;
        }
    };

    // Built-in standard-volume defaults (issue #43): merged in at the
    // very bottom of the precedence chain, below every real config
    // source, so `DEFAULTS`/`VOLUMES_DIR` (from *any* of cli/cwd/
    // program-dir/global -- checked on the merged result, not any one
    // source) can still override whether/where this layer applies.
    // `built_in_defaults` does no I/O itself (see its own doc) -- the
    // directories it names are created lazily, by `Vfs`, only if the
    // guest program actually uses them.
    //
    // Captured before `cli_overrides` is consumed by the merge below --
    // this is the CLI's own `--jit`/`--no-jit` value alone, needed by
    // `resolve` to tell "explicit --jit" apart from "JIT=true only in a
    // config file" for JitSource/check_clock_mhz_jit's error message.
    let cli_jit = cli_overrides.jit;
    let base = config::merge(cli_overrides, file_overrides);
    let defaults_enabled = base.standard_volumes.unwrap_or(true);
    let volumes_dir = base.volumes_dir.clone();
    let merged = if defaults_enabled {
        config::merge(base, config::built_in_defaults(volumes_dir))
    } else {
        base
    };

    let opts = resolve(merged, sanitize, clock_mhz, cli_jit, program, guest_args);

    // Cleanup happens here, once, after `run` (and every nested
    // System()/Execute() it spawned, all sharing this same
    // `ephemeral_dirs` list) has completely finished -- not via a
    // `Drop` impl anywhere -- see `VfsConfig::lazy_volumes`'s doc for
    // why an early per-Vfs `Drop` would be actively wrong here. This
    // also has to run *before* the `std::process::exit` below: that
    // call terminates the process immediately, skipping every pending
    // destructor on the stack, so relying on a scope guard's `Drop`
    // here wouldn't fire in time.
    let result = run(&opts);
    for dir in &opts.ephemeral_dirs {
        let _ = std::fs::remove_dir_all(dir);
    }

    match result {
        Ok(code) => {
            // Guest exit codes are conventionally small (AmigaOS process
            // return codes fit a byte in practice), but D0 is a full
            // 32-bit register; clamp to the host process exit code range
            // the same way a real shell would (low byte).
            std::process::exit(code);
        }
        Err(msg) => {
            eprintln!("volamos: {msg}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> impl Iterator<Item = String> {
        items
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn basic_program_and_guest_args() {
        let opts = parse_args(args(&["prog", "one", "two"])).unwrap();
        assert_eq!(opts.program, "prog");
        assert_eq!(opts.guest_args, vec!["one".to_string(), "two".to_string()]);
        assert!(!opts.verbose);
        assert!(opts.volumes.is_empty());
        assert!(opts.assigns.is_empty());
        assert!(opts.cwd.is_none());
        assert!(opts.auto_assign_root.is_none());
        assert!(!opts.wants_vfs());
        assert_eq!(opts.stack_size, DEFAULT_STACK_SIZE);
    }

    #[test]
    fn verbose_flag_before_program() {
        let opts = parse_args(args(&["-v", "prog"])).unwrap();
        assert!(opts.verbose);
        let opts = parse_args(args(&["--verbose", "prog"])).unwrap();
        assert!(opts.verbose);
    }

    #[test]
    fn snoop_flag_before_program() {
        let opts = parse_args(args(&["-s", "prog"])).unwrap();
        assert!(opts.snoop);
        assert!(!opts.verbose);
        let opts = parse_args(args(&["--snoop", "prog"])).unwrap();
        assert!(opts.snoop);
        let opts = parse_args(args(&["prog"])).unwrap();
        assert!(!opts.snoop);
    }

    #[test]
    fn repeated_volume_flags_accumulate() {
        let opts = parse_args(args(&[
            "-V",
            "SYS:/host/sys",
            "--volume",
            "WORK:/host/work",
            "prog",
        ]))
        .unwrap();
        assert_eq!(
            opts.volumes,
            vec![
                ("SYS".to_string(), PathBuf::from("/host/sys")),
                ("WORK".to_string(), PathBuf::from("/host/work")),
            ]
        );
        assert!(opts.wants_vfs());
    }

    #[test]
    fn volume_hostdir_may_contain_colon() {
        // Split on the FIRST ':' only -- the name can't contain ':', but
        // a host dir (rare on unix, but not impossible) could.
        let opts = parse_args(args(&["-V", "SYS:/host/weird:dir", "prog"])).unwrap();
        assert_eq!(
            opts.volumes,
            vec![("SYS".to_string(), PathBuf::from("/host/weird:dir"))]
        );
    }

    #[test]
    fn assign_with_single_target() {
        let opts = parse_args(args(&["-a", "LIBS:SYS:libs", "prog"])).unwrap();
        assert_eq!(
            opts.assigns,
            vec![("LIBS".to_string(), vec!["SYS:libs".to_string()])]
        );
    }

    #[test]
    fn assign_with_multiple_plus_separated_targets() {
        let opts = parse_args(args(&["-a", "LIBS:SYS:libsA+SYS:libsB", "prog"])).unwrap();
        assert_eq!(
            opts.assigns,
            vec![(
                "LIBS".to_string(),
                vec!["SYS:libsA".to_string(), "SYS:libsB".to_string()]
            )]
        );
    }

    #[test]
    fn repeated_assign_flags_accumulate() {
        let opts = parse_args(args(&[
            "-a",
            "LIBS:SYS:libs",
            "--assign",
            "FONTS:SYS:fonts",
            "prog",
        ]))
        .unwrap();
        assert_eq!(
            opts.assigns,
            vec![
                ("LIBS".to_string(), vec!["SYS:libs".to_string()]),
                ("FONTS".to_string(), vec!["SYS:fonts".to_string()]),
            ]
        );
    }

    #[test]
    fn cwd_flag_sets_explicit_cwd() {
        let opts = parse_args(args(&["--cwd", "SYS:work", "prog"])).unwrap();
        assert_eq!(opts.cwd.as_deref(), Some("SYS:work"));
        assert!(opts.wants_vfs());
    }

    #[test]
    fn auto_assign_flag_sets_root() {
        let opts = parse_args(args(&["--auto-assign", "/host/auto", "prog"])).unwrap();
        assert_eq!(opts.auto_assign_root, Some(PathBuf::from("/host/auto")));
        assert!(opts.wants_vfs());
    }

    #[test]
    fn missing_colon_in_volume_is_an_error() {
        let err = parse_args(args(&["-V", "SYS", "prog"])).unwrap_err();
        assert!(err.contains("NAME:VALUE"), "unexpected message: {err}");
    }

    #[test]
    fn missing_colon_in_assign_is_an_error() {
        let err = parse_args(args(&["-a", "LIBS", "prog"])).unwrap_err();
        assert!(err.contains("NAME:VALUE"), "unexpected message: {err}");
    }

    #[test]
    fn empty_name_before_colon_is_an_error() {
        let err = parse_args(args(&["-V", ":noname", "prog"])).unwrap_err();
        assert!(err.contains("NAME:VALUE"), "unexpected message: {err}");
    }

    #[test]
    fn volume_missing_value_is_an_error() {
        let err = parse_args(args(&["-V"])).unwrap_err();
        assert!(err.contains("-V requires"), "unexpected message: {err}");
    }

    #[test]
    fn assign_missing_value_is_an_error() {
        let err = parse_args(args(&["-a"])).unwrap_err();
        assert!(err.contains("-a requires"), "unexpected message: {err}");
    }

    #[test]
    fn cwd_missing_value_is_an_error() {
        let err = parse_args(args(&["--cwd"])).unwrap_err();
        assert!(err.contains("--cwd requires"), "unexpected message: {err}");
    }

    #[test]
    fn auto_assign_missing_value_is_an_error() {
        let err = parse_args(args(&["--auto-assign"])).unwrap_err();
        assert!(
            err.contains("--auto-assign requires"),
            "unexpected message: {err}"
        );
    }

    #[test]
    fn flags_after_program_go_to_guest_args_not_the_parser() {
        // Anything after the first non-flag positional is the guest
        // program's own argv, even if it looks like one of our flags.
        let opts = parse_args(args(&["prog", "-v", "-V", "SYS:foo", "--cwd", "x"])).unwrap();
        assert_eq!(opts.program, "prog");
        assert_eq!(
            opts.guest_args,
            vec![
                "-v".to_string(),
                "-V".to_string(),
                "SYS:foo".to_string(),
                "--cwd".to_string(),
                "x".to_string(),
            ]
        );
        assert!(!opts.verbose);
        assert!(opts.volumes.is_empty());
    }

    #[test]
    fn missing_program_is_an_error() {
        let err = parse_args(args(&["-v"])).unwrap_err();
        assert_eq!(err, "missing <program> argument");
    }

    #[test]
    fn help_flag_returns_empty_error() {
        let err = parse_args(args(&["-h"])).unwrap_err();
        assert_eq!(err, "");
        let err = parse_args(args(&["--help"])).unwrap_err();
        assert_eq!(err, "");
    }

    // --- default_cwd ---

    #[test]
    fn default_cwd_prefers_explicit_cwd() {
        let opts = parse_args(args(&["--cwd", "SYS:work", "-V", "OTHER:/x", "prog"])).unwrap();
        assert_eq!(default_cwd(&opts), "SYS:work");
    }

    #[test]
    fn default_cwd_falls_back_to_first_volume() {
        let opts = parse_args(args(&[
            "-V",
            "SYS:/host/sys",
            "-V",
            "WORK:/host/work",
            "prog",
        ]))
        .unwrap();
        assert_eq!(default_cwd(&opts), "SYS:");
    }

    #[test]
    fn default_cwd_falls_back_to_first_assign_when_no_volume() {
        let opts = parse_args(args(&["-a", "LIBS:SYS:libs", "prog"])).unwrap();
        assert_eq!(default_cwd(&opts), "LIBS:");
    }

    #[test]
    fn default_cwd_falls_back_to_root_when_only_auto_assign() {
        let opts = parse_args(args(&["--auto-assign", "/host/auto", "prog"])).unwrap();
        assert_eq!(default_cwd(&opts), "root:");
    }

    // --- --stack / --ram / parse_byte_size ---

    #[test]
    fn parse_byte_size_plain_bytes() {
        assert_eq!(parse_byte_size("--stack", "65536").unwrap(), 65536);
        assert_eq!(parse_byte_size("--stack", "0").unwrap(), 0);
    }

    #[test]
    fn parse_byte_size_kib_suffix() {
        assert_eq!(parse_byte_size("--stack", "64K").unwrap(), 64 * 1024);
        assert_eq!(parse_byte_size("--stack", "64k").unwrap(), 64 * 1024);
    }

    #[test]
    fn parse_byte_size_mib_suffix() {
        assert_eq!(parse_byte_size("--ram", "1M").unwrap(), 1024 * 1024);
        assert_eq!(parse_byte_size("--ram", "2m").unwrap(), 2 * 1024 * 1024);
    }

    #[test]
    fn parse_byte_size_rejects_garbage() {
        assert!(parse_byte_size("--stack", "").is_err());
        assert!(parse_byte_size("--stack", "abc").is_err());
        assert!(parse_byte_size("--stack", "4KB").is_err());
        assert!(parse_byte_size("--stack", "-1").is_err());
        assert!(parse_byte_size("--stack", "1.5K").is_err());
    }

    #[test]
    fn parse_byte_size_rejects_overflow() {
        assert!(parse_byte_size("--ram", "4294967295M").is_err());
    }

    #[test]
    fn parse_byte_size_error_names_the_flag() {
        let err = parse_byte_size("--ram", "abc").unwrap_err();
        assert!(err.contains("--ram"), "unexpected message: {err}");
    }

    #[test]
    fn stack_flag_sets_stack_size() {
        let opts = parse_args(args(&["--stack", "8192", "prog"])).unwrap();
        assert_eq!(opts.stack_size, 8192);
    }

    #[test]
    fn stack_flag_accepts_suffixes() {
        let opts = parse_args(args(&["--stack", "256K", "prog"])).unwrap();
        assert_eq!(opts.stack_size, 256 * 1024);
    }

    #[test]
    fn stack_missing_value_is_an_error() {
        let err = parse_args(args(&["--stack"])).unwrap_err();
        assert!(
            err.contains("--stack requires"),
            "unexpected message: {err}"
        );
    }

    #[test]
    fn stack_invalid_value_is_an_error() {
        let err = parse_args(args(&["--stack", "notanumber", "prog"])).unwrap_err();
        assert!(err.contains("--stack"), "unexpected message: {err}");
    }

    #[test]
    fn default_stack_size_used_when_flag_absent() {
        let opts = parse_args(args(&["prog"])).unwrap();
        assert_eq!(opts.stack_size, DEFAULT_STACK_SIZE);
    }

    // --- --ram ---

    #[test]
    fn ram_flag_sets_ram_size() {
        let opts = parse_args(args(&["--ram", "2M", "prog"])).unwrap();
        assert_eq!(opts.ram_size, 2 * 1024 * 1024);
    }

    #[test]
    fn ram_missing_value_is_an_error() {
        let err = parse_args(args(&["--ram"])).unwrap_err();
        assert!(err.contains("--ram requires"), "unexpected message: {err}");
    }

    #[test]
    fn ram_invalid_value_is_an_error() {
        let err = parse_args(args(&["--ram", "notanumber", "prog"])).unwrap_err();
        assert!(err.contains("--ram"), "unexpected message: {err}");
    }

    #[test]
    fn default_ram_size_used_when_flag_absent() {
        let opts = parse_args(args(&["prog"])).unwrap();
        assert_eq!(opts.ram_size, DEFAULT_RAM_SIZE);
    }

    // --- check_ram_fits ---

    #[test]
    fn check_ram_fits_accepts_when_there_is_room() {
        assert!(check_ram_fits(0x1000, 0x1000, DEFAULT_RAM_SIZE).is_ok());
    }

    #[test]
    fn check_ram_fits_rejects_when_stack_leaves_no_heap_room() {
        let err = check_ram_fits(0x1000, 0x1000, 0x2000).unwrap_err();
        assert!(err.contains("--stack"), "unexpected message: {err}");
        assert!(err.contains("--ram"), "unexpected message: {err}");
    }

    #[test]
    fn check_ram_fits_rejects_on_overflowing_sum() {
        assert!(check_ram_fits(u32::MAX, u32::MAX, u32::MAX).is_err());
    }

    #[test]
    fn the_default_ram_size_is_exactly_what_a_68000_can_address() {
        // Issue #98: the two constants have to agree, or the default
        // configuration is either broken or needlessly small. If a
        // future `DEFAULT_RAM_SIZE` bump ever wants more, it has to come
        // with a `--cpu` default change (or a clamp) as well.
        assert_eq!(
            u64::from(DEFAULT_RAM_SIZE),
            addressable_bytes(CpuType::M68000)
        );
        assert!(check_ram_addressable(DEFAULT_RAM_SIZE, CpuType::M68000).is_ok());
    }

    #[test]
    fn check_ram_addressable_rejects_over_16m_on_a_24_bit_cpu() {
        // One byte over is enough: the guest stack sits at the top of
        // the address space, so this is not a "close enough" situation.
        for cpu in [CpuType::M68000, CpuType::M68010] {
            let err = check_ram_addressable(DEFAULT_RAM_SIZE + 1, cpu).unwrap_err();
            assert!(err.contains("24-bit address bus"), "{err}");
            assert!(err.contains(cpu_model_name(cpu)), "{err}");
            assert!(err.contains("--cpu 68020"), "{err}");
        }
    }

    #[test]
    fn check_ram_addressable_accepts_any_size_on_a_32_bit_cpu() {
        for cpu in [
            CpuType::M68020,
            CpuType::M68EC020,
            CpuType::M68030,
            CpuType::M68EC030,
            CpuType::M68040,
            CpuType::M68EC040,
            CpuType::M68LC040,
            CpuType::M68060,
            CpuType::SCC68070,
        ] {
            assert!(
                check_ram_addressable(u32::MAX, cpu).is_ok(),
                "{} should take any u32 address space",
                cpu_model_name(cpu)
            );
        }
    }

    #[test]
    fn cpu_model_name_round_trips_through_parse_cpu_type() {
        // Keeps the diagnostic spelling honest: whatever name an error
        // message shows has to be a name the user could have typed.
        for cpu in [
            CpuType::M68000,
            CpuType::M68010,
            CpuType::M68020,
            CpuType::M68EC020,
            CpuType::M68030,
            CpuType::M68EC030,
            CpuType::M68040,
            CpuType::M68EC040,
            CpuType::M68LC040,
            CpuType::M68060,
            CpuType::SCC68070,
        ] {
            assert_eq!(parse_cpu_type(cpu_model_name(cpu)), Ok(cpu));
        }
    }

    #[test]
    fn default_cpu_is_68000_with_no_fpu() {
        let opts = parse_args(args(&["prog"])).unwrap();
        assert_eq!(opts.cpu_type, CpuType::M68000);
        assert!(!opts.fpu);
    }

    #[test]
    fn cpu_flag_parses_every_documented_model() {
        let cases: &[(&str, CpuType)] = &[
            ("68000", CpuType::M68000),
            ("68010", CpuType::M68010),
            ("68020", CpuType::M68020),
            ("68EC020", CpuType::M68EC020),
            ("68030", CpuType::M68030),
            ("68ec030", CpuType::M68EC030),
            ("68040", CpuType::M68040),
            ("68ec040", CpuType::M68EC040),
            ("68lc040", CpuType::M68LC040),
            ("68060", CpuType::M68060),
            ("scc68070", CpuType::SCC68070),
        ];
        for (name, expected) in cases {
            let opts = parse_args(args(&["--cpu", name, "prog"])).unwrap();
            assert_eq!(opts.cpu_type, *expected, "--cpu {name}");
        }
    }

    #[test]
    fn cpu_flag_with_an_unknown_model_is_an_error() {
        let err = parse_args(args(&["--cpu", "68080", "prog"])).unwrap_err();
        assert!(err.contains("--cpu"), "unexpected message: {err}");
    }

    #[test]
    fn cpu_missing_value_is_an_error() {
        let err = parse_args(args(&["--cpu"])).unwrap_err();
        assert!(err.contains("--cpu requires"), "unexpected message: {err}");
    }

    #[test]
    fn fpu_flag_enables_fpu() {
        let opts = parse_args(args(&["--fpu", "prog"])).unwrap();
        assert!(opts.fpu);
    }

    #[test]
    fn no_fpu_flag_after_fpu_wins() {
        // Last flag wins, same convention as every other boolean flag
        // here (e.g. -v doesn't have an "un-verbose" counterpart to
        // test this against, but the parse loop's plain assignment
        // makes this the natural, unsurprising behavior either way).
        let opts = parse_args(args(&["--fpu", "--no-fpu", "prog"])).unwrap();
        assert!(!opts.fpu);
    }

    #[test]
    fn default_jit_is_off() {
        let opts = parse_args(args(&["prog"])).unwrap();
        assert!(!opts.jit);
    }

    #[test]
    fn jit_flag_enables_jit() {
        let opts = parse_args(args(&["--jit", "prog"])).unwrap();
        assert!(opts.jit);
    }

    /// Test: the exit-time cycle report's wording and arithmetic.
    /// Pins the seconds conversion (cycles / clock_hz) and the MHz the
    /// line quotes back, so a future change to either is a deliberate
    /// one -- a benchmark harness may well be parsing this line.
    #[test]
    fn emulated_cycles_line_reports_cycles_and_derived_seconds() {
        // 25_000_000 cycles at 25 MHz is exactly one second, which makes
        // the conversion checkable by inspection rather than by
        // replicating the arithmetic the code under test performs.
        assert_eq!(
            format_emulated_cycles(25_000_000, 25_000_000.0).unwrap(),
            "25000000 emulated cycles, 1.000000 s at 25 MHz"
        );
        // A fractional rate, as Copperline's own benchmark config uses.
        assert_eq!(
            format_emulated_cycles(7_093_790, 7_093_790.0).unwrap(),
            "7093790 emulated cycles, 1.000000 s at 7.09379 MHz"
        );
        // A run that executed nothing still reports, rather than
        // silently omitting the line: "zero cycles" is a result.
        assert_eq!(
            format_emulated_cycles(0, 25_000_000.0).unwrap(),
            "0 emulated cycles, 0.000000 s at 25 MHz"
        );
    }

    /// Test: the work line's counts and both derived ratios.
    ///
    /// Pins the wording and the arithmetic, since a benchmark harness may
    /// parse this, and pins that reads and writes are reported separately
    /// rather than only as a total -- the write count is the one carrying
    /// no instruction-fetch component, so collapsing them would destroy
    /// the line's most useful signal (see `format_emulated_work`'s doc).
    #[test]
    fn emulated_work_line_reports_counts_and_both_ratios() {
        let line = format_emulated_work(1_000, 100, 180, 20).unwrap();
        assert_eq!(
            line,
            "100 instructions, 200 bus accesses (180 read / 20 write), 10.00 cycles/instr, 2.00 accesses/instr"
        );
    }

    /// Test: no work line at all when nothing retired, rather than a line
    /// full of divide-by-zero `NaN`/`inf` ratios.
    #[test]
    fn emulated_work_line_is_omitted_when_no_instruction_retired() {
        assert!(format_emulated_work(0, 0, 0, 0).is_none());
        // Cycles and accesses without a retired instruction still has no
        // meaningful denominator.
        assert!(format_emulated_work(500, 0, 12, 3).is_none());
    }

    /// Test: a non-positive clock rate formats to nothing rather than to
    /// an `inf`/`NaN` seconds figure. Unreachable through the CLI --
    /// `parse_clock_mhz` rejects these well before here -- but the
    /// formatter is responsible for what it emits regardless of who
    /// calls it, and a bogus number in a benchmark log is worse than a
    /// missing line.
    #[test]
    fn emulated_cycles_line_is_omitted_for_a_nonpositive_clock_rate() {
        assert!(format_emulated_cycles(1_000, 0.0).is_none());
        assert!(format_emulated_cycles(1_000, -25_000_000.0).is_none());
        assert!(format_emulated_cycles(1_000, f64::NAN).is_none());
    }

    #[test]
    fn no_jit_flag_after_jit_wins() {
        let opts = parse_args(args(&["--jit", "--no-jit", "prog"])).unwrap();
        assert!(!opts.jit);
    }

    #[test]
    fn default_sanitize_is_off() {
        let opts = parse_args(args(&["prog"])).unwrap();
        assert!(!opts.sanitize.enabled);
        assert!(!opts.sanitize.uninit);
        assert!(opts.sanitize.ignore_pcs.is_empty());
    }

    #[test]
    fn sanitize_flag_enables_sanitize() {
        let opts = parse_args(args(&["--sanitize", "prog"])).unwrap();
        assert!(opts.sanitize.enabled);
        assert!(
            !opts.sanitize.uninit,
            "--sanitize alone must not turn on uninitialized-read reporting"
        );
    }

    #[test]
    fn dirty_heap_is_independent_of_sanitize() {
        // --dirty-heap must NOT imply --sanitize: it changes what the
        // guest sees and is useful with no shadow map at all.
        let opts = parse_args(args(&["--dirty-heap", "prog"])).unwrap();
        assert!(opts.sanitize.dirty_heap);
        assert!(
            !opts.sanitize.enabled,
            "--dirty-heap must not turn the sanitizer on"
        );

        // ...and --sanitize must not imply --dirty-heap, or the
        // detector would start perturbing the program it watches.
        let opts = parse_args(args(&["--sanitize", "prog"])).unwrap();
        assert!(opts.sanitize.enabled);
        assert!(!opts.sanitize.dirty_heap);
    }

    #[test]
    fn sanitize_uninit_implies_sanitize() {
        // Asking for uninitialized-read reporting without the shadow
        // map installed could only be a mistake, so the flag implies
        // --sanitize rather than silently doing nothing.
        let opts = parse_args(args(&["--sanitize-uninit", "prog"])).unwrap();
        assert!(opts.sanitize.enabled);
        assert!(opts.sanitize.uninit);
    }

    #[test]
    fn sanitize_ignore_pc_accepts_hex_and_decimal_and_repeats() {
        let opts = parse_args(args(&[
            "--sanitize",
            "--sanitize-ignore-pc",
            "0xe14a",
            "--sanitize-ignore-pc",
            "4096",
            "prog",
        ]))
        .unwrap();
        assert_eq!(opts.sanitize.ignore_pcs, vec![0xe14a, 4096]);
    }

    #[test]
    fn sanitize_ignore_pc_rejects_a_bad_address_and_a_missing_one() {
        assert!(parse_args(args(&["--sanitize-ignore-pc", "nonsense", "prog"])).is_err());
        assert!(parse_args(args(&["--sanitize-ignore-pc"])).is_err());
    }

    #[test]
    fn sanitize_flag_does_not_disable_the_jit_flag_itself() {
        // --sanitize forces the *effective* JIT off at the CPU (see
        // `run`/`run_nested_program`), but `Options::jit` itself just
        // records what --jit/--no-jit said -- the two are independent
        // settings, combined only where the CPU is actually configured.
        let opts = parse_args(args(&["--jit", "--sanitize", "prog"])).unwrap();
        assert!(opts.jit);
        assert!(opts.sanitize.enabled);
    }

    // --- --clock-mhz (issue #102) ---

    #[test]
    fn default_clock_mhz_is_off() {
        let opts = parse_args(args(&["prog"])).unwrap();
        assert_eq!(opts.clock_mhz, None);
    }

    #[test]
    fn clock_mhz_flag_accepts_an_integer_value() {
        let opts = parse_args(args(&["--clock-mhz", "25", "prog"])).unwrap();
        assert_eq!(opts.clock_mhz, Some(25.0));
    }

    #[test]
    fn clock_mhz_flag_accepts_a_fractional_value() {
        // Copperline's own A600/Gayle configuration models a 25 MHz
        // 68000, and a real accelerator's rated speed is routinely a
        // fraction (e.g. an NTSC-derived clock-doubler board) -- an
        // integer-only parser would force rounding away the exact rate
        // a caller is trying to model, so fractional MHz must parse.
        let opts = parse_args(args(&["--clock-mhz", "7.14", "prog"])).unwrap();
        assert_eq!(opts.clock_mhz, Some(7.14));
    }

    #[test]
    fn clock_mhz_flag_rejects_zero_negative_and_non_numeric() {
        assert!(parse_args(args(&["--clock-mhz", "0", "prog"])).is_err());
        assert!(parse_args(args(&["--clock-mhz", "-5", "prog"])).is_err());
        assert!(parse_args(args(&["--clock-mhz", "nonsense", "prog"])).is_err());
        assert!(parse_args(args(&["--clock-mhz"])).is_err());
    }

    #[test]
    fn clock_mhz_flag_rejects_nan_and_infinity() {
        // f64::parse happily accepts these spellings; --clock-mhz must
        // not, or they'd poison every downstream division in
        // M68kCpu::run_via_cycles/read_eclock_handler.
        assert!(parse_args(args(&["--clock-mhz", "NaN", "prog"])).is_err());
        assert!(parse_args(args(&["--clock-mhz", "inf", "prog"])).is_err());
        assert!(parse_args(args(&["--clock-mhz", "-inf", "prog"])).is_err());
    }

    #[test]
    fn clock_mhz_flag_rejects_an_absurdly_high_value() {
        assert!(parse_args(args(&["--clock-mhz", "1000000", "prog"])).is_err());
    }

    #[test]
    fn clock_mhz_flag_rejects_an_absurdly_low_value() {
        // Without a floor, --clock-mhz 1e-30 parses as an ordinary
        // positive finite f64 and drives every ReadEClock tick count to
        // f64::INFINITY, silently saturating to u64::MAX on the cast --
        // no panic, but a benchmark reporting the largest possible tick
        // count instead of a clear rejection is exactly the "looks
        // fine, isn't" result this flag exists to avoid.
        assert!(parse_args(args(&["--clock-mhz", "1e-30", "prog"])).is_err());
    }

    #[test]
    fn clock_mhz_with_explicit_jit_is_rejected_by_check_clock_mhz_jit() {
        // run_batch (the JIT path) never surfaces a cycle count at all,
        // so there is nothing for --clock-mhz to derive ReadEClock's
        // emulated time from -- this must be a clean error, not a
        // silent fallback to the host clock or to --no-jit behavior.
        let opts = parse_args(args(&["--clock-mhz", "25", "--jit", "prog"])).unwrap();
        assert_eq!(opts.jit_source, JitSource::CommandLine);
        assert!(check_clock_mhz_jit(opts.clock_mhz, opts.jit, opts.jit_source).is_err());
    }

    #[test]
    fn clock_mhz_with_default_jit_is_accepted_by_check_clock_mhz_jit() {
        // --jit defaults to off (see default_jit_is_off), so an
        // ordinary --clock-mhz run with no --jit/--no-jit mentioned at
        // all must not spuriously trip the mutual-exclusion check --
        // only an explicit jit=true (which can only come from an
        // explicit --jit or a config file's JIT=true) may.
        let opts = parse_args(args(&["--clock-mhz", "25", "prog"])).unwrap();
        assert!(!opts.jit);
        assert_eq!(opts.jit_source, JitSource::Default);
        assert!(check_clock_mhz_jit(opts.clock_mhz, opts.jit, opts.jit_source).is_ok());
    }

    #[test]
    fn clock_mhz_with_explicit_no_jit_is_accepted_by_check_clock_mhz_jit() {
        let opts = parse_args(args(&["--clock-mhz", "25", "--no-jit", "prog"])).unwrap();
        assert!(check_clock_mhz_jit(opts.clock_mhz, opts.jit, opts.jit_source).is_ok());
    }

    #[test]
    fn clock_mhz_jit_error_names_no_jit_when_jit_came_from_the_command_line() {
        let err = check_clock_mhz_jit(Some(25.0), true, JitSource::CommandLine).unwrap_err();
        assert!(
            err.contains("drop --jit"),
            "error should point at the --jit the user actually typed: {err}"
        );
    }

    #[test]
    fn clock_mhz_jit_error_names_the_config_file_when_jit_came_from_one() {
        // A JIT=true sitting in ~/.volamos/.volamos, with no --jit typed
        // on this command line, must not be blamed on a --jit the user
        // never wrote -- the message should point at --no-jit/the
        // config file instead.
        let err = check_clock_mhz_jit(Some(25.0), true, JitSource::ConfigFile).unwrap_err();
        assert!(
            !err.contains("drop --jit"),
            "error should not blame a --jit the user never typed: {err}"
        );
        assert!(
            err.contains("--no-jit") && err.contains("config"),
            "error should mention overriding via --no-jit or editing the config file: {err}"
        );
    }

    #[test]
    fn clock_mhz_with_sanitize_is_rejected() {
        // run_via_cycles (the --clock-mhz execution path) skips every
        // per-instruction sanitizer hook -- see check_clock_mhz_sanitize's
        // doc. Running both together must be a clean error, not a
        // silently-incomplete "no violations" report.
        let opts = parse_args(args(&["--clock-mhz", "25", "--sanitize", "prog"])).unwrap();
        assert!(check_clock_mhz_sanitize(opts.clock_mhz, opts.sanitize.enabled).is_err());
    }

    #[test]
    fn clock_mhz_without_sanitize_is_accepted_by_check_clock_mhz_sanitize() {
        let opts = parse_args(args(&["--clock-mhz", "25", "prog"])).unwrap();
        assert!(check_clock_mhz_sanitize(opts.clock_mhz, opts.sanitize.enabled).is_ok());
    }

    #[test]
    fn sanitize_without_clock_mhz_is_accepted_by_check_clock_mhz_sanitize() {
        let opts = parse_args(args(&["--sanitize", "prog"])).unwrap();
        assert!(check_clock_mhz_sanitize(opts.clock_mhz, opts.sanitize.enabled).is_ok());
    }

    // --- --defaults/--no-defaults/--volumes-dir (issue #43) ---
    //
    // `standard_volumes`/`volumes_dir` are consumed by `main` before
    // `resolve` ever runs (deciding whether/where to merge in
    // `config::built_in_defaults`), so they don't surface on `Options`
    // itself -- these check `parse_args_raw`'s `Overrides` directly,
    // unlike every test above.

    #[test]
    fn defaults_flag_sets_standard_volumes_on() {
        let (overrides, _, _, _, _) = parse_args_raw(args(&["--defaults", "prog"])).unwrap();
        assert_eq!(overrides.standard_volumes, Some(true));
    }

    #[test]
    fn no_defaults_flag_sets_standard_volumes_off() {
        let (overrides, _, _, _, _) = parse_args_raw(args(&["--no-defaults", "prog"])).unwrap();
        assert_eq!(overrides.standard_volumes, Some(false));
    }

    #[test]
    fn default_standard_volumes_is_unset() {
        // Resolved to "on" by `main` (`.unwrap_or(true)`), but the raw
        // CLI parse itself must report "not specified", so a config
        // file's own DEFAULTS= can still be told apart from an explicit
        // --defaults.
        let (overrides, _, _, _, _) = parse_args_raw(args(&["prog"])).unwrap();
        assert_eq!(overrides.standard_volumes, None);
    }

    #[test]
    fn volumes_dir_flag_sets_the_override() {
        let (overrides, _, _, _, _) =
            parse_args_raw(args(&["--volumes-dir", "/custom/vols", "prog"])).unwrap();
        assert_eq!(overrides.volumes_dir, Some(PathBuf::from("/custom/vols")));
    }

    #[test]
    fn volumes_dir_missing_value_is_an_error() {
        let err = parse_args_raw(args(&["--volumes-dir"])).unwrap_err();
        assert!(
            err.contains("--volumes-dir requires"),
            "unexpected message: {err}"
        );
    }

    // --- attn_flags_for ---
    //
    // Independently-computed expected values (not just re-deriving the
    // same OR chain the implementation uses), matching the real,
    // verified exec/execbase.h bit positions: AFF_68010=1<<0,
    // AFF_68020=1<<1, AFF_68030=1<<2, AFF_68040=1<<3, AFF_68881=1<<4,
    // AFF_68882=1<<5, AFF_FPU40=1<<6, AFF_68060=1<<7.

    #[test]
    fn attn_flags_68000_is_zero() {
        assert_eq!(attn_flags_for(CpuType::M68000, false), 0);
        // --fpu on a 68000 is a no-op -- no coprocessor interface at
        // all below 68020 (see M68kCpu::with_config's doc).
        assert_eq!(attn_flags_for(CpuType::M68000, true), 0);
    }

    #[test]
    fn attn_flags_68010_sets_only_its_own_bit() {
        assert_eq!(attn_flags_for(CpuType::M68010, false), 0x1);
    }

    #[test]
    fn attn_flags_68020_sets_68010_and_68020_bits() {
        assert_eq!(attn_flags_for(CpuType::M68020, false), 0x1 | 0x2);
        assert_eq!(attn_flags_for(CpuType::M68EC020, false), 0x1 | 0x2);
    }

    #[test]
    fn attn_flags_68030_sets_68010_68020_68030_bits() {
        assert_eq!(attn_flags_for(CpuType::M68030, false), 0x1 | 0x2 | 0x4);
    }

    #[test]
    fn attn_flags_68040_sets_every_lower_cpu_bit() {
        assert_eq!(
            attn_flags_for(CpuType::M68040, false),
            0x1 | 0x2 | 0x4 | 0x8
        );
    }

    #[test]
    fn attn_flags_68060_sets_every_lower_cpu_bit_plus_its_own() {
        assert_eq!(
            attn_flags_for(CpuType::M68060, false),
            0x1 | 0x2 | 0x4 | 0x8 | 0x80
        );
    }

    #[test]
    fn attn_flags_68020_with_fpu_sets_68881_and_68882() {
        assert_eq!(
            attn_flags_for(CpuType::M68020, true),
            0x1 | 0x2 | (1 << 4) | (1 << 5)
        );
    }

    #[test]
    fn attn_flags_68030_with_fpu_sets_68881_and_68882() {
        assert_eq!(
            attn_flags_for(CpuType::M68030, true),
            0x1 | 0x2 | 0x4 | (1 << 4) | (1 << 5)
        );
    }

    #[test]
    fn attn_flags_68040_with_fpu_sets_fpu40_not_68881() {
        assert_eq!(
            attn_flags_for(CpuType::M68040, true),
            0x1 | 0x2 | 0x4 | 0x8 | (1 << 6)
        );
    }

    #[test]
    fn attn_flags_68ec040_with_fpu_has_no_fpu_bit() {
        // M68EC040 has no on-die FPU and no external-68881-socket
        // option -- --fpu is a documented no-op there.
        assert_eq!(
            attn_flags_for(CpuType::M68EC040, true),
            attn_flags_for(CpuType::M68EC040, false)
        );
    }

    #[test]
    fn attn_flags_scc68070_is_zero() {
        // No documented AFF_* bit exists for this model.
        assert_eq!(attn_flags_for(CpuType::SCC68070, false), 0);
        assert_eq!(attn_flags_for(CpuType::SCC68070, true), 0);
    }
}
