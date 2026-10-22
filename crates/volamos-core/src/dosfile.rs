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

use std::collections::{HashMap, HashSet};
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
/// Byte offset of `fh_Arg1` within `struct FileHandle` -- holds the
/// handle struct's own guest address. Real handlers store their private
/// per-file identifier here, and packet-level clients echo it back as
/// `dp_Arg1` in `ACTION_READ`/`ACTION_WRITE`/... packets (RKRM
/// `packet-documentation.md`: "`Arg1`: `fh_Arg1` of the file handle"),
/// so storing the handle's own address makes [`crate::dospkt`]'s packet
/// dispatch a direct `handles` lookup with no separate id map. (An
/// earlier version stored a monotonic debug counter here instead;
/// nothing ever read it back.)
const FH_ARG1_OFFSET: u32 = 36;
/// Byte offset of `fh_Type` within `struct FileHandle`
/// (`dos/dosextens.h`): the `MsgPort` of the *handler process* backing
/// this file -- the port packet-level file I/O sends `DosPacket`s to.
/// Real `Open()` always fills this in (it's how `SendPkt`-style clients
/// find the file system), and real programs genuinely bypass
/// `Read()`/`Write()` and talk packets straight to it: found via the
/// real SAS/C `sc` compiler, whose `sc1.library` reads `fh_Type` off the
/// source-file handle, `PutMsg`s `ACTION_READ`/`ACTION_WRITE` packets to
/// it, and `WaitPort`/`GetMsg`s the reply (issue #24 -- with this left
/// `0`, the packet went to a NULL port and the reply-wait deadlocked).
/// Every handle except `NIL:`'s points at [`DosState::fs_port_addr`]'s
/// host-serviced port; `NIL:` keeps `fh_Type = 0`, matching real
/// AmigaOS, where the NIL: pseudo-handler has no process (RKRM
/// `handlers-filesystems.md`: "If the path is relative to `NIL:`, no
/// handler exists and `NULL` is returned").
pub(crate) const FH_TYPE_OFFSET: u32 = 8;
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

/// Opens `host_path` per `access_mode` (`MODE_OLDFILE`/`MODE_NEWFILE`/
/// `MODE_READWRITE`) -- the actual host [`OpenOptions`] logic shared by
/// [`DosState::open_resolved`] (the `Open`/library-call path) and
/// [`DosState::open_into_handle`] (the `ACTION_FINDINPUT`/
/// `ACTION_FINDOUTPUT`/`ACTION_FINDUPDATE` packet-level path, issue
/// #186) -- both need the exact same "how do I actually open this
/// host file for this access mode" decision, just with a different
/// caller-supplied `FileHandle` to register the result into.
fn open_host_file_for_mode(host_path: &std::path::Path, access_mode: i32) -> Result<File, i32> {
    if access_mode == MODE_NEWFILE {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(host_path)
            .map_err(|e| map_io_error(&e))
    } else if access_mode == MODE_READWRITE {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(host_path)
            .map_err(|e| map_io_error(&e))
    } else {
        // MODE_OLDFILE (and OpenFromLock's own always-existing-file
        // case, which calls this with MODE_OLDFILE regardless of the
        // lock's access mode -- see that method's doc): read+write,
        // falling back to read-only.
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(host_path)
            .or_else(|_| OpenOptions::new().read(true).open(host_path))
            .map_err(|e| map_io_error(&e))
    }
}

/// A file handle's host-side backing.
///
/// `pub(crate)` (not just module-private): [`crate::doslock`]'s
/// `ExamineFH`/`DupLockFromFH`/`ParentOfFH` handlers (V36 calls that
/// turn a `FileHandle` back into lock/`FileInfoBlock` data -- the same
/// kind of thing [`crate::doslock::LockEntry`] already carries for a
/// real `Lock`) need to pattern-match on this from that sibling module,
/// the same "extensions live in the sibling module" convention this
/// file's own docs already establish for `locks`/`exnext`.
pub(crate) enum HostHandle {
    /// A real host file opened via `Open`, plus the normalized Amiga
    /// path it was resolved from and the host path it actually resolved
    /// to (see [`crate::doslock`]'s `LockEntry` for the same "record
    /// both paths" convention) -- [`DosState::name_from_fh`] reads the
    /// Amiga path back for `NameFromFH`; `ExamineFH`/`DupLockFromFH`/
    /// `ParentOfFH` (`crate::doslock`) need the host path too, the same
    /// way `Examine`/`Lock` do from a `LockEntry`.
    HostFile(File, String, std::path::PathBuf),
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
    /// `Open("NIL:", ...)`'s handle -- the real AmigaOS null device:
    /// `Write` always succeeds, discarding the data (real hardware's
    /// `NIL:` handler is a genuine, silent bit-bucket); `Read` always
    /// reports immediate EOF (`0` bytes, matching real `NIL:`'s
    /// documented behavior of never blocking and never producing data).
    /// Found needed alongside `*`/`CONSOLE:` (see [`DosState::open`]'s
    /// doc) -- real programs commonly try `NIL:` as a fallback
    /// destination when a real console/window isn't available.
    Null,
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
    /// `--libs-dir` (see [`crate::dispatch::Runtime::enable_libs_dir_override`]):
    /// when `true`, `OpenLibrary` checks the dedicated
    /// [`crate::dispatch::LIBS_DIR_OVERRIDE_VOLUME`] volume (mounted by
    /// the caller onto the CLI's `--libs-dir` host directory, via `vfs`
    /// above) before anything else -- a hit there loads and runs a real
    /// 68k library in place of this runtime's native implementation, and
    /// a present-but-broken file there is a hard error rather than a
    /// silent fallback.
    pub(crate) libs_dir_override: bool,
    /// Guest `FileHandle` struct address -> host backing. `pub(crate)`
    /// for the same reason [`HostHandle`] itself is -- see that type's
    /// doc.
    pub(crate) handles: HashMap<u32, HostHandle>,
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
    /// Guest address of the lazily-created filesystem handler `MsgPort`
    /// every non-`NIL:` `FileHandle`'s `fh_Type` points at -- see
    /// [`FH_TYPE_OFFSET`]'s doc and [`Self::fs_port_addr`]. `None` until
    /// the first handle is allocated.
    fs_port: Option<u32>,

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
    /// Monotonic counter for `fl_Key` debug values (`FileHandle`s carry
    /// their own guest address in `fh_Arg1` instead of a counter id --
    /// see [`FH_ARG1_OFFSET`]'s doc).
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
    /// Live overlay-root `LoadSeg` state, keyed by the `BPTR` of the
    /// root's `oh_Segments` array (the same value patched into hunk 0's
    /// `OverlayHeader`, and what `LoadSeg(NULL, table, fh)`'s `D2`
    /// carries back on every subsequent overlay-node load) -- see
    /// `crate::dosseg`'s module docs.
    pub(crate) overlay_roots: HashMap<u32, crate::dosseg::OverlayRootState>,
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
    /// The pretend filesystem cache-buffer count `AddBuffers` reports
    /// (see `crate::dosdevlist::add_buffers_handler`) -- one global
    /// count, not per-device (this runtime's VFS has no real cache to
    /// size). Starts at a plausible FFS default.
    pub(crate) fs_buffers: i32,

    // --- issue #186: AllocDosObject(DOS_FILEHANDLE) + packet-level
    // file I/O (ixemul). ---
    /// Guest `FileHandle` addresses allocated via
    /// `AllocDosObject(DOS_FILEHANDLE, ...)` rather than through
    /// `Open`/`OpenFromLock`'s own [`Self::alloc_file_handle`] call --
    /// see [`Self::close`]'s doc for why this distinction matters: a
    /// handle in this set must *not* have its heap block freed by
    /// `Close`/`ACTION_END`, only by a later, explicit
    /// `FreeDosObject(DOS_FILEHANDLE, ...)`, matching real AmigaOS's
    /// "whoever `AllocDosObject`'d it is the one who frees it" pairing
    /// (a real `ixemul.library` `__close()` sends a raw `ACTION_END`
    /// packet, then calls `FreeDosObject` itself -- see `dospkt.rs`'s
    /// `handle_fs_packet` doc for the packet sequence).
    pub(crate) dos_object_filehandles: HashSet<u32>,
}

impl DosState {
    /// Creates a fresh dos.library state. `vfs` may be `None` (see the
    /// module docs' "No VFS configured" section).
    pub fn new(vfs: Option<Vfs>) -> Self {
        Self {
            vfs,
            libs_dir_override: false,
            handles: HashMap::new(),
            io_err: 0,
            input_handle: None,
            output_handle: None,
            current_input: None,
            current_output: None,
            current_file_sys_task: DEFAULT_FILE_SYS_TASK,
            fs_port: None,
            locks: HashMap::new(),
            exnext: HashMap::new(),
            current_dir_lock: None,
            initial_cwd: None,
            next_lock_id: 0,
            seglists: HashMap::new(),
            overlay_roots: HashMap::new(),
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
            fs_buffers: 30,
            dos_object_filehandles: HashSet::new(),
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

    /// The guest address of the filesystem handler `MsgPort` (lazily
    /// created on first use): a real, minimally-initialized `struct
    /// MsgPort` in guest memory whose only job is to be a valid,
    /// non-`NULL` `fh_Type` target -- `PutMsg` to it is intercepted
    /// host-side ([`crate::execlist`]'s `PutMsg` handler routes it to
    /// [`crate::dospkt::handle_fs_packet`]), so nothing ever genuinely
    /// waits on its `mp_MsgList`; the list header is still initialized
    /// so a guest that inspects (or, defensively, `GetMsg`s) the port
    /// sees a coherent empty port rather than zeroed garbage.
    pub(crate) fn fs_port_addr(
        &mut self,
        heap: &mut GuestHeap,
        mem: &mut dyn AddressSpace,
    ) -> Result<u32, i32> {
        if let Some(addr) = self.fs_port {
            return Ok(addr);
        }
        let addr = heap
            .alloc(&mut *mem, crate::execlist::MSGPORT_SIZE)
            .map_err(|_| ERROR_NO_FREE_STORE)?;
        for i in 0..crate::execlist::MSGPORT_SIZE {
            mem.write_u8(addr.wrapping_add(i), 0);
        }
        crate::execlist::init_list_header(mem, addr + crate::execlist::MP_MSGLIST);
        self.fs_port = Some(addr);
        Ok(addr)
    }

    /// Whether `port` is the filesystem handler port -- the `PutMsg`
    /// handler's test for routing a guest-sent `DosPacket` to
    /// [`crate::dospkt::handle_fs_packet`] instead of queueing it.
    pub fn is_fs_handler_port(&self, port: u32) -> bool {
        self.fs_port == Some(port)
    }

    /// Allocates and initializes one guest `struct FileHandle`:
    /// zeroed, `fh_Arg1` = its own address (see [`FH_ARG1_OFFSET`]'s
    /// doc), and -- unless `nil` -- `fh_Type` = the filesystem handler
    /// port (see [`FH_TYPE_OFFSET`]'s doc; `NIL:` handles keep the real
    /// AmigaOS `fh_Type = 0`, "no handler process").
    fn alloc_file_handle(
        &mut self,
        heap: &mut GuestHeap,
        mem: &mut dyn AddressSpace,
        nil: bool,
    ) -> Result<u32, i32> {
        let fh_type = if nil {
            0
        } else {
            self.fs_port_addr(heap, mem)?
        };
        let addr = heap
            .alloc(&mut *mem, FILE_HANDLE_SIZE)
            .map_err(|_| ERROR_NO_FREE_STORE)?;
        for i in 0..FILE_HANDLE_SIZE {
            mem.write_u8(addr.wrapping_add(i), 0);
        }
        mem.write_u32(addr.wrapping_add(FH_ARG1_OFFSET), addr);
        mem.write_u32(addr.wrapping_add(FH_TYPE_OFFSET), fh_type);
        Ok(addr)
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
        // `*` (the process's current console/window) and `CONSOLE:` (a
        // request for a *new* one): neither is a real `Vfs` path, so
        // falling through to path resolution below would always fail
        // with `ERROR_OBJECT_NOT_FOUND` -- a real, confirmed divergence
        // (found running the real SAS/C `sc` compiler, which opens `*`
        // as one of its first two calls and takes a different,
        // eventually-`WaitPort`-deadlocking code path when that fails;
        // `vamos` backs the same call with a real stdout-connected
        // handle and `sc` completes normally). This runtime has no real
        // windowing model to open a genuinely *new* console for
        // `CONSOLE:` (a deliberate, documented non-goal -- see
        // `docs/plan.md`'s "Out of scope for all phases" section), so
        // both are given the same honest stand-in: the process's own
        // `Output()` default handle (real, host-stdout-backed,
        // already `FH_PORT_INTERACTIVE`) -- matching `vamos`'s own
        // choice for `*`. Returning the *same* handle `Output()` itself
        // uses (not a fresh copy) means `Close()` on it already hits
        // the existing "closing the Input()/Output() default is a
        // no-op" path for free, and any `Write`/`IsInteractive()` call
        // against it already works through the existing default-output
        // machinery.
        if name == "*" || name.eq_ignore_ascii_case("CONSOLE:") {
            return self.output_addr(heap, mem).map(bptr_from_addr);
        }
        if name.eq_ignore_ascii_case("NIL:") {
            let addr = self.alloc_file_handle(heap, mem, true)?;
            self.handles.insert(addr, HostHandle::Null);
            return Ok(bptr_from_addr(addr));
        }

        let vfs = self.vfs.as_ref().ok_or(ERROR_OBJECT_NOT_FOUND)?;

        let resolve_mode = match access_mode {
            MODE_OLDFILE => ResolveMode::MustExist,
            MODE_NEWFILE | MODE_READWRITE => ResolveMode::ParentMustExist,
            _ => return Err(ERROR_ACTION_NOT_KNOWN),
        };
        let resolved = vfs
            .resolve_with_amiga_path(name, resolve_mode)
            .map_err(|e| map_vfs_error(&e))?;
        self.open_resolved(
            heap,
            mem,
            resolved.host_path,
            resolved.amiga_path,
            access_mode,
        )
    }

    /// The actual "open a host file per `access_mode` and register a
    /// guest `FileHandle` for it" half of [`Self::open`], split out so
    /// [`Self::open_from_lock`] (`OpenFromLock`) can skip straight to it
    /// with a lock's already-resolved `(host_path, amiga_path)` instead
    /// of re-resolving a name through the `Vfs` -- issue #159's reason
    /// for existing. `open` itself still does its own path resolution
    /// first, then calls this.
    fn open_resolved(
        &mut self,
        heap: &mut GuestHeap,
        mem: &mut dyn AddressSpace,
        host_path: std::path::PathBuf,
        amiga_path: String,
        access_mode: i32,
    ) -> Result<u32, i32> {
        let file = open_host_file_for_mode(&host_path, access_mode)?;
        let addr = self.alloc_file_handle(heap, mem, false)?;
        self.handles
            .insert(addr, HostHandle::HostFile(file, amiga_path, host_path));
        Ok(bptr_from_addr(addr))
    }

    /// Resolves a packet-level `(parent lock, relative name)` pair down
    /// to a single Amiga path string, per the RKRM's `ACTION_LOCATE_OBJECT`
    /// contract (`packet-documentation.md`, reused verbatim by
    /// `ACTION_FINDINPUT`/`ACTION_FINDOUTPUT`/`ACTION_FINDUPDATE`): "the
    /// location of the object... is logically formed by appending the
    /// path in `dp_Arg2` to the directory identified by the lock in
    /// `dp_Arg1`." `parent_lock_addr == 0` means "relative to the root of
    /// the currently inserted volume" (here: resolved exactly like a
    /// plain, lock-free name, via the process's own `Vfs` state); a
    /// non-zero lock looks up its already-normalized
    /// [`crate::doslock::LockEntry::amiga_path`] and appends `name` with
    /// a `/` separator -- matching how [`crate::doslock::DosState::
    /// parent_dir`] and `CurrentDir` already reconstruct Amiga paths from
    /// a lock, rather than trying to resolve relative to a bare host
    /// path (which can't recover assign/volume ambiguity the way an
    /// Amiga path string can -- see this module's "`CurrentDir` and
    /// Amiga-path reconstruction" doc in `doslock.rs`).
    ///
    /// An empty `name` locks/opens the *same* object the parent lock
    /// already does -- the RKRM's own documented special case ("if the
    /// string in `dp_Arg2` is empty, the object to be locked is
    /// identical to the object locked by `dp_Arg1`"), handled here by
    /// simply not appending anything.
    ///
    /// Fails with [`ERROR_INVALID_LOCK`] if `parent_lock_addr != 0` isn't
    /// a currently-open lock.
    pub(crate) fn combine_lock_path(
        &self,
        parent_lock_addr: u32,
        name: &str,
    ) -> Result<String, i32> {
        if parent_lock_addr == 0 {
            return Ok(name.to_string());
        }
        let entry = self
            .locks
            .get(&parent_lock_addr)
            .ok_or(ERROR_INVALID_LOCK)?;
        if name.is_empty() {
            Ok(entry.amiga_path.clone())
        } else if entry.amiga_path.ends_with(':') {
            // A volume-root lock's own `amiga_path` already ends in
            // `:` (e.g. `"SYS:"`) -- appending another `/` would
            // produce `"SYS:/sub"`, which this runtime's path parser
            // (like real AmigaDOS) reads as *the volume root's own
            // parent* followed by `sub`, not as `sub` inside the
            // root: a leading `/` right after the volume name is the
            // same "go up a directory" token `/` is everywhere else in
            // an Amiga path, and a volume root has no parent to go up
            // to. Concatenating directly (`"SYS:sub"`) is what every
            // other Amiga-path-building call site in this runtime
            // already does for the same reason (e.g. `ParentDir`'s own
            // `format!("{vol}:{}", comps.join("/"))`).
            Ok(format!("{}{}", entry.amiga_path, name))
        } else {
            Ok(format!("{}/{}", entry.amiga_path, name))
        }
    }

    /// The packet-level open: `ACTION_FINDINPUT`/`ACTION_FINDOUTPUT`/
    /// `ACTION_FINDUPDATE` (RKRM `packet-documentation.md` -- each
    /// packet type's own numeric value is literally the matching
    /// `MODE_*` constant, see [`MODE_OLDFILE`]/[`MODE_NEWFILE`]/
    /// [`MODE_READWRITE`]'s docs, so `access_mode` here is exactly the
    /// packet's own `dp_Type`).
    ///
    /// Unlike [`Self::open`], this never allocates a `FileHandle` of its
    /// own: the real contract (same RKRM section, "the `FileHandle`
    /// shall be created by `AllocDosObject()`... the packets... then
    /// identify the file by the `fh_Arg1` element") is that the *caller*
    /// already allocated one via `AllocDosObject(DOS_FILEHANDLE)` --
    /// `fh_addr` is that struct's own guest address (same value as its
    /// `fh_Arg1`, per [`Self::alloc_file_handle`]). This just resolves
    /// `name` relative to `parent_lock_addr` (via [`Self::combine_lock_path`]),
    /// opens the host file per `access_mode` (via
    /// [`open_host_file_for_mode`], the same helper [`Self::open_resolved`]
    /// uses), and registers it in `self.handles` keyed by `fh_addr` --
    /// exactly what every other packet/LVO handler already looks up by.
    pub fn open_into_handle(
        &mut self,
        fh_addr: u32,
        parent_lock_addr: u32,
        name: &str,
        access_mode: i32,
    ) -> Result<(), i32> {
        let combined = self.combine_lock_path(parent_lock_addr, name)?;
        let vfs = self.vfs.as_ref().ok_or(ERROR_OBJECT_NOT_FOUND)?;
        let resolve_mode = match access_mode {
            MODE_OLDFILE => ResolveMode::MustExist,
            MODE_NEWFILE | MODE_READWRITE => ResolveMode::ParentMustExist,
            _ => return Err(ERROR_ACTION_NOT_KNOWN),
        };
        let resolved = vfs
            .resolve_with_amiga_path(&combined, resolve_mode)
            .map_err(|e| map_vfs_error(&e))?;
        let file = open_host_file_for_mode(&resolved.host_path, access_mode)?;
        self.handles.insert(
            fh_addr,
            HostHandle::HostFile(file, resolved.amiga_path, resolved.host_path),
        );
        Ok(())
    }

    /// `OpenFromLock(lock)`: opens the object `lock` refers to, reusing
    /// its already-resolved `(host_path, amiga_path)` from
    /// [`crate::doslock::LockEntry`] instead of resolving a name through
    /// the `Vfs` (see [`Self::open_resolved`]'s doc). Fails with
    /// [`ERROR_INVALID_LOCK`] for an unknown lock, or whatever
    /// [`Self::open_resolved`]'s underlying host open fails with.
    ///
    /// Per the real autodoc, "If the open succeeds, the lock is
    /// (effectively) relinquished, and should not be `UnLock()`ed or
    /// used [again]. If the open fails, the lock is still usable." This
    /// implementation takes that literally: on success it calls
    /// [`crate::doslock::DosState::unlock`] on `lock` itself, freeing
    /// the registry entry and the guest `FileLock` struct exactly as a
    /// real `UnLock()` would -- not a defensive guard against a guest
    /// that ignores the "don't touch it again" contract (real AmigaOS
    /// doesn't guard against that either; using the dangling lock
    /// afterwards is undefined there too, and here it would now just
    /// hit the ordinary "unknown lock" error paths every other lock call
    /// already has), simply this runtime's own honest way to free
    /// resources the caller has promised not to need again. On failure
    /// the lock is left completely untouched, matching the autodoc.
    ///
    /// The autodoc also says the resulting handle's access mode "is of
    /// the same access mode as the lock you gave up -- shared is
    /// similar to `MODE_OLDFILE`, exclusive is similar to
    /// `MODE_NEWFILE`." Taken literally for the *open* step that would
    /// mean creating-and-truncating the file for an exclusive lock --
    /// but the file necessarily already exists (that's how the lock
    /// came to exist at all), so `MODE_NEWFILE`'s create/truncate
    /// semantics would destroy data for no reason a real filesystem
    /// would ever have. This always opens the existing file read+write
    /// (falling back to read-only) via [`Self::open_resolved`]'s
    /// `MODE_OLDFILE` branch, for both access modes -- reading the
    /// autodoc's "NEWFILE-like" wording as describing the resulting
    /// handle's nominal access-mode *label* (which this runtime doesn't
    /// track per-[`HostHandle`] at all -- there's no field to set either
    /// way), not a license to re-create an already-locked file.
    pub fn open_from_lock(
        &mut self,
        heap: &mut GuestHeap,
        mem: &mut dyn AddressSpace,
        lock_addr: u32,
    ) -> Result<u32, i32> {
        let entry = self
            .locks
            .get(&lock_addr)
            .ok_or(ERROR_INVALID_LOCK)?
            .clone();
        let result = self.open_resolved(heap, mem, entry.host_path, entry.amiga_path, MODE_OLDFILE);
        if result.is_ok() {
            self.unlock(heap, lock_addr);
        }
        result
    }

    /// Opens `host_path` directly (no `Vfs` resolution at all) and
    /// allocates a guest `FileHandle` for it, exactly like
    /// [`Self::open`]'s `MODE_OLDFILE` case (read+write, falling back to
    /// read-only). For the one caller that needs a real, `Seek`/`Read`-
    /// able guest handle to a file that was never an Amiga path to begin
    /// with: `crate::dispatch::Runtime`'s top-level-program launch (the
    /// `<program>` argument on the CLI is a host path, resolved directly
    /// against the host filesystem, never through a `Vfs` -- see
    /// `crates/volamos/src/main.rs`), loading an overlay executable
    /// whose manager needs `oh_FileHandle` to be exactly this kind of
    /// handle (see `crate::dosseg`'s module docs). The handle's
    /// `NameFromFH`-visible path is `host_path` itself, stringified --
    /// not a real Amiga path (there isn't one), but nothing in this
    /// runtime's overlay-loading path ever reads it back.
    pub fn open_host_file_direct(
        &mut self,
        heap: &mut GuestHeap,
        mem: &mut dyn AddressSpace,
        host_path: &std::path::Path,
    ) -> Result<u32, i32> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(host_path)
            .or_else(|_| OpenOptions::new().read(true).open(host_path))
            .map_err(|e| map_io_error(&e))?;
        let addr = self.alloc_file_handle(heap, mem, false)?;
        self.handles.insert(
            addr,
            HostHandle::HostFile(
                file,
                host_path.to_string_lossy().into_owned(),
                host_path.to_path_buf(),
            ),
        );
        Ok(bptr_from_addr(addr))
    }

    /// `Close`: returns `true` on success (including the no-op case of
    /// closing an `Input()`/`Output()` default handle -- real `Close` on
    /// those succeeds without actually releasing anything), `false` if
    /// `addr` isn't a currently-open handle (caller sets `IoErr()`).
    ///
    /// Also the backing for `ACTION_END` (`dospkt.rs`'s
    /// `handle_fs_packet`): closing the host resource is identical
    /// either way, but whether the guest `FileHandle` struct's *heap
    /// block* gets freed here depends on how it was allocated (issue
    /// #186). A handle `Open`/`OpenFromLock` allocated themselves (via
    /// [`Self::alloc_file_handle`]) is library-managed memory that
    /// `Close` is responsible for freeing, same as always. A handle in
    /// [`Self::dos_object_filehandles`] -- i.e. one the guest itself
    /// `AllocDosObject(DOS_FILEHANDLE)`'d and is now driving with raw
    /// packets -- is *not* freed here: real AmigaOS pairs that
    /// allocation with an explicit, later `FreeDosObject(DOS_FILEHANDLE,
    /// ...)` call (which is what actually frees the struct; see
    /// [`free_dos_object_handler`]'s doc), and freeing it early here
    /// would make that `FreeDosObject` call fail against an
    /// already-freed block for a real, correctly-written program (a
    /// real `ixemul.library` `__close()` does exactly "send
    /// `ACTION_END`, then `FreeDosObject`").
    pub fn close(&mut self, heap: &mut GuestHeap, addr: u32) -> bool {
        if self.input_handle == Some(addr) || self.output_handle == Some(addr) {
            return true;
        }
        if self.handles.remove(&addr).is_some() {
            if !self.dos_object_filehandles.contains(&addr) {
                let _ = heap.free(addr);
            }
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
            Some(HostHandle::HostFile(f, _, _)) => {
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
            // NIL: always reports immediate EOF -- real hardware's null
            // device never produces data.
            Some(HostHandle::Null) => Ok(Vec::new()),
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
            Some(HostHandle::HostFile(f, _, _)) => f.write(data).map_err(|e| map_io_error(&e)),
            Some(HostHandle::Stdin) | Some(HostHandle::Stdout) => Err(ERROR_ACTION_NOT_KNOWN),
            // NIL: discards the data but reports success, as if every
            // byte were written -- real hardware's null device never
            // fails a Write.
            Some(HostHandle::Null) => Ok(data.len()),
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
    ///
    /// Real `Seek`'s own NDK autodoc is explicit that "you cannot Seek()
    /// beyond the end of a file" -- unlike a host `seek()` (standard
    /// POSIX behavior allows seeking arbitrarily far past EOF, creating
    /// a sparse file on a later write), so the target position is
    /// computed and validated against the file's actual length (and
    /// against `0` for a negative target) *before* touching the host
    /// file at all; an out-of-range target fails with
    /// [`ERROR_SEEK_ERROR`] and leaves the file's position untouched.
    /// (The autodoc's own BUGS note says pre-V39 filesystems returned
    /// the *current* position instead of `-1` on this exact error --
    /// amitools' own `dos_seek` test expects that old, documented-as-
    /// fixed behavior, since it's a hardcoded assertion in vamos's own
    /// `pytest` suite, not real hardware. volamos targets KS/WB 3.1
    /// (V40, well past the V39 fix), so `Err` here -- which the caller
    /// turns into `D0 = -1` -- is the correct, modern contract, not a
    /// bug to replicate.)
    pub fn seek(&mut self, addr: u32, position: i32, offset_mode: i32) -> Result<i32, i32> {
        match self.handles.get_mut(&addr) {
            Some(HostHandle::HostFile(f, _, _)) => {
                let old = f.stream_position().map_err(|_| ERROR_SEEK_ERROR)? as i64;
                let len = f.metadata().map_err(|_| ERROR_SEEK_ERROR)?.len() as i64;
                let target = match offset_mode {
                    OFFSET_BEGINNING => position as i64,
                    OFFSET_CURRENT => old + position as i64,
                    OFFSET_END => len + position as i64,
                    _ => return Err(ERROR_SEEK_ERROR),
                };
                if !(0..=len).contains(&target) {
                    return Err(ERROR_SEEK_ERROR);
                }
                f.seek(SeekFrom::Start(target as u64))
                    .map_err(|_| ERROR_SEEK_ERROR)?;
                Ok(old as i32)
            }
            Some(_) => Err(ERROR_SEEK_ERROR),
            None => Err(ERROR_INVALID_LOCK),
        }
    }

    /// `SetFileSize`: truncates or extends the file at `addr` to a new
    /// length computed exactly the way [`Self::seek`]'s target position
    /// is (`OFFSET_BEGINNING`/`OFFSET_CURRENT`/`OFFSET_END`), via
    /// [`File::set_len`]. Per the real documented contract, a target
    /// past the current length is a legal extension (unlike `Seek`,
    /// which rejects that) -- only a negative target is an error. If
    /// the handle's current seek position would now be past the new
    /// end-of-file, it's moved back to exactly the new EOF; otherwise
    /// left untouched (matching "the new size will not leave any
    /// filehandle pointing past the end-of-file", for the one
    /// filehandle this runtime actually tracks a position for).
    /// Returns the new length, or an `IoErr()` code
    /// ([`ERROR_SEEK_ERROR`] for a negative target or an unseekable
    /// handle, [`ERROR_INVALID_LOCK`] for an unknown one, or a mapped
    /// [`std::io::Error`] if the host `set_len` call itself fails).
    pub fn set_file_size(&mut self, addr: u32, offset: i32, offset_mode: i32) -> Result<i32, i32> {
        match self.handles.get_mut(&addr) {
            Some(HostHandle::HostFile(f, _, _)) => {
                let old_pos = f.stream_position().map_err(|_| ERROR_SEEK_ERROR)? as i64;
                let len = f.metadata().map_err(|_| ERROR_SEEK_ERROR)?.len() as i64;
                let target = match offset_mode {
                    OFFSET_BEGINNING => offset as i64,
                    OFFSET_CURRENT => old_pos + offset as i64,
                    OFFSET_END => len + offset as i64,
                    _ => return Err(ERROR_SEEK_ERROR),
                };
                if target < 0 {
                    return Err(ERROR_SEEK_ERROR);
                }
                f.set_len(target as u64).map_err(|e| map_io_error(&e))?;
                if old_pos > target {
                    f.seek(SeekFrom::Start(target as u64))
                        .map_err(|_| ERROR_SEEK_ERROR)?;
                }
                Ok(target as i32)
            }
            Some(_) => Err(ERROR_SEEK_ERROR),
            None => Err(ERROR_INVALID_LOCK),
        }
    }

    /// `LockRecord(fh, offset, length, mode, timeout)` / `UnLockRecord(fh,
    /// offset, length)`: byte-range record locking within an open file.
    /// This runtime models no real concurrency (single process, same
    /// posture as [`crate::execsem`]'s `InitSemaphore`/`ObtainSemaphore`
    /// family and [`crate::exectask`]'s unscheduled-task `AddTask` stub
    /// -- see [`crate::doslock::DosState::change_mode`]'s doc for the
    /// same reasoning spelled out in more detail), so there's nothing a
    /// record lock could actually enforce even if this runtime tracked
    /// one. `offset`/`length`/`mode`/`timeout` are accepted by the
    /// handler (so the real register convention is honored) but never
    /// reach here at all -- there's no equivalent of [`crate::doslock::
    /// LockEntry::access`]'s "recorded for debuggability only" field a
    /// record lock could usefully populate, since nothing else in this
    /// runtime ever inspects per-handle record-lock state either way.
    /// This just validates that `fh` refers to a currently-open
    /// filehandle and reports success; the real autodoc's own "may fail
    /// for implementation specific reasons" already tells callers not
    /// to rely on this unconditionally anyway.
    pub fn lock_record(&self, fh_addr: u32) -> bool {
        self.handles.contains_key(&fh_addr)
    }

    /// See [`Self::lock_record`]'s doc -- the same validate-only posture
    /// for `UnLockRecord`.
    pub fn unlock_record(&self, fh_addr: u32) -> bool {
        self.handles.contains_key(&fh_addr)
    }

    /// `LockRecords(recArray, timeout)` / `UnLockRecords(recArray)`:
    /// like [`Self::lock_record`]/[`Self::unlock_record`] above, no real
    /// concurrency to enforce -- but unlike those, there's also nothing
    /// meaningful to validate here. `recArray` is a caller-allocated
    /// array of individual lock-request records (each naming its own
    /// filehandle, offset, and length), and there's no single
    /// `struct RecordLock`-equivalent layout documented across real
    /// AmigaOS filesystems to walk it by -- the autodoc describes the
    /// array only loosely, by convention rather than by a fixed struct.
    /// Rather than guess at a layout purely to perform a validation this
    /// runtime can't act on anyway (no real concurrency, as above),
    /// this always reports success unconditionally -- real `LockRecords`/
    /// `UnLockRecords` are themselves documented as all-or-nothing calls
    /// (`LockRecords` either locks every record in the array or none of
    /// them), so "always succeeds" is a faithful enough stand-in for a
    /// single-process runtime with no actual contention to fail on.
    pub fn lock_records(&self) -> bool {
        true
    }

    /// See [`Self::lock_records`]'s doc -- the same unconditional-success
    /// posture for `UnLockRecords`.
    pub fn unlock_records(&self) -> bool {
        true
    }

    /// `NameFromFH`: the normalized Amiga path `addr`'s handle was
    /// opened from (recorded on it by [`Self::open`]). Fails with
    /// [`ERROR_INVALID_LOCK`] for an unknown handle, or
    /// [`ERROR_ACTION_NOT_KNOWN`] for the `Input()`/`Output()` default
    /// handles (stdin/stdout aren't Amiga path-backed objects at all).
    pub fn name_from_fh(&self, addr: u32) -> Result<String, i32> {
        match self.handles.get(&addr) {
            Some(HostHandle::HostFile(_, amiga_path, _)) => Ok(amiga_path.clone()),
            Some(HostHandle::Stdin) | Some(HostHandle::Stdout) | Some(HostHandle::Null) => {
                Err(ERROR_ACTION_NOT_KNOWN)
            }
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
                let addr = self.alloc_file_handle(heap, mem, false)?;
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
                let addr = self.alloc_file_handle(heap, mem, false)?;
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

/// `OpenFromLock` (`D1` = `BPTR` lock). `D0` = new file handle `BPTR`,
/// or `0` with `IoErr()` set on failure (the lock remains valid and
/// usable in that case -- see [`DosState::open_from_lock`]'s doc).
fn open_from_lock_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let lock_bptr = ctx.cpu.data_register(DataRegister(1));
    let lock_addr = addr_from_bptr(lock_bptr);
    match ctx.dos.open_from_lock(ctx.heap, ctx.mem, lock_addr) {
        Ok(bptr) => ctx.cpu.set_data_register(DataRegister(0), bptr),
        Err(code) => {
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

/// `SetFileSize` (`D1` = fh, `D2` = offset, `D3` = mode). `D0` = the
/// new length, or `-1` with `IoErr()` set.
fn set_file_size_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let fh = ctx.cpu.data_register(DataRegister(1));
    let offset = ctx.cpu.data_register(DataRegister(2)) as i32;
    let mode = ctx.cpu.data_register(DataRegister(3)) as i32;
    let addr = addr_from_bptr(fh);
    match ctx.dos.set_file_size(addr, offset, mode) {
        Ok(size) => ctx.cpu.set_data_register(DataRegister(0), size as u32),
        Err(code) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), RESULT_ERROR);
        }
    }
    Ok(())
}

/// `LockRecord` (`D1` = fh, `D2` = offset, `D3` = length, `D4` = mode,
/// `D5` = timeout). `D0` = `DOSTRUE`/`DOSFALSE`. See
/// [`DosState::lock_record`]'s doc for why `offset`/`length`/`mode`/
/// `timeout` are read but otherwise unused.
fn lock_record_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let fh = ctx.cpu.data_register(DataRegister(1));
    let _offset = ctx.cpu.data_register(DataRegister(2));
    let _length = ctx.cpu.data_register(DataRegister(3));
    let _mode = ctx.cpu.data_register(DataRegister(4));
    let _timeout = ctx.cpu.data_register(DataRegister(5));
    let addr = addr_from_bptr(fh);
    let ok = ctx.dos.lock_record(addr);
    ctx.cpu
        .set_data_register(DataRegister(0), if ok { DOSTRUE } else { DOSFALSE });
    Ok(())
}

/// `LockRecords` (`D1` = recArray, `D2` = timeout). `D0` =
/// `DOSTRUE`/`DOSFALSE` -- see [`DosState::lock_records`]'s doc for why
/// this never actually inspects `recArray`/`timeout`.
fn lock_records_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let _rec_array = ctx.cpu.data_register(DataRegister(1));
    let _timeout = ctx.cpu.data_register(DataRegister(2));
    let ok = ctx.dos.lock_records();
    ctx.cpu
        .set_data_register(DataRegister(0), if ok { DOSTRUE } else { DOSFALSE });
    Ok(())
}

/// `UnLockRecord` (`D1` = fh, `D2` = offset, `D3` = length). `D0` =
/// `DOSTRUE`/`DOSFALSE`.
fn unlock_record_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let fh = ctx.cpu.data_register(DataRegister(1));
    let _offset = ctx.cpu.data_register(DataRegister(2));
    let _length = ctx.cpu.data_register(DataRegister(3));
    let addr = addr_from_bptr(fh);
    let ok = ctx.dos.unlock_record(addr);
    ctx.cpu
        .set_data_register(DataRegister(0), if ok { DOSTRUE } else { DOSFALSE });
    Ok(())
}

/// `UnLockRecords` (`D1` = recArray). `D0` = `DOSTRUE`/`DOSFALSE`.
fn unlock_records_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let _rec_array = ctx.cpu.data_register(DataRegister(1));
    let ok = ctx.dos.unlock_records();
    ctx.cpu
        .set_data_register(DataRegister(0), if ok { DOSTRUE } else { DOSFALSE });
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
/// touch `IoErr()`, matching the real function. A `NULL` (`0`) handle
/// (real callers never pass one, but a guest reading an unset
/// `BPTR`-typed struct field -- e.g. a stale `cli_StandardInput` --
/// might) reports `DOSFALSE` directly rather than dereferencing
/// `fh_Port` at guest address 0.
fn is_interactive_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let bptr = ctx.cpu.data_register(DataRegister(1));
    if bptr == 0 {
        ctx.cpu.set_data_register(DataRegister(0), DOSFALSE);
        return Ok(());
    }
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

/// `Cli` (no args). `D0` = a real, dereferenceable pointer to the
/// current task's `struct CommandLineInterface` (per the RKRM's own
/// prototype, `struct CommandLineInterface *Cli(void)` -- unlike the
/// `pr_CLI` field it wraps, which is a raw `BPTR`, `Cli()` itself
/// returns the already-`BADDR`-converted address). This runtime
/// represents CLI-style direct execution (running a binary through
/// volamos is equivalent to running it from a real Shell), not a
/// Workbench icon launch, so `pr_CLI` is always non-`NULL` and this
/// always returns a real, non-`NULL` pointer. Found needed while
/// running the real `AmiSnap` binary (`~/src/amisnap`, linked with
/// libnix): its startup code checks `pr_CLI` directly (an inline
/// struct-field read, not a call through this handler) to decide
/// whether to `WaitPort()` for a `WBStartup` message that this runtime
/// never sends -- returning `0` here would have been consistent with
/// that same (wrong) inline read, but real `Cli()` and a real,
/// non-`NULL` `pr_CLI` are the correct match for a CLI-launched
/// program either way. Returning the *raw*, unconverted `BPTR` (rather
/// than converting it) was a separate bug found later, disassembling
/// the real `Search` binary (issue #13): its compiled code reads
/// `cli_StandardInput` via a direct, unshifted `28(a3)` offset off
/// `Cli()`'s own result, per the documented pointer-returning
/// prototype -- a caller that (wrongly, per real `pr_CLI` semantics)
/// tried to shift this back into `BPTR` units would get nonsense.
/// Doesn't touch `IoErr()`, matching the real function.
fn cli_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let cli_bptr = ctx
        .mem
        .read_u32(ctx.current_task + crate::exectask::PR_CLI_OFFSET);
    let cli = addr_from_bptr(cli_bptr);
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

/// `GetProgramDir` (no args). `D0` = `pr_HomeDir` verbatim -- a `BPTR`
/// lock on the directory the running program was loaded from, or `0`
/// if none exists (RKRM `processes.md`: "`ZERO` ... indicates that no
/// home directory exists"). Populated once, at startup, by
/// [`crate::dispatch::Runtime::set_program_dir`] -- see
/// [`crate::exectask::PR_HOMEDIR_OFFSET`]'s doc for why that isn't
/// [`create_current_task`][crate::exectask::create_current_task]
/// itself. Doesn't touch `IoErr()`, matching the real function.
fn get_program_dir_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let home_dir = ctx
        .mem
        .read_u32(ctx.current_task + crate::exectask::PR_HOMEDIR_OFFSET);
    ctx.cpu.set_data_register(DataRegister(0), home_dir);
    Ok(())
}

/// `SetProgramDir` (`D1` = new `BPTR` lock). `D0` = the *previous*
/// `pr_HomeDir` value. Per the RKRM: "installs `lock` into `pr_HomeDir`
/// ... and is used to resolve the `PROGDIR` pseudo-assign" -- so this
/// also repoints the `PROGDIR:` assign, not just the raw field: to
/// whatever `Vfs`-resolved `amiga_path` the registry has on file for
/// `lock` (every lock in this runtime carries one -- see
/// [`crate::doslock::LockEntry`]), or, if `lock` is `0` or isn't a
/// currently-open lock this runtime issued, removes the assign
/// entirely -- matching "if `ZERO` is installed, the current process
/// will be unable to resolve this pseudo-assign", and, for the unknown-
/// lock case, erring on the side of *not* leaving a stale `PROGDIR:`
/// pointed at the previous directory once the caller's own `pr_HomeDir`
/// no longer agrees with it.
fn set_program_dir_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let new_lock = ctx.cpu.data_register(DataRegister(1));
    let old = ctx
        .mem
        .read_u32(ctx.current_task + crate::exectask::PR_HOMEDIR_OFFSET);
    ctx.mem.write_u32(
        ctx.current_task + crate::exectask::PR_HOMEDIR_OFFSET,
        new_lock,
    );

    let amiga_path = (new_lock != 0)
        .then(|| addr_from_bptr(new_lock))
        .and_then(|addr| ctx.dos.locks.get(&addr))
        .map(|entry| entry.amiga_path.clone());
    if let Some(vfs) = ctx.dos.vfs.as_mut() {
        match amiga_path {
            Some(path) => vfs.set_assign("PROGDIR", vec![path]),
            None => vfs.remove_assign("PROGDIR"),
        }
    }

    ctx.cpu.set_data_register(DataRegister(0), old);
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

/// `SetProgramName` (`D1` = name `CString*`). `D0` = `DOSTRUE`/
/// `DOSFALSE`. The inverse of [`get_program_name_handler`]: replaces
/// `cli_CommandName` with a fresh `BSTR` holding `name`, freeing the
/// old one. Real `SetProgramName` "clips to a fixed (1.3 compatible)
/// size" on overflow and leaves the old value intact rather than
/// truncating it -- this runtime has no such fixed-size buffer (the
/// `BSTR` is allocated to fit exactly, same as
/// [`crate::exectask::create_current_task`]'s own initial one), so the
/// only real failure mode left is the guest heap itself being full,
/// which old-value-preserving `Err` branch below also matches (nothing
/// is freed or overwritten on that path either). Clips the same way
/// that initial allocation does (`.min(255)`, a `BSTR`'s length byte
/// can't address more anyway) rather than rejecting a longer name
/// outright. `pr_CLI` is always non-`NULL` in this runtime (see
/// [`get_program_name_handler`]'s doc), so real `SetProgramName`'s
/// documented "safe to call even if there is no CLI structure" case
/// never actually applies here.
fn set_program_name_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let name_ptr = ctx.cpu.data_register(DataRegister(1));
    let name = read_c_string(ctx.mem, name_ptr);
    let clipped = &name[..name.len().min(255)];

    let cli_bptr = ctx
        .mem
        .read_u32(ctx.current_task + crate::exectask::PR_CLI_OFFSET);
    let cli_addr = addr_from_bptr(cli_bptr);
    let old_name_bptr = ctx
        .mem
        .read_u32(cli_addr + crate::exectask::CLI_COMMAND_NAME_OFFSET);

    match ctx.heap.alloc(ctx.mem, 1 + clipped.len() as u32) {
        Ok(new_addr) => {
            crate::guestmem::write_bstr(ctx.mem, new_addr, clipped);
            ctx.mem.write_u32(
                cli_addr + crate::exectask::CLI_COMMAND_NAME_OFFSET,
                bptr_from_addr(new_addr),
            );
            let _ = ctx.heap.free(addr_from_bptr(old_name_bptr));
            ctx.cpu.set_data_register(DataRegister(0), DOSTRUE);
        }
        Err(_) => {
            ctx.cpu.set_data_register(DataRegister(0), DOSFALSE);
        }
    }
    Ok(())
}

/// `DOS_FILEHANDLE` (0), per `<dos/dos.h>` -- a real `struct FileHandle`,
/// built with the exact same [`DosState::alloc_file_handle`] call
/// `Open`/`OpenFromLock` use internally (not a generic zeroed block --
/// see [`alloc_dos_object_handler`]'s doc). Implemented for issue #186:
/// `ixemul.library`'s `__open()` does this as the first step of opening
/// any filesystem-backed file, then drives the rest of the I/O with raw
/// packets (see `dospkt.rs`'s `handle_fs_packet`) instead of further
/// library calls.
const DOS_FILEHANDLE: u32 = 0;
/// `DOS_RDARGS` (5), per `<dos/dos.h>` -- one of the `AllocDosObject`
/// `type`s this runtime implements as a plain zeroed block (see
/// [`alloc_dos_object_handler`]'s doc for why).
const DOS_RDARGS: u32 = 5;
/// `DOS_EXALLCONTROL` (1): the `struct ExAllControl` that `ExAll`
/// requires to be allocated through `AllocDosObject` (see
/// [`crate::dosexall`]'s module docs). Like [`DOS_RDARGS`], real
/// `AllocDosObject`'s treatment is just a zeroed block of the struct's
/// size -- `eac_LastKey == 0` is exactly the required
/// before-the-first-`ExAll`-call state.
const DOS_EXALLCONTROL: u32 = 1;
/// `DOS_FIB` (2): a `struct FileInfoBlock` (`<dos/dos.h>`), the same
/// shape [`crate::doslock`]'s `Examine`/`ExNext` already fill in guest
/// memory -- reuses that module's own [`crate::doslock::FIB_SIZE`]
/// rather than a second, possibly-drifting copy of the constant. Per
/// the NDK autodoc, `AllocDosObject(DOS_FIB, ...)` is just a zeroed
/// `sizeof(struct FileInfoBlock)` block, same as [`DOS_RDARGS`]/
/// [`DOS_EXALLCONTROL`] -- the caller passes it straight into
/// `Examine`/`ExNext`, which populate every field themselves. Found
/// running the real `sidick/micropython` Amiga port's `os.walk()`,
/// which allocates its own `FileInfoBlock` this way instead of using
/// `ExAll`.
const DOS_FIB: u32 = 2;
/// `DOS_STDPKT` (3): a `struct StandardPacket` (an exec `Message`
/// followed by a `DosPacket`, [`STANDARD_PACKET_SIZE`]), linked both
/// ways the way packet clients expect: `sp_Msg.mn_Node.ln_Name` points
/// at `sp_Pkt` and `sp_Pkt.dp_Link` at `sp_Msg`. The object handed back,
/// and the one `FreeDosObject` takes, is the `DosPacket` part
/// ([`SP_PKT_OFFSET`] bytes in), not the start of the allocation --
/// per the autodoc and AROS's `allocdospacket()`. The documented way to
/// get a packet for `SendPkt`/`WaitPkt`; nothing in it depends on this
/// runtime's own bookkeeping.
const DOS_STDPKT: u32 = 3;
/// `DOS_CLI` (4): a `struct CommandLineInterface` plus its four `BSTR`
/// buffers: `cli_SetName`, `cli_CommandName`, `cli_CommandFile` and
/// `cli_Prompt`, sized by the `ADO_DirLen`/`ADO_CommNameLen`/
/// `ADO_CommFileLen`/`ADO_PromptLen` tags (each [`CLI_BUFFER_DEFAULT`]
/// bytes when absent, as in AROS) plus the `BSTR` length byte, each an
/// empty string. Per `<dos/dostags.h>`, "Everything will be zero, except
/// cli_FailLevel (10) and cli_Background (DOSTRUE)". `FreeDosObject`
/// frees the four buffers along with the structure. This is only the
/// object, as real `AllocDosObject` returns it: it is not attached to
/// this runtime's own `pr_CLI`, which a caller that wants that installs
/// itself.
const DOS_CLI: u32 = 4;
/// `sizeof(struct StandardPacket)`: `struct Message` (20) plus `struct
/// DosPacket` (`dp_Link`, `dp_Port`, `dp_Type`, `dp_Res1`, `dp_Res2`,
/// `dp_Arg1`..`dp_Arg7`, 4 each = 48) = 68.
const STANDARD_PACKET_SIZE: u32 = 68;
/// Byte offset of `sp_Pkt` within `struct StandardPacket`, i.e.
/// `sizeof(struct Message)`. `dp_Link` is the packet's first field.
const SP_PKT_OFFSET: u32 = 20;
/// Byte offset of `ln_Name` within `struct Node`, the start of a
/// `struct Message` (`ln_Succ`, `ln_Pred` 4 each, `ln_Type`, `ln_Pri` 1
/// each = 10).
const LN_NAME_OFFSET: u32 = 10;
/// `ADO_Dummy` per `<dos/dostags.h>`: `TAG_USER + 2000`.
const ADO_DUMMY: u32 = 0x8000_0000 + 2000;
/// `ADO_DirLen`: size in bytes of `cli_SetName`'s buffer.
const ADO_DIR_LEN: u32 = ADO_DUMMY + 2;
/// `ADO_CommNameLen`: size in bytes of `cli_CommandName`'s buffer.
const ADO_COMM_NAME_LEN: u32 = ADO_DUMMY + 3;
/// `ADO_CommFileLen`: size in bytes of `cli_CommandFile`'s buffer.
const ADO_COMM_FILE_LEN: u32 = ADO_DUMMY + 4;
/// `ADO_PromptLen`: size in bytes of `cli_Prompt`'s buffer.
const ADO_PROMPT_LEN: u32 = ADO_DUMMY + 5;
/// Size of each `DOS_CLI` buffer whose tag isn't given: "dos will use
/// it's preferred values" (`<dos/dostags.h>`); AROS uses 255 for all
/// four.
const CLI_BUFFER_DEFAULT: u32 = 255;
/// `RETURN_ERROR` (10), per `<dos/dos.h>`: a fresh CLI's `cli_FailLevel`.
const RETURN_ERROR: u32 = 10;
/// The `DOS_CLI` buffers: each `struct CommandLineInterface` field that
/// holds one, and the tag that sizes it.
const CLI_BUFFERS: [(u32, u32); 4] = [
    (crate::exectask::CLI_SET_NAME_OFFSET, ADO_DIR_LEN),
    (crate::exectask::CLI_COMMAND_NAME_OFFSET, ADO_COMM_NAME_LEN),
    (crate::exectask::CLI_COMMAND_FILE_OFFSET, ADO_COMM_FILE_LEN),
    (crate::exectask::CLI_PROMPT_OFFSET, ADO_PROMPT_LEN),
];
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
/// approximating it. [`DOS_EXALLCONTROL`] is the same shape of case: a
/// zeroed `sizeof(struct ExAllControl)` block. [`DOS_STDPKT`] and
/// [`DOS_CLI`] are documented on their constants; with them every type
/// `<dos/dos.h>` defines is implemented, and any other fails loudly.
/// Found needed
/// running the real `AmiSnap` binary, which calls
/// `AllocDosObject(DOS_RDARGS, NULL)` to build the `RDArgs` its own
/// `ReadArgs()` call needs.
fn alloc_dos_object_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let object_type = ctx.cpu.data_register(DataRegister(1));

    if object_type == DOS_FILEHANDLE {
        let addr = ctx
            .dos
            .alloc_file_handle(ctx.heap, ctx.mem, false)
            .map_err(|code| DispatchError::HandlerFailed {
                library: "dos.library".to_string(),
                lvo: -228,
                handler_name: "AllocDosObject".to_string(),
                message: format!(
                    "AllocDosObject(DOS_FILEHANDLE): guest heap allocation failed (IoErr {code})"
                ),
            })?;
        // Marks this handle as owned by the guest (not by `Open`'s own
        // `alloc_file_handle` call) -- see [`DosState::close`]'s doc for
        // why this distinction matters for a later `FreeDosObject`.
        ctx.dos.dos_object_filehandles.insert(addr);
        // Same "zeroed, hence initialised" poisoning rule as the
        // generic path below -- `alloc_file_handle` zeroes the block
        // before writing `fh_Arg1`/`fh_Type`.
        crate::execmem::poison_allocation_edges(ctx, addr, false);
        ctx.cpu.set_data_register(DataRegister(0), addr);
        return Ok(());
    }

    let tags = ctx.cpu.data_register(DataRegister(2));
    let object = match object_type {
        DOS_EXALLCONTROL => alloc_dos_block(ctx, object_type, crate::dosexall::EXALLCONTROL_SIZE)?,
        DOS_FIB => alloc_dos_block(ctx, object_type, crate::doslock::FIB_SIZE)?,
        DOS_STDPKT => {
            let sp = alloc_dos_block(ctx, object_type, STANDARD_PACKET_SIZE)?;
            let pkt = sp + SP_PKT_OFFSET;
            ctx.mem.write_u32(sp + LN_NAME_OFFSET, pkt);
            ctx.mem.write_u32(pkt, sp);
            pkt
        }
        DOS_CLI => {
            let cli = alloc_dos_block(ctx, object_type, crate::exectask::CLI_STRUCT_SIZE)?;
            ctx.mem
                .write_u32(cli + crate::exectask::CLI_FAIL_LEVEL_OFFSET, RETURN_ERROR);
            ctx.mem
                .write_u32(cli + crate::exectask::CLI_BACKGROUND_OFFSET, DOSTRUE);
            for (offset, tag) in CLI_BUFFERS {
                let item = crate::utility::find_tag_item_impl(&*ctx.mem, tag, tags);
                let len = if item == 0 {
                    CLI_BUFFER_DEFAULT
                } else {
                    ctx.mem.read_u32(item + 4)
                };
                // The length byte plus the string; zeroed, so an empty BSTR.
                let buf = alloc_dos_block(ctx, object_type, len.saturating_add(1))?;
                ctx.mem.write_u32(cli + offset, bptr_from_addr(buf));
            }
            cli
        }
        DOS_RDARGS => alloc_dos_block(ctx, object_type, RDARGS_STRUCT_SIZE)?,
        _ => {
            return Err(DispatchError::HandlerFailed {
                library: "dos.library".to_string(),
                lvo: -228,
                handler_name: "AllocDosObject".to_string(),
                message: format!(
                    "AllocDosObject(type={object_type}): not a type <dos/dos.h> defines \
                     (DOS_FILEHANDLE {DOS_FILEHANDLE} .. DOS_RDARGS {DOS_RDARGS})"
                ),
            });
        }
    };

    ctx.cpu.set_data_register(DataRegister(0), object);
    Ok(())
}

/// Allocates a zeroed `size`-byte block on the guest heap for
/// `AllocDosObject(object_type)`. Heap exhaustion fails the run loudly,
/// as it always has here, rather than returning `NULL`.
fn alloc_dos_block<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
    object_type: u32,
    size: u32,
) -> Result<u32, DispatchError> {
    let addr = ctx
        .heap
        .alloc(&mut *ctx.mem, size)
        .map_err(|e| DispatchError::HandlerFailed {
            library: "dos.library".to_string(),
            lvo: -228,
            handler_name: "AllocDosObject".to_string(),
            message: format!(
                "AllocDosObject(type={object_type}): guest heap allocation failed: {e}"
            ),
        })?;
    for i in 0..size {
        ctx.mem.write_u8(addr.wrapping_add(i), 0);
    }
    // After the zeroing, never before -- those writes heal shadow
    // bytes, so poisoning first would flag this handler's own
    // initialisation. `false` because the block is zeroed, hence
    // initialised. See `crate::execmem::poison_allocation_edges`.
    crate::execmem::poison_allocation_edges(ctx, addr, false);
    Ok(addr)
}

/// `FreeDosObject` (`D1` = `type`, `D2` = the object). No return value.
/// Frees an object [`alloc_dos_object_handler`] allocated: a
/// [`DOS_STDPKT`] is passed as its `DosPacket` part, and a [`DOS_CLI`]
/// takes its four buffers with it. A `NULL` object is a
/// documented-legal no-op (matches every other
/// free-half-of-a-pair convention in this runtime, e.g.
/// `crate::execmem`'s `FreeVec`). Any other `type` fails loudly, same as
/// the allocation side.
///
/// `DOS_FILEHANDLE` gets one extra step before the generic heap free:
/// if the handle is still open (the guest sent `ACTION_END` -- or never
/// did), [`DosState::handles`] still has an entry for it, since
/// [`DosState::close`] deliberately leaves a `dos_object_filehandles`
/// member's heap block alone (see that method's doc). Dropping that
/// entry here, before the free, closes the underlying host file (best
/// effort -- a program that skips `ACTION_END` and frees anyway is
/// behaving non-standardly, but shouldn't leak a host file descriptor
/// over it) and un-marks the address, so an ordinary `Open`-allocated
/// handle at the same address later doesn't inherit a stale
/// `dos_object_filehandles` entry.
fn free_dos_object_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let object_type = ctx.cpu.data_register(DataRegister(1));
    let addr = ctx.cpu.data_register(DataRegister(2));

    if addr == 0 {
        return Ok(());
    }

    match object_type {
        DOS_FILEHANDLE => {
            ctx.dos.dos_object_filehandles.remove(&addr);
            ctx.dos.handles.remove(&addr);
            free_dos_block(ctx, addr)
        }
        DOS_EXALLCONTROL | DOS_FIB | DOS_RDARGS => free_dos_block(ctx, addr),
        DOS_STDPKT => free_dos_block(ctx, addr.wrapping_sub(SP_PKT_OFFSET)),
        DOS_CLI => {
            for (offset, _) in CLI_BUFFERS {
                let buf = ctx.mem.read_u32(addr + offset);
                if buf != 0 {
                    free_dos_block(ctx, addr_from_bptr(buf))?;
                }
            }
            free_dos_block(ctx, addr)
        }
        _ => Err(DispatchError::HandlerFailed {
            library: "dos.library".to_string(),
            lvo: -234,
            handler_name: "FreeDosObject".to_string(),
            message: format!(
                "FreeDosObject(type={object_type}): not a type <dos/dos.h> defines \
                 (DOS_FILEHANDLE {DOS_FILEHANDLE} .. DOS_RDARGS {DOS_RDARGS})"
            ),
        }),
    }
}

/// Frees one block [`alloc_dos_block`] (or, for `DOS_FILEHANDLE`,
/// `alloc_file_handle`) allocated.
fn free_dos_block<C: Cpu>(ctx: &mut HandlerContext<'_, C>, addr: u32) -> Result<(), DispatchError> {
    // Before the free, while the extent is still queryable.
    crate::execmem::poison_freed_block(ctx, addr);
    ctx.heap
        .free(addr)
        .map_err(|e| DispatchError::HandlerFailed {
            library: "dos.library".to_string(),
            lvo: -234,
            handler_name: "FreeDosObject".to_string(),
            message: format!(
                "FreeDosObject called on {addr:#010x}, which isn't a currently-live \
             AllocDosObject allocation (never allocated, already freed, or not an \
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
        Ok(old) => {
            ctx.mem
                .write_u32(ctx.current_task + crate::exectask::PR_CIS_OFFSET, bptr);
            ctx.cpu
                .set_data_register(DataRegister(0), bptr_from_addr(old))
        }
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
        Ok(old) => {
            ctx.mem
                .write_u32(ctx.current_task + crate::exectask::PR_COS_OFFSET, bptr);
            ctx.cpu
                .set_data_register(DataRegister(0), bptr_from_addr(old))
        }
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
    reg!("OpenFromLock", open_from_lock_handler::<C>);
    reg!("Close", close_handler::<C>);
    reg!("NameFromFH", name_from_fh_handler::<C>);
    reg!("Read", read_handler::<C>);
    reg!("Write", write_handler::<C>);
    reg!("Seek", seek_handler::<C>);
    reg!("SetFileSize", set_file_size_handler::<C>);
    reg!("LockRecord", lock_record_handler::<C>);
    reg!("LockRecords", lock_records_handler::<C>);
    reg!("UnLockRecord", unlock_record_handler::<C>);
    reg!("UnLockRecords", unlock_records_handler::<C>);
    reg!("Input", input_handler::<C>);
    reg!("Output", output_handler::<C>);
    reg!("IsInteractive", is_interactive_handler::<C>);
    reg!("SetMode", set_mode_handler::<C>);
    reg!("WaitForChar", wait_for_char_handler::<C>);
    reg!("Cli", cli_handler::<C>);
    reg!("MaxCli", max_cli_handler::<C>);
    reg!("GetProgramDir", get_program_dir_handler::<C>);
    reg!("SetProgramDir", set_program_dir_handler::<C>);
    reg!("GetProgramName", get_program_name_handler::<C>);
    reg!("SetProgramName", set_program_name_handler::<C>);
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
            ..Default::default()
        })
        .expect("build vfs")
    }

    // --- issue #186: AllocDosObject(DOS_FILEHANDLE)/FreeDosObject,
    // combine_lock_path, open_into_handle, and the close()/
    // dos_object_filehandles interaction. ---

    #[test]
    fn combine_lock_path_with_no_parent_lock_is_the_bare_name() {
        let dos = DosState::new(None);
        assert_eq!(dos.combine_lock_path(0, "SYS:f.txt").unwrap(), "SYS:f.txt");
    }

    #[test]
    fn combine_lock_path_on_an_unknown_lock_is_invalid_lock() {
        let dos = DosState::new(None);
        let err = dos.combine_lock_path(0x1234, "f.txt").unwrap_err();
        assert_eq!(err, ERROR_INVALID_LOCK);
    }

    #[test]
    fn open_into_handle_resolves_relative_to_a_parent_lock_and_creates_the_file() {
        let tmp = TempDir::new("open-into-handle");
        fs::create_dir(tmp.path().join("sub")).unwrap();
        let mut heap = GuestHeap::new(0x1000, 0x4000);
        let mut mem = FlatMemory::new(0x4000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));

        let lock_bptr = dos
            .lock(&mut heap, &mut mem, "SYS:", crate::doslock::SHARED_LOCK)
            .expect("lock SYS: should succeed");

        // Exactly what AllocDosObject(DOS_FILEHANDLE) does host-side --
        // see alloc_dos_object_handler's doc.
        let fh_addr = dos
            .alloc_file_handle(&mut heap, &mut mem, false)
            .expect("alloc_file_handle should succeed");
        dos.dos_object_filehandles.insert(fh_addr);

        dos.open_into_handle(fh_addr, addr_from_bptr(lock_bptr), "new.txt", MODE_NEWFILE)
            .expect("ACTION_FINDOUTPUT-equivalent open should succeed");

        assert!(tmp.path().join("new.txt").is_file());

        // The handle is now live under its own address, keyed the same
        // way ACTION_READ/WRITE/SEEK/END already look it up (fh_Arg1 ==
        // fh_addr, see alloc_file_handle's doc).
        dos.write(fh_addr, b"hello").expect("write should succeed");
        assert_eq!(
            fs::read(tmp.path().join("new.txt")).unwrap(),
            b"hello",
            "open_into_handle's HostFile should be the real, writable file"
        );
    }

    #[test]
    fn open_into_handle_on_an_unknown_parent_lock_is_invalid_lock() {
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let mut mem = FlatMemory::new(0x2000);
        let mut dos = DosState::new(Some(vfs_over(&std::env::temp_dir())));
        let fh_addr = dos.alloc_file_handle(&mut heap, &mut mem, false).unwrap();
        let err = dos
            .open_into_handle(fh_addr, 0x1234, "f.txt", MODE_OLDFILE)
            .unwrap_err();
        assert_eq!(err, ERROR_INVALID_LOCK);
    }

    #[test]
    fn close_on_a_dos_object_filehandle_does_not_free_its_heap_block() {
        // The whole point of issue #186's `dos_object_filehandles`
        // bookkeeping: a real `ixemul.library` `__close()` sends a raw
        // `ACTION_END` packet (-> `DosState::close`), then calls
        // `FreeDosObject` itself. If `close` already freed the heap
        // block, that `FreeDosObject` would fail against an
        // already-freed address for a completely correct program. This
        // asserts the heap block survives `close` and is still
        // freeable afterward (simulating the later `FreeDosObject`).
        let tmp = TempDir::new("close-dos-object");
        let mut heap = GuestHeap::new(0x1000, 0x4000);
        let mut mem = FlatMemory::new(0x4000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));

        let fh_addr = dos.alloc_file_handle(&mut heap, &mut mem, false).unwrap();
        dos.dos_object_filehandles.insert(fh_addr);
        dos.open_into_handle(fh_addr, 0, "SYS:f.txt", MODE_NEWFILE)
            .expect("open should succeed");

        assert!(dos.close(&mut heap, fh_addr), "close should succeed");
        heap.free(fh_addr)
            .expect("the heap block must still be live for FreeDosObject to free");
    }

    #[test]
    fn close_on_an_ordinary_open_allocated_handle_still_frees_its_heap_block() {
        // The pre-existing behavior for handles `Open`/`OpenFromLock`
        // allocate themselves must not regress: those remain
        // library-managed memory that `Close` itself frees.
        let tmp = TempDir::new("close-ordinary");
        fs::write(tmp.path().join("f.txt"), b"hi").unwrap();
        let mut heap = GuestHeap::new(0x1000, 0x4000);
        let mut mem = FlatMemory::new(0x4000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));

        let bptr = dos
            .open(&mut heap, &mut mem, "SYS:f.txt", MODE_OLDFILE)
            .expect("open should succeed");
        let addr = addr_from_bptr(bptr);

        assert!(dos.close(&mut heap, addr), "close should succeed");
        heap.free(addr)
            .expect_err("the heap block should already have been freed by close");
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
    fn open_star_returns_the_same_handle_output_uses() {
        // No Vfs at all -- `*` must still work, since it isn't a real
        // path lookup (issue #22/found running the real SAS/C `sc`
        // compiler: it opens `*` before anything else, unconditionally).
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let mut mem = FlatMemory::new(0x2000);
        let mut dos = DosState::new(None);
        let output_bptr = bptr_from_addr(
            dos.output_addr(&mut heap, &mut mem)
                .expect("Output() should succeed"),
        );
        let star_bptr = dos
            .open(&mut heap, &mut mem, "*", MODE_OLDFILE)
            .expect("Open(\"*\") should succeed");
        assert_eq!(
            star_bptr, output_bptr,
            "Open(\"*\") should hand back the exact same handle Output() uses"
        );
    }

    #[test]
    fn open_console_also_returns_the_output_handle() {
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let mut mem = FlatMemory::new(0x2000);
        let mut dos = DosState::new(None);
        let output_bptr = bptr_from_addr(
            dos.output_addr(&mut heap, &mut mem)
                .expect("Output() should succeed"),
        );
        let console_bptr = dos
            .open(&mut heap, &mut mem, "CONSOLE:", MODE_NEWFILE)
            .expect("Open(\"CONSOLE:\") should succeed");
        assert_eq!(console_bptr, output_bptr);
        // Case-insensitive, matching every other Amiga device/volume name.
        let console_bptr_lower = dos
            .open(&mut heap, &mut mem, "console:", MODE_OLDFILE)
            .expect("Open(\"console:\") should succeed");
        assert_eq!(console_bptr_lower, output_bptr);
    }

    #[test]
    fn open_sets_fh_type_to_the_fs_port_and_fh_arg1_to_the_handles_own_address() {
        let tmp = TempDir::new("fhtype");
        fs::write(tmp.path().join("f.txt"), b"x").unwrap();
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let mut mem = FlatMemory::new(0x2000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        let bptr = dos
            .open(&mut heap, &mut mem, "SYS:f.txt", MODE_OLDFILE)
            .unwrap();
        let addr = addr_from_bptr(bptr);
        let fs_port = dos.fs_port_addr(&mut heap, &mut mem).unwrap();
        assert_eq!(
            mem.read_u32(addr + FH_TYPE_OFFSET),
            fs_port,
            "fh_Type must name the filesystem handler port -- packet-level \
             clients (real sc1.library, issue #24) PutMsg straight to it"
        );
        assert_eq!(
            mem.read_u32(addr + FH_ARG1_OFFSET),
            addr,
            "fh_Arg1 must round-trip as the packet dispatch lookup key"
        );
        assert!(dos.is_fs_handler_port(fs_port));
        assert!(!dos.is_fs_handler_port(addr));
    }

    #[test]
    fn nil_handle_keeps_fh_type_zero() {
        // Real AmigaOS: the NIL: pseudo-handler has no process, so its
        // handles carry fh_Type = 0 -- see FH_TYPE_OFFSET's doc.
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let mut mem = FlatMemory::new(0x2000);
        let mut dos = DosState::new(None);
        let bptr = dos.open(&mut heap, &mut mem, "NIL:", MODE_OLDFILE).unwrap();
        assert_eq!(mem.read_u32(addr_from_bptr(bptr) + FH_TYPE_OFFSET), 0);
    }

    #[test]
    fn open_nil_reads_as_immediate_eof_and_discards_writes() {
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let mut mem = FlatMemory::new(0x2000);
        let mut dos = DosState::new(None);
        let bptr = dos
            .open(&mut heap, &mut mem, "NIL:", MODE_OLDFILE)
            .expect("Open(\"NIL:\") should succeed even with no Vfs");
        let addr = addr_from_bptr(bptr);

        let read = dos.read(addr, 64).expect("Read should succeed");
        assert!(read.is_empty(), "NIL: should report immediate EOF");

        let n = dos.write(addr, b"discarded").expect("Write should succeed");
        assert_eq!(n, 9, "NIL: should report every byte as written");

        assert!(dos.close(&mut heap, addr));
    }

    #[test]
    fn open_nil_is_case_insensitive_and_a_distinct_handle_per_open() {
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let mut mem = FlatMemory::new(0x2000);
        let mut dos = DosState::new(None);
        let a = dos
            .open(&mut heap, &mut mem, "nil:", MODE_OLDFILE)
            .expect("Open(\"nil:\") should succeed");
        let b = dos
            .open(&mut heap, &mut mem, "NIL:", MODE_OLDFILE)
            .expect("Open(\"NIL:\") should succeed");
        assert_ne!(
            a, b,
            "unlike */CONSOLE:, NIL: is a real per-open object, not an alias for a shared default"
        );
    }

    #[test]
    fn close_on_the_star_handle_is_the_existing_output_default_no_op() {
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let mut mem = FlatMemory::new(0x2000);
        let mut dos = DosState::new(None);
        let star_bptr = dos
            .open(&mut heap, &mut mem, "*", MODE_OLDFILE)
            .expect("Open(\"*\") should succeed");
        let addr = addr_from_bptr(star_bptr);
        assert!(dos.close(&mut heap, addr));
        // Output() must still work afterward -- closing "*" mustn't have
        // torn down the real default handle it aliases.
        assert!(dos.output_addr(&mut heap, &mut mem).is_ok());
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
    fn seek_rejects_a_target_position_beyond_end_of_file() {
        // amitools' own dos_seek_gcc (issue #47): Seek(fh, 10,
        // OFFSET_END) on a 14-byte file targets byte 24, past EOF --
        // real Seek()'s own NDK autodoc says "you cannot Seek() beyond
        // the end of a file". Must fail *without* moving the file's
        // actual position (a later read from the pre-seek position
        // must still see the right bytes).
        let tmp = TempDir::new("seek_oob");
        fs::write(tmp.path().join("f.txt"), b"0123456789").unwrap();
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let mut mem = FlatMemory::new(0x2000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        let bptr = dos
            .open(&mut heap, &mut mem, "SYS:f.txt", MODE_OLDFILE)
            .unwrap();
        let addr = addr_from_bptr(bptr);

        dos.seek(addr, 3, OFFSET_BEGINNING).unwrap();
        assert_eq!(
            dos.seek(addr, 100, OFFSET_END),
            Err(ERROR_SEEK_ERROR),
            "seeking 100 bytes past a 10-byte file's end must fail"
        );
        // Position must be unchanged by the failed seek.
        assert_eq!(dos.read(addr, 3).unwrap(), b"345");
    }

    #[test]
    fn seek_rejects_a_negative_target_position() {
        let tmp = TempDir::new("seek_neg");
        fs::write(tmp.path().join("f.txt"), b"0123456789").unwrap();
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let mut mem = FlatMemory::new(0x2000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        let bptr = dos
            .open(&mut heap, &mut mem, "SYS:f.txt", MODE_OLDFILE)
            .unwrap();
        let addr = addr_from_bptr(bptr);

        dos.seek(addr, 3, OFFSET_BEGINNING).unwrap();
        assert_eq!(
            dos.seek(addr, -10, OFFSET_CURRENT),
            Err(ERROR_SEEK_ERROR),
            "seeking to a negative absolute position must fail"
        );
        assert_eq!(dos.read(addr, 3).unwrap(), b"345");
    }

    #[test]
    fn seek_to_exactly_end_of_file_still_succeeds() {
        // The boundary itself (position == length) is explicitly
        // allowed by the same autodoc note ("the end of the file is a
        // Seek() positioned by zero from end") -- only *beyond* it is
        // rejected.
        let tmp = TempDir::new("seek_exact_eof");
        fs::write(tmp.path().join("f.txt"), b"0123456789").unwrap();
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let mut mem = FlatMemory::new(0x2000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        let bptr = dos
            .open(&mut heap, &mut mem, "SYS:f.txt", MODE_OLDFILE)
            .unwrap();
        let addr = addr_from_bptr(bptr);

        assert_eq!(dos.seek(addr, 10, OFFSET_BEGINNING), Ok(0));
        assert!(dos.read(addr, 1).unwrap().is_empty(), "at EOF, not past it");
    }

    // --- SetFileSize (issue #156 follow-up) ---

    #[test]
    fn set_file_size_extends_a_file() {
        let tmp = TempDir::new("setfilesize_extend");
        fs::write(tmp.path().join("f.txt"), b"0123456789").unwrap();
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let mut mem = FlatMemory::new(0x2000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        let bptr = dos
            .open(&mut heap, &mut mem, "SYS:f.txt", MODE_OLDFILE)
            .unwrap();
        let addr = addr_from_bptr(bptr);

        let new_size = dos
            .set_file_size(addr, 20, OFFSET_BEGINNING)
            .expect("extending should succeed");
        assert_eq!(new_size, 20);
        assert_eq!(
            fs::metadata(tmp.path().join("f.txt")).unwrap().len(),
            20,
            "the host file itself should now be 20 bytes"
        );
    }

    #[test]
    fn set_file_size_truncates_and_moves_a_position_past_the_new_end() {
        let tmp = TempDir::new("setfilesize_truncate");
        fs::write(tmp.path().join("f.txt"), b"0123456789").unwrap();
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let mut mem = FlatMemory::new(0x2000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        let bptr = dos
            .open(&mut heap, &mut mem, "SYS:f.txt", MODE_OLDFILE)
            .unwrap();
        let addr = addr_from_bptr(bptr);
        dos.seek(addr, 8, OFFSET_BEGINNING).unwrap();

        let new_size = dos
            .set_file_size(addr, 3, OFFSET_BEGINNING)
            .expect("truncating should succeed");
        assert_eq!(new_size, 3);
        // The seek position (8) was past the new end (3) -- must have
        // been moved back to exactly the new EOF, not left dangling.
        assert!(dos.read(addr, 1).unwrap().is_empty(), "now sitting at EOF");
        assert_eq!(dos.seek(addr, 0, OFFSET_BEGINNING), Ok(3));
    }

    #[test]
    fn set_file_size_truncating_above_the_current_position_leaves_it_untouched() {
        let tmp = TempDir::new("setfilesize_truncate_above_pos");
        fs::write(tmp.path().join("f.txt"), b"0123456789").unwrap();
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let mut mem = FlatMemory::new(0x2000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        let bptr = dos
            .open(&mut heap, &mut mem, "SYS:f.txt", MODE_OLDFILE)
            .unwrap();
        let addr = addr_from_bptr(bptr);
        dos.seek(addr, 2, OFFSET_BEGINNING).unwrap();

        dos.set_file_size(addr, 5, OFFSET_BEGINNING).unwrap();
        // Position (2) was already before the new end (5) -- untouched.
        assert_eq!(dos.seek(addr, 0, OFFSET_CURRENT), Ok(2));
    }

    #[test]
    fn set_file_size_rejects_a_negative_target() {
        let tmp = TempDir::new("setfilesize_negative");
        fs::write(tmp.path().join("f.txt"), b"0123456789").unwrap();
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let mut mem = FlatMemory::new(0x2000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        let bptr = dos
            .open(&mut heap, &mut mem, "SYS:f.txt", MODE_OLDFILE)
            .unwrap();
        let addr = addr_from_bptr(bptr);

        assert_eq!(
            dos.set_file_size(addr, -100, OFFSET_END),
            Err(ERROR_SEEK_ERROR)
        );
    }

    #[test]
    fn set_file_size_on_an_unknown_handle_is_invalid_lock() {
        let mut dos = DosState::new(None);
        assert_eq!(
            dos.set_file_size(0x1234, 0, OFFSET_BEGINNING),
            Err(ERROR_INVALID_LOCK)
        );
    }

    // --- LockRecord / UnLockRecord (issue #160) ---

    #[test]
    fn lock_record_on_an_open_handle_succeeds() {
        let tmp = TempDir::new("lockrecord-ok");
        fs::write(tmp.path().join("f.txt"), b"hi").unwrap();
        let mut heap = GuestHeap::new(0x1000, 0x4000);
        let mut mem = FlatMemory::new(0x4000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        let fh_bptr = dos
            .open(&mut heap, &mut mem, "SYS:f.txt", MODE_OLDFILE)
            .unwrap();
        let fh_addr = addr_from_bptr(fh_bptr);

        assert!(dos.lock_record(fh_addr));
        assert!(dos.unlock_record(fh_addr));
    }

    #[test]
    fn lock_record_on_an_unknown_handle_is_false() {
        let dos = DosState::new(None);
        assert!(!dos.lock_record(0x1234));
    }

    #[test]
    fn unlock_record_on_an_unknown_handle_is_false() {
        let dos = DosState::new(None);
        assert!(!dos.unlock_record(0x1234));
    }

    // --- LockRecords / UnLockRecords (issue #160) ---

    #[test]
    fn lock_records_and_unlock_records_always_succeed() {
        let dos = DosState::new(None);
        assert!(dos.lock_records());
        assert!(dos.unlock_records());
    }

    // --- OpenFromLock (issue #159) ---

    #[test]
    fn open_from_lock_opens_the_file_and_relinquishes_a_shared_lock() {
        let tmp = TempDir::new("openfromlock_ok");
        fs::write(tmp.path().join("f.txt"), b"hello").unwrap();
        let mut heap = GuestHeap::new(0x1000, 0x4000);
        let mut mem = FlatMemory::new(0x4000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        let lock_bptr = dos
            .lock(
                &mut heap,
                &mut mem,
                "SYS:f.txt",
                crate::doslock::SHARED_LOCK,
            )
            .expect("lock should succeed");
        let lock_addr = addr_from_bptr(lock_bptr);

        let fh_bptr = dos
            .open_from_lock(&mut heap, &mut mem, lock_addr)
            .expect("open_from_lock should succeed");
        let fh_addr = addr_from_bptr(fh_bptr);
        assert_eq!(dos.read(fh_addr, 5).unwrap(), b"hello");
        assert!(
            !dos.locks.contains_key(&lock_addr),
            "a successful OpenFromLock must relinquish the lock"
        );
    }

    #[test]
    fn open_from_lock_on_an_unknown_lock_is_invalid_lock() {
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let mut mem = FlatMemory::new(0x2000);
        let mut dos = DosState::new(None);
        assert_eq!(
            dos.open_from_lock(&mut heap, &mut mem, 0x1234),
            Err(ERROR_INVALID_LOCK)
        );
    }

    #[test]
    fn open_from_lock_leaves_the_lock_usable_on_a_failed_open() {
        let tmp = TempDir::new("openfromlock_fail");
        let mut heap = GuestHeap::new(0x1000, 0x4000);
        let mut mem = FlatMemory::new(0x4000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        // Fabricate a lock entry pointing at a host path that doesn't
        // exist, forcing the underlying host open to fail cleanly (a
        // mapped `std::io::Error`) without needing `Lock` itself to
        // ever have accepted a bad path -- the point here is purely
        // "did a failed open leave the lock alone".
        let lock_addr = 0x3000;
        dos.locks.insert(
            lock_addr,
            crate::doslock::LockEntry {
                host_path: tmp.path().join("missing.txt"),
                amiga_path: "SYS:missing.txt".to_string(),
                access: crate::doslock::SHARED_LOCK,
            },
        );

        let err = dos
            .open_from_lock(&mut heap, &mut mem, lock_addr)
            .expect_err("opening a nonexistent host path should fail");
        assert_eq!(err, ERROR_OBJECT_NOT_FOUND);
        assert!(
            dos.locks.contains_key(&lock_addr),
            "a failed OpenFromLock must leave the lock usable"
        );
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
        let mut mem = FlatMemory::new(0x2000);
        let mut dos = DosState::new(Some(vfs_over(tmp.path())));
        // Force the singleton filesystem handler port into existence
        // first -- it deliberately persists for the process lifetime
        // (every later handle's fh_Type names it), so it must not count
        // against the open/close round-trip measured below.
        dos.fs_port_addr(&mut heap, &mut mem).unwrap();
        let free_before = heap.free_bytes();
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
    fn end_to_end_get_program_dir_returns_zero_by_default() {
        // No Runtime::set_program_dir call -- matches real AmigaOS's
        // own "no home directory exists" case (issue #17).
        let words = [jsr_disp16(6), (-600i16) as u16, RTS]; // jsr GetProgramDir(a6); rts
        let mut rt = runtime_with_program_and_extra(&words, TRAP_TABLE_END, &[], None);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 0);
    }

    #[test]
    fn end_to_end_get_program_dir_returns_non_null_after_set_program_dir() {
        let tmp = TempDir::new("progdir-e2e");
        let words = [jsr_disp16(6), (-600i16) as u16, RTS]; // jsr GetProgramDir(a6); rts
        let mut rt = runtime_with_program_and_extra(&words, TRAP_TABLE_END, &[], Some(tmp.path()));
        rt.set_program_dir(tmp.path());

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_ne!(
            code, 0,
            "GetProgramDir() should report a real, non-NULL lock once Runtime::set_program_dir \
             has resolved the launched binary's own directory"
        );
    }

    #[test]
    fn end_to_end_progdir_resolves_via_open_after_set_program_dir() {
        let tmp = TempDir::new("progdir-open-e2e");
        fs::write(tmp.path().join("marker.txt"), b"hi").unwrap();
        let name = b"PROGDIR:marker.txt\0";

        let mut words = Vec::new();
        let name_idx = words.len();
        words.push(move_imm_to_d(1)); // D1 = name (patched below)
        words.push(0);
        words.push(0);
        push_move_imm_to_d(&mut words, 2, MODE_OLDFILE as u32);
        push_jsr(&mut words, 6, -30); // Open(a6): D0 = BPTR or 0
        words.push(RTS); // exit code = D0

        let name_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        patch_imm32(&mut words, name_idx, name_addr);

        let mut rt = runtime_with_program_and_extra(&words, name_addr, name, Some(tmp.path()));
        rt.set_program_dir(tmp.path());

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_ne!(
            code, 0,
            "PROGDIR:marker.txt should resolve through the ordinary Vfs path once \
             Runtime::set_program_dir has installed the PROGDIR: assign (issue #17)"
        );
    }

    #[test]
    fn end_to_end_set_program_dir_zero_removes_the_progdir_assign() {
        let tmp = TempDir::new("setprogdir-e2e");
        fs::write(tmp.path().join("marker.txt"), b"hi").unwrap();
        let name = b"PROGDIR:marker.txt\0";

        let mut words = Vec::new();
        push_move_imm_to_d(&mut words, 1, 0); // D1 = 0 (new lock)
        push_jsr(&mut words, 6, -594); // SetProgramDir(a6): D0 = old lock
        let name_idx = words.len();
        words.push(move_imm_to_d(1)); // D1 = name (patched below)
        words.push(0);
        words.push(0);
        push_move_imm_to_d(&mut words, 2, MODE_OLDFILE as u32);
        push_jsr(&mut words, 6, -30); // Open(a6): D0 = BPTR or 0
        words.push(RTS); // exit code = D0

        let name_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        patch_imm32(&mut words, name_idx, name_addr);

        let mut rt = runtime_with_program_and_extra(&words, name_addr, name, Some(tmp.path()));
        rt.set_program_dir(tmp.path());

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, 0,
            "SetProgramDir(0) should remove the PROGDIR: assign, matching the RKRM's own \
             \"if ZERO is installed, the current process will be unable to resolve this \
             pseudo-assign\""
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

    // --- SetProgramName (issue #156 follow-up) ---

    #[test]
    fn end_to_end_set_program_name_then_get_program_name_round_trips() {
        let name_addr: u32 = 0x1_8000;
        let buf_addr: u32 = 0x1_8100;
        let mut words = vec![
            move_imm_to_d(1), // D1 = name_addr ("replaced")
            (name_addr >> 16) as u16,
            name_addr as u16,
        ];
        words.extend_from_slice(&[jsr_disp16(6), (-570i16) as u16]); // SetProgramName(a6)
        words.extend_from_slice(&[
            move_imm_to_d(1), // D1 = buf_addr
            (buf_addr >> 16) as u16,
            buf_addr as u16,
            move_imm_to_d(2), // D2 = 64 (buffer size)
            0,
            64,
        ]);
        words.extend_from_slice(&[jsr_disp16(6), (-576i16) as u16]); // GetProgramName(a6)
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &words);
        write_c_string(&mut mem, name_addr, b"replaced");
        let load_end = entry + 0x400;
        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end,
                args: Vec::new(),
                program_name: "original".to_string(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, DOSTRUE as i32, "GetProgramName should still succeed");
        assert_eq!(
            crate::guestmem::read_c_string(rt.memory(), buf_addr),
            b"replaced",
            "GetProgramName should see SetProgramName's new value, not the original"
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
    fn end_to_end_alloc_and_free_dos_object_fib_round_trip() {
        // Found running the real `sidick/micropython` Amiga port's
        // `os.walk()`, which allocates its own `FileInfoBlock` this
        // way (rather than via `ExAll`).
        let mut words = vec![
            move_imm_to_d(1), // D1 = DOS_FIB (2)
            0,
            2,
            move_imm_to_d(2), // D2 = NULL (no tags)
            0,
            0,
        ];
        words.extend_from_slice(&[jsr_disp16(6), (-228i16) as u16]); // AllocDosObject(a6)
        words.push(move_d0_to_d(3)); // D3 = the FileInfoBlock* (save before D2 gets reused)
        words.push(move_imm_to_d(1)); // D1 = DOS_FIB again
        words.push(0);
        words.push(2);
        words.push(0x2E02); // move.l d3,d2 (the FileInfoBlock* -> D2, FreeDosObject's arg)
        words.extend_from_slice(&[jsr_disp16(6), (-234i16) as u16]); // FreeDosObject(a6)
        words.push(RTS);

        let mut rt = runtime_with_program_and_extra(&words, TRAP_TABLE_END, &[], None);
        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed");
    }

    #[test]
    fn end_to_end_alloc_dos_object_returns_a_real_zeroed_fib() {
        let mut words = vec![
            move_imm_to_d(1), // D1 = DOS_FIB (2)
            0,
            2,
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
        for i in 0..crate::doslock::FIB_SIZE {
            assert_eq!(
                rt.memory().read_u8(addr + i),
                0,
                "byte {i} of a fresh DOS_FIB block should be zeroed"
            );
        }
    }

    /// `move.l Ds,Dd`.
    fn move_d_to_d(src: u16, dst: u16) -> u16 {
        0x2000 | (dst << 9) | src
    }

    /// Runs `AllocDosObject(object_type, NULL)` and returns the runtime
    /// with the new object's address (the program's exit code).
    fn alloc_dos_object_run(object_type: u32) -> (Runtime<M68kCpu>, u32) {
        let mut words = Vec::new();
        push_move_imm_to_d(&mut words, 1, object_type);
        push_move_imm_to_d(&mut words, 2, 0); // no tags
        push_jsr(&mut words, 6, -228); // AllocDosObject(a6)
        words.push(RTS);

        let mut rt = runtime_with_program_and_extra(&words, TRAP_TABLE_END, &[], None);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        (rt, code as u32)
    }

    /// Runs `AllocDosObject(object_type, NULL)` then `FreeDosObject` on
    /// the result, and returns the runtime with the (now freed) object.
    fn alloc_free_dos_object_run(object_type: u32) -> (Runtime<M68kCpu>, u32) {
        let mut words = Vec::new();
        push_move_imm_to_d(&mut words, 1, object_type);
        push_move_imm_to_d(&mut words, 2, 0); // no tags
        push_jsr(&mut words, 6, -228); // AllocDosObject(a6)
        words.push(move_d0_to_d(3)); // D3 = the object
        push_move_imm_to_d(&mut words, 1, object_type);
        words.push(move_d_to_d(3, 2)); // D2 = the object
        push_jsr(&mut words, 6, -234); // FreeDosObject(a6)
        words.push(move_d_to_d(3, 0)); // exit code = the object
        words.push(RTS);

        let mut rt = runtime_with_program_and_extra(&words, TRAP_TABLE_END, &[], None);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        (rt, code as u32)
    }

    #[test]
    fn end_to_end_alloc_dos_object_stdpkt_returns_the_linked_dos_packet() {
        let (mut rt, pkt) = alloc_dos_object_run(DOS_STDPKT);
        let sp = pkt - SP_PKT_OFFSET;
        assert_eq!(
            rt.heap_mut().size_of_live_alloc(sp),
            Some(STANDARD_PACKET_SIZE)
        );
        assert_eq!(
            rt.memory().read_u32(sp + LN_NAME_OFFSET),
            pkt,
            "ln_Name -> sp_Pkt"
        );
        assert_eq!(rt.memory().read_u32(pkt), sp, "dp_Link -> sp_Msg");
    }

    #[test]
    fn end_to_end_alloc_dos_object_cli_defaults() {
        let (mut rt, cli) = alloc_dos_object_run(DOS_CLI);
        assert_eq!(
            rt.heap_mut().size_of_live_alloc(cli),
            Some(crate::exectask::CLI_STRUCT_SIZE)
        );
        let mut buffers = Vec::new();
        for (offset, _) in CLI_BUFFERS {
            let buf = addr_from_bptr(rt.memory().read_u32(cli + offset));
            assert_eq!(
                rt.heap_mut().size_of_live_alloc(buf),
                Some(CLI_BUFFER_DEFAULT + 1),
                "buffer at CLI offset {offset}"
            );
            assert_eq!(
                rt.memory().read_u8(buf),
                0,
                "empty BSTR at CLI offset {offset}"
            );
            buffers.push(offset);
        }
        for i in (0..crate::exectask::CLI_STRUCT_SIZE).step_by(4) {
            let value = rt.memory().read_u32(cli + i);
            if i == crate::exectask::CLI_FAIL_LEVEL_OFFSET {
                assert_eq!(value, RETURN_ERROR);
            } else if i == crate::exectask::CLI_BACKGROUND_OFFSET {
                assert_eq!(value, DOSTRUE);
            } else if !buffers.contains(&i) {
                assert_eq!(value, 0, "CLI offset {i}");
            }
        }
    }

    #[test]
    fn end_to_end_alloc_dos_object_cli_sizes_buffers_from_tags() {
        // The tag list { ADO_PromptLen, 40 }, { TAG_DONE } on the stack.
        let mut words = Vec::new();
        for value in [0u32, 0, 40, ADO_PROMPT_LEN] {
            words.push(0x2F3C); // move.l #imm,-(sp)
            words.push((value >> 16) as u16);
            words.push(value as u16);
        }
        words.push(0x240F); // move.l sp,d2: the tags
        push_move_imm_to_d(&mut words, 1, DOS_CLI);
        push_jsr(&mut words, 6, -228); // AllocDosObject(a6)
        words.extend_from_slice(&[0x4FEF, 0x0010]); // lea 16(sp),sp
        words.push(RTS);

        let mut rt = runtime_with_program_and_extra(&words, TRAP_TABLE_END, &[], None);
        let mut out = Vec::new();
        let cli = rt.run(&mut out, None).expect("run should succeed") as u32;
        for (offset, tag) in CLI_BUFFERS {
            let buf = addr_from_bptr(rt.memory().read_u32(cli + offset));
            // 41 for the prompt, rounded up to a longword by the heap.
            let expected = if tag == ADO_PROMPT_LEN {
                44
            } else {
                CLI_BUFFER_DEFAULT + 1
            };
            assert_eq!(
                rt.heap_mut().size_of_live_alloc(buf),
                Some(expected),
                "buffer at CLI offset {offset}"
            );
        }
    }

    #[test]
    fn end_to_end_free_dos_object_frees_stdpkt_and_cli() {
        let (mut rt, pkt) = alloc_free_dos_object_run(DOS_STDPKT);
        assert_eq!(rt.heap_mut().size_of_live_alloc(pkt - SP_PKT_OFFSET), None);

        let (mut rt, cli) = alloc_free_dos_object_run(DOS_CLI);
        assert_eq!(rt.heap_mut().size_of_live_alloc(cli), None);
        for (offset, _) in CLI_BUFFERS {
            let buf = addr_from_bptr(rt.memory().read_u32(cli + offset));
            assert_eq!(
                rt.heap_mut().size_of_live_alloc(buf),
                None,
                "buffer at CLI offset {offset}"
            );
        }
    }

    #[test]
    fn end_to_end_alloc_dos_object_unknown_type_fails_loudly() {
        let mut words = vec![
            move_imm_to_d(1), // D1 = 6, not a type <dos/dos.h> defines
            0,
            6,
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
            .expect_err("an unknown AllocDosObject type should fail loudly");
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

    // --- OpenFromLock (issue #159) ---

    #[test]
    fn end_to_end_lock_then_open_from_lock_then_read_via_trap_dispatch() {
        let tmp = TempDir::new("e2e-openfromlock");
        fs::write(tmp.path().join("f.txt"), b"hello").unwrap();
        let name = b"SYS:f.txt\0";

        let mut words = Vec::new();
        let name_idx = words.len();
        push_move_imm_to_d(&mut words, 1, 0); // D1 = name (patched below)
        push_move_imm_to_d(&mut words, 2, crate::doslock::SHARED_LOCK as u32); // D2 = SHARED_LOCK
        push_jsr(&mut words, 6, -84); // Lock(a6): D0 = lock BPTR
        words.push(move_d0_to_d(1)); // D1 = lock (for OpenFromLock)
        push_jsr(&mut words, 6, -378); // OpenFromLock(a6): D0 = fh BPTR
        words.push(move_d0_to_d(1)); // D1 = fh (for Read)
        let buf_idx = push_move_imm_to_d(&mut words, 2, 0); // D2 = buffer (patched)
        push_move_imm_to_d(&mut words, 3, 5); // D3 = length
        push_jsr(&mut words, 6, -42); // Read(a6): D0 = bytes read or -1
        words.push(RTS);

        let name_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        let buf_addr = name_addr + name.len() as u32;
        patch_imm32(&mut words, name_idx, name_addr);
        patch_imm32(&mut words, buf_idx, buf_addr);

        let mut rt = runtime_with_program_and_extra(&words, name_addr, name, Some(tmp.path()));
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, 5,
            "Read should see all 5 bytes through the lock-opened handle"
        );
        let mut buf = [0u8; 5];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = rt.memory().read_u8(buf_addr + i as u32);
        }
        assert_eq!(&buf, b"hello");
    }

    #[test]
    fn end_to_end_open_from_lock_on_an_invalid_lock_returns_zero_in_d0() {
        let mut words = vec![
            move_imm_to_d(1), // D1 = a bogus lock BPTR, never allocated
            0,
            0x7FFF,
        ];
        words.extend_from_slice(&[jsr_disp16(6), (-378i16) as u16]); // OpenFromLock(a6)
        words.push(RTS);

        let mut rt = runtime_with_program_and_extra(&words, TRAP_TABLE_END, &[], None);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, 0,
            "OpenFromLock on an unknown lock should return 0 in D0"
        );
    }

    // --- LockRecord / UnLockRecord / LockRecords / UnLockRecords (issue #160) ---

    #[test]
    fn end_to_end_lock_record_then_unlock_record_via_trap_dispatch() {
        let tmp = TempDir::new("e2e-lockrecord");
        fs::write(tmp.path().join("f.txt"), b"hello").unwrap();
        let name = b"SYS:f.txt\0";

        let mut words = Vec::new();
        let name_idx = words.len();
        push_move_imm_to_d(&mut words, 1, 0); // D1 = name (patched)
        push_move_imm_to_d(&mut words, 2, MODE_OLDFILE as u32); // D2 = MODE_OLDFILE
        push_jsr(&mut words, 6, -30); // Open(a6): D0 = fh BPTR
        words.push(0x2200); // move.l d0,d1 (fh -> D1 for LockRecord)
        push_move_imm_to_d(&mut words, 2, 0); // D2 = offset
        push_move_imm_to_d(&mut words, 3, 0); // D3 = length
        push_move_imm_to_d(&mut words, 4, 0); // D4 = mode
        push_move_imm_to_d(&mut words, 5, 0); // D5 = timeout
        push_jsr(&mut words, 6, -270); // LockRecord(a6): D0 = DOSTRUE/DOSFALSE
        // LockRecord doesn't touch D1, so it's still the fh here.
        push_move_imm_to_d(&mut words, 2, 0); // D2 = offset
        push_move_imm_to_d(&mut words, 3, 0); // D3 = length
        push_jsr(&mut words, 6, -282); // UnLockRecord(a6): D0 = DOSTRUE/DOSFALSE
        words.push(RTS);

        let name_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        patch_imm32(&mut words, name_idx, name_addr);

        let mut rt = runtime_with_program_and_extra(&words, name_addr, name, Some(tmp.path()));
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, DOSTRUE as i32,
            "LockRecord then UnLockRecord on an open handle should both succeed"
        );
    }

    #[test]
    fn end_to_end_lock_record_on_an_unknown_handle_returns_dosfalse() {
        let mut words = vec![
            move_imm_to_d(1), // D1 = a bogus fh BPTR, never allocated
            0,
            0x7FFF,
        ];
        words.extend_from_slice(&[
            move_imm_to_d(2),
            0,
            0,
            move_imm_to_d(3),
            0,
            0,
            move_imm_to_d(4),
            0,
            0,
            move_imm_to_d(5),
            0,
            0,
        ]);
        words.extend_from_slice(&[jsr_disp16(6), (-270i16) as u16]); // LockRecord(a6)
        words.push(RTS);

        let mut rt = runtime_with_program_and_extra(&words, TRAP_TABLE_END, &[], None);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, DOSFALSE as i32);
    }

    #[test]
    fn end_to_end_unlock_record_on_an_unknown_handle_returns_dosfalse() {
        let mut words = vec![
            move_imm_to_d(1), // D1 = a bogus fh BPTR, never allocated
            0,
            0x7FFF,
        ];
        words.extend_from_slice(&[move_imm_to_d(2), 0, 0, move_imm_to_d(3), 0, 0]);
        words.extend_from_slice(&[jsr_disp16(6), (-282i16) as u16]); // UnLockRecord(a6)
        words.push(RTS);

        let mut rt = runtime_with_program_and_extra(&words, TRAP_TABLE_END, &[], None);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, DOSFALSE as i32);
    }

    #[test]
    fn end_to_end_lock_records_and_unlock_records_via_trap_dispatch() {
        let mut words = vec![
            move_imm_to_d(1), // D1 = recArray (arbitrary, never inspected)
            0,
            0x1234,
            move_imm_to_d(2), // D2 = timeout (arbitrary, never inspected)
            0,
            0,
        ];
        words.extend_from_slice(&[jsr_disp16(6), (-276i16) as u16]); // LockRecords(a6)
        words.push(0x2200); // move.l d0,d1 (save LockRecords' result for UnLockRecords' D1)
        words.extend_from_slice(&[jsr_disp16(6), (-288i16) as u16]); // UnLockRecords(a6)
        words.push(RTS);

        let mut rt = runtime_with_program_and_extra(&words, TRAP_TABLE_END, &[], None);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, DOSTRUE as i32,
            "LockRecords and UnLockRecords should both always succeed"
        );
    }
}
