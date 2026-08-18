//! `volamos` command-line entry point.
//!
//! Loads an AmigaOS "hunk" CLI executable, runs it against
//! `volamos-core`'s fake-library dispatch runtime, and exits with the
//! guest program's own exit code.
//!
//! ```text
//! volamos [-v|--verbose] [-V NAME:hostdir]... [-a NAME:target[+target...]]...
//!         [--cwd AMIGAPATH] [--auto-assign HOSTDIR] <program> [args...]
//! ```
//!
//! `[args...]` is passed through to the guest program per AmigaOS
//! startup convention (joined with spaces into a command-line buffer,
//! `A0`/`D0` -- see `volamos_core::dispatch::Runtime::new`); a program
//! that parses its own arguments (e.g. via `ReadArgs`) can read them
//! from there. `-v`/`--verbose` logs each trapped library call (library
//! name, LVO, and handler name) to stderr as it happens.
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
//! (MiB) -- see [`parse_stack_size`]. Values below
//! [`volamos_core::MIN_STACK_SIZE`] are silently clamped up to it by
//! [`volamos_core::dispatch::Runtime::new`], mirroring real AmigaOS's
//! own stack-size clamp.

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use volamos_core::backend::{M68kCpu, TRAP_TABLE_END};
use volamos_core::dispatch::{Runtime, StartConfig, TraceEvent};
use volamos_core::exectask::install_host_break_handler;
use volamos_core::memory::FlatMemory;
use volamos_core::vfs::{Vfs, VfsConfig};
use volamos_core::{DEFAULT_STACK_SIZE, LoadError, loader};

/// Guest address space size. Generous for the tiny CLI binaries this
/// runtime currently targets; large enough to leave headroom above the
/// loaded program for its stack.
const GUEST_MEMORY_SIZE: usize = 1 << 20; // 1 MiB

#[derive(Debug)]
struct Options {
    verbose: bool,
    program: String,
    guest_args: Vec<String>,
    volumes: Vec<(String, PathBuf)>,
    assigns: Vec<(String, Vec<String>)>,
    cwd: Option<String>,
    auto_assign_root: Option<PathBuf>,
    stack_size: u32,
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
        "usage: {program_name} [-v|--verbose] [-V NAME:hostdir]... \
         [-a NAME:target[+target...]]... [--cwd AMIGAPATH] \
         [--auto-assign HOSTDIR] [--stack SIZE] <program> [args...]"
    );
    eprintln!();
    eprintln!("Runs an AmigaOS CLI hunk executable under volamos.");
    eprintln!();
    eprintln!("options:");
    eprintln!("  -v, --verbose             log each emulated library call to stderr");
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
    eprintln!();
    eprintln!("[args...] is passed to the guest program's command line (A0/D0).");
    eprintln!();
    eprintln!(
        "If none of -V/-a/--cwd/--auto-assign are given, no volume/assign filesystem is \
         installed at all: dos.library path-based calls (Open, Lock, Examine, ...) fail \
         cleanly with an IoErr; Input/Output/PutStr/IoErr/SetIoErr work either way."
    );
}

/// Parses a `--stack SIZE` value: a plain non-negative byte count, or the
/// same followed by a single `K`/`k` (KiB, `* 1024`) or `M`/`m` (MiB,
/// `* 1024 * 1024`) suffix -- e.g. `"65536"`, `"64K"`, `"1M"`. Rejects
/// empty input, non-digit content before the optional suffix, more than
/// one suffix character, and multiplications that would overflow `u32`
/// (a guest address space is at most 4 GiB, so an overflowing stack
/// request is never satisfiable anyway).
fn parse_stack_size(s: &str) -> Result<u32, String> {
    let (digits, multiplier) = match s.as_bytes().last() {
        Some(b'K') | Some(b'k') => (&s[..s.len() - 1], 1024u32),
        Some(b'M') | Some(b'm') => (&s[..s.len() - 1], 1024 * 1024u32),
        _ => (s, 1u32),
    };
    let value: u32 = digits.parse().map_err(|_| {
        format!("--stack expects a byte count (optionally K/M-suffixed), got {s:?}")
    })?;
    value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("--stack value {s:?} overflows"))
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
fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut verbose = false;
    let mut program = None;
    let mut guest_args = Vec::new();
    let mut volumes = Vec::new();
    let mut assigns = Vec::new();
    let mut cwd = None;
    let mut auto_assign_root = None;
    let mut stack_size = DEFAULT_STACK_SIZE;

    while let Some(arg) = args.next() {
        if program.is_some() {
            guest_args.push(arg);
            continue;
        }
        match arg.as_str() {
            "-v" | "--verbose" => verbose = true,
            "-h" | "--help" => return Err(String::new()), // caller prints usage and exits 0
            "-V" | "--volume" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("{arg} requires a NAME:hostdir argument"))?;
                let (name, hostdir) = split_name_value(&arg, &value)?;
                volumes.push((name.to_string(), PathBuf::from(hostdir)));
            }
            "-a" | "--assign" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("{arg} requires a NAME:target[+target...] argument"))?;
                let (name, targets) = split_name_value(&arg, &value)?;
                let targets: Vec<String> = targets.split('+').map(str::to_string).collect();
                assigns.push((name.to_string(), targets));
            }
            "--cwd" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--cwd requires an AMIGAPATH argument".to_string())?;
                cwd = Some(value);
            }
            "--auto-assign" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--auto-assign requires a HOSTDIR argument".to_string())?;
                auto_assign_root = Some(PathBuf::from(value));
            }
            "--stack" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--stack requires a SIZE argument".to_string())?;
                stack_size = parse_stack_size(&value)?;
            }
            _ => program = Some(arg),
        }
    }

    let program = program.ok_or_else(|| "missing <program> argument".to_string())?;
    Ok(Options {
        verbose,
        program,
        guest_args,
        volumes,
        assigns,
        cwd,
        auto_assign_root,
        stack_size,
    })
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
fn run_nested_program(
    host_path: &std::path::Path,
    args: &[String],
    vfs_config: Option<VfsConfig>,
    stack_size: u32,
) -> i32 {
    let Ok(bytes) = std::fs::read(host_path) else {
        return -1;
    };
    let Ok(hunk_file) = loader::parse(&bytes) else {
        return -1;
    };
    let mut mem = FlatMemory::new(GUEST_MEMORY_SIZE);
    let Ok(load_result) = loader::load(&hunk_file, &mut mem, TRAP_TABLE_END) else {
        return -1;
    };

    let config = StartConfig {
        entry: load_result.entry,
        load_end: load_result.end,
        args: args.to_vec(),
        stack_size,
    };
    let mut runtime = Runtime::new(M68kCpu::new(), mem, config);

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

    let mut mem = FlatMemory::new(GUEST_MEMORY_SIZE);
    let load_result = loader::load(&hunk_file, &mut mem, TRAP_TABLE_END)
        .map_err(|e| format!("couldn't load '{}': {e}", opts.program))?;

    let cpu = M68kCpu::new();
    let config = StartConfig {
        entry: load_result.entry,
        load_end: load_result.end,
        args: opts.guest_args.clone(),
        stack_size: opts.stack_size,
    };
    let mut runtime = Runtime::new(cpu, mem, config);

    let vfs_config = vfs_config_from_opts(opts);
    if let Some(config) = vfs_config.clone() {
        let vfs = Vfs::new(config).map_err(|e| format!("couldn't set up volumes/assigns: {e}"))?;
        runtime.set_vfs(vfs);
    }

    // System()/Execute (Phase 3 stage 7): a nested program is loaded and
    // run through run_nested_program, sharing this run's volumes/assigns
    // and --stack size -- see volamos_core::dosseg's module docs.
    let nested_stack_size = opts.stack_size;
    runtime.set_system_runner(move |req| {
        run_nested_program(
            &req.resolved_program_host_path,
            &req.args,
            vfs_config.clone(),
            nested_stack_size,
        )
    });

    let stdout = io::stdout();
    let mut out = stdout.lock();

    let verbose = opts.verbose;
    let mut trace = move |event: &TraceEvent| {
        if verbose {
            eprintln!("volamos: {event}");
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

    let opts = match parse_args(args) {
        Ok(opts) => opts,
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

    // --- --stack / parse_stack_size ---

    #[test]
    fn parse_stack_size_plain_bytes() {
        assert_eq!(parse_stack_size("65536").unwrap(), 65536);
        assert_eq!(parse_stack_size("0").unwrap(), 0);
    }

    #[test]
    fn parse_stack_size_kib_suffix() {
        assert_eq!(parse_stack_size("64K").unwrap(), 64 * 1024);
        assert_eq!(parse_stack_size("64k").unwrap(), 64 * 1024);
    }

    #[test]
    fn parse_stack_size_mib_suffix() {
        assert_eq!(parse_stack_size("1M").unwrap(), 1024 * 1024);
        assert_eq!(parse_stack_size("2m").unwrap(), 2 * 1024 * 1024);
    }

    #[test]
    fn parse_stack_size_rejects_garbage() {
        assert!(parse_stack_size("").is_err());
        assert!(parse_stack_size("abc").is_err());
        assert!(parse_stack_size("4KB").is_err());
        assert!(parse_stack_size("-1").is_err());
        assert!(parse_stack_size("1.5K").is_err());
    }

    #[test]
    fn parse_stack_size_rejects_overflow() {
        assert!(parse_stack_size("4294967295M").is_err());
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
}
