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
//! grammar and the `--cwd` defaulting rule. If none of those flags are
//! given, no `Vfs` is installed at all (unchanged from pre-T13
//! behavior): path-based dos.library calls fail cleanly with an IoErr,
//! everything else (`Input`/`Output`/`PutStr`/...) still works.
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
//! exceeds `--ram`.
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
//! `~/.volamos` and a `.volamos` in the current directory supply
//! default values for all of the above (except `<program>`/
//! `[args...]` themselves) so a repeated-use project doesn't need to
//! retype them -- explicit flags on the command line always win, then
//! the local file, then the global one -- see [`config`]'s module doc
//! for the exact grammar and merge semantics.

mod config;

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use volamos_core::backend::{CpuType, M68kCpu, TRAP_TABLE_END};
use volamos_core::dispatch::{Runtime, StartConfig, TraceEvent};
use volamos_core::exectask::install_host_break_handler;
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
}

impl Options {
    /// Whether any VFS-related flag was given at all -- if not, `run`
    /// doesn't install a [`Vfs`] on the [`Runtime`], preserving
    /// pre-T13 behavior exactly.
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
         [--auto-assign HOSTDIR] [--stack SIZE] [--ram SIZE] [--cpu MODEL] \
         [--fpu|--no-fpu] <program> [args...]"
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
    eprintln!("                            runtime's own guest heap");
    eprintln!("  --cpu MODEL               emulated CPU (default 68000): 68000, 68010, 68020,");
    eprintln!("                            68ec020, 68030, 68ec030, 68040, 68ec040, 68lc040,");
    eprintln!("                            68060, or scc68070");
    eprintln!("  --fpu / --no-fpu          whether a coprocessor FPU is fitted (default: no FPU);");
    eprintln!("                            only meaningful for --cpu 68020 and later -- earlier");
    eprintln!("                            models have no coprocessor interface at all, so F-line");
    eprintln!("                            (FPU) instructions always trap on them regardless");
    eprintln!();
    eprintln!("[args...] is passed to the guest program's command line (A0/D0).");
    eprintln!();
    eprintln!(
        "If none of -V/-a/--cwd/--auto-assign are given, no volume/assign filesystem is \
         installed at all: dos.library path-based calls (Open, Lock, Examine, ...) fail \
         cleanly with an IoErr; Input/Output/PutStr/IoErr/SetIoErr work either way."
    );
    eprintln!();
    eprintln!(
        "~/.volamos supplies default values for the flags above (KEY=value lines, e.g. \
         STACK=256K); a .volamos in the current directory overrides it; explicit flags on \
         this command line win over both. See the Configuration page in the docs."
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
fn parse_args_raw(
    mut args: impl Iterator<Item = String>,
) -> Result<(config::Overrides, String, Vec<String>), String> {
    let mut overrides = config::Overrides::default();
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
            _ => program = Some(arg),
        }
    }

    let program = program.ok_or_else(|| "missing <program> argument".to_string())?;
    Ok((overrides, program, guest_args))
}

/// Fills every unset field of `overrides` with its built-in default,
/// producing the final [`Options`] `run` consumes. Used both by
/// [`parse_args`] (CLI-only, no config files) and by `main` (after
/// merging CLI overrides with `~/.volamos`/`.volamos`).
fn resolve(overrides: config::Overrides, program: String, guest_args: Vec<String>) -> Options {
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
    }
}

/// CLI-only argument parsing, ignoring `~/.volamos`/`.volamos` entirely.
/// `main` doesn't use this directly (it needs [`parse_args_raw`]'s CLI
/// overrides kept separate so it can merge in config-file values before
/// resolving defaults) -- this is the convenience every test in this
/// module wants instead.
#[cfg(test)]
fn parse_args(args: impl Iterator<Item = String>) -> Result<Options, String> {
    let (overrides, program, guest_args) = parse_args_raw(args)?;
    Ok(resolve(overrides, program, guest_args))
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
) -> i32 {
    let Ok(bytes) = std::fs::read(host_path) else {
        return -1;
    };
    let Ok(hunk_file) = loader::parse(&bytes) else {
        return -1;
    };
    let mut mem = FlatMemory::new(ram_size as usize);
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
    let mut runtime = Runtime::new(M68kCpu::with_config(cpu_type, fpu), mem, config);

    if let Some(vfs_config) = vfs_config {
        match Vfs::new(vfs_config) {
            Ok(vfs) => runtime.set_vfs(vfs),
            Err(_) => return -1,
        }
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    runtime.run(&mut out, None).unwrap_or(-1)
}

fn run(opts: &Options) -> Result<i32, String> {
    let bytes = std::fs::read(&opts.program)
        .map_err(|e| format!("couldn't read '{}': {e}", opts.program))?;

    let hunk_file = loader::parse(&bytes).map_err(|e: LoadError| {
        format!("'{}' is not a valid hunk executable: {e}", opts.program)
    })?;

    let mut mem = FlatMemory::new(opts.ram_size as usize);
    let load_result = loader::load(&hunk_file, &mut mem, TRAP_TABLE_END)
        .map_err(|e| format!("couldn't load '{}': {e}", opts.program))?;
    check_ram_fits(load_result.end, opts.stack_size, opts.ram_size)?;

    let cpu = M68kCpu::with_config(opts.cpu_type, opts.fpu);
    let config = StartConfig {
        entry: load_result.entry,
        load_end: load_result.end,
        args: opts.guest_args.clone(),
        raw_command_line: None,
        stack_size: opts.stack_size,
        attn_flags: attn_flags_for(opts.cpu_type, opts.fpu),
        program_name: program_name_from_path(std::path::Path::new(&opts.program)),
    };
    let mut runtime = Runtime::new(cpu, mem, config);

    let vfs_config = vfs_config_from_opts(opts);
    if let Some(config) = vfs_config.clone() {
        let vfs = Vfs::new(config).map_err(|e| format!("couldn't set up volumes/assigns: {e}"))?;
        runtime.set_vfs(vfs);
    }

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

    runtime
        .run(&mut out, Some(&mut trace))
        .map_err(|e| format!("{}: {e}", opts.program))
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
    let (cli_overrides, program, guest_args) = match parse_args_raw(args) {
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

    let file_overrides = match config::load_all() {
        Ok(overrides) => overrides,
        Err(msg) => {
            eprintln!("volamos: {msg}");
            return ExitCode::FAILURE;
        }
    };

    let opts = resolve(
        config::merge(cli_overrides, file_overrides),
        program,
        guest_args,
    );

    match run(&opts) {
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
