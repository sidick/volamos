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
use crate::guestmem::{GuestHeap, addr_from_bptr, bptr_from_addr, read_c_string};
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
    /// A real host file opened via `Open`.
    HostFile(File),
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
    /// `FileHandle` struct.
    input_handle: Option<u32>,
    /// Guest address of the lazily-created `Output()` default handle's
    /// `FileHandle` struct.
    output_handle: Option<u32>,
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
            next_debug_id: 0,
            locks: HashMap::new(),
            exnext: HashMap::new(),
            current_dir_lock: None,
            initial_cwd: None,
            next_lock_id: 0,
            seglists: HashMap::new(),
            system_runner: None,
            cmdline: None,
            cmdline_pos: 0,
            rdargs: HashMap::new(),
            local_vars: HashMap::new(),
            anchor_states: HashMap::new(),
            ungetc_buf: HashMap::new(),
            last_getc: HashMap::new(),
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
        let path = vfs
            .resolve(name, resolve_mode)
            .map_err(|e| map_vfs_error(&e))?;

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
        self.handles.insert(addr, HostHandle::HostFile(file));
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
            Some(HostHandle::HostFile(f)) => {
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
            Some(HostHandle::HostFile(f)) => f.write(data).map_err(|e| map_io_error(&e)),
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
            Some(HostHandle::HostFile(f)) => {
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

    /// `Input()`: returns the guest address of the (lazily-created)
    /// default input `FileHandle`, or an `IoErr()` code if the guest
    /// heap has no room to create it (extremely unlikely; the struct is
    /// 44 bytes).
    pub fn input_addr(
        &mut self,
        heap: &mut GuestHeap,
        mem: &mut dyn AddressSpace,
    ) -> Result<u32, i32> {
        if let Some(addr) = self.input_handle {
            return Ok(addr);
        }
        let id = self.next_id();
        let addr = alloc_file_handle(heap, mem, id).map_err(|_| ERROR_NO_FREE_STORE)?;
        self.handles.insert(addr, HostHandle::Stdin);
        self.input_handle = Some(addr);
        Ok(addr)
    }

    /// `Output()`: as [`Self::input_addr`], for the default output
    /// handle.
    pub fn output_addr(
        &mut self,
        heap: &mut GuestHeap,
        mem: &mut dyn AddressSpace,
    ) -> Result<u32, i32> {
        if let Some(addr) = self.output_handle {
            return Ok(addr);
        }
        let id = self.next_id();
        let addr = alloc_file_handle(heap, mem, id).map_err(|_| ERROR_NO_FREE_STORE)?;
        self.handles.insert(addr, HostHandle::Stdout);
        self.output_handle = Some(addr);
        Ok(addr)
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
/// Always writes to `Output()`, which -- per this runtime's convention
/// (see the module docs) -- means `ctx.out` directly, exactly like
/// Phase 1's hand-registered `PutStr` did, so output still lands
/// wherever the caller of `Runtime::run` pointed it.
fn putstr_via_output_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let ptr = ctx.cpu.data_register(DataRegister(1));
    let bytes = read_c_string(ctx.mem, ptr);
    match ctx.out.write_all(&bytes) {
        Ok(()) => ctx.cpu.set_data_register(DataRegister(0), 0),
        Err(e) => {
            ctx.dos.set_io_err(map_io_error(&e));
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
    reg!("Read", read_handler::<C>);
    reg!("Write", write_handler::<C>);
    reg!("Seek", seek_handler::<C>);
    reg!("Input", input_handler::<C>);
    reg!("Output", output_handler::<C>);
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
