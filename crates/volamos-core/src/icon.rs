//! `icon.library`: reading `.info` icon files into guest `DiskObject`s,
//! writing them back, the free-list calls, and the pure string helpers
//! -- the 3.1 (V40) interface, plus `BumpRevisionLength`.
//!
//! # Why a partial library
//!
//! `icon.library` is ROM-resident on 3.x, so it has to open (see
//! [`crate::dispatch::STANDARD_WORKBENCH_LIBRARIES`]: the real
//! `BindDrivers` dies on a `NULL` base). It first opened as an
//! auto-created fake, so the *first* call into it aborted the run --
//! including `GetDiskObject` on a path with no icon, which a real
//! program makes routinely and expects to answer `NULL` with
//! `IoErr()` = `ERROR_OBJECT_NOT_FOUND` (the MicroPython Amiga port's
//! `_icon.read` is one).
//!
//! # What is here
//!
//! The file format is [`crate::iconfile`]; the guest structures
//! (`DiskObject`, `FreeList`, and everything they point at) are
//! [`crate::iconobj`]. This module is the calls:
//!
//! - `GetDiskObject`: `name.info` parsed and built in guest memory; a
//!   missing file is `NULL` + `IoErr()` from resolving the path (the
//!   autodoc: "If the call fails, it will return NULL. The reason for
//!   the failure may be obtained via IoErr()"); a file that is not an
//!   icon, or is cut short, is `NULL` with `IoErr()` `0` (what real
//!   Kickstart 3.1 does); a directory where the file should be is
//!   `ERROR_OBJECT_WRONG_TYPE`; a full heap is `ERROR_NO_FREE_STORE`. A
//!   `NULL` name allocates an all-zero object
//!   ([`DiskObjectFile::empty`]), "useful when calling `AddAppIcon()`".
//!   A volume's icon is `SYS:.info` (the name plus `.info`, no special
//!   `Disk` name).
//! - `GetDefDiskObject`: `ENV:Sys/def_<type>.info` the same way, and a
//!   **built-in default** ([`DiskObjectFile::builtin_default`]) when
//!   that file is missing, as real Kickstart 3.1 does (the ROM icon's
//!   gadget geometry, drawer window and default tool; blank pictures of
//!   the real size). A default file that exists but is not an icon is
//!   an error.
//! - `GetDiskObjectNew`: `GetDiskObject`, and where there is no `.info`
//!   file the default for the object's type -- a volume root is
//!   `WBDISK`, another directory `WBDRAWER`, a file `WBTOOL` unless its
//!   execute bit is denied, when it is `WBPROJECT` (real 3.1's test,
//!   measured: a plain file with no protection bits is a tool). An
//!   object that is not there at all keeps the original failure.
//! - `FreeDiskObject`, `FreeFreeList`, `AddFreeList`: the autodoc's
//!   `FreeList` just after the `DiskObject`; see [`crate::iconobj`].
//!   `NULL` is ignored. A pointer that is not such an object fails the
//!   run naming the call, since freeing it would corrupt the guest
//!   heap.
//! - `PutDiskObject`: reads the guest `DiskObject` back through
//!   [`crate::iconobj::read_disk_object`] and writes `name.info`
//!   (`SYS:.info` for a volume), replacing any file there; the
//!   parent directory must exist. A pointer that does not lead to a
//!   readable `DiskObject` fails the run naming the call. The file is
//!   replaced atomically. Only the classic part is written: a
//!   ColorIcon/PNG icon read and put back loses its extra picture
//!   data (the guest `DiskObject` has nowhere to carry it). Workbench
//!   is not notified: there is none.
//! - `PutDefDiskObject`: the same for `ENV:Sys/def_<type>.info`, where
//!   `GetDefDiskObject` finds it; `ENV:Sys` is created if missing.
//!   `ENVARC:` is not written.
//! - `DeleteDiskObject`: deletes `name.info` exactly as `DeleteFile`
//!   would, failures and all.
//! - `FindToolType`, `MatchToolValue`, `BumpRevision`: pure string work
//!   over guest memory, implemented in full to their V40 autodocs.
//!
//! Signatures and LVOs are from the NDK 3.2 autodocs (`icon.doc`) and
//! [`crate::lvos::icon`]; `DiskObject` type codes from
//! `workbench/workbench.h`.

use crate::cpu::{AddressRegister, Cpu, DataRegister};
use crate::dispatch::{DispatchError, HandlerContext, ICON_LIBRARY_BASE, LibraryTable};
use crate::dosfile::map_vfs_error;
use crate::guestmem::read_c_string;
use crate::iconfile::{DiskObjectFile, WBDISK, WBDRAWER, WBPROJECT, WBTOOL};
use crate::lvos::icon::ICON_LVOS;
use crate::memory::AddressSpace;
use crate::utility::amiga_tolower;
use crate::vfs::ResolveMode;

/// `exec/types.h`'s `TRUE`/`FALSE`: the autodocs give these calls a C
/// `BOOL` result, not `dos.library`'s `DOSTRUE` (`-1`). Callers test for
/// non-zero either way.
const TRUE: u32 = 1;
const FALSE: u32 = 0;

/// `workbench/workbench.h`'s `WBDISK`..`WBKICK` paired with the
/// `ENV:Sys/def_<name>.info` file each default icon lives in on a real
/// Workbench install. `WBDEVICE` (6) is absent on purpose: the
/// autodoc notes V36 through V40 "did not support the WBDEVICE default
/// icon type".
const DEFAULT_ICON_NAMES: [(u32, &str); 6] = [
    (1, "disk"),
    (2, "drawer"),
    (3, "tool"),
    (4, "project"),
    (5, "trashcan"),
    (7, "kick"),
];

/// The longest name `BumpRevision` produces: "the maximum dos name size
/// (currently 30 characters)", so `newbuf` must hold 31 bytes.
const BUMP_REVISION_MAX: usize = 30;

/// The NUL-terminated guest string at `addr`, as (lossy) UTF-8.
fn guest_string(mem: &dyn AddressSpace, addr: u32) -> String {
    String::from_utf8_lossy(&read_c_string(mem, addr)).into_owned()
}

/// Whether `err` means the icon (or the directory it would be in) is
/// simply not there, as opposed to being there and unreadable.
fn is_missing(err: i32) -> bool {
    err == crate::dosfile::ERROR_OBJECT_NOT_FOUND || err == crate::dosfile::ERROR_DIR_NOT_FOUND
}

/// The icon file for the object `name`: `name.info`, always -- a volume
/// `SYS:` is `SYS:.info`, as real Kickstart 3.1 writes it (Workbench's own
/// disk icon is `GetDiskObject("SYS:Disk")`, a plain name).
fn info_path(name: &str) -> String {
    format!("{name}.info")
}

/// `IoErr()` after a file that is not an icon (junk, empty, cut short):
/// real Kickstart 3.1 answers `NULL` and leaves `0` there (measured on
/// icon.library 40.1, phase 0 of #313), not an `ERROR_` code.
const BAD_ICON_IO_ERR: i32 = 0;

/// The largest `.info` file read: far beyond any real icon (a big
/// `ColorIcon` is some hundreds of KiB), small enough that a stray huge
/// file named `*.info` is refused, not read into host memory.
const MAX_INFO_FILE_SIZE: u64 = 64 << 20;

/// Reads and parses the icon file at `path` (a full Amiga path, `.info`
/// included), or says what `IoErr()` should be: a file that is not an
/// icon, or is cut short, is [`BAD_ICON_IO_ERR`]; a directory (or a file
/// too big to be an icon) in its place is `ERROR_OBJECT_WRONG_TYPE`, as
/// `Open` gives.
fn load_icon(ctx: &HandlerContext<'_, impl Cpu>, path: &str) -> Result<DiskObjectFile, i32> {
    let vfs = ctx
        .dos
        .vfs
        .as_ref()
        .ok_or(crate::dosfile::ERROR_OBJECT_NOT_FOUND)?;
    let host = vfs
        .resolve(path, ResolveMode::MustExist)
        .map_err(|e| map_vfs_error(&e))?;
    let meta = std::fs::metadata(&host).map_err(|e| crate::dosfile::map_io_error(&e))?;
    if !meta.is_file() || meta.len() > MAX_INFO_FILE_SIZE {
        return Err(crate::dosfile::ERROR_OBJECT_WRONG_TYPE);
    }
    let bytes = std::fs::read(host).map_err(|e| crate::dosfile::map_io_error(&e))?;
    DiskObjectFile::parse(&bytes)
        .map(|(icon, _)| icon)
        .map_err(|_| BAD_ICON_IO_ERR)
}

/// Builds `icon` in guest memory and sets `D0` to it, or to `NULL` with
/// `IoErr()` = `ERROR_NO_FREE_STORE` when the heap is full.
fn deliver<C: Cpu>(ctx: &mut HandlerContext<'_, C>, icon: &DiskObjectFile) {
    match crate::iconobj::build_disk_object(ctx.heap, ctx.mem, icon) {
        Ok(obj) => ctx.cpu.set_data_register(DataRegister(0), obj),
        Err(crate::iconobj::NoMemory) => {
            ctx.dos.set_io_err(crate::dosfile::ERROR_NO_FREE_STORE);
            ctx.cpu.set_data_register(DataRegister(0), 0);
        }
    }
}

/// The object `GetDiskObject(NULL)` allocates: all zeros, magic included
/// ([`DiskObjectFile::empty`]), with a `FreeList` so `FreeDiskObject`
/// works on it.
fn deliver_empty<C: Cpu>(ctx: &mut HandlerContext<'_, C>) {
    deliver(ctx, &DiskObjectFile::empty());
    let obj = ctx.cpu.data_register(DataRegister(0));
    if obj != 0 {
        ctx.mem.write_u16(obj, 0);
    }
}

/// `NULL` in `D0` with `IoErr()` set to `code`.
fn fail<C: Cpu>(ctx: &mut HandlerContext<'_, C>, code: i32) {
    ctx.dos.set_io_err(code);
    ctx.cpu.set_data_register(DataRegister(0), 0);
}

/// `GetDiskObject` (`A0` = name). The `DiskObject` for `name.info`, with
/// its `FreeList` (see [`crate::iconobj`]); `NULL` + `IoErr()` when the
/// file is missing or not an icon. A `NULL` name allocates an empty
/// object ([`DiskObjectFile::empty`]).
fn get_disk_object_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let name_ptr = ctx.cpu.address_register(AddressRegister(0));
    if name_ptr == 0 {
        deliver_empty(ctx);
        return Ok(());
    }
    let info = info_path(&guest_string(ctx.mem, name_ptr));
    match load_icon(ctx, &info) {
        Ok(icon) => deliver(ctx, &icon),
        Err(code) => fail(ctx, code),
    }
    Ok(())
}

/// The `def_type` for an object that has no icon file of its own, for
/// `GetDiskObjectNew`: a volume root is a disk, any other directory a
/// drawer, a file a tool unless its execute bit is denied (`Protect
/// +e`... that is, `FIBF_EXECUTE` set), when it is a project. That last
/// test is what real Kickstart 3.1 does (icon.library 40.1, measured on
/// Copperline, phase 0 of #313): a plain file with no protection bits is a
/// *tool*, whatever it contains. Host files have no protection bits of
/// their own; the `.uaem` sidecar's ([`crate::dosmeta`]) are used, `0`
/// without one. `None` when `name` is not there at all.
fn default_type_for(ctx: &HandlerContext<'_, impl Cpu>, name: &str) -> Option<u8> {
    /// `FIBF_EXECUTE`: set means the file may *not* be executed.
    const FIBF_EXECUTE: u32 = 1 << 1;
    let vfs = ctx.dos.vfs.as_ref()?;
    let host = vfs.resolve(name, ResolveMode::MustExist).ok()?;
    if host.is_dir() {
        // A volume root, not an assign to a subdirectory (`LIBS:`).
        let is_root = name.strip_suffix(':').is_some_and(|v| {
            vfs.volumes()
                .iter()
                .any(|(n, _)| n.trim_end_matches(':').eq_ignore_ascii_case(v))
        });
        return Some(if is_root { WBDISK } else { WBDRAWER });
    }
    let protection = crate::dosmeta::read_sidecar(&host).map_or(0, |meta| meta.prot);
    Some(if protection & FIBF_EXECUTE == 0 {
        WBTOOL
    } else {
        WBPROJECT
    })
}

/// `GetDiskObjectNew` (`A0` = name): `GetDiskObject`, and where there is
/// no `.info` file, the default icon for the object's type -- "useful when there
/// is no .info file for the icon you are trying to get a disk object
/// for". If the object itself is not there, the original failure stands.
fn get_disk_object_new_handler<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
) -> Result<(), DispatchError> {
    let name_ptr = ctx.cpu.address_register(AddressRegister(0));
    if name_ptr == 0 {
        deliver_empty(ctx);
        return Ok(());
    }
    let name = guest_string(ctx.mem, name_ptr);
    match load_icon(ctx, &info_path(&name)) {
        Ok(icon) => deliver(ctx, &icon),
        // Only a missing icon is replaced by a default: a corrupt or
        // unreadable one is the caller's to hear about, not to overwrite.
        Err(code) if !is_missing(code) => fail(ctx, code),
        Err(code) => match default_type_for(ctx, &name) {
            Some(def_type) => match default_icon(
                ctx,
                def_type as u32,
                default_icon_name(def_type as u32).expect("a type default_type_for returns"),
            ) {
                Ok(icon) => deliver(ctx, &icon),
                Err(code) => fail(ctx, code),
            },
            None => fail(ctx, code),
        },
    }
    Ok(())
}

/// The default icon of `def_type` (one of [`DEFAULT_ICON_NAMES`]'s
/// types): `ENV:Sys/def_<type>.info` if there is one, else the built-in
/// ([`DiskObjectFile::builtin_default`]), which is what real Kickstart
/// 3.1 does too (icon.library 40.1, measured on Copperline, phase 0 of
/// #313). An `ENV:` file that exists but is not an icon is an error, not
/// a reason to quietly use the built-in. Both `GetDefDiskObject` and
/// `GetDiskObjectNew` answer through this, so a bad file gets the same
/// outcome from each.
fn default_icon(
    ctx: &HandlerContext<'_, impl Cpu>,
    def_type: u32,
    name: &str,
) -> Result<DiskObjectFile, i32> {
    match load_icon(ctx, &format!("ENV:Sys/def_{name}.info")) {
        Err(code) if is_missing(code) => Ok(DiskObjectFile::builtin_default(def_type as u8)),
        other => other,
    }
}

/// `def_type`'s entry in [`DEFAULT_ICON_NAMES`].
fn default_icon_name(def_type: u32) -> Option<&'static str> {
    DEFAULT_ICON_NAMES
        .iter()
        .find(|(t, _)| *t == def_type)
        .map(|&(_, name)| name)
}

/// `GetDefDiskObject` (`D0` = `WBDISK`..`WBKICK`). `ENV:Sys/def_<type>.info`
/// if it exists, else the built-in default of that type -- as real
/// Kickstart 3.1 answers (icon.library 40.1, phase 0 of #313). A type
/// 3.1 has no default for is `NULL`: `WBDEVICE` with `IoErr()` =
/// `ERROR_OBJECT_NOT_FOUND`, and `WBAPPICON` or anything out of range
/// with `IoErr()` left as it was.
fn get_def_disk_object_handler<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
) -> Result<(), DispatchError> {
    /// `WBDEVICE`: in 3.1's type range, but with no default icon.
    const WBDEVICE_TYPE: u32 = 6;
    let def_type = ctx.cpu.data_register(DataRegister(0));
    let Some(name) = default_icon_name(def_type) else {
        if def_type == WBDEVICE_TYPE {
            fail(ctx, crate::dosfile::ERROR_OBJECT_NOT_FOUND);
        } else {
            ctx.cpu.set_data_register(DataRegister(0), 0);
        }
        return Ok(());
    };
    match default_icon(ctx, def_type, name) {
        Ok(icon) => deliver(ctx, &icon),
        Err(code) => fail(ctx, code),
    }
    Ok(())
}

/// Writes `icon` to the `.info` file at `path` (a full Amiga path,
/// `.info` included), creating or replacing it. With `make_dir`, a
/// missing last directory of the path is created first (for
/// `ENV:Sys`); otherwise the parent must exist, as for `Open`.
fn store_icon(
    ctx: &HandlerContext<'_, impl Cpu>,
    path: &str,
    icon: &DiskObjectFile,
    make_dir: bool,
) -> Result<(), i32> {
    let vfs = ctx
        .dos
        .vfs
        .as_ref()
        .ok_or(crate::dosfile::ERROR_OBJECT_NOT_FOUND)?;
    if make_dir && let Some((dir, _)) = path.rsplit_once('/') {
        let dir = vfs
            .resolve(dir, ResolveMode::ParentMustExist)
            .map_err(|e| map_vfs_error(&e))?;
        if !dir.exists() {
            std::fs::create_dir(&dir).map_err(|e| crate::dosfile::map_io_error(&e))?;
        }
    }
    let host = vfs
        .resolve(path, ResolveMode::ParentMustExist)
        .map_err(|e| map_vfs_error(&e))?;
    // Write beside the target and rename over it, so a failed or
    // interrupted write leaves the old icon intact rather than truncated.
    // A symlinked target is written through, not replaced by a file; the
    // temp name is short and fixed so a long legal name cannot overflow
    // the host's limit with a suffix.
    let host = std::fs::canonicalize(&host).unwrap_or(host);
    let tmp = host.with_file_name(".volamos-icon-tmp");
    std::fs::write(&tmp, icon.to_bytes())
        .and_then(|()| std::fs::rename(&tmp, &host))
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            crate::dosfile::map_io_error(&e)
        })
}

/// The guest `DiskObject` at `obj`, or the run failing in `handler`: a
/// pointer that does not lead to a `DiskObject` is a guest bug, and
/// writing guesswork to disk would bury it.
fn read_object<C: Cpu>(
    ctx: &HandlerContext<'_, C>,
    handler: &str,
    obj: u32,
) -> Result<DiskObjectFile, DispatchError> {
    crate::iconobj::read_disk_object(ctx.mem, obj).map_err(|e| {
        handler_failed(
            handler,
            format!("{obj:#010x} is not a readable DiskObject: {e}"),
        )
    })
}

/// `D0` = `TRUE`, or `FALSE` with `IoErr()` = the code.
fn finish_put<C: Cpu>(ctx: &mut HandlerContext<'_, C>, result: Result<(), i32>) {
    match result {
        Ok(()) => ctx.cpu.set_data_register(DataRegister(0), TRUE),
        Err(code) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), FALSE);
        }
    }
}

/// `PutDiskObject` (`A0` = name, `A1` = object): writes `name.info`
/// (`SYS:.info` for a volume) from the
/// guest `DiskObject`, replacing any file there. Workbench is not
/// notified: there is none.
fn put_disk_object_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let name_ptr = ctx.cpu.address_register(AddressRegister(0));
    let obj = ctx.cpu.address_register(AddressRegister(1));
    if name_ptr == 0 {
        return Err(handler_failed("PutDiskObject", "name is NULL".to_string()));
    }
    let icon = read_object(ctx, "PutDiskObject", obj)?;
    let path = info_path(&guest_string(ctx.mem, name_ptr));
    let result = store_icon(ctx, &path, &icon, false);
    finish_put(ctx, result);
    Ok(())
}

/// `PutDefDiskObject` (`A0` = object): writes
/// `ENV:Sys/def_<type>.info` for the object's `do_Type`, creating
/// `ENV:Sys` if needed -- where `GetDefDiskObject` reads it back. A type
/// with no default (`WBDEVICE`, `WBAPPICON`, anything else) is `FALSE`
/// plus `ERROR_BAD_NUMBER`, matching `GetDefDiskObject`. `ENVARC:` is not
/// written; whether real V40 does is unverified (phase 0 of #313).
fn put_def_disk_object_handler<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
) -> Result<(), DispatchError> {
    let obj = ctx.cpu.address_register(AddressRegister(0));
    let icon = read_object(ctx, "PutDefDiskObject", obj)?;
    let result = match DEFAULT_ICON_NAMES
        .iter()
        .find(|(t, _)| *t == icon.object_type as u32)
    {
        Some(&(_, name)) => store_icon(ctx, &format!("ENV:Sys/def_{name}.info"), &icon, true),
        None => Err(crate::dosargs::ERROR_BAD_NUMBER),
    };
    finish_put(ctx, result);
    Ok(())
}

/// `DeleteDiskObject` (`A0` = name). Deletes `name.info` as `DeleteFile`
/// does: `D0` = `TRUE`, or `FALSE` + `IoErr()`.
fn delete_disk_object_handler<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
) -> Result<(), DispatchError> {
    let name_ptr = ctx.cpu.address_register(AddressRegister(0));
    if name_ptr == 0 {
        return Err(handler_failed(
            "DeleteDiskObject",
            "name is NULL".to_string(),
        ));
    }
    let info = info_path(&guest_string(ctx.mem, name_ptr));
    match ctx.dos.delete_file(&info) {
        Ok(()) => ctx.cpu.set_data_register(DataRegister(0), TRUE),
        Err(code) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), FALSE);
        }
    }
    Ok(())
}

/// A guest-visible failure of `handler`, as the call that hit it.
fn handler_failed(handler: &str, message: String) -> DispatchError {
    let lvo = crate::lvos::find_by_name(ICON_LVOS, handler)
        .expect("in the table")
        .lvo;
    DispatchError::HandlerFailed {
        library: "icon.library".to_string(),
        lvo,
        handler_name: handler.to_string(),
        message,
    }
}

/// `FreeDiskObject` (`A0` = object): `FreeFreeList` on the `FreeList`
/// just after the `DiskObject`. `NULL` is ignored (V44+; harmless
/// earlier). A pointer that was not returned by `GetDiskObject` and
/// friends fails the run, naming the call: freeing it would corrupt the
/// guest heap.
fn free_disk_object_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let obj = ctx.cpu.address_register(AddressRegister(0));
    if obj == 0 {
        return Ok(());
    }
    crate::iconobj::free_disk_object(ctx.heap, ctx.mem, obj)
        .map(|_| ())
        .map_err(|e| {
            handler_failed(
                "FreeDiskObject",
                format!("{obj:#010x} is not a DiskObject: {e}"),
            )
        })
}

/// `AddFreeList` (`A0` = free list, `A1` = memory, `A2` = length): `D0` =
/// `TRUE`, or `FALSE` when the heap has no room for the bookkeeping.
fn add_free_list_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let list = ctx.cpu.address_register(AddressRegister(0));
    let addr = ctx.cpu.address_register(AddressRegister(1));
    let len = ctx.cpu.address_register(AddressRegister(2));
    let ok = crate::iconobj::add_free_list(ctx.heap, ctx.mem, list, addr, len);
    ctx.cpu
        .set_data_register(DataRegister(0), if ok { TRUE } else { FALSE });
    Ok(())
}

/// `FreeFreeList` (`A0` = free list): frees everything recorded in it and
/// the list itself, if it is in the first block. `NULL` is ignored.
fn free_free_list_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let list = ctx.cpu.address_register(AddressRegister(0));
    if list == 0 {
        return Ok(());
    }
    crate::iconobj::free_free_list(ctx.heap, ctx.mem, list)
        .map(|_| ())
        .map_err(|e| handler_failed("FreeFreeList", format!("{list:#010x}: {e}")))
}

/// Whether `entry` starts with `name`, compared as `icon.library` does
/// (case-insensitively), followed by `=` or the end of the string.
/// Returns the offset of the value: past the `=`, or at the `NUL` for a
/// bare flag entry, so `FindToolType(tt, "DONOTWAIT")` is non-`NULL` for
/// a `DONOTWAIT` entry -- the idiom flag tool types rely on.
fn tool_type_value(entry: &[u8], name: &[u8]) -> Option<usize> {
    if entry.len() < name.len()
        || !entry[..name.len()]
            .iter()
            .zip(name)
            .all(|(&a, &b)| amiga_tolower(a) == amiga_tolower(b))
    {
        return None;
    }
    match entry.get(name.len()) {
        None => Some(name.len()),
        Some(b'=') => Some(name.len() + 1),
        Some(_) => None,
    }
}

/// `FindToolType` (`A0` = `NULL`-terminated `STRPTR` array, `A1` =
/// name). `D0` = a pointer into the matching entry, just past
/// `NAME=` (not a copy), or `NULL`. An empty or `NULL` array finds
/// nothing. V44's tolerance of blanks around `=` is deliberately not
/// implemented: the autodoc notes older versions "did not support this".
fn find_tool_type_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let mut array = ctx.cpu.address_register(AddressRegister(0));
    let name = read_c_string(ctx.mem, ctx.cpu.address_register(AddressRegister(1)));
    let mut found = 0;
    if array != 0 {
        loop {
            let entry_ptr = ctx.mem.read_u32(array);
            if entry_ptr == 0 {
                break;
            }
            let entry = read_c_string(ctx.mem, entry_ptr);
            if let Some(offset) = tool_type_value(&entry, &name) {
                found = entry_ptr + offset as u32;
                break;
            }
            array += 4;
        }
    }
    ctx.cpu.set_data_register(DataRegister(0), found);
    Ok(())
}

/// Whether `value` equals one of `type_string`'s `|`-separated
/// alternatives, case-insensitively -- so a `value` that itself contains
/// `|` never matches, as the autodoc's `"a|b"` example shows. V44's
/// skipping of blanks around alternatives isn't implemented, as for
/// [`tool_type_value`].
fn match_tool_value(type_string: &[u8], value: &[u8]) -> bool {
    type_string.split(|&b| b == b'|').any(|alt| {
        alt.len() == value.len()
            && alt
                .iter()
                .zip(value)
                .all(|(&a, &b)| amiga_tolower(a) == amiga_tolower(b))
    })
}

/// `MatchToolValue` (`A0` = type string, `A1` = value). `D0` =
/// `TRUE`/`FALSE`.
fn match_tool_value_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let type_string = read_c_string(ctx.mem, ctx.cpu.address_register(AddressRegister(0)));
    let value = read_c_string(ctx.mem, ctx.cpu.address_register(AddressRegister(1)));
    let result = if match_tool_value(&type_string, &value) {
        TRUE
    } else {
        FALSE
    };
    ctx.cpu.set_data_register(DataRegister(0), result);
    Ok(())
}

/// Which library's `BumpRevision` naming rules.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BumpStyle {
    /// Kickstart 3.1's `BumpRevision` (V40).
    V40,
    /// `BumpRevisionLength` as 3.2's `icon.library` 47.5 does it.
    V47,
}

/// `old` without its first `prefix.len()` bytes, if they spell `prefix`
/// in any case.
fn strip_prefix_ci<'a>(old: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    old.get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| &old[prefix.len()..])
}

/// The new name for `old`, truncated to `max` bytes, from real
/// `icon.library` output (Copperline, phase 0 of #313) -- not the
/// autodoc's examples, which spell the prefix `copy_` where both
/// libraries write `Copy_`.
///
/// Common to both: `Copy_of_<old>`, or `Copy_<n+1>_of_<rest>` when `old`
/// is already `copy_of_<rest>` (counting as one) or `copy_<n>_of_<rest>`
/// with `n` all digits, the match ignoring case.
///
/// - **V40** (`BumpRevision`): `copy_0_of_foo` -> `Copy_1_of_foo`; any
///   other `copy...` name, `copy foo` and `copy_x_of_foo` included, is
///   a plain name: `Copy_of_copy foo`.
/// - **V47** (`BumpRevisionLength`): a new count of one is written
///   `Copy_of_` (`copy_0_of_foo` -> `Copy_of_foo`), and a name starting
///   `copy_` or `copy ` has those five bytes dropped and counts as a
///   first copy: `copy foo` -> `Copy_2_of_foo`, `copy_x_of_foo` ->
///   `Copy_2_of_x_of_foo`. It returns `None` unless `max` exceeds the
///   room the prefix can need: 8 for `Copy_of_`, 19 for a numbered one
///   whatever the actual number (so `Copy_2_of_foo` is refused at 19,
///   though it is only 13 bytes long). V40 never returns `None`.
fn bump_revision(old: &[u8], max: usize, style: BumpStyle) -> Option<Vec<u8>> {
    let counted = strip_prefix_ci(old, b"copy_of_")
        .map(|rest| (1u64, rest))
        .or_else(|| {
            let tail = strip_prefix_ci(old, b"copy_")?;
            let digits = tail.iter().take_while(|b| b.is_ascii_digit()).count();
            let rest = strip_prefix_ci(&tail[digits..], b"_of_")?;
            let n = std::str::from_utf8(&tail[..digits]).ok()?.parse().ok()?;
            Some((n, rest))
        })
        .or_else(|| {
            let dropped = old.get(..5).filter(|h| h[4] == b'_' || h[4] == b' ')?;
            (style == BumpStyle::V47 && dropped[..4].eq_ignore_ascii_case(b"copy"))
                .then(|| (1u64, &old[5..]))
        });
    let (count, rest) = match counted {
        Some((n, rest)) => (Some(n.saturating_add(1)), rest),
        None => (None, old),
    };
    let numbered = match (style, count) {
        (BumpStyle::V47, Some(1)) | (_, None) => None,
        (_, n) => n,
    };
    let mut new = match numbered {
        Some(n) => format!("Copy_{n}_of_").into_bytes(),
        None => b"Copy_of_".to_vec(),
    };
    if style == BumpStyle::V47 {
        let room = if numbered.is_some() { 19 } else { new.len() };
        if max <= room {
            return None;
        }
    }
    new.extend_from_slice(rest);
    new.truncate(max);
    Some(new)
}

/// `BumpRevision` (`A0` = new buffer, at least 31 bytes; `A1` = old
/// name). `D0` = the new buffer.
fn bump_revision_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let new_buf = ctx.cpu.address_register(AddressRegister(0));
    let old = read_c_string(ctx.mem, ctx.cpu.address_register(AddressRegister(1)));
    let new = bump_revision(&old, BUMP_REVISION_MAX, BumpStyle::V40).expect("V40 never refuses");
    crate::guestmem::write_c_string(ctx.mem, new_buf, &new);
    ctx.cpu.set_data_register(DataRegister(0), new_buf);
    Ok(())
}

/// `BumpRevisionLength` (`A0` = new buffer, `A1` = old name, `D0` =
/// length): `BumpRevision` with a name limit of `length` bytes (not
/// counting the NUL: `length` 9 gives `Copy_of_f`) instead of 30, so
/// copies of icons with long file names can be named. `D0` = the new
/// buffer, or `NULL` when `length` is too small for the prefix -- see
/// [`bump_revision`] for the exact rule, measured on `icon.library`
/// 47.5 (3.2); a real 3.1.4 (V45) was not available to compare.
fn bump_revision_length_handler<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
) -> Result<(), DispatchError> {
    let new_buf = ctx.cpu.address_register(AddressRegister(0));
    let old = read_c_string(ctx.mem, ctx.cpu.address_register(AddressRegister(1)));
    let max = ctx.cpu.data_register(DataRegister(0)) as usize;
    match bump_revision(&old, max, BumpStyle::V47) {
        Some(new) => {
            crate::guestmem::write_c_string(ctx.mem, new_buf, &new);
            ctx.cpu.set_data_register(DataRegister(0), new_buf);
        }
        None => ctx.cpu.set_data_register(DataRegister(0), 0),
    }
    Ok(())
}

/// Registers this module's `icon.library` handlers, looked up by name
/// through [`ICON_LVOS`], following
/// [`crate::locale::register_locale_handlers`]'s pattern. Unregistered
/// vectors fail loudly by name through the jump table's prefill.
pub fn register_icon_handlers<C: Cpu + 'static>(table: &mut LibraryTable<C>, mem: &mut C::Memory) {
    macro_rules! reg {
        ($name:literal, $handler:expr) => {
            table
                .register_by_name(
                    mem,
                    ICON_LIBRARY_BASE,
                    ICON_LVOS,
                    "icon.library",
                    $name,
                    $handler,
                )
                .unwrap_or_else(|e| panic!("{} should be in ICON_LVOS: {e}", $name));
        };
    }
    reg!("GetDiskObject", get_disk_object_handler::<C>);
    reg!("GetDefDiskObject", get_def_disk_object_handler::<C>);
    reg!("DeleteDiskObject", delete_disk_object_handler::<C>);
    reg!("GetDiskObjectNew", get_disk_object_new_handler::<C>);
    reg!("FreeDiskObject", free_disk_object_handler::<C>);
    reg!("AddFreeList", add_free_list_handler::<C>);
    reg!("FreeFreeList", free_free_list_handler::<C>);
    reg!("PutDiskObject", put_disk_object_handler::<C>);
    reg!("PutDefDiskObject", put_def_disk_object_handler::<C>);
    reg!("FindToolType", find_tool_type_handler::<C>);
    reg!("MatchToolValue", match_tool_value_handler::<C>);
    reg!("BumpRevision", bump_revision_handler::<C>);
    reg!("BumpRevisionLength", bump_revision_length_handler::<C>);
}

#[cfg(test)]
#[allow(clippy::vec_init_then_push)]
mod tests {
    use super::*;
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{DOS_LIBRARY_BASE, Runtime, RuntimeError, StartConfig};
    use crate::memory::FlatMemory;
    use crate::vfs::{Vfs, VfsConfig};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let path = std::env::temp_dir().join(format!("volamos-icon-test-{tag}-{pid}-{n}"));
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

    /// Guest addresses the tests below put data at, well clear of the
    /// code at [`TRAP_TABLE_END`].
    const DATA: u32 = 0x3_0000;
    const RESULT: u32 = 0x3_F000;

    fn push_imm_a(words: &mut Vec<u16>, n: u16, imm: u32) {
        words.push(0x207C | (n << 9));
        words.push((imm >> 16) as u16);
        words.push(imm as u16);
    }
    fn push_imm_d(words: &mut Vec<u16>, n: u16, imm: u32) {
        words.push(0x203C | (n << 9));
        words.push((imm >> 16) as u16);
        words.push(imm as u16);
    }
    fn push_jsr_a6(words: &mut Vec<u16>, lvo: &str) {
        let entry = crate::lvos::find_by_name(ICON_LVOS, lvo).expect("in table");
        words.extend_from_slice(&[0x4EAE, entry.lvo as u16]);
    }
    /// `move.l d0,RESULT` then `IoErr()`, so the run's exit code is
    /// `IoErr()` and [`RESULT`] holds the icon call's own `D0`.
    fn push_store_d0_then_io_err(words: &mut Vec<u16>) {
        words.extend_from_slice(&[0x23C0, (RESULT >> 16) as u16, RESULT as u16]);
        push_imm_a(words, 6, DOS_LIBRARY_BASE);
        words.extend_from_slice(&[0x4EAE, (-132i32) as u16]); // IoErr
    }

    /// Runs `words` with `A6` = [`ICON_LIBRARY_BASE`], `data` at
    /// [`DATA`], and a `SYS:` volume over `sys` if given.
    fn run(
        words: &[u16],
        data: &[u8],
        sys: Option<&Path>,
    ) -> (Result<i32, RuntimeError>, Runtime<M68kCpu>) {
        let entry = TRAP_TABLE_END;
        let mut full = Vec::new();
        push_imm_a(&mut full, 6, ICON_LIBRARY_BASE);
        full.extend_from_slice(words);
        full.push(0x4E75); // rts
        let mut mem = FlatMemory::new(0x4_0000);
        for (i, &w) in full.iter().enumerate() {
            mem.write_u16(entry + 2 * i as u32, w);
        }
        for (i, &b) in data.iter().enumerate() {
            mem.write_u8(DATA + i as u32, b);
        }
        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: entry + (full.len() as u32) * 2 + 0x400,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        if let Some(root) = sys {
            rt.set_vfs(
                Vfs::new(VfsConfig {
                    volumes: vec![("SYS".to_string(), root.to_path_buf())],
                    assigns: vec![("ENV".to_string(), vec!["SYS:env".to_string()])],
                    cwd: "SYS:".to_string(),
                    ..Default::default()
                })
                .expect("build vfs"),
            );
        }
        let mut out = Vec::new();
        let result = rt.run(&mut out, None);
        (result, rt)
    }

    #[test]
    fn get_disk_object_with_no_icon_is_null_with_object_not_found() {
        // The regression: a program asking for a missing icon used to
        // abort the whole run, because icon.library was an auto-created
        // fake with no vectors.
        let tmp = TempDir::new("missing");
        let mut words = Vec::new();
        push_imm_a(&mut words, 0, DATA);
        push_jsr_a6(&mut words, "GetDiskObject");
        push_store_d0_then_io_err(&mut words);
        let (result, rt) = run(&words, b"SYS:nothing\0", Some(tmp.path()));
        assert_eq!(result.unwrap(), crate::dosfile::ERROR_OBJECT_NOT_FOUND);
        assert_eq!(rt.memory().read_u32(RESULT), 0);
    }

    use crate::iconfile::{DiskObjectFile, WBDISK, WBDRAWER, WBPROJECT, WBTOOL};

    /// `move.l d0,a0`: feeds one call's result to the next.
    const MOVE_D0_TO_A0: u16 = 0x2040;

    fn project_icon() -> DiskObjectFile {
        DiskObjectFile {
            object_type: WBPROJECT,
            stack_size: 9000,
            default_tool: Some(b"SYS:Tools/Edit".to_vec()),
            tool_types: Some(vec![b"A=1".to_vec(), b"FLAG".to_vec()]),
            ..DiskObjectFile::default()
        }
    }

    /// Runs one call taking a name in `A0` and returns the guest object
    /// it produced (`0` for `NULL`) with the runtime and the exit code
    /// (`IoErr()`).
    fn call_with_name(lvo: &str, name: &[u8], sys: &Path) -> (u32, Runtime<M68kCpu>, i32) {
        let mut words = Vec::new();
        push_imm_a(&mut words, 0, DATA);
        push_jsr_a6(&mut words, lvo);
        push_store_d0_then_io_err(&mut words);
        let (result, rt) = run(&words, name, Some(sys));
        let code = result.unwrap();
        let obj = rt.memory().read_u32(RESULT);
        (obj, rt, code)
    }

    #[test]
    fn get_disk_object_reads_a_present_icon_into_a_real_disk_object() {
        let tmp = TempDir::new("present");
        fs::write(tmp.path().join("thing.info"), project_icon().to_bytes()).unwrap();
        let (obj, rt, _) = call_with_name("GetDiskObject", b"SYS:thing\0", tmp.path());
        assert_ne!(obj, 0);
        let mem = rt.memory();
        assert_eq!(mem.read_u16(obj), crate::iconfile::WB_DISKMAGIC);
        assert_eq!(mem.read_u8(obj + 48), WBPROJECT);
        assert_eq!(mem.read_u32(obj + 74), 9000, "do_StackSize");
        assert_eq!(
            read_c_string(mem, mem.read_u32(obj + 50)),
            b"SYS:Tools/Edit"
        );
        let tt = mem.read_u32(obj + 54);
        assert_eq!(read_c_string(mem, mem.read_u32(tt)), b"A=1");
        assert_eq!(read_c_string(mem, mem.read_u32(tt + 4)), b"FLAG");
        assert_eq!(mem.read_u32(tt + 8), 0);
    }

    #[test]
    fn find_tool_type_works_on_a_tool_types_array_from_get_disk_object() {
        // The point of reading icons at all: a program asks the object
        // it got for a tool type.
        let tmp = TempDir::new("findtt");
        fs::write(tmp.path().join("thing.info"), project_icon().to_bytes()).unwrap();
        let mut words = Vec::new();
        push_imm_a(&mut words, 0, DATA);
        push_jsr_a6(&mut words, "GetDiskObject");
        // A0 = do_ToolTypes (offset 54), A1 = "a"
        words.extend_from_slice(&[MOVE_D0_TO_A0, 0x2068, 54]); // move.l 54(a0),a0
        push_imm_a(&mut words, 1, DATA + 16);
        push_jsr_a6(&mut words, "FindToolType");
        let mut data = b"SYS:thing\0".to_vec();
        data.resize(16, 0);
        data.extend_from_slice(b"a\0");
        let (result, rt) = run(&words, &data, Some(tmp.path()));
        let value = result.unwrap() as u32;
        assert_eq!(read_c_string(rt.memory(), value), b"1");
    }

    /// The tool types of the guest `DiskObject` at `obj`.
    fn guest_tool_types(rt: &Runtime<M68kCpu>, obj: u32) -> Vec<Vec<u8>> {
        let mem = rt.memory();
        let array = mem.read_u32(obj + 54);
        (0..)
            .map(|i| mem.read_u32(array + 4 * i))
            .take_while(|&p| p != 0)
            .map(|p| read_c_string(mem, p))
            .collect()
    }

    #[test]
    fn color_icon_and_png_data_after_the_classic_part_is_ignored() {
        // OS 3.5 ColorIcons and PNG icons append `FORM ICON` chunks (the
        // PNG ones holding a PNG stream); the classic part comes first,
        // so a 3.1 program sees exactly what it would without them.
        let tmp = TempDir::new("colouricon");
        let mut bytes = project_icon().to_bytes();
        bytes.extend_from_slice(b"FORM\0\0\0\x20ICONFACE\0\0\0\x04\x01\x02\x03\x04");
        bytes.extend_from_slice(b"IMAG\0\0\0\x0c\x89PNG\r\n\x1a\n\0\0\0\0");
        fs::write(tmp.path().join("thing.info"), &bytes).unwrap();
        let (obj, rt, code) = call_with_name("GetDiskObject", b"SYS:thing\0", tmp.path());
        assert_ne!(obj, 0, "IoErr {code}");
        assert_eq!(rt.memory().read_u32(obj + 74), 9000);
        assert_eq!(
            guest_tool_types(&rt, obj),
            [b"A=1".to_vec(), b"FLAG".to_vec()]
        );
    }

    #[test]
    fn newicons_image_data_arrives_as_ordinary_tool_types() {
        // NewIcons keep their pictures in `IM1=...` tool types, which a
        // program that only wants its own settings must be able to walk
        // past: they are plain entries of the array.
        let tmp = TempDir::new("newicons");
        let mut icon = project_icon();
        icon.tool_types = Some(vec![
            b"STACK=9000".to_vec(),
            b"*** DON'T EDIT THE FOLLOWING LINES!! ***".to_vec(),
            b"IM1=BB8a8N8e".to_vec(),
            b"IM2=BB8a8N8e".to_vec(),
        ]);
        fs::write(tmp.path().join("thing.info"), icon.to_bytes()).unwrap();
        let (obj, rt, _) = call_with_name("GetDiskObject", b"SYS:thing\0", tmp.path());
        let types = guest_tool_types(&rt, obj);
        assert_eq!(types.len(), 4);
        assert_eq!(types[2], b"IM1=BB8a8N8e");
    }

    #[test]
    fn get_disk_object_on_a_file_that_is_not_an_icon_is_null_with_io_err_zero() {
        let tmp = TempDir::new("junk");
        fs::write(tmp.path().join("thing.info"), b"hello, not an icon").unwrap();
        let (obj, _, code) = call_with_name("GetDiskObject", b"SYS:thing\0", tmp.path());
        assert_eq!(obj, 0);
        assert_eq!(code, BAD_ICON_IO_ERR, "Kickstart 3.1 leaves IoErr at 0");
    }

    #[test]
    fn a_cut_off_icon_fails_like_a_non_icon() {
        let tmp = TempDir::new("short");
        let bytes = project_icon().to_bytes();
        fs::write(tmp.path().join("thing.info"), &bytes[..bytes.len() - 3]).unwrap();
        let (obj, _, code) = call_with_name("GetDiskObject", b"SYS:thing\0", tmp.path());
        assert_eq!((obj, code), (0, BAD_ICON_IO_ERR));
    }

    #[test]
    fn get_disk_object_with_a_null_name_allocates_an_empty_object() {
        let mut words = Vec::new();
        push_imm_a(&mut words, 0, 0);
        push_jsr_a6(&mut words, "GetDiskObject");
        push_store_d0_then_io_err(&mut words);
        let (result, rt) = run(&words, &[], None);
        result.unwrap();
        let obj = rt.memory().read_u32(RESULT);
        assert_ne!(obj, 0);
        // Real Kickstart 3.1: every field zero, even the magic.
        let all_zero =
            (0..crate::iconobj::DISK_OBJECT_MEM_SIZE).all(|i| rt.memory().read_u8(obj + i) == 0);
        assert!(all_zero, "GetDiskObject(NULL) is an all-zero DiskObject");
    }

    #[test]
    fn get_def_disk_object_uses_the_env_sys_file_when_there_is_one() {
        let tmp = TempDir::new("envdef");
        fs::create_dir_all(tmp.path().join("env/Sys")).unwrap();
        let mut icon = project_icon();
        icon.stack_size = 1234;
        fs::write(tmp.path().join("env/Sys/def_project.info"), icon.to_bytes()).unwrap();
        let mut words = Vec::new();
        push_imm_d(&mut words, 0, 4); // WBPROJECT
        push_jsr_a6(&mut words, "GetDefDiskObject");
        push_store_d0_then_io_err(&mut words);
        let (result, rt) = run(&words, &[], Some(tmp.path()));
        result.unwrap();
        let obj = rt.memory().read_u32(RESULT);
        assert_eq!(rt.memory().read_u32(obj + 74), 1234, "from the file");
    }

    #[test]
    fn get_def_disk_object_falls_back_to_a_builtin_when_the_file_is_missing() {
        // As real Kickstart 3.1 does: gadget size, drawer and default tool.
        for (def_type, width, drawer, tool, selected) in [
            (WBDISK, 35, true, true, false),
            (WBDRAWER, 57, true, false, true),
            (WBTOOL, 54, false, false, false),
            (WBPROJECT, 54, false, false, false),
            (crate::iconfile::WBGARBAGE, 51, true, false, true),
            (crate::iconfile::WBKICK, 35, false, false, false),
        ] {
            let tmp = TempDir::new("nodefault");
            let mut words = Vec::new();
            push_imm_d(&mut words, 0, def_type as u32);
            push_jsr_a6(&mut words, "GetDefDiskObject");
            push_store_d0_then_io_err(&mut words);
            let (result, rt) = run(&words, &[], Some(tmp.path()));
            result.unwrap();
            let obj = rt.memory().read_u32(RESULT);
            assert_ne!(obj, 0, "type {def_type}");
            let mem = rt.memory();
            assert_eq!(mem.read_u8(obj + 48), def_type);
            assert_eq!(
                mem.read_u16(obj + 12),
                width,
                "gadget width, type {def_type}"
            );
            assert_ne!(mem.read_u32(obj + 22), 0, "a picture, type {def_type}");
            assert_eq!(mem.read_u32(obj + 26) != 0, selected, "type {def_type}");
            assert_eq!(mem.read_u32(obj + 66) != 0, drawer, "type {def_type}");
            assert_eq!(mem.read_u32(obj + 50) != 0, tool, "type {def_type}");
            assert_eq!(mem.read_u32(obj + 74), 0, "no stack, type {def_type}");
        }
    }

    #[test]
    fn get_def_disk_object_with_a_corrupt_env_file_is_an_error_not_a_fallback() {
        let tmp = TempDir::new("envbad");
        fs::create_dir_all(tmp.path().join("env/Sys")).unwrap();
        fs::write(tmp.path().join("env/Sys/def_project.info"), b"junk").unwrap();
        let mut words = Vec::new();
        push_imm_d(&mut words, 0, 4);
        push_jsr_a6(&mut words, "GetDefDiskObject");
        push_store_d0_then_io_err(&mut words);
        let (result, rt) = run(&words, &[], Some(tmp.path()));
        assert_eq!(result.unwrap(), BAD_ICON_IO_ERR);
        assert_eq!(rt.memory().read_u32(RESULT), 0);
    }

    /// The `do_Type` `GetDiskObjectNew("SYS:<name>")` gives, `0` for NULL.
    fn new_type(tmp: &TempDir, name: &str) -> (u32, u8, i32) {
        let mut data = format!("SYS:{name}").into_bytes();
        data.push(0);
        let (obj, rt, code) = call_with_name("GetDiskObjectNew", &data, tmp.path());
        let ty = if obj == 0 {
            0
        } else {
            rt.memory().read_u8(obj + 48)
        };
        (obj, ty, code)
    }

    #[test]
    fn get_disk_object_new_falls_back_to_a_default_of_the_objects_type() {
        let tmp = TempDir::new("new");
        fs::create_dir(tmp.path().join("adrawer")).unwrap();
        fs::write(tmp.path().join("locked"), b"plain text").unwrap();
        // `Protect locked +e`'s sidecar: FIBF_EXECUTE denied.
        fs::write(
            tmp.path().join("locked.uaem"),
            b"----ts-- 2025-01-01 00:00:00.00 \n",
        )
        .unwrap();
        fs::write(tmp.path().join("notes.txt"), b"plain text").unwrap();
        assert_eq!(new_type(&tmp, "adrawer").1, WBDRAWER);
        assert_eq!(
            new_type(&tmp, "notes.txt").1,
            WBTOOL,
            "real 3.1: a file with no protection bits is a tool, whatever it holds"
        );
        assert_eq!(new_type(&tmp, "locked").1, WBPROJECT, "execute denied");
        assert_eq!(new_type(&tmp, "").1, WBDISK, "a volume root");
    }

    #[test]
    fn get_disk_object_new_prefers_the_icon_file_when_there_is_one() {
        let tmp = TempDir::new("newicon");
        fs::write(tmp.path().join("notes.txt"), b"plain text").unwrap();
        let mut icon = project_icon();
        icon.object_type = WBTOOL;
        fs::write(tmp.path().join("notes.txt.info"), icon.to_bytes()).unwrap();
        assert_eq!(
            new_type(&tmp, "notes.txt").1,
            WBTOOL,
            "the file's type, not a guess"
        );
    }

    #[test]
    fn a_corrupt_default_file_fails_get_disk_object_new_like_get_def_disk_object() {
        let tmp = TempDir::new("newcorruptdef");
        fs::create_dir_all(tmp.path().join("env/Sys")).unwrap();
        fs::write(tmp.path().join("env/Sys/def_tool.info"), b"junk").unwrap();
        fs::write(tmp.path().join("notes.txt"), b"plain text").unwrap();
        let (obj, _, code) = new_type(&tmp, "notes.txt");
        assert_eq!(obj, 0);
        assert_eq!(code, BAD_ICON_IO_ERR);
    }

    #[test]
    fn a_volumes_icon_is_the_name_plus_info() {
        // Real Kickstart 3.1 writes and reads `SYS:.info` -- there is no
        // special `Disk` name (Workbench asks for `SYS:Disk`).
        let tmp = TempDir::new("diskinfo");
        let mut icon = project_icon();
        icon.object_type = WBDISK;
        fs::write(tmp.path().join(".info"), icon.to_bytes()).unwrap();
        let (obj, rt, _) = call_with_name("GetDiskObject", b"SYS:\0", tmp.path());
        assert_ne!(obj, 0, "SYS: reads SYS:.info");
        assert_eq!(rt.memory().read_u8(obj + 48), WBDISK);
        let (obj, rt, _) = call_with_name("GetDiskObjectNew", b"SYS:\0", tmp.path());
        assert_eq!(rt.memory().read_u8(obj + 48), WBDISK);
    }

    #[test]
    fn workbenchs_disk_icon_name_reads_disk_info() {
        // Workbench asks for the disk icon as `<volume>:Disk`; the library
        // only appends `.info`, so that is `Disk.info` at the root.
        let tmp = TempDir::new("workbenchdisk");
        let mut icon = project_icon();
        icon.object_type = WBDISK;
        fs::write(tmp.path().join("Disk.info"), icon.to_bytes()).unwrap();
        let (obj, rt, _) = call_with_name("GetDiskObject", b"SYS:Disk\0", tmp.path());
        assert_ne!(obj, 0);
        assert_eq!(rt.memory().read_u8(obj + 48), WBDISK);
        let (obj, _, code) = call_with_name("GetDiskObject", b"SYS:\0", tmp.path());
        assert_eq!(
            (obj, code),
            (0, crate::dosfile::ERROR_OBJECT_NOT_FOUND),
            "SYS: alone is the empty name, so `.info`"
        );
    }

    #[test]
    fn get_disk_object_new_does_not_hide_a_corrupt_icon_behind_a_default() {
        let tmp = TempDir::new("newcorrupt");
        fs::write(tmp.path().join("notes.txt"), b"plain text").unwrap();
        fs::write(tmp.path().join("notes.txt.info"), b"junk").unwrap();
        let (obj, _, code) = new_type(&tmp, "notes.txt");
        assert_eq!(obj, 0);
        assert_eq!(code, BAD_ICON_IO_ERR);
    }

    #[test]
    fn a_directory_named_like_an_icon_is_wrong_type_not_a_crash() {
        let tmp = TempDir::new("infodir");
        fs::create_dir(tmp.path().join("thing.info")).unwrap();
        let (obj, _, code) = call_with_name("GetDiskObject", b"SYS:thing\0", tmp.path());
        assert_eq!(obj, 0);
        assert_eq!(code, crate::dosfile::ERROR_OBJECT_WRONG_TYPE);
    }

    /// Reads `SYS:src` with `GetDiskObject`, then hands the result to
    /// `put` (`PutDiskObject` as `A1`, with `SYS:dst` in `A0`, or
    /// `PutDefDiskObject` in `A0`); returns the put's `D0` and `IoErr()`.
    fn get_then_put(put: &str, dst: &[u8], sys: &Path) -> (u32, i32) {
        const NAMES: u32 = DATA + 0x100;
        let mut words = Vec::new();
        push_imm_a(&mut words, 0, DATA);
        push_jsr_a6(&mut words, "GetDiskObject");
        if put == "PutDiskObject" {
            words.push(0x2240); // move.l d0,a1
            push_imm_a(&mut words, 0, NAMES);
        } else {
            words.push(MOVE_D0_TO_A0);
        }
        push_jsr_a6(&mut words, put);
        push_store_d0_then_io_err(&mut words);
        let mut data = vec![0u8; 0x100];
        data[..8].copy_from_slice(b"SYS:src\0");
        data.extend_from_slice(dst);
        data.push(0);
        let (result, rt) = run(&words, &data, Some(sys));
        (rt.memory().read_u32(RESULT), result.unwrap())
    }

    fn full_icon() -> DiskObjectFile {
        let mut icon = project_icon();
        icon.tool_window = Some(b"CON:0/0/320/100/win".to_vec());
        icon.current_x = 40;
        icon.current_y = 60;
        icon.gadget_image = Some(crate::iconfile::IconImage {
            width: 20,
            height: 4,
            depth: 2,
            data: (0..16u8).map(|i| i * 3).collect::<Vec<_>>().repeat(2)[..16].to_vec(),
            ..Default::default()
        });
        icon.gadget_image
            .as_mut()
            .unwrap()
            .data
            .resize(crate::iconfile::plane_data_size(20, 4, 2) as usize, 7);
        icon
    }

    #[test]
    fn put_disk_object_writes_what_get_disk_object_read() {
        let tmp = TempDir::new("put");
        let icon = full_icon();
        fs::write(tmp.path().join("src.info"), icon.to_bytes()).unwrap();
        let (ok, code) = get_then_put("PutDiskObject", b"SYS:dst", tmp.path());
        assert_eq!((ok, code), (TRUE, 0));
        let written = fs::read(tmp.path().join("dst.info")).unwrap();
        let (back, used) = DiskObjectFile::parse(&written).unwrap();
        assert_eq!(used, written.len());
        assert_eq!(back, icon, "every field survives the guest round trip");
    }

    #[test]
    fn put_disk_object_replaces_an_existing_file() {
        let tmp = TempDir::new("putreplace");
        fs::write(tmp.path().join("src.info"), full_icon().to_bytes()).unwrap();
        fs::write(tmp.path().join("dst.info"), vec![0xAA; 5000]).unwrap();
        let (ok, _) = get_then_put("PutDiskObject", b"SYS:dst", tmp.path());
        assert_eq!(ok, TRUE);
        let written = fs::read(tmp.path().join("dst.info")).unwrap();
        assert!(DiskObjectFile::parse(&written).is_ok());
        assert_eq!(written, full_icon().to_bytes(), "no stale tail");
    }

    #[test]
    fn put_disk_object_into_a_missing_directory_fails_with_io_err() {
        let tmp = TempDir::new("putnodir");
        fs::write(tmp.path().join("src.info"), full_icon().to_bytes()).unwrap();
        let (ok, code) = get_then_put("PutDiskObject", b"SYS:nowhere/dst", tmp.path());
        assert_eq!(ok, FALSE);
        assert_eq!(code, crate::dosfile::ERROR_OBJECT_NOT_FOUND);
    }

    #[test]
    fn put_disk_object_for_a_volume_writes_dot_info() {
        let tmp = TempDir::new("putdisk");
        fs::write(tmp.path().join("src.info"), full_icon().to_bytes()).unwrap();
        let (ok, _) = get_then_put("PutDiskObject", b"SYS:", tmp.path());
        assert_eq!(ok, TRUE);
        assert!(tmp.path().join(".info").exists());
    }

    #[test]
    fn put_def_disk_object_writes_the_env_file_get_def_disk_object_reads() {
        let tmp = TempDir::new("putdef");
        fs::create_dir(tmp.path().join("env")).unwrap(); // ENV:, without Sys
        let icon = full_icon(); // WBPROJECT
        fs::write(tmp.path().join("src.info"), icon.to_bytes()).unwrap();
        let (ok, code) = get_then_put("PutDefDiskObject", b"", tmp.path());
        assert_eq!((ok, code), (TRUE, 0));
        let written = fs::read(tmp.path().join("env/Sys/def_project.info")).unwrap();
        assert_eq!(DiskObjectFile::parse(&written).unwrap().0, icon);
        // ...and Get reads it back instead of the built-in.
        let (obj, rt, _) = {
            let mut words = Vec::new();
            push_imm_d(&mut words, 0, WBPROJECT as u32);
            push_jsr_a6(&mut words, "GetDefDiskObject");
            push_store_d0_then_io_err(&mut words);
            let (r, rt) = run(&words, b"", Some(tmp.path()));
            let code = r.unwrap();
            (rt.memory().read_u32(RESULT), rt, code)
        };
        assert_eq!(rt.memory().read_u32(obj + 74), 9000, "the stack we put");
    }

    #[test]
    fn put_def_disk_object_for_a_type_with_no_default_is_bad_number() {
        let tmp = TempDir::new("putdefdev");
        let mut icon = full_icon();
        icon.object_type = crate::iconfile::WBDEVICE;
        fs::write(tmp.path().join("src.info"), icon.to_bytes()).unwrap();
        let (ok, code) = get_then_put("PutDefDiskObject", b"", tmp.path());
        assert_eq!(ok, FALSE);
        assert_eq!(code, crate::dosargs::ERROR_BAD_NUMBER);
    }

    #[test]
    fn delete_disk_object_for_a_volume_removes_dot_info() {
        let tmp = TempDir::new("deldisk");
        fs::write(tmp.path().join(".info"), b"icon").unwrap();
        let (ok, _, _) = call_with_name("DeleteDiskObject", b"SYS:\0", tmp.path());
        assert_eq!(ok, TRUE);
        assert!(!tmp.path().join(".info").exists());
    }

    #[test]
    fn put_disk_object_leaves_no_temp_file_behind() {
        let tmp = TempDir::new("puttmp");
        fs::write(tmp.path().join("src.info"), full_icon().to_bytes()).unwrap();
        get_then_put("PutDiskObject", b"SYS:dst", tmp.path());
        let names: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(
            names.iter().all(|n| !n.contains("volamos-tmp")),
            "{names:?}"
        );
    }

    #[test]
    fn put_disk_object_with_a_wild_pointer_fails_the_run_naming_the_call() {
        let tmp = TempDir::new("putwild");
        let mut words = Vec::new();
        push_imm_a(&mut words, 0, DATA);
        push_imm_a(&mut words, 1, 0xFFFF_FF00);
        push_jsr_a6(&mut words, "PutDiskObject");
        let (result, _) = run(&words, b"SYS:x\0", Some(tmp.path()));
        let err = format!("{:?}", result.unwrap_err());
        assert!(err.contains("PutDiskObject"), "{err}");
        assert!(!tmp.path().join("x.info").exists());
    }

    #[test]
    fn get_disk_object_new_for_a_missing_object_keeps_the_original_failure() {
        let tmp = TempDir::new("newmissing");
        let (obj, _, code) = new_type(&tmp, "nothing");
        assert_eq!((obj, code), (0, crate::dosfile::ERROR_OBJECT_NOT_FOUND));
    }

    #[test]
    fn free_disk_object_returns_every_byte_to_the_heap() {
        let tmp = TempDir::new("free");
        fs::write(tmp.path().join("thing.info"), project_icon().to_bytes()).unwrap();
        let mut words = Vec::new();
        push_imm_a(&mut words, 0, DATA);
        push_jsr_a6(&mut words, "GetDiskObject");
        words.push(MOVE_D0_TO_A0);
        push_jsr_a6(&mut words, "FreeDiskObject");
        let (result, mut rt) = run(&words, b"SYS:thing\0", Some(tmp.path()));
        result.unwrap();

        // The heap starts after the program, so the baseline is a
        // program of the same length that calls nothing.
        let nops = vec![0x4E71u16; words.len()];
        let (result, mut base) = run(&nops, &[], Some(tmp.path()));
        result.unwrap();
        assert_eq!(rt.heap_mut().free_bytes(), base.heap_mut().free_bytes());
    }

    #[test]
    fn freeing_a_disk_object_twice_fails_loudly_by_name() {
        let tmp = TempDir::new("free2");
        fs::write(tmp.path().join("thing.info"), project_icon().to_bytes()).unwrap();
        let mut words = Vec::new();
        push_imm_a(&mut words, 0, DATA);
        push_jsr_a6(&mut words, "GetDiskObject");
        words.push(0x2840); // move.l d0,a4: keep the object
        words.push(MOVE_D0_TO_A0);
        push_jsr_a6(&mut words, "FreeDiskObject");
        words.push(0x204C); // move.l a4,a0
        push_jsr_a6(&mut words, "FreeDiskObject");
        let (result, _) = run(&words, b"SYS:thing\0", Some(tmp.path()));
        match result {
            Err(RuntimeError::Dispatch(DispatchError::HandlerFailed { handler_name, .. })) => {
                assert_eq!(handler_name, "FreeDiskObject");
            }
            other => panic!("expected a loud FreeDiskObject failure, got {other:?}"),
        }
    }

    #[test]
    fn add_free_list_records_memory_and_free_free_list_ignores_null() {
        let mut words = Vec::new();
        push_imm_a(&mut words, 0, 0);
        push_jsr_a6(&mut words, "GetDiskObject"); // an empty object
        words.push(MOVE_D0_TO_A0);
        words.extend_from_slice(&[0x41E8, 78]); // lea 78(a0),a0: its FreeList
        push_imm_a(&mut words, 1, 0x3_8000); // memory to record
        push_imm_a(&mut words, 2, 16);
        push_jsr_a6(&mut words, "AddFreeList");
        words.extend_from_slice(&[0x23C0, (RESULT >> 16) as u16, RESULT as u16]);
        push_imm_a(&mut words, 0, 0);
        push_jsr_a6(&mut words, "FreeFreeList");
        let (result, rt) = run(&words, &[], None);
        result.unwrap();
        assert_eq!(rt.memory().read_u32(RESULT), TRUE);
    }

    #[test]
    fn get_def_disk_object_has_no_default_for_wbdevice_or_out_of_range_types() {
        // Real 3.1: WBDEVICE is NULL with OBJECT_NOT_FOUND; anything else
        // is NULL with IoErr() left as it was (here 777).
        for (def_type, io_err) in [
            (6, crate::dosfile::ERROR_OBJECT_NOT_FOUND),
            (8, 777),
            (0, 777),
            (99, 777),
        ] {
            let mut words = Vec::new();
            push_imm_a(&mut words, 6, DOS_LIBRARY_BASE);
            push_imm_d(&mut words, 1, 777);
            words.extend_from_slice(&[0x4EAE, (-462i32) as u16]); // SetIoErr(d1)
            push_imm_a(&mut words, 6, ICON_LIBRARY_BASE);
            push_imm_d(&mut words, 0, def_type);
            push_jsr_a6(&mut words, "GetDefDiskObject");
            push_store_d0_then_io_err(&mut words);
            let (result, rt) = run(&words, &[], None);
            assert_eq!(result.unwrap(), io_err, "type {def_type}");
            assert_eq!(rt.memory().read_u32(RESULT), 0);
        }
    }

    #[test]
    fn delete_disk_object_deletes_the_info_file_only() {
        let tmp = TempDir::new("delete");
        fs::write(tmp.path().join("thing"), b"data").unwrap();
        fs::write(tmp.path().join("thing.info"), b"icon").unwrap();
        let mut words = Vec::new();
        push_imm_a(&mut words, 0, DATA);
        push_jsr_a6(&mut words, "DeleteDiskObject");
        let (result, _) = run(&words, b"SYS:thing\0", Some(tmp.path()));
        assert_eq!(result.unwrap() as u32, TRUE);
        assert!(!tmp.path().join("thing.info").exists());
        assert!(tmp.path().join("thing").exists());
    }

    #[test]
    fn free_disk_object_ignores_null() {
        let mut words = Vec::new();
        push_imm_a(&mut words, 0, 0);
        push_jsr_a6(&mut words, "FreeDiskObject");
        push_imm_d(&mut words, 0, 7);
        let (result, _) = run(&words, &[], None);
        assert_eq!(result.unwrap(), 7);
    }

    #[test]
    fn find_tool_type_returns_a_pointer_into_the_matching_entry() {
        // DATA: a 3-entry STRPTR array, then its strings, then the name.
        let strings: [&[u8]; 3] = [b"TEMPDIR=:t\0", b"DONOTWAIT\0", b"FILETYPE=text\0"];
        let mut data = Vec::new();
        let mut addr = DATA + 16;
        for s in strings {
            data.extend_from_slice(&addr.to_be_bytes());
            addr += s.len() as u32;
        }
        data.extend_from_slice(&0u32.to_be_bytes());
        let filetype_entry = addr - strings[2].len() as u32;
        for s in strings {
            data.extend_from_slice(s);
        }
        let name_addr = DATA + data.len() as u32;
        data.extend_from_slice(b"filetype\0");

        let mut words = Vec::new();
        push_imm_a(&mut words, 0, DATA);
        push_imm_a(&mut words, 1, name_addr);
        push_jsr_a6(&mut words, "FindToolType");
        let (result, rt) = run(&words, &data, None);
        let value = result.unwrap() as u32;
        assert_eq!(value, filetype_entry + 9);
        assert_eq!(read_c_string(rt.memory(), value), b"text");
    }

    #[test]
    fn bump_revision_matches_kickstart_31() {
        // Every line is what `BumpRevision` printed on a real Kickstart
        // 3.1 (icon.library 40.1): note the capital `C`, which the
        // autodoc's examples do not show.
        for (old, new) in [
            ("foo", "Copy_of_foo"),
            ("copy_of_foo", "Copy_2_of_foo"),
            ("Copy_of_foo", "Copy_2_of_foo"),
            ("copy_2_of_foo", "Copy_3_of_foo"),
            ("Copy_2_of_foo", "Copy_3_of_foo"),
            ("COPY_OF_foo", "Copy_2_of_foo"),
            ("copy_0_of_foo", "Copy_1_of_foo"),
            ("copy_199_of_foo", "Copy_200_of_foo"),
            (
                "012345678901234567890123456789",
                "Copy_of_0123456789012345678901",
            ),
            ("copy_x_of_foo", "Copy_of_copy_x_of_foo"),
            ("copy foo", "Copy_of_copy foo"),
            ("copy_", "Copy_of_copy_"),
        ] {
            assert_eq!(
                bump_revision(old.as_bytes(), BUMP_REVISION_MAX, BumpStyle::V40).unwrap(),
                new.as_bytes(),
                "{old}"
            );
        }
    }

    /// `BumpRevisionLength(buf, old, length)` from guest code: the name it
    /// wrote and `D0` (`0` for `NULL`).
    fn bump_length(old: &str, length: u32) -> (Option<Vec<u8>>, u32) {
        const OLD: u32 = DATA;
        const BUF: u32 = DATA + 0x400;
        let mut data = old.as_bytes().to_vec();
        data.push(0);
        let mut words = Vec::new();
        push_imm_a(&mut words, 0, BUF);
        push_imm_a(&mut words, 1, OLD);
        push_imm_d(&mut words, 0, length);
        push_jsr_a6(&mut words, "BumpRevisionLength");
        push_store_d0_then_io_err(&mut words);
        let (result, rt) = run(&words, &data, None);
        result.unwrap();
        let d0 = rt.memory().read_u32(RESULT);
        let name = (d0 != 0).then(|| read_c_string(rt.memory(), BUF));
        (name, d0)
    }

    #[test]
    fn bump_revision_length_allows_names_longer_than_30() {
        let long = "a".repeat(60);
        let (name, d0) = bump_length(&long, 100);
        assert_eq!(d0, DATA + 0x400, "the buffer");
        assert_eq!(name.unwrap(), format!("Copy_of_{long}").as_bytes());
        let (name, _) = bump_length(&format!("Copy_of_{long}"), 100);
        assert_eq!(name.unwrap(), format!("Copy_2_of_{long}").as_bytes());
    }

    #[test]
    fn bump_revision_length_matches_icon_library_47() {
        // Real 3.2 (icon.library 47.5) results; `None` is NULL.
        let cases: &[(&str, u32, Option<&str>)] = &[
            ("foo", 8, None),
            ("foo", 9, Some("Copy_of_f")),
            ("foo", 10, Some("Copy_of_fo")),
            ("foo", 11, Some("Copy_of_foo")),
            ("foo", 107, Some("Copy_of_foo")),
            ("", 8, None),
            ("", 9, Some("Copy_of_")),
            ("copy_0_of_foo", 9, Some("Copy_of_f")),
            ("copy_0_of_foo", 13, Some("Copy_of_foo")),
            ("copy_1_of_foo", 19, None),
            ("copy_1_of_foo", 20, Some("Copy_2_of_foo")),
            ("Copy_of_foo", 13, None),
            ("Copy_of_foo", 19, None),
            ("Copy_of_foo", 20, Some("Copy_2_of_foo")),
            ("copy_99_of_foo", 18, None),
            ("copy_99_of_foo", 20, Some("Copy_100_of_foo")),
            ("copy_x_of_foo", 19, None),
            ("copy_x_of_foo", 30, Some("Copy_2_of_x_of_foo")),
            ("copy foo", 18, None),
            ("copy foo", 30, Some("Copy_2_of_foo")),
        ];
        for &(old, length, want) in cases {
            let (got, d0) = bump_length(old, length);
            assert_eq!(got.as_deref(), want.map(str::as_bytes), "{old:?} {length}");
            assert_eq!(d0 == 0, want.is_none(), "{old:?} {length}");
        }
    }

    #[test]
    fn bump_revision_v47_numbered_names_are_truncated_once_there_is_room() {
        let long = "x".repeat(40);
        let got =
            bump_revision(format!("copy_2_of_{long}").as_bytes(), 25, BumpStyle::V47).unwrap();
        assert_eq!(got.len(), 25);
        assert!(got.starts_with(b"Copy_3_of_"));
    }

    #[test]
    fn tool_type_value_matches_case_insensitively_and_finds_flags() {
        assert_eq!(tool_type_value(b"FILETYPE=text", b"filetype"), Some(9));
        assert_eq!(tool_type_value(b"DONOTWAIT", b"DONOTWAIT"), Some(9));
        assert_eq!(tool_type_value(b"FILETYPES=x", b"FILETYPE"), None);
        assert_eq!(tool_type_value(b"FILETYPE=text", b"text"), None);
        assert_eq!(tool_type_value(b"(FILETYPE=text)", b"FILETYPE"), None);
    }

    #[test]
    fn match_tool_value_matches_every_autodoc_example() {
        for (type_string, value, expected) in [
            ("text", "text", true),
            ("text", "TEXT", true),
            ("text", "data", false),
            ("a|b|c", "a", true),
            ("a|b|c", "b", true),
            ("a|b|c", "d", false),
            ("a|b|c", "a|b", false),
        ] {
            assert_eq!(
                match_tool_value(type_string.as_bytes(), value.as_bytes()),
                expected,
                "{type_string} / {value}"
            );
        }
    }
}
