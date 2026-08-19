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
pub mod dosanchor;
pub mod dosargs;
pub mod dosassign;
pub mod dosbuf;
pub mod dosdate;
pub mod dosdatestr;
pub mod dosdevlist;
pub mod dosdevproc;
pub mod dosfault;
pub mod dosfile;
pub mod dosfs;
pub mod doslock;
pub mod dosmeta;
pub mod dosnote;
pub mod dospath;
pub mod dospattern;
pub mod dospkt;
pub mod dosprintf;
pub mod dosprotect;
pub mod dosseg;
pub mod dossetfiledate;
pub mod dosstr;
pub mod dosvar;
pub mod execchunk;
pub mod execfmt;
pub mod execlist;
pub mod execmem;
pub mod execsem;
pub mod exectask;
pub mod guestmem;
pub mod loader;
pub mod locale;
pub mod lvos;
pub mod mathlibs;
pub mod memory;
pub mod utility;
pub mod vfs;

pub use backend::M68kCpu;
pub use cpu::Cpu;
pub use dispatch::{
    ABS_EXEC_BASE_ADDR, CallInfo, DOS_LIBRARY_BASE, DispatchError, EXEC_LIBRARY_BASE,
    EXIT_STUB_ADDR, HandlerContext, LVO_PUTSTR, LibraryHandler, LibraryKind, LibraryRegistry,
    LibraryTable, MAX_LIBRARY_SLOTS, Runtime, RuntimeError, StartConfig, TraceEvent,
    UTILITY_LIBRARY_BASE,
};
pub use dosanchor::ERROR_BUFFER_OVERFLOW;
pub use dosargs::{
    ERROR_BAD_NUMBER, ERROR_BAD_TEMPLATE, ERROR_KEY_NEEDS_ARG, ERROR_REQUIRED_ARG_MISSING,
    ERROR_TOO_MANY_ARGS, ERROR_UNMATCHED_QUOTES, RdArgsEntry,
};
pub use dosfile::{
    DosState, ERROR_ACTION_NOT_KNOWN, ERROR_DIR_NOT_FOUND, ERROR_DIRECTORY_NOT_EMPTY,
    ERROR_DISK_WRITE_PROTECTED, ERROR_FILE_NOT_OBJECT, ERROR_INVALID_COMPONENT_NAME,
    ERROR_INVALID_LOCK, ERROR_NO_FREE_STORE, ERROR_OBJECT_EXISTS, ERROR_OBJECT_IN_USE,
    ERROR_OBJECT_NOT_FOUND, ERROR_OBJECT_WRONG_TYPE, ERROR_SEEK_ERROR, MODE_NEWFILE, MODE_OLDFILE,
    MODE_READWRITE, OFFSET_BEGINNING, OFFSET_CURRENT, OFFSET_END,
};
pub use doslock::{
    ACCESS_READ, ACCESS_WRITE, ERROR_NO_MORE_ENTRIES, EXCLUSIVE_LOCK, LockEntry, SHARED_LOCK,
};
pub use dospattern::ERROR_LINE_TOO_LONG;
pub use dosseg::{SegList, SystemRequest};
pub use exectask::{NT_TASK, SIGBREAKF_CTRL_C, TASK_STRUCT_SIZE, install_host_break_handler};
pub use guestmem::{
    DEFAULT_STACK_SIZE, GuestHeap, GuestHeapError, MIN_STACK_SIZE, addr_from_bptr, bptr_from_addr,
    read_bstr, read_c_string, write_bstr, write_c_string,
};
pub use loader::{HunkFile, LoadError, LoadResult, load, parse};
pub use lvos::{ArgReg, LvoEntry, find_by_lvo, find_by_name};
pub use memory::AddressSpace;
pub use vfs::{MAX_ASSIGN_DEPTH, ResolveMode, Resolved, Vfs, VfsConfig, VfsError};
