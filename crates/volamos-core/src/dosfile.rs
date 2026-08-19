//! `dos.library` file I/O: `Open`/`Close`/`Read`/`Write`/`Seek`,
//! `Input`/`Output`, `IoErr`/`SetIoErr`, and a `PutStr` reimplemented on
//! top of `Output()`.
//!
//! # [`DosState`]
//!
//! [`DosState`] is the host-side state a running guest program's
//! dos.library calls need, threaded through [`crate::dispatch::
//! HandlerContext`] (see `dos` field) exactly like `heap`/`registry`
//! already are. It owns:
//!
//! - an optional [`Vfs`] for Amiga-path -> host-path resolution (`None`
//!   when the runtime wasn't given one -- see "No VFS configured" below);
//! - a registry mapping a guest `FileHandle` struct's *address* (not its
//!   BPTR -- see "Guest FileHandle layout" below) to a host backing
//!   ([`HostHandle`]: an open [`std::fs::File`], or a marker for the
//!   `Input()`/`Output()` default handles);
//! - the current `IoErr()` value, set by every handler that can fail;
//! - (T11) `locks`/`exnext`/`current_dir_lock`/`initial_cwd`: the
//!   `Lock`/`UnLock`/`DupLock`/`Examine`/`ExNext`/`CurrentDir`/`ParentDir`
//!   state. These fields live here (per this module's own convention: a
//!   guest struct's address keys a host-side registry entry, exactly like
//!   `handles` above), but their methods and the handlers themselves live
//!   in [`crate::doslock`] -- see that module's docs for the full design.
//!
//! # Guest `FileHandle` layout
//!
//! A real AmigaOS `struct FileHandle` is 44 bytes (`sizeof(struct
//! FileHandle)`); guest code mostly treats it as opaque (it goes through
//! dos.library calls, not direct field access), except that `fh_Arg1` at
//! byte offset 36 is the conventional "handler-private" slot. [`Open`]
//! allocates 44 zeroed bytes on the guest heap for each opened file and
//! writes a small debug id (not the lookup key -- see below) into
//! `fh_Arg1`, purely so a guest-side or host-side debugger inspecting the
//! struct can see *something* meaningful there. The actual host-side
//! lookup is keyed by the struct's guest *address* (`handles: HashMap<u32,
//! HostHandle>`), not by `fh_Arg1` -- simpler, and every handler already
//! has the address on hand (it's `addr_from_bptr` of the BPTR the guest
//! passes in `D1`).
//!
//! `Open` returns a BPTR (`bptr_from_addr(addr)`) in `D0`, or `0` with
//! `IoErr()` set on failure. `Close`/`Read`/`Write`/`Seek` all take that
//! same BPTR back in `D1`.
//!
//! # No VFS configured
//!
//! `Runtime::new` installs a [`DosState`] with `vfs: None` by default (see
//! [`crate::dispatch::Runtime::set_vfs`] to install one). With no `Vfs`,
//! `Input()`/`Output()`/`PutStr()`/`IoErr()`/`SetIoErr()` all still work
//! (they don't need path resolution), but `Open` can't resolve any name at
//! all and fails every call with [`ERROR_OBJECT_NOT_FOUND`] -- chosen
//! (over, say, a dedicated "no filesystem" code, which AmigaOS doesn't
//! have) because it's the closest real error code to "that name doesn't
//! exist," which is true from the guest's point of view either way.
//!
//! # Error code mapping
//!
//! [`map_io_error`] and [`map_vfs_error`] are the two places
//! [`std::io::Error`]/[`VfsError`] get turned into an AmigaOS `IoErr()`
//! code; every handler routes failures through one or the other rather
//! than inventing its own mapping inline. See their doc comments for the
//! specific choices (several `std::io::ErrorKind`/[`VfsError`] variants
//! don't have an exact AmigaOS equivalent, so the mapping is a documented
//! judgment call, not a spec).

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};

use crate::cpu::{Cpu, DataRegister};
use crate::dispatch::{DOS_LIBRARY_BASE, DispatchError, HandlerContext, LibraryTable};
use crate::guestmem::{GuestHeap, addr_from_bptr, bptr_from_addr, read_c_string, write_c_string};
use crate::lvos::dos::DOS_LVOS;
use crate::memory::AddressSpace;
use crate::vfs::{ResolveMode, Vfs, VfsError};

// --- AmigaOS dos.library error codes (a subset; add more as handlers
// need them). Values are the real AmigaOS ones (see `<dos/dos.h>` /
// RKRM Appendix); this module is the one place they're defined and
// mapped to from host errors. ---

/// Not enough memory to complete the request.
pub const ERROR_NO_FREE_STORE: i32 = 103;
/// An object with that name is already in use (e.g. a lock preventing
/// deletion). Not currently produced by any T10 handler, but part of the
/// documented mapping surface.
pub const ERROR_OBJECT_IN_USE: i32 = 202;
/// `Open(MODE_NEWFILE)`-style creation target already exists in a way
/// that conflicts with the request.
pub const ERROR_OBJECT_EXISTS: i32 = 203;
/// A directory component of the path doesn't exist.
pub const ERROR_DIR_NOT_FOUND: i32 = 204;
/// The final path component doesn't exist.
pub const ERROR_OBJECT_NOT_FOUND: i32 = 205;
/// `LoadSeg`'s "the file exists but isn't a valid AmigaOS object module"
/// error. Real AmigaOS's `<dos/dos.h>` documents this exact code (121,
/// `ERROR_FILE_NOT_OBJECT`) for precisely this situation -- a genuine file
/// was found and read, but it didn't parse as a hunk executable -- as
/// distinct from [`ERROR_OBJECT_WRONG_TYPE`] (212, used elsewhere in this
/// module for "a directory where a file was expected" and vice versa,
/// which is a different real AmigaOS error condition). Defined here
/// rather than in `dosseg.rs` per this module's own convention: every
/// `IoErr()` code this runtime produces is defined in one place.
pub const ERROR_FILE_NOT_OBJECT: i32 = 121;
/// A component that should have been a directory (or vice versa) has the
/// wrong type.
pub const ERROR_OBJECT_WRONG_TYPE: i32 = 212;
/// The trapping A-line opcode's LVO isn't one this dos.library
/// implementation handles, or `Open`'s access-mode argument wasn't one
/// of `MODE_OLDFILE`/`MODE_NEWFILE`/`MODE_READWRITE`.
pub const ERROR_ACTION_NOT_KNOWN: i32 = 209;
/// A path component's syntax is invalid.
pub const ERROR_INVALID_COMPONENT_NAME: i32 = 210;
/// The lock/file-handle address the guest passed doesn't refer to a
/// currently-open object.
pub const ERROR_INVALID_LOCK: i32 = 211;
/// A write (or write-implying open) failed because the target is
/// read-only from the host's point of view.
pub const ERROR_DISK_WRITE_PROTECTED: i32 = 214;
/// `DeleteFile` on a directory that still has entries in it.
pub const ERROR_DIRECTORY_NOT_EMPTY: i32 = 216;
/// `Seek` failed (out-of-range offset, or the handle isn't seekable --
/// e.g. the `Input()`/`Output()` default handles).
pub const ERROR_SEEK_ERROR: i32 = 219;

/// `Open`'s `MODE_OLDFILE`: the file must already exist; opened
/// read/write where possible (falling back to read-only -- see
/// [`DosState::open`]).
pub const MODE_OLDFILE: i32 = 1005;
/// `Open`'s `MODE_NEWFILE`: created (truncated if it already exists),
/// opened read/write (real `MODE_NEWFILE` handles support both,
/// matching `MODE_OLDFILE`); the parent directory must exist.
pub const MODE_NEWFILE: i32 = 1006;
/// `Open`'s `MODE_READWRITE`: opened for read/write, created if it
/// doesn't exist, *not* truncated if it does.
pub const MODE_READWRITE: i32 = 1004;

/// `Seek`'s `offset_mode`: relative to the start of the file.
pub const OFFSET_BEGINNING: i32 = -1;
/// `Seek`'s `offset_mode`: relative to the current position.
pub const OFFSET_CURRENT: i32 = 0;
/// `Seek`'s `offset_mode`: relative to the end of the file.
pub const OFFSET_END: i32 = 1;

/// `Close`/boolean-success convention: AmigaOS `BOOL` true is `-1`
/// (`0xFFFFFFFF` as a 32-bit register value), not `1`.
const DOSTRUE: u32 = 0xFFFF_FFFF;
/// `Close`/boolean-failure convention.
const DOSFALSE: u32 = 0;
/// `Read`/`Write`/`Seek` failure sentinel written to `D0`.
const RESULT_ERROR: u32 = 0xFFFF_FFFF;

/// `sizeof(struct FileHandle)`: 44 bytes. [`DosState::open`] (and the
/// `Input()`/`Output()` default-handle constructors) allocate exactly
/// this much, zeroed, on the guest heap per opened file.
const FILE_HANDLE_SIZE: u32 = 44;
/// Byte offset of `fh_Arg1` within `struct FileHandle` -- see the module
/// docs' "Guest `FileHandle` layout" section for why this is written but
/// not used as the lookup key.
const FH_ARG1_OFFSET: u32 = 36;
/// Byte offset of `fh_Port` within `struct FileHandle` (`dos/dosextens.h`).
/// Despite the name/type, real AmigaOS treats it as a plain `LONG`, not a
/// pointer: "If it is non-zero, the file is interactive" (RKRM
/// `files.md`), consulted directly by `IsInteractive()`. Written non-zero
/// only for the lazily-created `Input()`/`Output()` default handles
/// (stdin/stdout are conceptually a console, always interactive); real
/// host files opened via `Open()` leave it `0` (correctly
/// non-interactive, matching "the FFS and the RAM-Handler are file
/// systems and thus create non-interactive files").
const FH_PORT_OFFSET: u32 = 4;
/// The non-zero sentinel written to `fh_Port` for interactive handles --
/// any non-zero value works, since callers only ever test it for
/// zero-ness, never interpret it as a real pointer.
const FH_PORT_INTERACTIVE: u32 = 1;

/// Maps a [`std::io::Error`] to an AmigaOS `IoErr()` code. The one place
/// this runtime translates host I/O errors; every handler that touches
/// `std::fs`/stdin routes failures through this.
///
/// Judgment calls, since `ErrorKind` alone can't always distinguish real
/// AmigaOS error conditions:
/// - `PermissionDenied` -> [`ERROR_DISK_WRITE_PROTECTED`]. The real
///   AmigaOS split (`ERROR_WRITE_PROTECTED` vs `ERROR_READ_PROTECTED` vs
///   `ERROR_OBJECT_IN_USE`) depends on *why* access was denied, which
///   `std::io::Error` doesn't expose portably; `ERROR_DISK_WRITE_PROTECTED`
///   is the closest single code and matches the common case (opening
///   something for writing that the host filesystem won't allow).
/// - `OutOfMemory` -> [`ERROR_NO_FREE_STORE`] (the one unambiguous case).
/// - `NotFound` -> [`ERROR_OBJECT_NOT_FOUND`] (the `Vfs` layer already
///   handles directory-vs-file distinctions before a raw `std::fs` call
///   would ever see a `NotFound`, so this only fires for genuinely
///   unexpected races, e.g. TOCTOU between `Vfs::resolve` and `open`).
/// - Anything else (`InvalidInput`, `Interrupted`, `UnexpectedEof`,
///   platform-specific kinds, ...) -> [`ERROR_ACTION_NOT_KNOWN`], the
///   closest thing AmigaOS has to a generic I/O failure code.
pub fn map_io_error(err: &std::io::Error) -> i32 {
    use std::io::ErrorKind;
    match err.kind() {
        ErrorKind::NotFound => ERROR_OBJECT_NOT_FOUND,
        ErrorKind::AlreadyExists => ERROR_OBJECT_EXISTS,
        ErrorKind::PermissionDenied => ERROR_DISK_WRITE_PROTECTED,
        ErrorKind::OutOfMemory => ERROR_NO_FREE_STORE,
        _ => ERROR_ACTION_NOT_KNOWN,
    }
}

/// Maps a [`VfsError`] (path resolution failure) to an AmigaOS `IoErr()`
/// code.
pub fn map_vfs_error(err: &VfsError) -> i32 {
    match err {
        VfsError::NotFound { .. } => ERROR_OBJECT_NOT_FOUND,
        VfsError::NotADirectory { .. } => ERROR_OBJECT_WRONG_TYPE,
        // No known volume/assign for the leading name: from the guest's
        // point of view this looks just like "that device/dir doesn't
        // exist" -- there's no dedicated AmigaOS code for "unknown
        // volume" distinct from "not found" that `IoErr()` callers
        // meaningfully act on differently.
        VfsError::UnknownVolume { .. } => ERROR_OBJECT_NOT_FOUND,
        // An assign cycle is a configuration bug, not something a guest
        // program can act on; map it to the same "can't get there"
        // bucket as UnknownVolume rather than inventing a new code.
        VfsError::AssignLoop { .. } => ERROR_OBJECT_NOT_FOUND,
        VfsError::InvalidPath { .. } => ERROR_INVALID_COMPONENT_NAME,
        VfsError::Io { .. } => ERROR_ACTION_NOT_KNOWN,
    }
}

/// A file handle's host-side backing.
enum HostHandle {
    /// A real host file opened via `Open`, plus the normalized Amiga
    /// path it was resolved from (see [`crate::doslock`]'s `LockEntry`
    /// for the same "record the path that produced it" convention) --
    /// [`DosState::name_from_fh`] reads this back for `NameFromFH`.
    HostFile(File, String),
    /// The `Input()` default handle: reads come from host stdin.
    Stdin,
    /// The `Output()` default handle. Writes to this handle are
    /// special-cased by the `Write`/`PutStr` handlers to go through
    /// [`HandlerContext::out`] directly (so output still lands wherever
    /// the caller of [`crate::dispatch::Runtime::run`] pointed it,
    /// e.g. a `Vec<u8>` a test is asserting on) rather than through this
    /// enum; this variant exists so `Close`/`Read`/lookup-by-address
    /// still see a consistent entry for it.
    Stdout,
}

/// Host-side dos.library state for one running guest program. See the
/// module docs for the full design.
pub struct DosState {
    /// Volume/assign path resolver. `None` if the runtime wasn't given
    /// one (see [`crate::dispatch::Runtime::set_vfs`]); path-based calls
    /// (`Open`) then always fail with [`ERROR_OBJECT_NOT_FOUND`], while
    /// path-free calls (`Input`/`Output`/`PutStr`/`IoErr`/`SetIoErr`)
    /// still work.
    pub vfs: Option<Vfs>,
    /// Guest `FileHandle` struct address -> host backing.
    handles: HashMap<u32, HostHandle>,
    /// The current `IoErr()` value.
    io_err: i32,
    /// Guest address of the lazily-created `Input()` default handle's
    /// `FileHandle` struct -- always backed by [`HostHandle::Stdin`].
    /// Distinct from `current_input`: this is only ever the *host stdin*
    /// handle, used by [`Self::close`]'s no-op case, regardless of
    /// what `SelectInput` has redirected `Input()` to.
    input_handle: Option<u32>,
    /// Guest address of the lazily-created `Output()` default handle's
    /// `FileHandle` struct -- always backed by [`HostHandle::Stdout`].
    /// Distinct from `current_output`: this is only ever the *host
    /// stdout* handle, used by [`Self::is_output_default`]/
    /// [`Self::close`], regardless of what `SelectOutput` has
    /// redirected `Output()` to.
    output_handle: Option<u32>,
    /// The handle `Input()`/`FGetC` et al currently read from -- starts
    /// out mirroring `input_handle` once that's created, but
    /// `SelectInput` can repoint it at any open handle.
    current_input: Option<u32>,
    /// The handle `Output()`/`WriteChars`/`FPuts` et al currently write
    /// to -- starts out mirroring `output_handle` once that's created,
    /// but `SelectOutput` can repoint it at any open handle (e.g. a real
    /// file, as `Type ... TO file` does).
    current_output: Option<u32>,
    /// `GetFileSysTask`/`SetFileSysTask`'s current value -- see
    /// [`get_file_sys_task_handler`]'s doc comment for why a fixed
    /// sentinel (never a real, dereferenceable `MsgPort`) is sufficient.
    current_file_sys_task: u32,
    /// Monotonic counter used only to give each opened handle a distinct
    /// debug id written into `fh_Arg1` (see the module docs) -- not used
    /// for lookups.
    next_debug_id: u32,

    // --- T11: locks / Examine-ExNext state. Methods and handlers for
    // these fields live in `crate::doslock`, not here (see that module's
    // docs) -- kept as plain `pub(crate)` fields, per this module's own
    // "extensions live here, handler code lives in the sibling module"
    // note above, rather than duplicating DosState's own accessor style
    // for a chunk of state this module otherwise never touches.
    /// Guest `struct FileLock` address -> host-side lock registry entry.
    pub(crate) locks: HashMap<u32, crate::doslock::LockEntry>,
    /// Per-lock `ExNext` iterator state (directory entries + cursor),
    /// keyed by the same guest `FileLock` address as `locks`. Populated
    /// by `Examine` on a directory lock, consumed by `ExNext`.
    pub(crate) exnext: HashMap<u32, crate::doslock::ExNextState>,
    /// Guest address of the `FileLock` `CurrentDir` last switched to, or
    /// `None` if the process is still on its initial (no-lock) current
    /// directory.
    pub(crate) current_dir_lock: Option<u32>,
    /// The `Vfs` current directory (Amiga path string) captured the first
    /// time `CurrentDir` runs, before it's ever changed -- `CurrentDir(0)`
    /// restores this. `None` until the first `CurrentDir` call.
    pub(crate) initial_cwd: Option<String>,
    /// Monotonic counter for `fl_Key` debug values, independent of
    /// `next_debug_id` (which is for `FileHandle`s) so the two id spaces
    /// don't visually collide when debugging.
    pub(crate) next_lock_id: u32,

    // --- Phase 3 stage 7: LoadSeg/UnLoadSeg + System()/Execute state.
    // Methods and handlers live in `crate::dosseg` (same "extensions live
    // here, handler code lives in the sibling module" convention as the
    // T11 fields above), but the fields themselves are declared here per
    // that same convention.
    /// Live seglists: first-segment `BPTR` (exactly the value [`LoadSeg`]
    /// returned in `D0`, i.e. also the map key `UnLoadSeg` is called
    /// with) -> every segment's guest-heap allocation *address* (not
    /// BPTR) in load order, so [`crate::dosseg::DosState::unload_seg`]
    /// knows exactly which [`GuestHeap`] blocks to free without having to
    /// re-walk the guest-memory `next_seg` chain. See
    /// `crate::dosseg`'s module docs for the seglist memory layout.
    ///
    /// [`LoadSeg`]: crate::dosseg
    pub(crate) seglists: HashMap<u32, Vec<u32>>,
    /// The same keys as [`Self::seglists`], mapping instead to the host
    /// path [`LoadSeg`] actually read the seglist's bytes from --
    /// [`crate::dosseg::run_command_handler`] (`RunCommand`) needs this
    /// to re-run the program via the same [`Self::system_runner`]
    /// nested-execution path `System()`/`Execute()` use, since re-using
    /// the already-loaded (and already-relocated-in-place) seglist bytes
    /// directly isn't possible without a real "call into guest code
    /// while still processing library-call traps normally" execution
    /// mode this runtime doesn't have (see `crate::dosseg`'s module docs
    /// for the full reasoning). A deliberate approximation: faithful for
    /// the overwhelmingly common `LoadSeg` immediately followed by
    /// `RunCommand` (never re-run the same seglist to run *different*,
    /// hand-patched code), not for a guest that pokes the loaded
    /// seglist's memory before calling `RunCommand`.
    ///
    /// [`LoadSeg`]: crate::dosseg
    pub(crate) seglist_host_paths: HashMap<u32, std::path::PathBuf>,
    /// Host-side callback installed by a CLI (never by library code
    /// itself) to actually run a resolved `System()`/`Execute()` command
    /// as a nested guest invocation -- see `crate::dosseg`'s module docs
    /// ("System()/Execute architecture") for why this indirection exists
    /// and what it's given. `None` (the default) means no host is able to
    /// run nested programs; `System`/`Execute` then fail cleanly (see
    /// `crate::dosseg::DosState::system`/`execute`) rather than panicking
    /// or silently no-oping.
    pub system_runner: Option<crate::dosseg::SystemRunner>,

    // --- ReadArgs/FreeArgs state. Methods and handlers live in
    // `crate::dosargs` (same "extensions live here, handler code lives in
    // the sibling module" convention as the T11/Phase-3-stage-7 fields
    // above), but the fields themselves are declared here per that same
    // convention.
    /// Guest address of the process's command-line buffer (the `A0`/`D0`
    /// buffer [`crate::dispatch::Runtime::new`] builds from
    /// [`crate::dispatch::StartConfig::args`]) and its length in bytes
    /// (including the trailing `'\n'`). This is `ReadArgs`'s default
    /// input source (`rdargs == NULL`), mirroring how real AmigaOS
    /// delivers the CLI command tail through the process's buffered
    /// `Input()` -- see `crate::dosargs`'s module docs.
    pub(crate) cmdline: Option<(u32, u32)>,
    /// Byte offset into `cmdline` that the next default-source `ReadArgs`
    /// call resumes from -- real `ReadArgs(NULL)` reads from a shared,
    /// stateful input stream, so repeated calls in one process walk
    /// forward through the same buffer rather than each re-parsing it
    /// from the start.
    pub(crate) cmdline_pos: u32,
    /// Live `ReadArgs` results, keyed by the `struct RDArgs*` anchor
    /// address returned in `D0` (see `crate::dosargs::RDARGS_STRUCT_SIZE`).
    /// `FreeArgs` looks up and frees every heap block an entry lists, plus
    /// (only when [`crate::dosargs::RdArgsEntry::owns_anchor`]) the anchor
    /// block itself.
    pub(crate) rdargs: HashMap<u32, crate::dosargs::RdArgsEntry>,

    /// Local shell variables (`SetVar`/`GetVar`/`DeleteVar`, `LV_VAR`
    /// only -- see `crate::dosvar`'s module docs for scope), keyed by
    /// upper-cased name (variable names are case-insensitive) to raw
    /// byte content. No `ENV:`-backed global-variable storage is
    /// implemented; see that module's docs.
    pub(crate) local_vars: HashMap<String, Vec<u8>>,

    /// Live `MatchFirst`/`MatchNext` scan state, keyed by the guest
    /// `AnchorPath*` address -- see `crate::dosanchor`'s module docs.
    /// `MatchEnd` (and this runtime's own error paths) remove entries
    /// and unlock every directory lock a scan is still holding.
    pub(crate) anchor_states: HashMap<u32, crate::dosanchor::AnchorMatchState>,

    /// `UnGetC`'s one-byte pushback, keyed by guest `FileHandle*`
    /// address -- present only while a pushed-back byte (or `ENDSTREAMCH`
    /// EOF marker) is waiting to be re-delivered by the next `FGetC`.
    /// See `crate::dosbuf`'s module docs.
    pub(crate) ungetc_buf: HashMap<u32, i32>,
    /// The last value `FGetC` actually returned for a handle (a byte
    /// `0..=255`, or `ENDSTREAMCH` on EOF/error) -- consulted by
    /// `UnGetC(fh, -1)`, which pushes back "whatever was last read"
    /// rather than a caller-specified byte.
    pub(crate) last_getc: HashMap<u32, i32>,

    /// Heap addresses allocated by the most recent `LockDosList` (the
    /// header node, every `DosList` entry built, and their `dol_Name`
    /// buffers) -- freed by `UnLockDosList`. See `crate::dosdevlist`'s
    /// module docs for why a single session (rather than one keyed by
    /// handle) is enough here.
    pub(crate) dos_list_active: Vec<u32>,
    /// Uppercased volume name -> a stable, process-lifetime, non-
    /// dereferenceable synthetic `dol_Task` id, allocated lazily the
    /// first time a `DosList` entry is built for that volume (see
    /// `crate::dosdevlist::task_id_for_volume`). `crate::dospkt`'s
    /// `DoPkt` uses this (in reverse) to tell which volume a packet's
    /// destination `port` identifies.
    pub(crate) volume_task_ids: HashMap<String, u32>,
}

impl DosState {
    /// Creates a fresh dos.library state. `vfs` may be `None` (see the
    /// module docs' "No VFS configured" section).
    pub fn new(vfs: Option<Vfs>) -> Self {
        Self {
            vfs,
            handles: HashMap::new(),
            io_err: 0,
            input_handle: None,
            output_handle: None,
            current_input: None,
            current_output: None,
            current_file_sys_task: DEFAULT_FILE_SYS_TASK,
            next_debug_id: 0,
            locks: HashMap::new(),
            exnext: HashMap::new(),
            current_dir_lock: None,
            initial_cwd: None,
            next_lock_id: 0,
            seglists: HashMap::new(),
            seglist_host_paths: HashMap::new(),
            system_runner: None,
            cmdline: None,
            cmdline_pos: 0,
            rdargs: HashMap::new(),
            local_vars: HashMap::new(),
            anchor_states: HashMap::new(),
            ungetc_buf: HashMap::new(),
            last_getc: HashMap::new(),
            dos_list_active: Vec::new(),
            volume_task_ids: HashMap::new(),
        }
    }

    /// The current `IoErr()` value.
    pub fn io_err(&self) -> i32 {
        self.io_err
    }

    /// Sets the current `IoErr()` value, returning the previous one
    /// (`SetIoErr`'s own return convention).
    pub fn set_io_err(&mut self, value: i32) -> i32 {
        std::mem::replace(&mut self.io_err, value)
    }

    fn next_id(&mut self) -> u32 {
        self.next_debug_id = self.next_debug_id.wrapping_add(1);
        self.next_debug_id
    }

    /// Whether `addr` (a guest `FileHandle` struct address, *not* a
    /// BPTR) is the `Output()` default handle -- used by the `Write`/
    /// `PutStr` handlers to decide whether to write through
    /// [`HandlerContext::out`] instead of consulting `handles`.
    pub fn is_output_default(&self, addr: u32) -> bool {
        self.output_handle == Some(addr)
    }

    /// `Open`: resolves `name` (an Amiga path) through `self.vfs` per
    /// `access_mode` (`MODE_OLDFILE`/`MODE_NEWFILE`/`MODE_READWRITE`),
    /// opens the resulting host path, and allocates a guest `FileHandle`
    /// struct for it. Returns the struct's BPTR on success, or an
    /// `IoErr()` code on failure.
    ///
    /// `MODE_OLDFILE` tries read+write first (matching real `Open`,
    /// which hands back a handle usable for both unless the file itself
    /// is read-only), falling back to read-only if the host denies
    /// write access -- a real AmigaOS `Open(MODE_OLDFILE)` on a
    /// write-protected file succeeds too, just with a handle that later
    /// fails on `Write`.
    pub fn open(
        &mut self,
        heap: &mut GuestHeap,
        mem: &mut dyn AddressSpace,
        name: &str,
        access_mode: i32,
    ) -> Result<u32, i32> {
        let vfs = self.vfs.as_ref().ok_or(ERROR_OBJECT_NOT_FOUND)?;

        let (resolve_mode, is_new) = match access_mode {
            MODE_OLDFILE => (ResolveMode::MustExist, false),
            MODE_NEWFILE => (ResolveMode::ParentMustExist, true),
            MODE_READWRITE => (ResolveMode::ParentMustExist, false),
            _ => return Err(ERROR_ACTION_NOT_KNOWN),
        };
        let resolved = vfs
            .resolve_with_amiga_path(name, resolve_mode)
            .map_err(|e| map_vfs_error(&e))?;
        let path = resolved.host_path;

        let file = if is_new {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
                .map_err(|e| map_io_error(&e))?
        } else if access_mode == MODE_READWRITE {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .map_err(|e| map_io_error(&e))?
        } else {
            // MODE_OLDFILE: read+write, falling back to read-only.
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .or_else(|_| OpenOptions::new().read(true).open(&path))
                .map_err(|e| map_io_error(&e))?
        };

        let id = self.next_id();
        let addr = alloc_file_handle(heap, mem, id).map_err(|_| ERROR_NO_FREE_STORE)?;
        self.handles
            .insert(addr, HostHandle::HostFile(file, resolved.amiga_path));
        Ok(bptr_from_addr(addr))
    }

    /// `Close`: returns `true` on success (including the no-op case of
    /// closing an `Input()`/`Output()` default handle -- real `Close` on
    /// those succeeds without actually releasing anything), `false` if
    /// `addr` isn't a currently-open handle (caller sets `IoErr()`).
    pub fn close(&mut self, heap: &mut GuestHeap, addr: u32) -> bool {
        if self.input_handle == Some(addr) || self.output_handle == Some(addr) {
            return true;
        }
        if self.handles.remove(&addr).is_some() {
            let _ = heap.free(addr);
            true
        } else {
            false
        }
    }

    /// `Read`: reads up to `len` bytes from the handle at `addr`,
    /// returning the bytes actually read (may be shorter than `len`,
    /// including empty at EOF -- matching real `Read`'s "actual count"
    /// contract) or an `IoErr()` code.
    pub fn read(&mut self, addr: u32, len: usize) -> Result<Vec<u8>, i32> {
        match self.handles.get_mut(&addr) {
            Some(HostHandle::HostFile(f, _)) => {
                let mut buf = vec![0u8; len];
                let n = f.read(&mut buf).map_err(|e| map_io_error(&e))?;
                buf.truncate(n);
                Ok(buf)
            }
            Some(HostHandle::Stdin) => {
                let mut buf = vec![0u8; len];
                let n = std::io::stdin()
                    .read(&mut buf)
                    .map_err(|e| map_io_error(&e))?;
                buf.truncate(n);
                Ok(buf)
            }
            Some(HostHandle::Stdout) => Err(ERROR_ACTION_NOT_KNOWN),
            None => Err(ERROR_INVALID_LOCK),
        }
    }

    /// `Write` to a *non-default* handle (the `Write`/`PutStr` handlers
    /// special-case the `Output()` default via [`Self::is_output_default`]
    /// before ever calling this, so it goes through
    /// [`HandlerContext::out`] rather than here). Returns the number of
    /// bytes actually written, or an `IoErr()` code.
    pub fn write(&mut self, addr: u32, data: &[u8]) -> Result<usize, i32> {
        match self.handles.get_mut(&addr) {
            Some(HostHandle::HostFile(f, _)) => f.write(data).map_err(|e| map_io_error(&e)),
            Some(HostHandle::Stdin) | Some(HostHandle::Stdout) => Err(ERROR_ACTION_NOT_KNOWN),
            None => Err(ERROR_INVALID_LOCK),
        }
    }

    /// `Seek`: seeks the handle at `addr` per `offset_mode`
    /// (`OFFSET_BEGINNING`/`OFFSET_CURRENT`/`OFFSET_END`), returning the
    /// *old* position (matching real `Seek`'s "returns previous
    /// position" contract) or an `IoErr()` code. The `Input()`/
    /// `Output()` default handles (and any handle backed by stdin/
    /// stdout) aren't seekable and always fail with
    /// [`ERROR_SEEK_ERROR`].
    pub fn seek(&mut self, addr: u32, position: i32, offset_mode: i32) -> Result<i32, i32> {
        match self.handles.get_mut(&addr) {
            Some(HostHandle::HostFile(f, _)) => {
                let old = f.stream_position().map_err(|_| ERROR_SEEK_ERROR)? as i32;
                let seek_from = match offset_mode {
                    OFFSET_BEGINNING => SeekFrom::Start(position.max(0) as u64),
                    OFFSET_CURRENT => SeekFrom::Current(position as i64),
                    OFFSET_END => SeekFrom::End(position as i64),
                    _ => return Err(ERROR_SEEK_ERROR),
                };
                f.seek(seek_from).map_err(|_| ERROR_SEEK_ERROR)?;
                Ok(old)
            }
            Some(_) => Err(ERROR_SEEK_ERROR),
            None => Err(ERROR_INVALID_LOCK),
        }
    }

    /// `NameFromFH`: the normalized Amiga path `addr`'s handle was
    /// opened from (recorded on it by [`Self::open`]). Fails with
    /// [`ERROR_INVALID_LOCK`] for an unknown handle, or
    /// [`ERROR_ACTION_NOT_KNOWN`] for the `Input()`/`Output()` default
    /// handles (stdin/stdout aren't Amiga path-backed objects at all).
    pub fn name_from_fh(&self, addr: u32) -> Result<String, i32> {
        match self.handles.get(&addr) {
            Some(HostHandle::HostFile(_, amiga_path)) => Ok(amiga_path.clone()),
            Some(HostHandle::Stdin) | Some(HostHandle::Stdout) => Err(ERROR_ACTION_NOT_KNOWN),
            None => Err(ERROR_INVALID_LOCK),
        }
    }

    /// `Input()`: returns the guest address of the *currently selected*
    /// input handle (lazily-creating and selecting the default,
    /// stdin-backed one on first use), or an `IoErr()` code if the guest
    /// heap has no room to create it (extremely unlikely; the struct is
    /// 44 bytes).
    pub fn input_addr(
        &mut self,
        heap: &mut GuestHeap,
        mem: &mut dyn AddressSpace,
    ) -> Result<u32, i32> {
        if let Some(addr) = self.current_input {
            return Ok(addr);
        }
        let addr = match self.input_handle {
            Some(addr) => addr,
            None => {
                let id = self.next_id();
                let addr = alloc_file_handle(heap, mem, id).map_err(|_| ERROR_NO_FREE_STORE)?;
                mem.write_u32(addr.wrapping_add(FH_PORT_OFFSET), FH_PORT_INTERACTIVE);
                self.handles.insert(addr, HostHandle::Stdin);
                self.input_handle = Some(addr);
                addr
            }
        };
        self.current_input = Some(addr);
        Ok(addr)
    }

    /// `Output()`: as [`Self::input_addr`], for the current output
    /// handle.
    pub fn output_addr(
        &mut self,
        heap: &mut GuestHeap,
        mem: &mut dyn AddressSpace,
    ) -> Result<u32, i32> {
        if let Some(addr) = self.current_output {
            return Ok(addr);
        }
        let addr = match self.output_handle {
            Some(addr) => addr,
            None => {
                let id = self.next_id();
                let addr = alloc_file_handle(heap, mem, id).map_err(|_| ERROR_NO_FREE_STORE)?;
                mem.write_u32(addr.wrapping_add(FH_PORT_OFFSET), FH_PORT_INTERACTIVE);
                self.handles.insert(addr, HostHandle::Stdout);
                self.output_handle = Some(addr);
                addr
            }
        };
        self.current_output = Some(addr);
        Ok(addr)
    }

    /// `SelectInput` (`D1` = new input `FileHandle` `BPTR`): repoints
    /// [`Self::input_addr`]/`Input()` at `new_addr`, returning the
    /// *previous* selection's address (lazily creating the stdin-backed
    /// default first if nothing had been selected yet, matching a real
    /// process always having a valid `pr_CIS`).
    pub fn select_input(
        &mut self,
        heap: &mut GuestHeap,
        mem: &mut dyn AddressSpace,
        new_addr: u32,
    ) -> Result<u32, i32> {
        let old = self.input_addr(heap, mem)?;
        self.current_input = Some(new_addr);
        Ok(old)
    }

    /// `SelectOutput`: as [`Self::select_input`], for `Output()`.
    pub fn select_output(
        &mut self,
        heap: &mut GuestHeap,
        mem: &mut dyn AddressSpace,
        new_addr: u32,
    ) -> Result<u32, i32> {
        let old = self.output_addr(heap, mem)?;
        self.current_output = Some(new_addr);
        Ok(old)
    }
}

/// Allocates a zeroed `sizeof(struct FileHandle)`-byte block on `heap`
/// and writes `debug_id` into `fh_Arg1` (see the module docs).
fn alloc_file_handle(
    heap: &mut GuestHeap,
    mem: &mut dyn AddressSpace,
    debug_id: u32,
) -> Result<u32, crate::guestmem::GuestHeapError> {
    let addr = heap.alloc(FILE_HANDLE_SIZE)?;
    for i in 0..FILE_HANDLE_SIZE {
        mem.write_u8(addr.wrapping_add(i), 0);
    }
    mem.write_u32(addr.wrapping_add(FH_ARG1_OFFSET), debug_id);
    Ok(addr)
}

// --- LVO handlers ---

/// `Open` (`D1` = name `CString*`, `D2` = access mode). `D0` = BPTR or 0.
fn open_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let name_ptr = ctx.cpu.data_register(DataRegister(1));
    let name = String::from_utf8_lossy(&read_c_string(ctx.mem, name_ptr)).into_owned();
    let mode = ctx.cpu.data_register(DataRegister(2)) as i32;
    let mode_label = match mode {
        MODE_OLDFILE => "MODE_OLDFILE",
        MODE_NEWFILE => "MODE_NEWFILE",
        MODE_READWRITE => "MODE_READWRITE",
        _ => "MODE_?",
    };
    match ctx.dos.open(ctx.heap, ctx.mem, &name, mode) {
        Ok(bptr) => {
            *ctx.call_detail = Some(format!("file {name:?} ({mode_label}) -> ok"));
            ctx.cpu.set_data_register(DataRegister(0), bptr)
        }
        Err(code) => {
            *ctx.call_detail = Some(format!(
                "file {name:?} ({mode_label}) -> FAILED (IoErr {code})"
            ));
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), 0);
        }
    }
    Ok(())
}

/// `Close` (`D1` = BPTR). `D0` = `DOSTRUE`/`DOSFALSE`.
fn close_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let bptr = ctx.cpu.data_register(DataRegister(1));
    let addr = addr_from_bptr(bptr);
    let ok = ctx.dos.close(ctx.heap, addr);
    if !ok {
        ctx.dos.set_io_err(ERROR_INVALID_LOCK);
    }
    ctx.cpu
        .set_data_register(DataRegister(0), if ok { DOSTRUE } else { DOSFALSE });
    Ok(())
}

/// `NameFromFH` (`D1` = `BPTR` file handle, `D2` = buffer, `D3` =
/// buffer capacity). `D0` = `DOSTRUE`/`DOSFALSE` (`DOSFALSE` +
/// `IoErr()` = [`crate::dospattern::ERROR_LINE_TOO_LONG`] if the path
/// doesn't fit) -- same contract and truncation behavior as
/// `crate::doslock`'s `NameFromLock`.
fn name_from_fh_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let bptr = ctx.cpu.data_register(DataRegister(1));
    let buf_addr = ctx.cpu.data_register(DataRegister(2));
    let cap = ctx.cpu.data_register(DataRegister(3)) as usize;
    let addr = addr_from_bptr(bptr);

    match ctx.dos.name_from_fh(addr) {
        Ok(path) if path.len() < cap => {
            write_c_string(ctx.mem, buf_addr, path.as_bytes());
            ctx.cpu.set_data_register(DataRegister(0), DOSTRUE);
        }
        Ok(path) => {
            let mut truncated = path.into_bytes();
            truncated.truncate(cap.saturating_sub(1));
            write_c_string(ctx.mem, buf_addr, &truncated);
            ctx.dos.set_io_err(crate::dospattern::ERROR_LINE_TOO_LONG);
            ctx.cpu.set_data_register(DataRegister(0), DOSFALSE);
        }
        Err(code) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), DOSFALSE);
        }
    }
    Ok(())
}

/// `Read` (`D1` = BPTR, `D2` = buffer, `D3` = length). `D0` = bytes read
/// or -1.
fn read_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let bptr = ctx.cpu.data_register(DataRegister(1));
    let buf_addr = ctx.cpu.data_register(DataRegister(2));
    let len = ctx.cpu.data_register(DataRegister(3)) as usize;
    let addr = addr_from_bptr(bptr);
    match ctx.dos.read(addr, len) {
        Ok(bytes) => {
            for (i, &b) in bytes.iter().enumerate() {
                ctx.mem.write_u8(buf_addr.wrapping_add(i as u32), b);
            }
            ctx.cpu
                .set_data_register(DataRegister(0), bytes.len() as u32);
        }
        Err(code) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), RESULT_ERROR);
        }
    }
    Ok(())
}

/// `Write` (`D1` = BPTR, `D2` = buffer, `D3` = length). `D0` = bytes
/// written or -1. Writing to the `Output()` default handle goes straight
/// to `ctx.out` (see [`HostHandle::Stdout`]'s docs).
fn write_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let bptr = ctx.cpu.data_register(DataRegister(1));
    let buf_addr = ctx.cpu.data_register(DataRegister(2));
    let len = ctx.cpu.data_register(DataRegister(3)) as usize;
    let addr = addr_from_bptr(bptr);

    let mut buf = vec![0u8; len];
    for (i, b) in buf.iter_mut().enumerate() {
        *b = ctx.mem.read_u8(buf_addr.wrapping_add(i as u32));
    }

    if ctx.dos.is_output_default(addr) {
        match ctx.out.write_all(&buf) {
            Ok(()) => ctx.cpu.set_data_register(DataRegister(0), len as u32),
            Err(e) => {
                ctx.dos.set_io_err(map_io_error(&e));
                ctx.cpu.set_data_register(DataRegister(0), RESULT_ERROR);
            }
        }
        return Ok(());
    }

    match ctx.dos.write(addr, &buf) {
        Ok(n) => ctx.cpu.set_data_register(DataRegister(0), n as u32),
        Err(code) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), RESULT_ERROR);
        }
    }
    Ok(())
}

/// `Seek` (`D1` = BPTR, `D2` = position, `D3` = offset mode). `D0` = old
/// position or -1.
fn seek_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let bptr = ctx.cpu.data_register(DataRegister(1));
    let position = ctx.cpu.data_register(DataRegister(2)) as i32;
    let offset_mode = ctx.cpu.data_register(DataRegister(3)) as i32;
    let addr = addr_from_bptr(bptr);
    match ctx.dos.seek(addr, position, offset_mode) {
        Ok(old) => ctx.cpu.set_data_register(DataRegister(0), old as u32),
        Err(code) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), RESULT_ERROR);
        }
    }
    Ok(())
}

/// `Input()`. `D0` = BPTR of the default input handle (or 0 on the
/// extremely unlikely guest-heap-exhaustion failure, with `IoErr()`
/// set).
fn input_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    match ctx.dos.input_addr(ctx.heap, ctx.mem) {
        Ok(addr) => ctx
            .cpu
            .set_data_register(DataRegister(0), bptr_from_addr(addr)),
        Err(code) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), 0);
        }
    }
    Ok(())
}

/// `Output()`. As [`input_handler`], for the default output handle.
fn output_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    match ctx.dos.output_addr(ctx.heap, ctx.mem) {
        Ok(addr) => ctx
            .cpu
            .set_data_register(DataRegister(0), bptr_from_addr(addr)),
        Err(code) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), 0);
        }
    }
    Ok(())
}

/// `IsInteractive` (`D1` = `BPTR`). `D0` = `DOSTRUE`/`DOSFALSE`. Reads
/// `fh_Port` directly out of guest memory -- cannot fail, and doesn't
/// touch `IoErr()`, matching the real function.
fn is_interactive_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let bptr = ctx.cpu.data_register(DataRegister(1));
    let addr = addr_from_bptr(bptr);
    let fh_port = ctx.mem.read_u32(addr.wrapping_add(FH_PORT_OFFSET));
    ctx.cpu.set_data_register(
        DataRegister(0),
        if fh_port != 0 { DOSTRUE } else { DOSFALSE },
    );
    Ok(())
}

/// `SetMode` (`D1` = `BPTR`, `D2` = mode). `D0` = `DOSTRUE`, always --
/// this runtime has no real `CON:`/`RAW:`/`AUX:` console handler for
/// the concept of a buffer mode to apply to (see [`crate::dosfile`]'s
/// module docs: `Input()`/`Output()` are backed directly by host
/// stdin/stdout, not a console handler process), so there is nothing to
/// change and nothing that can fail. `IoErr()` is set to `0` on success
/// (real `SetMode` sets it to `1` only "if the console is attached to a
/// window of the Amiga graphical user interface" -- never true here).
fn set_mode_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    ctx.dos.set_io_err(0);
    ctx.cpu.set_data_register(DataRegister(0), DOSTRUE);
    Ok(())
}

/// `WaitForChar` (`D1` = `BPTR`, `D2` = timeout in microseconds). `D0` =
/// `DOSFALSE`, always -- this runtime has no way to non-blockingly peek
/// at host stdin (and no real console handler to ask instead), so
/// rather than actually block for up to `timeout` this always reports
/// "nothing available yet" immediately. Real callers (e.g. `Dir`'s
/// abort-on-keypress check during a long listing) treat that as "no key
/// was pressed" and carry on, which is the correct behavior for this
/// runtime's typical non-interactive/piped corpus-testing use. `IoErr()`
/// is set to `0`, matching "no bytes became available and the handler
/// was able to complete the function" (not an error).
fn wait_for_char_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    ctx.dos.set_io_err(0);
    ctx.cpu.set_data_register(DataRegister(0), DOSFALSE);
    Ok(())
}

/// `Cli` (no args). `D0` = the current task's `pr_CLI` field (a `BPTR` to
/// a real, heap-allocated `struct CommandLineInterface` -- see
/// [`crate::exectask::PR_CLI_OFFSET`]'s doc). This runtime represents
/// CLI-style direct execution (running a binary through volamos is
/// equivalent to running it from a real Shell), not a Workbench icon
/// launch, so `pr_CLI` is always non-`NULL` and this always returns a
/// real `BPTR`. Found needed while running the real `AmiSnap` binary
/// (`~/src/amisnap`, linked with libnix): its startup code checks
/// `pr_CLI` directly (an inline struct-field read, not a call through
/// this handler) to decide whether to `WaitPort()` for a `WBStartup`
/// message that this runtime never sends -- returning `0` here would
/// have been consistent with that same (wrong) inline read, but real
/// `Cli()` and a real, non-`NULL` `pr_CLI` are the correct match for a
/// CLI-launched program either way. Doesn't touch `IoErr()`, matching
/// the real function.
fn cli_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let cli = ctx
        .mem
        .read_u32(ctx.current_task + crate::exectask::PR_CLI_OFFSET);
    ctx.cpu.set_data_register(DataRegister(0), cli);
    Ok(())
}

/// `MaxCli` (no args). `D0` = `0` -- consistent with [`cli_handler`]'s
/// own choice: this runtime never simulates a CLI process table at
/// all (no `CommandLineInterface`/`rn_CliList`), so honestly reporting
/// "the table holds zero entries" is the correct answer, not a missing
/// feature. Found missing while running the real Workbench 3.1.4
/// `C:/Break` binary, which calls this (under `Forbid()`, per the
/// RKRM's documented calling convention) to learn the valid CLI-number
/// range before validating its own numeric argument.
fn max_cli_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    ctx.cpu.set_data_register(DataRegister(0), 0);
    Ok(())
}

/// `GetProgramName` (`D1` = buffer, `D2` = buffer size in bytes). `D0` =
/// `DOSTRUE`/`DOSFALSE`. Copies `cli_CommandName` (a `BSTR`, see
/// [`crate::exectask::create_current_task`]) into the caller's buffer,
/// NUL-terminated, truncating (and returning `DOSFALSE` with `IoErr()`
/// = [`crate::dospattern::ERROR_LINE_TOO_LONG`]) if it doesn't fit --
/// traced against AROS's `rom/dos/getprogramname.c` since the NDK
/// autodoc for this function is thin on exactly which error codes it
/// sets. `pr_CLI` is always non-`NULL` in this runtime (see
/// [`crate::exectask::PR_CLI_OFFSET`]'s doc -- this runtime represents
/// CLI-style execution, never a Workbench launch), so the "no CLI
/// structure" failure branch real `GetProgramName` also has is
/// unreachable here. Found needed running the real `AmiSnap` binary.
fn get_program_name_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let buf = ctx.cpu.data_register(DataRegister(1));
    let len = ctx.cpu.data_register(DataRegister(2)) as i32;

    let cli_bptr = ctx
        .mem
        .read_u32(ctx.current_task + crate::exectask::PR_CLI_OFFSET);
    let cli_addr = addr_from_bptr(cli_bptr);
    let name_bptr = ctx
        .mem
        .read_u32(cli_addr + crate::exectask::CLI_COMMAND_NAME_OFFSET);
    let name = crate::guestmem::read_bstr(ctx.mem, addr_from_bptr(name_bptr));

    let capacity = (len - 1).max(0) as usize;
    let (copy_len, ok) = if name.len() > capacity {
        (capacity, false)
    } else {
        (name.len(), true)
    };
    for (i, &b) in name[..copy_len].iter().enumerate() {
        ctx.mem.write_u8(buf.wrapping_add(i as u32), b);
    }
    ctx.mem.write_u8(buf.wrapping_add(copy_len as u32), 0);

    if !ok {
        ctx.dos.set_io_err(crate::dospattern::ERROR_LINE_TOO_LONG);
    }
    ctx.cpu
        .set_data_register(DataRegister(0), if ok { DOSTRUE } else { DOSFALSE });
    Ok(())
}

/// `DOS_RDARGS` (5), per `<dos/dos.h>` -- the only `AllocDosObject`
/// `type` this runtime implements (see [`alloc_dos_object_handler`]'s
/// doc for why). The other four documented types (`DOS_FILEHANDLE` 0,
/// `DOS_EXALLCONTROL` 1, `DOS_FIB` 2, `DOS_STDPKT` 3, `DOS_CLI` 4)
/// aren't -- `DOS_FILEHANDLE`/`DOS_CLI` in particular would need real
/// integration with this runtime's own `FileHandle`/`pr_CLI`
/// bookkeeping to be genuinely usable, not just a same-shaped zeroed
/// block, so a real implementation of those is deferred until a corpus
/// binary actually needs one.
const DOS_RDARGS: u32 = 5;
/// `sizeof(struct RDArgs)` per `<dos/rdargs.h>`: `RDA_Source` (a
/// `struct CSource`: `CS_Buffer`/`CS_Length`/`CS_CurChr`, 4 each = 12)
/// plus `RDA_DAList`/`RDA_Buffer`/`RDA_BufSiz`/`RDA_ExtHelp`/
/// `RDA_Flags` (4 each = 20) = 32.
const RDARGS_STRUCT_SIZE: u32 = 32;

/// `AllocDosObject` (`D1` = `type`, `D2` = `struct TagItem*` tags).
/// `D0` = the new object, or `0` (`NULL`) on failure. Real
/// `AllocDosObject`'s `DOS_RDARGS` case (traced against AROS's
/// `rom/dos/allocdosobject.c`, since the NDK autodoc doesn't spell out
/// per-type initial contents) is just `AllocVec(sizeof(struct
/// RDArgs), MEMF_CLEAR)` -- a plain zeroed block, no tag processing at
/// all for this type (the caller, e.g. real `ReadArgs()` callers that
/// want `RDA_ExtHelp`, fills in whatever fields it needs itself
/// afterward) -- so `tags` is accepted but never read here, matching
/// real behavior for this specific type exactly rather than only
/// approximating it. Every other `type` fails loudly (see
/// [`DOS_RDARGS`]'s doc) rather than silently returning a
/// same-shaped-but-non-functional block. Found needed running the
/// real `AmiSnap` binary, which calls `AllocDosObject(DOS_RDARGS,
/// NULL)` to build the `RDArgs` its own `ReadArgs()` call needs.
fn alloc_dos_object_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let object_type = ctx.cpu.data_register(DataRegister(1));

    if object_type != DOS_RDARGS {
        return Err(DispatchError::HandlerFailed {
            library: "dos.library".to_string(),
            lvo: -228,
            handler_name: "AllocDosObject".to_string(),
            message: format!(
                "AllocDosObject(type={object_type}): only DOS_RDARGS ({DOS_RDARGS}) is \
                 implemented -- see DOS_RDARGS's doc for why the other types aren't"
            ),
        });
    }

    let addr = ctx
        .heap
        .alloc(RDARGS_STRUCT_SIZE)
        .map_err(|e| DispatchError::HandlerFailed {
            library: "dos.library".to_string(),
            lvo: -228,
            handler_name: "AllocDosObject".to_string(),
            message: format!("AllocDosObject(DOS_RDARGS): guest heap allocation failed: {e}"),
        })?;
    for i in 0..RDARGS_STRUCT_SIZE {
        ctx.mem.write_u8(addr.wrapping_add(i), 0);
    }
    ctx.cpu.set_data_register(DataRegister(0), addr);
    Ok(())
}

/// `FreeDosObject` (`D1` = `type`, `D2` = the object). No return value.
/// Frees a [`DOS_RDARGS`] block allocated by
/// [`alloc_dos_object_handler`]; a `NULL` object is a documented-legal
/// no-op (matches every other free-half-of-a-pair convention in this
/// runtime, e.g. `crate::execmem`'s `FreeVec`). Any other `type`
/// fails loudly, same as the allocation side.
fn free_dos_object_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let object_type = ctx.cpu.data_register(DataRegister(1));
    let addr = ctx.cpu.data_register(DataRegister(2));

    if addr == 0 {
        return Ok(());
    }

    if object_type != DOS_RDARGS {
        return Err(DispatchError::HandlerFailed {
            library: "dos.library".to_string(),
            lvo: -234,
            handler_name: "FreeDosObject".to_string(),
            message: format!(
                "FreeDosObject(type={object_type}): only DOS_RDARGS ({DOS_RDARGS}) is \
                 implemented -- see DOS_RDARGS's doc for why the other types aren't"
            ),
        });
    }

    ctx.heap
        .free(addr)
        .map_err(|e| DispatchError::HandlerFailed {
            library: "dos.library".to_string(),
            lvo: -234,
            handler_name: "FreeDosObject".to_string(),
            message: format!(
                "FreeDosObject called on {addr:#010x}, which isn't a currently-live \
             AllocDosObject(DOS_RDARGS) allocation (never allocated, already freed, or not an \
             AllocDosObject pointer at all): {e}"
            ),
        })
}

/// A fixed, non-`NULL` sentinel `MsgPort*` for
/// [`get_file_sys_task_handler`] -- see its own doc comment for why a
/// real (dereferenceable) `MsgPort` isn't needed. Also reused by
/// `crate::dosdevlist` for `dol_Task` on the `DosList` entries it
/// builds, for the same reason: real callers (`Info`, notably) check
/// this field for `NULL` to decide whether a volume is "live" without
/// ever dereferencing it.
pub(crate) const DEFAULT_FILE_SYS_TASK: u32 = 1;

/// `GetFileSysTask` (no args). `D0` = [`DosState::current_file_sys_task`]
/// (initially [`DEFAULT_FILE_SYS_TASK`], a fixed non-`NULL` sentinel --
/// real callers use this value only to compare against other
/// `MsgPort*`s or pass it along to other calls this runtime doesn't
/// implement (e.g. `DoPkt`, see [`crate::dosdevproc`]'s module docs for
/// why raw packets are out of scope), never to actually dereference it,
/// so a real backing `MsgPort` struct isn't needed. Real
/// `GetFileSysTask` never returns `NULL` on a booted system, so `0`
/// would be the wrong choice here (unlike [`crate::dosdevproc`]'s
/// `dvp_Port`, which real callers do check for `NULL`)). Doesn't touch
/// `IoErr()`, matching the real function.
fn get_file_sys_task_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    ctx.cpu
        .set_data_register(DataRegister(0), ctx.dos.current_file_sys_task);
    Ok(())
}

/// `SetFileSysTask` (`D1` = new `MsgPort*`). `D0` = the previous value
/// of [`DosState::current_file_sys_task`]. Doesn't touch `IoErr()`,
/// matching the real function.
fn set_file_sys_task_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let new_task = ctx.cpu.data_register(DataRegister(1));
    let old_task = std::mem::replace(&mut ctx.dos.current_file_sys_task, new_task);
    ctx.cpu.set_data_register(DataRegister(0), old_task);
    Ok(())
}

/// `SelectInput` (`D1` = `BPTR`). `D0` = the previous input handle's
/// `BPTR`.
fn select_input_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let bptr = ctx.cpu.data_register(DataRegister(1));
    let new_addr = addr_from_bptr(bptr);
    match ctx.dos.select_input(ctx.heap, ctx.mem, new_addr) {
        Ok(old) => ctx
            .cpu
            .set_data_register(DataRegister(0), bptr_from_addr(old)),
        Err(code) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), 0);
        }
    }
    Ok(())
}

/// `SelectOutput` (`D1` = `BPTR`). `D0` = the previous output handle's
/// `BPTR`.
fn select_output_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let bptr = ctx.cpu.data_register(DataRegister(1));
    let new_addr = addr_from_bptr(bptr);
    match ctx.dos.select_output(ctx.heap, ctx.mem, new_addr) {
        Ok(old) => ctx
            .cpu
            .set_data_register(DataRegister(0), bptr_from_addr(old)),
        Err(code) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), 0);
        }
    }
    Ok(())
}

/// `IoErr()`. `D0` = current `IoErr()` value.
fn ioerr_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    ctx.cpu
        .set_data_register(DataRegister(0), ctx.dos.io_err() as u32);
    Ok(())
}

/// `SetIoErr` (`D1` = new value). `D0` = previous value.
fn setioerr_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let new_value = ctx.cpu.data_register(DataRegister(1)) as i32;
    let old = ctx.dos.set_io_err(new_value);
    ctx.cpu.set_data_register(DataRegister(0), old as u32);
    Ok(())
}

/// `PutStr` (`D1` = `CString*`). `D0` = 0 on success, -1 on failure.
/// Writes to the *current* `Output()` selection (so a preceding
/// `SelectOutput` redirects it, exactly like real `PutStr`), which by
/// default is `ctx.out` directly, so output still lands wherever the
/// caller of `Runtime::run` pointed it.
fn putstr_via_output_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let ptr = ctx.cpu.data_register(DataRegister(1));
    let bytes = read_c_string(ctx.mem, ptr);
    let out_addr = match ctx.dos.output_addr(ctx.heap, ctx.mem) {
        Ok(addr) => addr,
        Err(code) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), RESULT_ERROR);
            return Ok(());
        }
    };
    match crate::dosbuf::write_bytes(ctx, out_addr, &bytes) {
        Ok(_) => ctx.cpu.set_data_register(DataRegister(0), 0),
        Err(code) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), RESULT_ERROR);
        }
    }
    Ok(())
}

/// Registers every T10 dos.library handler onto [`DOS_LIBRARY_BASE`],
/// looked up by name through [`DOS_LVOS`] (the T7 table). Called
/// unconditionally from [`crate::dispatch::Runtime::new`] -- these
/// handlers work (for `Input`/`Output`/`PutStr`/`IoErr`/`SetIoErr`) even
/// without a `Vfs` installed, so there's no reason to gate registration
/// on one being configured.
pub fn register_dos_handlers<C: Cpu + 'static>(table: &mut LibraryTable<C>, mem: &mut C::Memory) {
    macro_rules! reg {
        ($name:literal, $handler:expr) => {
            table
                .register_by_name(
                    mem,
                    DOS_LIBRARY_BASE,
                    DOS_LVOS,
                    "dos.library",
                    $name,
                    $handler,
                )
                .unwrap_or_else(|e| panic!("{} should be in DOS_LVOS: {e}", $name));
        };
    }
    reg!("Open", open_handler::<C>);
    reg!("Close", close_handler::<C>);
    reg!("NameFromFH", name_from_fh_handler::<C>);
    reg!("Read", read_handler::<C>);
    reg!("Write", write_handler::<C>);
    reg!("Seek", seek_handler::<C>);
    reg!("Input", input_handler::<C>);
    reg!("Output", output_handler::<C>);
    reg!("IsInteractive", is_interactive_handler::<C>);
    reg!("SetMode", set_mode_handler::<C>);
    reg!("WaitForChar", wait_for_char_handler::<C>);
    reg!("Cli", cli_handler::<C>);
    reg!("MaxCli", max_cli_handler::<C>);
    reg!("GetProgramName", get_program_name_handler::<C>);
    reg!("AllocDosObject", alloc_dos_object_handler::<C>);
    reg!("FreeDosObject", free_dos_object_handler::<C>);
    reg!("GetFileSysTask", get_file_sys_task_handler::<C>);
    reg!("SetFileSysTask", set_file_sys_task_handler::<C>);
    reg!("SelectInput", select_input_handler::<C>);
    reg!("SelectOutput", select_output_handler::<C>);
    reg!("IoErr", ioerr_handler::<C>);
    reg!("SetIoErr", setioerr_handler::<C>);
    reg!("PutStr", putstr_via_output_handler::<C>);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{Runtime, StartConfig};
    use crate::memory::FlatMemory;
    use crate::vfs::VfsConfig;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A unique temp directory, cleaned up on drop (same pattern as
    /// `vfs.rs`'s tests).
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let path = std::env::temp_dir().join(format!("volamos-dosfile-test-{tag}-{pid}-{n}"));
            fs::create_dir_all(&path).expect("create temp dir");
            TempDir { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn vfs_over(root: &Path) -> Vfs {
        Vfs::new(VfsConfig {
            volumes: vec![("SYS".to_string(), root.to_path_buf())],
            assigns: vec![],
            auto_assign_root: None,
            cwd: "SYS:".to_string(),
        })
        .expect("build vfs")
    }

    // --- DosState unit tests (no CPU/guest program involved) ---

    #[test]
    fn open_missing_oldfile_without_vfs_fails_with_object_not_found() {
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let mut mem = FlatMemory::new(0x2000);
        let mut dos = DosState::new(None);
        let err = dos
            .open(&mut heap, &mut mem, "SYS:nope.txt", MODE_OLDFILE)
            .unwrap_err();
        assert_eq!(err, ERROR_OBJECT_NOT_FOUND);
    }

    #[test]
    fn open_missing_oldfile_with_vfs_fails_with_object_not_found() {
        let tmp = TempDir::new("oldfile-missing");
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let mut mem = FlatMemory::new(0x2000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        let err = dos
            .open(&mut heap, &mut mem, "SYS:nope.txt", MODE_OLDFILE)
            .unwrap_err();
        assert_eq!(err, ERROR_OBJECT_NOT_FOUND);
    }

    #[test]
    fn open_newfile_creates_and_writes() {
        let tmp = TempDir::new("newfile");
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let mut mem = FlatMemory::new(0x2000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        let bptr = dos
            .open(&mut heap, &mut mem, "SYS:new.txt", MODE_NEWFILE)
            .expect("MODE_NEWFILE should create the file");
        let addr = addr_from_bptr(bptr);
        let n = dos.write(addr, b"hello").expect("write should succeed");
        assert_eq!(n, 5);
        assert!(dos.close(&mut heap, addr));
        assert_eq!(fs::read(tmp.path().join("new.txt")).unwrap(), b"hello");
    }

    #[test]
    fn name_from_fh_returns_the_normalized_amiga_path() {
        let tmp = TempDir::new("name-from-fh");
        fs::write(tmp.path().join("existing.txt"), b"hi").unwrap();
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let mut mem = FlatMemory::new(0x2000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        let bptr = dos
            .open(&mut heap, &mut mem, "SYS:existing.txt", MODE_OLDFILE)
            .expect("open should succeed");
        let addr = addr_from_bptr(bptr);
        assert_eq!(dos.name_from_fh(addr).unwrap(), "SYS:existing.txt");
    }

    #[test]
    fn name_from_fh_unknown_handle_is_invalid_lock() {
        let dos = DosState::new(None);
        assert_eq!(dos.name_from_fh(0x1234), Err(ERROR_INVALID_LOCK));
    }

    #[test]
    fn name_from_fh_on_stdout_default_handle_fails() {
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let mut mem = FlatMemory::new(0x2000);
        let mut dos = DosState::new(None);
        let addr = dos.output_addr(&mut heap, &mut mem).unwrap();
        assert_eq!(dos.name_from_fh(addr), Err(ERROR_ACTION_NOT_KNOWN));
    }

    #[test]
    fn read_write_round_trip_through_guest_memory() {
        let tmp = TempDir::new("readwrite");
        fs::write(tmp.path().join("existing.txt"), b"0123456789").unwrap();
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let mut mem = FlatMemory::new(0x2000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        let bptr = dos
            .open(&mut heap, &mut mem, "SYS:existing.txt", MODE_OLDFILE)
            .expect("file exists, MODE_OLDFILE should open it");
        let addr = addr_from_bptr(bptr);
        let bytes = dos.read(addr, 5).expect("read should succeed");
        assert_eq!(bytes, b"01234");
        let bytes2 = dos
            .read(addr, 100)
            .expect("read past midpoint should succeed");
        assert_eq!(bytes2, b"56789");
        // EOF: 0 bytes, not an error.
        let eof = dos.read(addr, 10).expect("EOF read should succeed");
        assert!(eof.is_empty());
    }

    #[test]
    fn seek_returns_old_position_and_offset_end_works() {
        let tmp = TempDir::new("seek");
        fs::write(tmp.path().join("f.txt"), b"0123456789").unwrap();
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let mut mem = FlatMemory::new(0x2000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        let bptr = dos
            .open(&mut heap, &mut mem, "SYS:f.txt", MODE_OLDFILE)
            .unwrap();
        let addr = addr_from_bptr(bptr);

        // Read 3 bytes to move the position to 3.
        dos.read(addr, 3).unwrap();
        let old = dos.seek(addr, 0, OFFSET_END).expect("seek should succeed");
        assert_eq!(old, 3, "Seek should return the position *before* seeking");
        // After seeking to the end, a further read is empty (EOF).
        assert!(dos.read(addr, 10).unwrap().is_empty());

        let old2 = dos
            .seek(addr, 0, OFFSET_BEGINNING)
            .expect("seek to beginning should succeed");
        assert_eq!(old2, 10, "position after the OFFSET_END seek was 10");
        assert_eq!(dos.read(addr, 4).unwrap(), b"0123");
    }

    #[test]
    fn set_io_err_returns_old_value() {
        let mut dos = DosState::new(None);
        assert_eq!(dos.io_err(), 0);
        let old = dos.set_io_err(205);
        assert_eq!(old, 0);
        assert_eq!(dos.io_err(), 205);
        let old2 = dos.set_io_err(999);
        assert_eq!(old2, 205);
    }

    #[test]
    fn input_and_output_return_nonzero_bptrs_and_are_stable() {
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let mut mem = FlatMemory::new(0x2000);
        let mut dos = DosState::new(None);
        let in_addr = dos.input_addr(&mut heap, &mut mem).unwrap();
        let out_addr = dos.output_addr(&mut heap, &mut mem).unwrap();
        assert_ne!(in_addr, 0);
        assert_ne!(out_addr, 0);
        assert_ne!(in_addr, out_addr);
        // Repeat calls return the same handle.
        assert_eq!(dos.input_addr(&mut heap, &mut mem).unwrap(), in_addr);
        assert_eq!(dos.output_addr(&mut heap, &mut mem).unwrap(), out_addr);
    }

    #[test]
    fn input_and_output_default_handles_are_marked_interactive() {
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let mut mem = FlatMemory::new(0x2000);
        let mut dos = DosState::new(None);
        let in_addr = dos.input_addr(&mut heap, &mut mem).unwrap();
        let out_addr = dos.output_addr(&mut heap, &mut mem).unwrap();
        assert_ne!(mem.read_u32(in_addr + FH_PORT_OFFSET), 0);
        assert_ne!(mem.read_u32(out_addr + FH_PORT_OFFSET), 0);
    }

    #[test]
    fn a_real_opened_file_is_not_marked_interactive() {
        let tmp = TempDir::new("not-interactive");
        fs::write(tmp.path().join("f.txt"), b"hi").unwrap();
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let mut mem = FlatMemory::new(0x2000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        let bptr = dos
            .open(&mut heap, &mut mem, "SYS:f.txt", MODE_OLDFILE)
            .unwrap();
        let addr = addr_from_bptr(bptr);
        assert_eq!(mem.read_u32(addr + FH_PORT_OFFSET), 0);
    }

    #[test]
    fn select_output_redirects_output_addr_and_returns_the_previous_handle() {
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let mut mem = FlatMemory::new(0x2000);
        let mut dos = DosState::new(None);
        let default_out = dos.output_addr(&mut heap, &mut mem).unwrap();

        let new_addr = 0x1234;
        let old = dos.select_output(&mut heap, &mut mem, new_addr).unwrap();
        assert_eq!(old, default_out, "should return the previous selection");
        assert_eq!(dos.output_addr(&mut heap, &mut mem).unwrap(), new_addr);

        // is_output_default must still only recognize the real
        // stdout-backed handle -- not whatever's currently selected --
        // so a direct Write() to the redirected handle isn't hijacked.
        assert!(dos.is_output_default(default_out));
        assert!(!dos.is_output_default(new_addr));

        // Selecting back restores the previous value on the next call.
        let old2 = dos.select_output(&mut heap, &mut mem, default_out).unwrap();
        assert_eq!(old2, new_addr);
        assert_eq!(dos.output_addr(&mut heap, &mut mem).unwrap(), default_out);
    }

    #[test]
    fn select_input_redirects_input_addr_and_returns_the_previous_handle() {
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let mut mem = FlatMemory::new(0x2000);
        let mut dos = DosState::new(None);
        let default_in = dos.input_addr(&mut heap, &mut mem).unwrap();

        let new_addr = 0x5678;
        let old = dos.select_input(&mut heap, &mut mem, new_addr).unwrap();
        assert_eq!(old, default_in);
        assert_eq!(dos.input_addr(&mut heap, &mut mem).unwrap(), new_addr);
    }

    #[test]
    fn close_of_default_output_handle_is_a_no_op_success() {
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let mut mem = FlatMemory::new(0x2000);
        let mut dos = DosState::new(None);
        let addr = dos.output_addr(&mut heap, &mut mem).unwrap();
        assert!(dos.close(&mut heap, addr));
        // Still usable afterwards -- it wasn't actually freed.
        assert_eq!(dos.output_addr(&mut heap, &mut mem).unwrap(), addr);
    }

    #[test]
    fn close_frees_the_guest_heap_block() {
        let tmp = TempDir::new("closefree");
        fs::write(tmp.path().join("f.txt"), b"x").unwrap();
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let free_before = heap.free_bytes();
        let mut mem = FlatMemory::new(0x2000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        let bptr = dos
            .open(&mut heap, &mut mem, "SYS:f.txt", MODE_OLDFILE)
            .unwrap();
        assert!(heap.free_bytes() < free_before);
        assert!(dos.close(&mut heap, addr_from_bptr(bptr)));
        assert_eq!(heap.free_bytes(), free_before);
    }

    // --- End-to-end tests: hand-assembled guest programs through
    // Runtime::run, matching dispatch.rs's own test style. All guest
    // data (name strings, scratch buffers) is written into `mem` up
    // front, before it's moved into `Runtime::new` -- `Runtime` doesn't
    // expose a mutable-memory accessor (deliberately: real callers only
    // touch guest memory via library calls), so tests that need
    // arbitrary data present at run time have to place it before
    // construction, exactly like dispatch.rs's own tests do.

    fn load_words(mem: &mut FlatMemory, addr: u32, words: &[u16]) {
        let mut offset = addr;
        for &w in words {
            mem.write_u16(offset, w);
            offset += 2;
        }
    }

    /// `move.l #imm32,Dn`.
    fn move_imm_to_d(n: u16) -> u16 {
        0x203C | (n << 9)
    }

    /// `move.l D0,Dn` (copies D0 into another data register, e.g. to
    /// save a just-returned BPTR out of D0 before the next library call
    /// overwrites it).
    fn move_d0_to_d(n: u16) -> u16 {
        0x2000 | (n << 9)
    }

    /// `jsr <disp16>(An)`.
    fn jsr_disp16(an: u16) -> u16 {
        0x4EA8 | an
    }

    const RTS: u16 = 0x4E75;

    /// Appends `move.l #imm,Dn` and returns the index of its first word
    /// (the immediate's high word is at `index + 1`), so callers can
    /// patch in an address computed only after the whole program's
    /// length is known.
    fn push_move_imm_to_d(words: &mut Vec<u16>, dn: u16, imm: u32) -> usize {
        let idx = words.len();
        words.push(move_imm_to_d(dn));
        words.push((imm >> 16) as u16);
        words.push(imm as u16);
        idx
    }

    fn push_jsr(words: &mut Vec<u16>, an: u16, disp: i32) {
        words.push(jsr_disp16(an));
        words.push(disp as u16);
    }

    fn patch_imm32(words: &mut [u16], idx: usize, value: u32) {
        words[idx + 1] = (value >> 16) as u16;
        words[idx + 2] = value as u16;
    }

    /// Builds a `Runtime` around `words` placed at the program's entry
    /// point (A6 pre-seeded to `DOS_LIBRARY_BASE`, per Phase 1
    /// compatibility -- see `dispatch::Runtime::new`'s docs), with
    /// `extra` (e.g. a C-string name, or scratch bytes for a
    /// read/write buffer) written starting at `extra_addr`, and
    /// (optionally) a `Vfs` rooted at `vfs_root` installed before
    /// returning.
    fn runtime_with_program_and_extra(
        words: &[u16],
        extra_addr: u32,
        extra: &[u8],
        vfs_root: Option<&Path>,
    ) -> Runtime<M68kCpu> {
        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, words);
        for (i, &b) in extra.iter().enumerate() {
            mem.write_u8(extra_addr + i as u32, b);
        }
        let load_end = entry + 0x400;
        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        if let Some(root) = vfs_root {
            rt.set_vfs(vfs_over(root));
        }
        rt
    }

    #[test]
    fn end_to_end_open_missing_oldfile_returns_zero_in_d0() {
        let tmp = TempDir::new("e2e-missing-d0");
        let name = b"SYS:nope.txt\0";

        let mut words = Vec::new();
        let name_idx = words.len();
        words.push(move_imm_to_d(1)); // D1 = name (patched below)
        words.push(0);
        words.push(0);
        push_move_imm_to_d(&mut words, 2, MODE_OLDFILE as u32);
        push_jsr(&mut words, 6, -30); // Open(a6): D0 = BPTR or 0
        words.push(RTS); // exit code = D0, untouched since Open

        let name_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        patch_imm32(&mut words, name_idx, name_addr);

        let mut rt = runtime_with_program_and_extra(&words, name_addr, name, Some(tmp.path()));

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, 0,
            "Open of a missing MODE_OLDFILE file should return 0 (NULL) in D0"
        );
    }

    #[test]
    fn end_to_end_open_missing_oldfile_sets_ioerr_readable_via_ioerr_call() {
        let tmp = TempDir::new("e2e-missing-ioerr");
        let name = b"SYS:nope.txt\0";

        let mut words = Vec::new();
        let name_idx = words.len();
        words.push(move_imm_to_d(1)); // D1 = name (patched below)
        words.push(0);
        words.push(0);
        push_move_imm_to_d(&mut words, 2, MODE_OLDFILE as u32);
        push_jsr(&mut words, 6, -30); // Open(a6): D0 = BPTR or 0 (discarded)
        push_jsr(&mut words, 6, -132); // IoErr(a6): D0 = current IoErr()
        words.push(RTS); // exit code = IoErr()'s result

        let name_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        patch_imm32(&mut words, name_idx, name_addr);

        let mut rt = runtime_with_program_and_extra(&words, name_addr, name, Some(tmp.path()));

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, ERROR_OBJECT_NOT_FOUND);
    }

    #[test]
    fn end_to_end_name_from_fh_writes_the_normalized_path() {
        let tmp = TempDir::new("e2e-name-from-fh");
        fs::write(tmp.path().join("existing.txt"), b"hi").unwrap();
        let name = b"SYS:existing.txt\0";

        let mut words = Vec::new();
        let name_idx = words.len();
        words.push(move_imm_to_d(1)); // D1 = name (patched below)
        words.push(0);
        words.push(0);
        push_move_imm_to_d(&mut words, 2, MODE_OLDFILE as u32);
        push_jsr(&mut words, 6, -30); // Open(a6): D0 = BPTR
        words.push(0x2200); // move.l d0,d1 (fh -> D1 for NameFromFH)
        let buf_idx = push_move_imm_to_d(&mut words, 2, 0); // D2 = buffer (patched)
        push_move_imm_to_d(&mut words, 3, 64); // D3 = capacity
        push_jsr(&mut words, 6, -408); // NameFromFH(a6): D0 = DOSTRUE/DOSFALSE
        words.push(RTS);

        let name_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        let buf_addr = name_addr + name.len() as u32;
        patch_imm32(&mut words, name_idx, name_addr);
        patch_imm32(&mut words, buf_idx, buf_addr);

        let mut rt = runtime_with_program_and_extra(&words, name_addr, name, Some(tmp.path()));

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, DOSTRUE as i32);
        assert_eq!(
            crate::guestmem::read_c_string(rt.memory(), buf_addr),
            b"SYS:existing.txt"
        );
    }

    #[test]
    fn end_to_end_mode_newfile_creates_file() {
        let tmp = TempDir::new("e2e-newfile");
        let name = b"SYS:created.txt\0";

        let mut words = Vec::new();
        let name_idx = words.len();
        words.push(move_imm_to_d(1));
        words.push(0);
        words.push(0);
        push_move_imm_to_d(&mut words, 2, MODE_NEWFILE as u32);
        push_jsr(&mut words, 6, -30); // Open(a6): D0 = BPTR or 0
        words.push(RTS);

        let name_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        patch_imm32(&mut words, name_idx, name_addr);

        let mut rt = runtime_with_program_and_extra(&words, name_addr, name, Some(tmp.path()));

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_ne!(code, 0, "Open(MODE_NEWFILE) should return a nonzero BPTR");
        assert!(tmp.path().join("created.txt").exists());
    }

    #[test]
    fn end_to_end_write_and_read_round_trip_through_guest_memory() {
        let tmp = TempDir::new("e2e-readwrite");
        let name = b"SYS:roundtrip.txt\0";

        let mut words = Vec::new();
        let name_idx = words.len();
        words.push(move_imm_to_d(1)); // D1 = name (patched below)
        words.push(0);
        words.push(0);
        push_move_imm_to_d(&mut words, 2, MODE_NEWFILE as u32);
        push_jsr(&mut words, 6, -30); // Open(a6): D0 = BPTR
        words.push(move_d0_to_d(1)); // D1 = handle (survives later calls,
        //                              which only ever touch D0)
        let write_buf_idx = push_move_imm_to_d(&mut words, 2, 0); // D2 = write-buffer addr (patched)
        push_move_imm_to_d(&mut words, 3, 2); // D3 = length (2: "hi")
        push_jsr(&mut words, 6, -48); // Write(a6): D0 = bytes written
        push_move_imm_to_d(&mut words, 2, 0); // D2 = 0 (seek position)
        push_move_imm_to_d(&mut words, 3, OFFSET_BEGINNING as u32); // D3 = OFFSET_BEGINNING
        push_jsr(&mut words, 6, -66); // Seek(a6): D0 = old position (discarded)
        let read_buf_idx = push_move_imm_to_d(&mut words, 2, 0); // D2 = read-buffer addr (patched)
        push_move_imm_to_d(&mut words, 3, 8); // D3 = length (generous)
        push_jsr(&mut words, 6, -42); // Read(a6): D0 = bytes actually read
        words.push(RTS); // exit code = bytes read

        let name_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        let write_buf_addr = name_addr + name.len() as u32;
        let read_buf_addr = write_buf_addr + 4; // 4-byte pad, plenty
        patch_imm32(&mut words, name_idx, name_addr);
        patch_imm32(&mut words, write_buf_idx, write_buf_addr);
        patch_imm32(&mut words, read_buf_idx, read_buf_addr);

        let mut extra = name.to_vec();
        extra.extend_from_slice(b"hi");
        let mut rt = runtime_with_program_and_extra(&words, name_addr, &extra, Some(tmp.path()));

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, 2,
            "Read should return the byte count (2) as the exit code"
        );

        let read_back: Vec<u8> = (0..2)
            .map(|i| rt.memory().read_u8(read_buf_addr + i))
            .collect();
        assert_eq!(read_back, b"hi");
    }

    #[test]
    fn end_to_end_write_to_output_lands_in_ctx_out() {
        let mut words = Vec::new();
        push_jsr(&mut words, 6, -60); // Output(a6): D0 = BPTR
        words.push(move_d0_to_d(1)); // D1 = handle
        let buf_idx = push_move_imm_to_d(&mut words, 2, 0); // D2 = buffer addr (patched)
        push_move_imm_to_d(&mut words, 3, 2); // D3 = length (2: "hi")
        push_jsr(&mut words, 6, -48); // Write(a6): D0 = bytes written
        words.push(RTS); // exit code = bytes written

        let buf_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        patch_imm32(&mut words, buf_idx, buf_addr);

        let mut rt = runtime_with_program_and_extra(&words, buf_addr, b"hi", None);

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, 2,
            "Write should return the byte count (2) as the exit code"
        );
        assert_eq!(out, b"hi");
    }

    #[test]
    fn end_to_end_select_output_redirects_putstr_to_a_real_file() {
        let tmp = TempDir::new("e2e-selectoutput");
        let name = b"SYS:redirected.txt\0";

        let mut words = Vec::new();
        let name_idx = words.len();
        words.push(move_imm_to_d(1)); // D1 = name (patched below)
        words.push(0);
        words.push(0);
        push_move_imm_to_d(&mut words, 2, MODE_NEWFILE as u32);
        push_jsr(&mut words, 6, -30); // Open(a6): D0 = BPTR
        words.push(move_d0_to_d(1)); // D1 = the new file's handle
        push_jsr(&mut words, 6, -300); // SelectOutput(a6): D0 = old handle (discarded)
        let msg_idx = push_move_imm_to_d(&mut words, 1, 0); // D1 = message (patched below)
        push_jsr(&mut words, 6, -948); // PutStr(a6): D0 = 0 on success
        words.push(RTS);

        let name_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        let msg = b"hello redirected\0";
        let msg_addr = name_addr + name.len() as u32;
        patch_imm32(&mut words, name_idx, name_addr);
        patch_imm32(&mut words, msg_idx, msg_addr);

        let mut extra = name.to_vec();
        extra.extend_from_slice(msg);
        let mut rt = runtime_with_program_and_extra(&words, name_addr, &extra, Some(tmp.path()));

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 0, "PutStr should report success");
        assert!(
            out.is_empty(),
            "nothing should reach ctx.out after redirection"
        );
        assert_eq!(
            fs::read(tmp.path().join("redirected.txt")).unwrap(),
            b"hello redirected"
        );
    }

    #[test]
    fn end_to_end_input_and_output_return_nonzero_bptrs() {
        let words = [jsr_disp16(6), (-54i16) as u16, RTS]; // jsr Input(a6); rts
        let mut rt = runtime_with_program_and_extra(&words, TRAP_TABLE_END, &[], None);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_ne!(code, 0, "Input() should return a nonzero BPTR");

        let words2 = [jsr_disp16(6), (-60i16) as u16, RTS]; // jsr Output(a6); rts
        let mut rt2 = runtime_with_program_and_extra(&words2, TRAP_TABLE_END, &[], None);
        let mut out2 = Vec::new();
        let code2 = rt2.run(&mut out2, None).expect("run should succeed");
        assert_ne!(code2, 0, "Output() should return a nonzero BPTR");
    }

    #[test]
    fn end_to_end_is_interactive_true_for_output_false_for_a_real_file() {
        let tmp = TempDir::new("e2e-isinteractive");
        let name = b"SYS:f.txt\0";

        let mut words = Vec::new();
        push_jsr(&mut words, 6, -60); // Output(a6): D0 = BPTR
        words.push(move_d0_to_d(1)); // D1 = Output() handle
        push_jsr(&mut words, 6, -216); // IsInteractive(a6): D0 = DOSTRUE
        words.push(move_d0_to_d(2)); // D2 = save Output()'s result

        let name_idx = words.len();
        words.push(move_imm_to_d(1)); // D1 = name (patched)
        words.push(0);
        words.push(0);
        push_move_imm_to_d(&mut words, 2, MODE_NEWFILE as u32);
        push_jsr(&mut words, 6, -30); // Open(a6): D0 = BPTR
        words.push(move_d0_to_d(1)); // D1 = the new file's handle
        push_jsr(&mut words, 6, -216); // IsInteractive(a6): D0 = DOSFALSE
        words.push(RTS); // exit code = the file's IsInteractive result

        let name_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        patch_imm32(&mut words, name_idx, name_addr);

        let mut rt = runtime_with_program_and_extra(&words, name_addr, name, Some(tmp.path()));
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 0, "a real opened file should not be interactive");
    }

    #[test]
    fn end_to_end_set_mode_always_succeeds() {
        let words = [
            move_imm_to_d(1),
            0,
            0, // D1 = 0 (bptr, unused)
            move_imm_to_d(2),
            0,
            1, // D2 = 1 (raw mode)
            jsr_disp16(6),
            (-426i16) as u16, // SetMode(a6)
            RTS,
        ];
        let mut rt = runtime_with_program_and_extra(&words, TRAP_TABLE_END, &[], None);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, DOSTRUE as i32);
    }

    #[test]
    fn end_to_end_wait_for_char_reports_nothing_available() {
        let words = [
            move_imm_to_d(1),
            0,
            0, // D1 = 0 (bptr, unused)
            move_imm_to_d(2),
            0,
            0, // D2 = 0 (timeout, unused)
            jsr_disp16(6),
            (-204i16) as u16, // WaitForChar(a6)
            RTS,
        ];
        let mut rt = runtime_with_program_and_extra(&words, TRAP_TABLE_END, &[], None);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, DOSFALSE as i32);
    }

    #[test]
    fn end_to_end_get_file_sys_task_returns_a_nonzero_sentinel() {
        let words = [jsr_disp16(6), (-522i16) as u16, RTS]; // jsr GetFileSysTask(a6); rts
        let mut rt = runtime_with_program_and_extra(&words, TRAP_TABLE_END, &[], None);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_ne!(code, 0, "GetFileSysTask() should never report NULL");
    }

    #[test]
    fn end_to_end_set_file_sys_task_returns_previous_and_updates_get() {
        let words = [
            move_imm_to_d(1),
            0,
            0x2A, // D1 = 42 (new "task")
            jsr_disp16(6),
            (-528i16) as u16, // SetFileSysTask(a6): D0 = old value (discarded)
            jsr_disp16(6),
            (-522i16) as u16, // GetFileSysTask(a6): D0 = 42
            RTS,
        ];
        let mut rt = runtime_with_program_and_extra(&words, TRAP_TABLE_END, &[], None);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 42);
    }

    #[test]
    fn end_to_end_cli_returns_non_null() {
        let words = [jsr_disp16(6), (-492i16) as u16, RTS]; // jsr Cli(a6); rts
        let mut rt = runtime_with_program_and_extra(&words, TRAP_TABLE_END, &[], None);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_ne!(
            code, 0,
            "Cli() should report a real, non-NULL BPTR -- this runtime represents \
             CLI-style direct execution, not a Workbench launch"
        );
    }

    #[test]
    fn end_to_end_max_cli_returns_zero() {
        let words = [jsr_disp16(6), (-552i16) as u16, RTS]; // jsr MaxCli(a6); rts
        let mut rt = runtime_with_program_and_extra(&words, TRAP_TABLE_END, &[], None);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, 0,
            "MaxCli() should report an empty table -- no simulated CLI process table"
        );
    }

    #[test]
    fn end_to_end_get_program_name_copies_cli_command_name() {
        let buf_addr: u32 = 0x1_8000;
        let mut words = vec![
            move_imm_to_d(1), // D1 = buf_addr
            (buf_addr >> 16) as u16,
            buf_addr as u16,
            move_imm_to_d(2), // D2 = 64 (buffer size)
            0,
            64,
        ];
        words.extend_from_slice(&[jsr_disp16(6), (-576i16) as u16]); // GetProgramName(a6)
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &words);
        let load_end = entry + 0x400;
        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end,
                args: Vec::new(),
                program_name: "AmiSnap".to_string(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, DOSTRUE as i32,
            "buffer was big enough, should succeed"
        );
        assert_eq!(
            crate::guestmem::read_c_string(rt.memory(), buf_addr),
            b"AmiSnap"
        );
    }

    #[test]
    fn end_to_end_get_program_name_truncates_and_fails_when_buffer_too_small() {
        let buf_addr: u32 = 0x1_8000;
        let mut words = vec![
            move_imm_to_d(1), // D1 = buf_addr
            (buf_addr >> 16) as u16,
            buf_addr as u16,
            move_imm_to_d(2), // D2 = 4 (too small for "AmiSnap\0")
            0,
            4,
        ];
        words.extend_from_slice(&[jsr_disp16(6), (-576i16) as u16]); // GetProgramName(a6)
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &words);
        let load_end = entry + 0x400;
        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end,
                args: Vec::new(),
                program_name: "AmiSnap".to_string(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, DOSFALSE as i32, "buffer too small, should fail");
        assert_eq!(
            crate::guestmem::read_c_string(rt.memory(), buf_addr),
            b"Ami",
            "truncated to capacity - 1, still NUL-terminated"
        );
    }

    #[test]
    fn end_to_end_alloc_and_free_dos_object_rdargs_round_trip() {
        let mut words = vec![
            move_imm_to_d(1), // D1 = DOS_RDARGS (5)
            0,
            5,
            move_imm_to_d(2), // D2 = NULL (no tags)
            0,
            0,
        ];
        words.extend_from_slice(&[jsr_disp16(6), (-228i16) as u16]); // AllocDosObject(a6)
        words.push(move_d0_to_d(3)); // D3 = the RDArgs* (save before D2 gets reused)
        words.push(move_imm_to_d(1)); // D1 = DOS_RDARGS again
        words.push(0);
        words.push(5);
        words.push(0x2E02); // move.l d3,d2 (the RDArgs* -> D2, FreeDosObject's arg)
        words.extend_from_slice(&[jsr_disp16(6), (-234i16) as u16]); // FreeDosObject(a6)
        words.push(RTS);

        let mut rt = runtime_with_program_and_extra(&words, TRAP_TABLE_END, &[], None);
        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed");
    }

    #[test]
    fn end_to_end_alloc_dos_object_returns_a_real_zeroed_rdargs() {
        let mut words = vec![
            move_imm_to_d(1), // D1 = DOS_RDARGS (5)
            0,
            5,
            move_imm_to_d(2), // D2 = NULL (no tags)
            0,
            0,
        ];
        words.extend_from_slice(&[jsr_disp16(6), (-228i16) as u16]); // AllocDosObject(a6)
        words.push(RTS);

        let mut rt = runtime_with_program_and_extra(&words, TRAP_TABLE_END, &[], None);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        let addr = code as u32;
        assert_ne!(addr, 0);
        for i in 0..32u32 {
            assert_eq!(
                rt.memory().read_u8(addr + i),
                0,
                "byte {i} of a fresh DOS_RDARGS block should be zeroed"
            );
        }
    }

    #[test]
    fn end_to_end_alloc_dos_object_unknown_type_fails_loudly() {
        let mut words = vec![
            move_imm_to_d(1), // D1 = DOS_CLI (4), not implemented
            0,
            4,
            move_imm_to_d(2),
            0,
            0,
        ];
        words.extend_from_slice(&[jsr_disp16(6), (-228i16) as u16]); // AllocDosObject(a6)
        words.push(RTS);

        let mut rt = runtime_with_program_and_extra(&words, TRAP_TABLE_END, &[], None);
        let mut out = Vec::new();
        let err = rt
            .run(&mut out, None)
            .expect_err("unimplemented AllocDosObject type should fail loudly");
        match err {
            crate::dispatch::RuntimeError::Dispatch(DispatchError::HandlerFailed {
                library,
                lvo,
                handler_name,
                ..
            }) => {
                assert_eq!(library, "dos.library");
                assert_eq!(lvo, -228);
                assert_eq!(handler_name, "AllocDosObject");
            }
            other => panic!("expected HandlerFailed, got {other:?}"),
        }
    }

    #[test]
    fn end_to_end_set_io_err_returns_old_value() {
        let mut words = Vec::new();
        push_move_imm_to_d(&mut words, 1, 999); // D1 = new IoErr value
        push_jsr(&mut words, 6, -462); // SetIoErr(a6): D0 = previous value (0)
        words.push(RTS);

        let mut rt = runtime_with_program_and_extra(&words, TRAP_TABLE_END, &[], None);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, 0,
            "SetIoErr should return the previous IoErr() value (0)"
        );
    }
}
