//! Core library for `volamos`.
//!
//! This crate holds the pieces of the runtime that don't depend on any
//! particular host CLI: the CPU/memory abstractions used to swap m68k
//! emulator backends in and out, and (in later stages) the
//! `exec.library`/`dos.library` call implementations that make guest
//! binaries actually run.
//!
//! Currently this crate only contains the CPU/memory scaffolding
//! ([`cpu`], [`memory`]); no concrete m68k emulator is integrated yet.

pub mod backend;
pub mod cpu;
pub mod dispatch;
pub mod loader;
pub mod memory;

pub use backend::M68kCpu;
pub use cpu::Cpu;
pub use dispatch::{
    CallInfo, DOS_LIBRARY_BASE, DispatchError, EXIT_STUB_ADDR, HandlerContext, LVO_PUTSTR,
    LibraryHandler, LibraryTable, MAX_LIBRARY_SLOTS, Runtime, RuntimeError, TraceEvent,
};
pub use loader::{HunkFile, LoadError, LoadResult, load, parse};
pub use memory::AddressSpace;
