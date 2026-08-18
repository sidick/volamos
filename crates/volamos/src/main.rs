//! `volamos` command-line entry point.
//!
//! Loads an AmigaOS "hunk" CLI executable, runs it against
//! `volamos-core`'s fake-library dispatch runtime, and exits with the
//! guest program's own exit code.
//!
//! ```text
//! volamos [-v|--verbose] <program> [args...]
//! ```
//!
//! `[args...]` is passed through to the guest program per AmigaOS
//! startup convention (joined with spaces into a command-line buffer,
//! `A0`/`D0` -- see `volamos_core::dispatch::Runtime::new`); a program
//! that parses its own arguments (e.g. via `ReadArgs`) can read them
//! from there. `-v`/`--verbose` logs each trapped library call (library
//! name, LVO, and handler name) to stderr as it happens.

use std::io;
use std::process::ExitCode;

use volamos_core::backend::{M68kCpu, TRAP_TABLE_END};
use volamos_core::dispatch::{Runtime, StartConfig, TraceEvent};
use volamos_core::memory::FlatMemory;
use volamos_core::{LoadError, loader};

/// Guest address space size. Generous for the tiny CLI binaries this
/// runtime currently targets; large enough to leave headroom above the
/// loaded program for its stack.
const GUEST_MEMORY_SIZE: usize = 1 << 20; // 1 MiB

struct Options {
    verbose: bool,
    program: String,
    guest_args: Vec<String>,
}

fn print_usage(program_name: &str) {
    eprintln!("usage: {program_name} [-v|--verbose] <program> [args...]");
    eprintln!();
    eprintln!("Runs an AmigaOS CLI hunk executable under volamos.");
    eprintln!();
    eprintln!("options:");
    eprintln!("  -v, --verbose   log each emulated library call to stderr");
    eprintln!();
    eprintln!("[args...] is passed to the guest program's command line (A0/D0).");
}

/// Hand-rolled argument parsing: this CLI's surface is small enough that
/// pulling in an argument-parsing crate isn't worth the dependency.
fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut verbose = false;
    let mut program = None;
    let mut guest_args = Vec::new();

    for arg in &mut args {
        if program.is_some() {
            guest_args.push(arg);
            continue;
        }
        match arg.as_str() {
            "-v" | "--verbose" => verbose = true,
            "-h" | "--help" => return Err(String::new()), // caller prints usage and exits 0
            _ => program = Some(arg),
        }
    }

    let program = program.ok_or_else(|| "missing <program> argument".to_string())?;
    Ok(Options {
        verbose,
        program,
        guest_args,
    })
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
    };
    let mut runtime = Runtime::new(cpu, mem, config);

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
