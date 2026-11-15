//! `icon.library`: the calls whose answer doesn't depend on reading or
//! writing a `.info` file's contents -- and a loud, named failure for
//! the ones that do.
//!
//! # Why a partial library
//!
//! `icon.library` is ROM-resident on 3.x, so it has to open (see
//! [`crate::dispatch::STANDARD_WORKBENCH_LIBRARIES`]: the real
//! `BindDrivers` dies on a `NULL` base). Before this module it opened as
//! an auto-created fake, so the *first* call into it aborted the run --
//! including `GetDiskObject` on a path with no icon, which a real
//! program makes routinely and expects to answer `NULL` with
//! `IoErr()` = `ERROR_OBJECT_NOT_FOUND` (the MicroPython Amiga port's
//! `_icon.read` is one).
//!
//! What's here is everything that can be answered *correctly* without
//! the `.info` disk format:
//!
//! - `GetDiskObject` on a name with no `name.info`: `NULL`, with
//!   `IoErr()` from resolving the path (the autodoc: "If the call fails,
//!   it will return NULL. The reason for the failure may be obtained via
//!   IoErr()").
//! - `GetDefDiskObject` when `ENV:Sys/def_<type>.info` is absent: the
//!   same. The V40 autodoc describes the call as reading the default
//!   object "from disk" and names no fallback; V44 and later (and AROS,
//!   which follows V44) fall back to built-in images instead. Which one
//!   a real 3.1 `icon.library` does with the file missing is **not yet
//!   hardware-verified**.
//! - `DeleteDiskObject`: deletes `name.info` exactly as `DeleteFile`
//!   would, failures and all.
//! - `FreeDiskObject(NULL)`: ignored, as V44+ documents (no `DiskObject`
//!   this module hands out can be non-`NULL`).
//! - `FindToolType`, `MatchToolValue`, `BumpRevision`: pure string work
//!   over guest memory, implemented in full to their V40 autodocs.
//!
//! Anything that would need a real `DiskObject` -- `GetDiskObject` or
//! `GetDefDiskObject` finding the file *present*, `GetDiskObject(NULL)`
//! (which allocates an empty one), `FreeDiskObject` on a non-`NULL`
//! object, `PutDiskObject`/`PutDefDiskObject`, and the free-list
//! calls -- fails the run naming the call, rather than pretending: a
//! guest told "no icon" when one exists, or that a write succeeded when
//! nothing was written, would be wrong in a way it can't detect.
//!
//! Signatures and LVOs are from the NDK 3.2 autodocs (`icon.doc`) and
//! [`crate::lvos::icon`]; `DiskObject` type codes from
//! `workbench/workbench.h`.

use crate::cpu::{AddressRegister, Cpu, DataRegister};
use crate::dispatch::{DispatchError, HandlerContext, ICON_LIBRARY_BASE, LibraryTable};
use crate::dosfile::map_vfs_error;
use crate::guestmem::read_c_string;
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

/// Fails the run naming `name`, for a call this module can't answer
/// truthfully yet -- see the module docs.
fn unimplemented(lvo: &str, why: &str) -> DispatchError {
    let entry =
        crate::lvos::find_by_name(ICON_LVOS, lvo).expect("icon.library LVO is in the table");
    DispatchError::HandlerFailed {
        library: "icon.library".to_string(),
        lvo: entry.lvo,
        handler_name: lvo.to_string(),
        message: format!("{why}; volamos doesn't read or write .info icon files yet"),
    }
}

/// Whether `path` exists, or the `IoErr()` code resolving it fails with.
fn probe(ctx: &HandlerContext<'_, impl Cpu>, path: &str) -> Result<(), i32> {
    let vfs = ctx
        .dos
        .vfs
        .as_ref()
        .ok_or(crate::dosfile::ERROR_OBJECT_NOT_FOUND)?;
    vfs.resolve(path, ResolveMode::MustExist)
        .map(|_| ())
        .map_err(|e| map_vfs_error(&e))
}

fn guest_string(mem: &dyn AddressSpace, addr: u32) -> String {
    String::from_utf8_lossy(&read_c_string(mem, addr)).into_owned()
}

/// `GetDiskObject` (`A0` = name). `D0` = `NULL` + `IoErr()` when
/// `name.info` doesn't resolve; see the module docs for the rest.
fn get_disk_object_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let name_ptr = ctx.cpu.address_register(AddressRegister(0));
    if name_ptr == 0 {
        return Err(unimplemented(
            "GetDiskObject",
            "GetDiskObject(NULL) allocates an empty DiskObject",
        ));
    }
    let info = format!("{}.info", guest_string(ctx.mem, name_ptr));
    match probe(ctx, &info) {
        Ok(()) => Err(unimplemented(
            "GetDiskObject",
            &format!("{info} exists and would have to be parsed"),
        )),
        Err(code) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), 0);
            Ok(())
        }
    }
}

/// `GetDefDiskObject` (`D0` = `WBDISK`..`WBKICK`). `D0` = `NULL` +
/// `IoErr()` when `ENV:Sys/def_<type>.info` doesn't resolve. A type
/// V40 has no default for (`WBDEVICE`, `WBAPPICON`, anything out of
/// range) is `NULL` + `ERROR_BAD_NUMBER`; the real library's `IoErr()`
/// for that case is unverified.
fn get_def_disk_object_handler<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
) -> Result<(), DispatchError> {
    let def_type = ctx.cpu.data_register(DataRegister(0));
    let Some(&(_, name)) = DEFAULT_ICON_NAMES.iter().find(|(t, _)| *t == def_type) else {
        ctx.dos.set_io_err(crate::dosargs::ERROR_BAD_NUMBER);
        ctx.cpu.set_data_register(DataRegister(0), 0);
        return Ok(());
    };
    let info = format!("ENV:Sys/def_{name}.info");
    match probe(ctx, &info) {
        Ok(()) => Err(unimplemented(
            "GetDefDiskObject",
            &format!("{info} exists and would have to be parsed"),
        )),
        Err(code) => {
            ctx.dos.set_io_err(code);
            ctx.cpu.set_data_register(DataRegister(0), 0);
            Ok(())
        }
    }
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

/// `FreeDiskObject` (`A0` = object). `NULL` is ignored; nothing else can
/// have come from this module.
fn free_disk_object_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    if ctx.cpu.address_register(AddressRegister(0)) == 0 {
        return Ok(());
    }
    Err(unimplemented(
        "FreeDiskObject",
        "a non-NULL DiskObject can't have come from this icon.library",
    ))
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
    reg!("FreeDiskObject", free_disk_object_handler::<C>);
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

    #[test]
    fn get_disk_object_with_an_icon_present_fails_loudly_by_name() {
        // Answering NULL here would tell the guest there's no icon when
        // there is one.
        let tmp = TempDir::new("present");
        fs::write(tmp.path().join("thing.info"), [0xE3, 0x10]).unwrap();
        let mut words = Vec::new();
        push_imm_a(&mut words, 0, DATA);
        push_jsr_a6(&mut words, "GetDiskObject");
        let (result, _) = run(&words, b"SYS:thing\0", Some(tmp.path()));
        match result {
            Err(RuntimeError::Dispatch(DispatchError::HandlerFailed { handler_name, .. })) => {
                assert_eq!(handler_name, "GetDiskObject");
            }
            other => panic!("expected a loud GetDiskObject failure, got {other:?}"),
        }
    }

    #[test]
    fn get_def_disk_object_with_no_env_sys_default_is_null_with_object_not_found() {
        let tmp = TempDir::new("nodefault");
        let mut words = Vec::new();
        push_imm_d(&mut words, 0, 4); // WBPROJECT
        push_jsr_a6(&mut words, "GetDefDiskObject");
        push_store_d0_then_io_err(&mut words);
        let (result, rt) = run(&words, &[], Some(tmp.path()));
        assert_eq!(result.unwrap(), crate::dosfile::ERROR_OBJECT_NOT_FOUND);
        assert_eq!(rt.memory().read_u32(RESULT), 0);
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
