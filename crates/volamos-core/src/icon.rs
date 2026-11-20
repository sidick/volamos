//! `icon.library`: reading `.info` icon files into guest `DiskObject`s,
//! the free-list calls, and the pure string helpers -- everything in
//! the 3.1 (V40) interface except writing icons.
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
//!   icon, or is cut short, is `ERROR_OBJECT_WRONG_TYPE` (**the real
//!   code is unverified**); a full heap is `ERROR_NO_FREE_STORE`. A
//!   `NULL` name allocates an empty object ([`DiskObjectFile::empty`]),
//!   "useful when calling `AddAppIcon()`".
//! - `GetDefDiskObject`: `ENV:Sys/def_<type>.info` the same way, and a
//!   **built-in default** ([`DiskObjectFile::builtin_default`]) when
//!   that file is missing. The V40 autodoc describes the call as
//!   reading the default object "from disk" and names no fallback; V44
//!   and later (and AROS) answer with built-in images. Which one a real
//!   3.1 `icon.library` does is **not yet hardware-verified**; the
//!   fallback is kept deliberately, since a program that wants *an*
//!   icon is better served by one. A default file that exists but is
//!   not an icon is still an error.
//! - `GetDiskObjectNew`: `GetDiskObject`, and on failure the default
//!   for the object's type -- a volume root is `WBDISK`, another
//!   directory `WBDRAWER`, an executable `WBTOOL`, any other file
//!   `WBPROJECT` (executable meaning a `HUNK_HEADER`, since host files
//!   have no execute bit; **V40's own test is unverified**). An object
//!   that is not there at all keeps the original failure.
//! - `FreeDiskObject`, `FreeFreeList`, `AddFreeList`: the autodoc's
//!   `FreeList` just after the `DiskObject`; see [`crate::iconobj`].
//!   `NULL` is ignored. A pointer that is not such an object fails the
//!   run naming the call, since freeing it would corrupt the guest
//!   heap.
//! - `DeleteDiskObject`: deletes `name.info` exactly as `DeleteFile`
//!   would, failures and all.
//! - `FindToolType`, `MatchToolValue`, `BumpRevision`: pure string work
//!   over guest memory, implemented in full to their V40 autodocs.
//!
//! `PutDiskObject` and `PutDefDiskObject` are not here yet
//! (issue #313, phase 3): like every vector without a handler they fail
//! the run naming the call, rather than telling the guest a write
//! succeeded when nothing was written.
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

/// `HUNK_HEADER`: the first longword of an AmigaOS executable.
const HUNK_HEADER: u32 = 0x0000_03F3;

/// Fails the run naming `name`, for a call this module can't answer
/// truthfully yet -- see the module docs.
fn guest_string(mem: &dyn AddressSpace, addr: u32) -> String {
    String::from_utf8_lossy(&read_c_string(mem, addr)).into_owned()
}

/// Whether `err` means the icon (or the directory it would be in) is
/// simply not there, as opposed to being there and unreadable.
fn is_missing(err: i32) -> bool {
    err == crate::dosfile::ERROR_OBJECT_NOT_FOUND || err == crate::dosfile::ERROR_DIR_NOT_FOUND
}

/// Reads and parses the icon file at `path` (a full Amiga path, `.info`
/// included), or says what `IoErr()` should be. A file that is not an
/// icon, or is cut short, is `ERROR_OBJECT_WRONG_TYPE`; the real
/// library's code for that is unverified (phase 0 of #313).
fn load_icon(ctx: &HandlerContext<'_, impl Cpu>, path: &str) -> Result<DiskObjectFile, i32> {
    let vfs = ctx
        .dos
        .vfs
        .as_ref()
        .ok_or(crate::dosfile::ERROR_OBJECT_NOT_FOUND)?;
    let host = vfs
        .resolve(path, ResolveMode::MustExist)
        .map_err(|e| map_vfs_error(&e))?;
    let bytes = std::fs::read(host).map_err(|e| crate::dosfile::map_io_error(&e))?;
    DiskObjectFile::parse(&bytes)
        .map(|(icon, _)| icon)
        .map_err(|_| crate::dosfile::ERROR_OBJECT_WRONG_TYPE)
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
        deliver(ctx, &DiskObjectFile::empty());
        return Ok(());
    }
    let info = format!("{}.info", guest_string(ctx.mem, name_ptr));
    match load_icon(ctx, &info) {
        Ok(icon) => deliver(ctx, &icon),
        Err(code) => fail(ctx, code),
    }
    Ok(())
}

/// The `def_type` for an object that has no icon file of its own, for
/// `GetDiskObjectNew`: a volume root is a disk, any other directory a
/// drawer, an executable file a tool, anything else a project. `None`
/// when `name` is not there at all. Executables are recognised by
/// their `HUNK_HEADER` rather than by a protection bit, which host files
/// do not have; V40's own test is unverified (phase 0 of #313).
fn default_type_for(ctx: &HandlerContext<'_, impl Cpu>, name: &str) -> Option<u8> {
    let vfs = ctx.dos.vfs.as_ref()?;
    let host = vfs.resolve(name, ResolveMode::MustExist).ok()?;
    if host.is_dir() {
        let is_root = name.ends_with(':');
        return Some(if is_root { WBDISK } else { WBDRAWER });
    }
    let mut magic = [0u8; 4];
    let is_hunk = std::fs::File::open(&host)
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut magic))
        .is_ok()
        && u32::from_be_bytes(magic) == HUNK_HEADER;
    Some(if is_hunk { WBTOOL } else { WBPROJECT })
}

/// `GetDiskObjectNew` (`A0` = name): `GetDiskObject`, and where that
/// fails, the default icon for the object's type -- "useful when there
/// is no .info file for the icon you are trying to get a disk object
/// for". If the object itself is not there, the original failure stands.
fn get_disk_object_new_handler<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
) -> Result<(), DispatchError> {
    let name_ptr = ctx.cpu.address_register(AddressRegister(0));
    if name_ptr == 0 {
        deliver(ctx, &DiskObjectFile::empty());
        return Ok(());
    }
    let name = guest_string(ctx.mem, name_ptr);
    match load_icon(ctx, &format!("{name}.info")) {
        Ok(icon) => deliver(ctx, &icon),
        Err(code) => match default_type_for(ctx, &name) {
            Some(def_type) => {
                let icon = default_icon(ctx, def_type);
                deliver(ctx, &icon);
            }
            None => fail(ctx, code),
        },
    }
    Ok(())
}

/// The default icon of `def_type`: `ENV:Sys/def_<type>.info` if there is
/// one, else the built-in ([`DiskObjectFile::builtin_default`]). An
/// `ENV:` file that exists but is not an icon is an error for the caller
/// to report rather than a reason to quietly use the built-in.
fn default_icon_file(
    ctx: &HandlerContext<'_, impl Cpu>,
    def_type: u8,
    name: &str,
) -> Result<DiskObjectFile, i32> {
    match load_icon(ctx, &format!("ENV:Sys/def_{name}.info")) {
        Err(code) if is_missing(code) => Ok(DiskObjectFile::builtin_default(def_type)),
        other => other,
    }
}

/// [`default_icon_file`] for a `def_type` already known to be valid; a
/// corrupt default file falls back to the built-in here, since
/// `GetDiskObjectNew` has no error left to give.
fn default_icon(ctx: &HandlerContext<'_, impl Cpu>, def_type: u8) -> DiskObjectFile {
    let name = DEFAULT_ICON_NAMES
        .iter()
        .find(|(t, _)| *t == def_type as u32)
        .map(|&(_, n)| n);
    match name {
        Some(name) => default_icon_file(ctx, def_type, name)
            .unwrap_or_else(|_| DiskObjectFile::builtin_default(def_type)),
        None => DiskObjectFile::builtin_default(def_type),
    }
}

/// `GetDefDiskObject` (`D0` = `WBDISK`..`WBKICK`). `ENV:Sys/def_<type>.info`
/// if it exists, else the built-in default of that type. A type V40 has
/// no default for (`WBDEVICE`, `WBAPPICON`, anything out of range) is
/// `NULL` + `ERROR_BAD_NUMBER`; the real library's `IoErr()` for that is
/// unverified. The built-in fallback is deliberate: V44+ and AROS also
/// answer with a built-in icon, where the V40 autodoc ("reads ... from
/// disk") suggests a missing file is an error; which one real V40 does
/// is a phase 0 check, and the difference is the safer way round for a
/// program that just wants *an* icon.
fn get_def_disk_object_handler<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
) -> Result<(), DispatchError> {
    let def_type = ctx.cpu.data_register(DataRegister(0));
    let Some(&(_, name)) = DEFAULT_ICON_NAMES.iter().find(|(t, _)| *t == def_type) else {
        fail(ctx, crate::dosargs::ERROR_BAD_NUMBER);
        return Ok(());
    };
    match default_icon_file(ctx, def_type as u8, name) {
        Ok(icon) => deliver(ctx, &icon),
        Err(code) => fail(ctx, code),
    }
    Ok(())
}

/// `DeleteDiskObject` (`A0` = name). Deletes `name.info` as `DeleteFile`
/// does: `D0` = `TRUE`, or `FALSE` + `IoErr()`.
fn delete_disk_object_handler<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
) -> Result<(), DispatchError> {
    let name_ptr = ctx.cpu.address_register(AddressRegister(0));
    let info = format!("{}.info", guest_string(ctx.mem, name_ptr));
    match ctx.dos.delete_file(&info) {
        Ok(()) => ctx.cpu.set_data_register(DataRegister(0), TRUE),
        Err(code) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), FALSE);
        }
    }
    Ok(())
}

/// A [`crate::iconobj`] failure as the call that hit it.
fn free_failed(handler: &str, lvo: i32, message: String) -> DispatchError {
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
    let lvo = crate::lvos::find_by_name(ICON_LVOS, "FreeDiskObject")
        .expect("in the table")
        .lvo;
    crate::iconobj::free_free_list(
        ctx.heap,
        ctx.mem,
        obj + crate::iconobj::DISK_OBJECT_MEM_SIZE,
    )
    .map(|_| ())
    .map_err(|e| {
        free_failed(
            "FreeDiskObject",
            lvo,
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
    let lvo = crate::lvos::find_by_name(ICON_LVOS, "FreeFreeList")
        .expect("in the table")
        .lvo;
    crate::iconobj::free_free_list(ctx.heap, ctx.mem, list)
        .map(|_| ())
        .map_err(|e| free_failed("FreeFreeList", lvo, format!("{list:#010x}: {e}")))
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

/// `BumpRevision`'s new name for `old`: `copy_of_<old>`, or
/// `copy_<n+1>_of_<rest>` when `old` is already `copy_<n>_of_<rest>`
/// with `n` all digits (plain `copy_of_<rest>` counting as `n` = one),
/// truncated to [`BUMP_REVISION_MAX`]. Matches every example in the autodoc,
/// including `"copy_0_of_foo"` -> `"copy_1_of_foo"` and `"copy foo"` ->
/// `"copy_of_copy foo"`.
fn bump_revision(old: &[u8]) -> Vec<u8> {
    let bumped = old
        .strip_prefix(b"copy_of_")
        .map(|rest| (1u64, rest))
        .or_else(|| {
            let tail = old.strip_prefix(b"copy_")?;
            let digits = tail.iter().take_while(|b| b.is_ascii_digit()).count();
            let rest = tail[digits..].strip_prefix(b"_of_")?;
            let n = std::str::from_utf8(&tail[..digits]).ok()?.parse().ok()?;
            Some((n, rest))
        });
    let mut new = match bumped {
        Some((n, rest)) => {
            let mut v = format!("copy_{}_of_", n + 1).into_bytes();
            v.extend_from_slice(rest);
            v
        }
        None => {
            let mut v = b"copy_of_".to_vec();
            v.extend_from_slice(old);
            v
        }
    };
    new.truncate(BUMP_REVISION_MAX);
    new
}

/// `BumpRevision` (`A0` = new buffer, at least 31 bytes; `A1` = old
/// name). `D0` = the new buffer.
fn bump_revision_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let new_buf = ctx.cpu.address_register(AddressRegister(0));
    let old = read_c_string(ctx.mem, ctx.cpu.address_register(AddressRegister(1)));
    crate::guestmem::write_c_string(ctx.mem, new_buf, &bump_revision(&old));
    ctx.cpu.set_data_register(DataRegister(0), new_buf);
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
    reg!("FindToolType", find_tool_type_handler::<C>);
    reg!("MatchToolValue", match_tool_value_handler::<C>);
    reg!("BumpRevision", bump_revision_handler::<C>);
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

    #[test]
    fn get_disk_object_on_a_file_that_is_not_an_icon_is_wrong_type() {
        let tmp = TempDir::new("junk");
        fs::write(tmp.path().join("thing.info"), b"hello, not an icon").unwrap();
        let (obj, _, code) = call_with_name("GetDiskObject", b"SYS:thing\0", tmp.path());
        assert_eq!(obj, 0);
        assert_eq!(code, crate::dosfile::ERROR_OBJECT_WRONG_TYPE);
    }

    #[test]
    fn a_cut_off_icon_is_wrong_type_too() {
        let tmp = TempDir::new("short");
        let bytes = project_icon().to_bytes();
        fs::write(tmp.path().join("thing.info"), &bytes[..bytes.len() - 3]).unwrap();
        let (obj, _, code) = call_with_name("GetDiskObject", b"SYS:thing\0", tmp.path());
        assert_eq!((obj, code), (0, crate::dosfile::ERROR_OBJECT_WRONG_TYPE));
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
        assert_eq!(rt.memory().read_u16(obj), crate::iconfile::WB_DISKMAGIC);
        assert_eq!(rt.memory().read_u32(obj + 50), 0, "no default tool");
        assert_eq!(rt.memory().read_u32(obj + 54), 0, "no tool types");
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
        // A deliberate difference from the V40 autodoc: see the handler.
        for (def_type, drawer, stack) in [
            (WBDISK, true, 0),
            (WBDRAWER, true, 0),
            (WBTOOL, false, 4096),
            (WBPROJECT, false, 0),
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
            assert_eq!(rt.memory().read_u8(obj + 48), def_type);
            assert_eq!(
                rt.memory().read_u32(obj + 66) != 0,
                drawer,
                "type {def_type}"
            );
            assert_eq!(rt.memory().read_u32(obj + 74), stack, "type {def_type}");
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
        assert_eq!(result.unwrap(), crate::dosfile::ERROR_OBJECT_WRONG_TYPE);
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
        fs::write(tmp.path().join("prog"), [0, 0, 3, 0xF3, 0, 0, 0, 0]).unwrap();
        fs::write(tmp.path().join("notes.txt"), b"plain text").unwrap();
        assert_eq!(new_type(&tmp, "adrawer").1, WBDRAWER);
        assert_eq!(new_type(&tmp, "prog").1, WBTOOL, "an executable");
        assert_eq!(new_type(&tmp, "notes.txt").1, WBPROJECT);
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
    fn get_def_disk_object_rejects_wbdevice_which_v40_has_no_default_for() {
        let mut words = Vec::new();
        push_imm_d(&mut words, 0, 6); // WBDEVICE
        push_jsr_a6(&mut words, "GetDefDiskObject");
        push_store_d0_then_io_err(&mut words);
        let (result, rt) = run(&words, &[], None);
        assert_eq!(result.unwrap(), crate::dosargs::ERROR_BAD_NUMBER);
        assert_eq!(rt.memory().read_u32(RESULT), 0);
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
    fn bump_revision_matches_every_autodoc_example() {
        for (old, new) in [
            ("foo", "copy_of_foo"),
            ("copy_of_foo", "copy_2_of_foo"),
            ("copy_2_of_foo", "copy_3_of_foo"),
            ("copy_199_of_foo", "copy_200_of_foo"),
            ("copy foo", "copy_of_copy foo"),
            ("copy_0_of_foo", "copy_1_of_foo"),
            (
                "012345678901234567890123456789",
                "copy_of_0123456789012345678901",
            ),
        ] {
            assert_eq!(bump_revision(old.as_bytes()), new.as_bytes(), "{old}");
        }
    }

    #[test]
    fn bump_revision_treats_a_non_numeric_copy_prefix_as_a_plain_name() {
        assert_eq!(bump_revision(b"copy_x_of_foo"), b"copy_of_copy_x_of_foo");
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
