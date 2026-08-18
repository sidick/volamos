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
pub mod guestmem;
pub mod loader;
pub mod lvos;
pub mod memory;
pub mod vfs;

pub use backend::M68kCpu;
pub use cpu::Cpu;
pub use dispatch::{
    ABS_EXEC_BASE_ADDR, CallInfo, DOS_LIBRARY_BASE, DispatchError, EXEC_LIBRARY_BASE,
    EXIT_STUB_ADDR, HandlerContext, LVO_PUTSTR, LibraryHandler, LibraryKind, LibraryRegistry,
    LibraryTable, MAX_LIBRARY_SLOTS, Runtime, RuntimeError, StartConfig, TraceEvent,
};
pub use guestmem::{
    GuestHeap, GuestHeapError, STACK_SIZE, addr_from_bptr, bptr_from_addr, read_bstr,
    read_c_string, write_bstr, write_c_string,
};
pub use loader::{HunkFile, LoadError, LoadResult, load, parse};
pub use lvos::{ArgReg, LvoEntry, find_by_lvo, find_by_name};
pub use memory::AddressSpace;
pub use vfs::{MAX_ASSIGN_DEPTH, ResolveMode, Vfs, VfsConfig, VfsError};
