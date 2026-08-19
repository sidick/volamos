//! `utility.library`: tag-list helpers, case-insensitive string compare,
//! character case conversion, and date helpers (Phase 3 stage 3).
//!
//! # Tag lists
//!
//! A tag list is guest memory: a (possibly chained) array of `TagItem`
//! pairs, `(ULONG ti_Tag, ULONG ti_Data)`, 8 bytes each (see
//! `<utility/tagitem.h>`). Three control tag values change how the list is
//! walked rather than being ordinary data:
//!
//! - `TAG_DONE`/`TAG_END` (`0`) terminates the current array.
//! - `TAG_IGNORE` (`1`) is skipped -- not the end of the array, just an
//!   entry to pass over.
//! - `TAG_MORE` (`2`): `ti_Data` is a pointer to another `TagItem` array to
//!   continue into; this also terminates the *current* array (i.e. it
//!   behaves like a "goto", not a "insert here").
//! - `TAG_SKIP` (`3`): skip this entry and the next `ti_Data` entries
//!   after it (so `ti_Data == 0` skips only this one entry).
//!
//! [`next_tag_item_impl`] is the single traversal primitive all three
//! guest-visible functions ([`find_tag_item_handler`],
//! [`get_tag_data_handler`], [`next_tag_item_handler`]) are built from,
//! mirroring how real `utility.library` implements `FindTagItem`/
//! `GetTagData` on top of `NextTagItem` (see the NDK Autodocs' "SEE ALSO"
//! cross-references between the three). A `NULL` tag-list pointer is a
//! legal empty list (walks to `NULL` immediately, matching every
//! `FindTagItem`/`GetTagData` autodoc's "tagList - ... (may be NULL)").
//!
//! # `NextTagItem`'s iteration contract
//!
//! Per the NDK 3.2 Autodocs (`utility.doc`, confirmed against the shipped
//! `<utility/tagitem.h>`): `NextTagItem(struct TagItem **tagItemPtr)` takes
//! the *address of* a variable that holds the current position in the tag
//! list (`A0`). Before the first call, that variable is initialized to the
//! address of the first item in the list. Each call walks forward from
//! that position -- transparently skipping `TAG_IGNORE`, following
//! `TAG_MORE` continuations, and jumping over `TAG_SKIP`'s run of entries
//! -- until it finds a real (non-control) `TagItem` or hits `TAG_DONE`. On
//! success it returns a pointer to that `TagItem` *and* advances the
//! stored variable to just past it (ready for the next call); on
//! `TAG_DONE` it returns `NULL` (the Autodoc's `WARNING` -- "do NOT use
//! the value of `*tagItemPtr`, but rather use the pointer returned by
//! `NextTagItem()`" -- is about exactly this: `*tagItemPtr` after a
//! `NULL` return is left at `0`/unspecified from the caller's point of
//! view, not at anything meaningful).
//!
//! This runtime implements that literally: [`next_tag_item_handler`] reads
//! the current position from guest memory at `A0`, calls
//! [`next_tag_item_impl`] (which both resolves control tags and computes
//! the resume position), writes the resume position back to `*A0`, and
//! returns the found item's address (or `0`) in `D0`.
//!
//! # Case-insensitive comparison: case-mapping convention
//!
//! Real `Stricmp`/`Strnicmp`/`ToUpper`/`ToLower` are explicitly documented
//! (NDK 3.2 Autodocs) as "handling international character sets" and as
//! being replaced wholesale by `locale.library` when one is installed --
//! i.e. there is no single universally-correct case mapping, only a
//! documented *default* (no-locale) behavior. This runtime doesn't
//! implement `locale.library`, so it always uses that default: the
//! classic Amiga "international mode" convention also used by vamos and
//! AROS's own `utility.library` when no locale is active --
//! ASCII `A`-`Z` <-> `a`-`z` (`+`/`- 0x20`), plus the Latin-1 (ISO 8859-1)
//! accented-letter ranges `0xC0`-`0xDE` (uppercase) <-> `0xE0`-`0xFE`
//! (lowercase), *excluding* `0xD7`/`0xF7` (the multiplication/division
//! signs, which sit in the middle of those ranges but aren't letters and
//! have no case). [`amiga_tolower`]/[`amiga_toupper`] implement this once
//! and back every handler in this module (`ToUpper`/`ToLower` directly,
//! `Stricmp`/`Strnicmp` via [`amiga_tolower`] on each compared byte).
//!
//! # Date helpers: unit is seconds, not days
//!
//! `Amiga2Date`/`Date2Amiga`/`CheckDate` all operate on a `ULONG` count of
//! *seconds* since 1978-01-01 00:00:00 (confirmed against the NDK 3.2
//! Autodocs' `SYNOPSIS`/`FUNCTION` text for all three -- "the number of
//! seconds from 01-Jan-1978"), not days; despite the "Amiga epoch" often
//! being described casually in terms of a date, none of these three calls
//! take or return a day count. `struct ClockData` (`<utility/date.h>`) is
//! seven `UWORD` fields in this order: `sec, min, hour, mday, month, year,
//! wday` (`year` is the full four-digit year, e.g. `1978`, not an offset)
//! -- 14 bytes total, matching the register/offset math in
//! [`read_clock_data`]/[`write_clock_data`].
//!
//! `wday` convention: this module uses `0 == Sunday` (matching the
//! standard C `tm_wday`/`struct tm` convention), computed as
//! `days_since_epoch % 7`. This requires knowing 1978-01-01's real weekday
//! to anchor the mapping: independently verified two ways here (a
//! from-Unix-epoch day-count delta, and Zeller's congruence) that
//! **1978-01-01 was a Sunday**, so `wday == 0` for day 0 falls out
//! directly with no additional offset needed. (This module's original
//! task brief suggested Thursday as the expected weekday for day 0, which
//! doesn't check out against either calculation above -- likely a mixup
//! with the *Unix* epoch, 1970-01-01, which really was a Thursday. Flagged
//! here rather than silently "fixed" against an unverified premise; if a
//! corpus program's expected output ever disagrees, that's the moment to
//! revisit this with a concrete counter-example instead of in the
//! abstract.)
//!
//! [`CheckDate`]'s real contract (per the Autodoc's `RESULTS`/`BUGS`
//! sections) is "returns the number of seconds from 01-Jan-1978 to that
//! date, or 0 if the ClockData structure contains illegal data" -- so `0`
//! is simultaneously "valid date exactly at the epoch" and "invalid date"
//! in the real API (the Autodoc even calls this out as unresolved -- the
//! `wday` field specifically "is not checked" -- rather than a bug this
//! module should silently paper over); this module's [`check_date_handler`]
//! reproduces that ambiguity faithfully rather than inventing a
//! different, easier-to-use contract.
//!
//! [`CheckDate`]: check_date_handler

use crate::cpu::{AddressRegister, Cpu, DataRegister};
use crate::dispatch::{DispatchError, HandlerContext, LibraryTable};
use crate::lvos::utility::UTILITY_LVOS;
use crate::memory::AddressSpace;

/// `TagItem`/`ClockData` are simple flat structures; only the traversal
/// logic below needs comments beyond their field layout, which is
/// documented in the module docs.
const TAG_DONE: u32 = 0;
const TAG_IGNORE: u32 = 1;
const TAG_MORE: u32 = 2;
const TAG_SKIP: u32 = 3;

/// Size in bytes of one `TagItem` (`ti_Tag: ULONG`, `ti_Data: ULONG`).
const TAG_ITEM_SIZE: u32 = 8;

/// Size in bytes of one `ClockData` (seven `UWORD` fields). Only consumed
/// by this module's own tests (to size heap/`load_end` placements); the
/// handlers themselves compute each field's offset directly.
#[cfg(test)]
const CLOCK_DATA_SIZE: u32 = 14;

/// Amiga "international mode" lowercase mapping -- see the module docs'
/// "Case-insensitive comparison" section for why this convention (rather
/// than plain ASCII) was chosen, and its provenance.
fn amiga_tolower(c: u8) -> u8 {
    if c.is_ascii_uppercase() || ((0xC0..=0xDE).contains(&c) && c != 0xD7) {
        c + 0x20
    } else {
        c
    }
}

/// Amiga "international mode" uppercase mapping -- the inverse of
/// [`amiga_tolower`]; see the module docs. `pub(crate)` so
/// [`crate::dospattern`] can fold case the same way `MatchPatternNoCase`
/// documents (via `utility.library`'s `ToUpper`).
pub(crate) fn amiga_toupper(c: u8) -> u8 {
    if c.is_ascii_lowercase() || ((0xE0..=0xFE).contains(&c) && c != 0xF7) {
        c - 0x20
    } else {
        c
    }
}

/// The shared tag-list traversal primitive. `cur` is the address of the
/// `TagItem` to start looking at (already resolved past any earlier
/// control tags by a previous call, or the caller-supplied list head on
/// the first call). Resolves `TAG_IGNORE`/`TAG_MORE`/`TAG_SKIP` entries
/// transparently and stops at the first real entry or `TAG_DONE`.
///
/// Returns `(found, resume)`: `found` is the address of the real
/// `TagItem` found, or `0` if the list ended (`TAG_DONE`) first; `resume`
/// is where a subsequent call should start looking (only meaningful when
/// `found != 0` -- it's the position just past the found item).
///
/// `cur == 0` (a `NULL` tag list, or a chain that ran off the end without
/// an explicit `TAG_DONE`) is treated the same as hitting `TAG_DONE`
/// immediately -- this is what makes a `NULL` tag-list pointer a legal
/// empty list (see the module docs).
fn next_tag_item_impl<M: AddressSpace>(mem: &M, mut cur: u32) -> (u32, u32) {
    loop {
        if cur == 0 {
            return (0, 0);
        }
        let tag = mem.read_u32(cur);
        match tag {
            TAG_DONE => return (0, 0),
            TAG_IGNORE => cur = cur.wrapping_add(TAG_ITEM_SIZE),
            TAG_MORE => cur = mem.read_u32(cur.wrapping_add(4)),
            TAG_SKIP => {
                let skip = mem.read_u32(cur.wrapping_add(4));
                cur = cur
                    .wrapping_add(TAG_ITEM_SIZE)
                    .wrapping_add(skip.wrapping_mul(TAG_ITEM_SIZE));
            }
            _ => return (cur, cur.wrapping_add(TAG_ITEM_SIZE)),
        }
    }
}

/// Scans a tag list (starting at `list`) for the first `TagItem` whose
/// `ti_Tag` equals `tag_val`, honoring `TAG_MORE`/`TAG_SKIP`/`TAG_IGNORE`
/// traversal via [`next_tag_item_impl`]. Returns the found item's address,
/// or `0` if not found.
fn find_tag_item_impl<M: AddressSpace>(mem: &M, tag_val: u32, list: u32) -> u32 {
    let mut cur = list;
    loop {
        let (found, resume) = next_tag_item_impl(mem, cur);
        if found == 0 {
            return 0;
        }
        if mem.read_u32(found) == tag_val {
            return found;
        }
        cur = resume;
    }
}

/// `FindTagItem` (LVO -30): `D0` = tag value, `A0` = tag list. `D0` =
/// pointer to the matching `TagItem`, or `0` if not found.
fn find_tag_item_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let tag_val = ctx.cpu.data_register(DataRegister(0));
    let list = ctx.cpu.address_register(AddressRegister(0));
    let found = find_tag_item_impl(ctx.mem, tag_val, list);
    ctx.cpu.set_data_register(DataRegister(0), found);
    Ok(())
}

/// `GetTagData` (LVO -36): `D0` = tag value, `D1` = default, `A0` = tag
/// list. `D0` = the found item's `ti_Data`, or `D1` (the default) if not
/// found.
fn get_tag_data_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let tag_val = ctx.cpu.data_register(DataRegister(0));
    let default = ctx.cpu.data_register(DataRegister(1));
    let list = ctx.cpu.address_register(AddressRegister(0));
    let found = find_tag_item_impl(ctx.mem, tag_val, list);
    let result = if found == 0 {
        default
    } else {
        ctx.mem.read_u32(found.wrapping_add(4))
    };
    ctx.cpu.set_data_register(DataRegister(0), result);
    Ok(())
}

/// `NextTagItem` (LVO -48): `A0` = address of a guest `ULONG` holding the
/// current iteration position (updated in place). `D0` = the next real
/// `TagItem`'s address, or `0` at the end of the list. See the module
/// docs' "`NextTagItem`'s iteration contract" section.
fn next_tag_item_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let state_ptr = ctx.cpu.address_register(AddressRegister(0));
    let cur = ctx.mem.read_u32(state_ptr);
    let (found, resume) = next_tag_item_impl(ctx.mem, cur);
    ctx.mem.write_u32(state_ptr, resume);
    ctx.cpu.set_data_register(DataRegister(0), found);
    Ok(())
}

/// Reads a NUL-terminated string and compares it against another,
/// case-insensitively via [`amiga_tolower`], for at most `max_len` bytes
/// (`u32::MAX` for the unbounded `Stricmp` case). Matches the Autodocs'
/// documented behavior for both `Stricmp` and `Strnicmp`: "If the strings
/// have different lengths, the shorter is treated as if it were extended
/// with zeros" -- i.e. comparison continues past one string's NUL
/// (reading `0` bytes from guest memory past it would already read as `0`
/// past the end of `FlatMemory`, but a string's own in-bounds NUL
/// terminator is what this function actually stops on) by comparing NUL
/// against the other string's next byte, which correctly sorts the
/// shorter string first.
fn amiga_str_compare<M: AddressSpace>(mem: &M, a: u32, b: u32, max_len: u32) -> i32 {
    let mut i = 0u32;
    loop {
        if i >= max_len {
            return 0;
        }
        let ca = mem.read_u8(a.wrapping_add(i));
        let cb = mem.read_u8(b.wrapping_add(i));
        let la = amiga_tolower(ca);
        let lb = amiga_tolower(cb);
        if la != lb {
            return la as i32 - lb as i32;
        }
        if ca == 0 {
            // Both bytes are 0 (la == lb == 0 given ca == 0 implies
            // amiga_tolower(0) == 0), i.e. both strings ended here equal.
            return 0;
        }
        i = i.wrapping_add(1);
    }
}

/// `Stricmp` (LVO -162): `A0` = string1, `A1` = string2. `D0` = `<0`/`0`/
/// `>0` per [`amiga_str_compare`] (unbounded).
fn stricmp_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let a = ctx.cpu.address_register(AddressRegister(0));
    let b = ctx.cpu.address_register(AddressRegister(1));
    let result = amiga_str_compare(ctx.mem, a, b, u32::MAX);
    ctx.cpu.set_data_register(DataRegister(0), result as u32);
    Ok(())
}

/// `Strnicmp` (LVO -168): `A0` = string1, `A1` = string2, `D0` = max
/// length. `D0` = `<0`/`0`/`>0` per [`amiga_str_compare`], bounded to at
/// most `length` bytes.
fn strnicmp_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let a = ctx.cpu.address_register(AddressRegister(0));
    let b = ctx.cpu.address_register(AddressRegister(1));
    let length = ctx.cpu.data_register(DataRegister(0));
    let result = amiga_str_compare(ctx.mem, a, b, length);
    ctx.cpu.set_data_register(DataRegister(0), result as u32);
    Ok(())
}

/// `ToUpper` (LVO -174): `D0` = character (low byte). `D0` = the
/// uppercased character, per [`amiga_toupper`].
fn to_upper_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let c = ctx.cpu.data_register(DataRegister(0)) as u8;
    ctx.cpu
        .set_data_register(DataRegister(0), amiga_toupper(c) as u32);
    Ok(())
}

/// `ToLower` (LVO -180): `D0` = character (low byte). `D0` = the
/// lowercased character, per [`amiga_tolower`].
fn to_lower_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let c = ctx.cpu.data_register(DataRegister(0)) as u8;
    ctx.cpu
        .set_data_register(DataRegister(0), amiga_tolower(c) as u32);
    Ok(())
}

/// One decoded `ClockData` (see the module docs for field order/units).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClockData {
    sec: u16,
    min: u16,
    hour: u16,
    mday: u16,
    month: u16,
    year: u16,
    wday: u16,
}

/// Reads a `ClockData` structure out of guest memory at `addr`.
fn read_clock_data<M: AddressSpace>(mem: &M, addr: u32) -> ClockData {
    ClockData {
        sec: mem.read_u16(addr),
        min: mem.read_u16(addr.wrapping_add(2)),
        hour: mem.read_u16(addr.wrapping_add(4)),
        mday: mem.read_u16(addr.wrapping_add(6)),
        month: mem.read_u16(addr.wrapping_add(8)),
        year: mem.read_u16(addr.wrapping_add(10)),
        wday: mem.read_u16(addr.wrapping_add(12)),
    }
}

/// Writes a `ClockData` structure into guest memory at `addr`.
fn write_clock_data<M: AddressSpace>(mem: &mut M, addr: u32, cd: &ClockData) {
    mem.write_u16(addr, cd.sec);
    mem.write_u16(addr.wrapping_add(2), cd.min);
    mem.write_u16(addr.wrapping_add(4), cd.hour);
    mem.write_u16(addr.wrapping_add(6), cd.mday);
    mem.write_u16(addr.wrapping_add(8), cd.month);
    mem.write_u16(addr.wrapping_add(10), cd.year);
    mem.write_u16(addr.wrapping_add(12), cd.wday);
}

/// `true` if `year` (a real, full Gregorian year, e.g. `1978`) is a leap
/// year.
fn is_leap_year(year: u32) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

/// Number of days in `month` (1-12) of `year`. Returns `0` for an
/// out-of-range month (callers treat that as an invalid date).
fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// The AMIGA epoch: 1978-01-01, verified (see the module docs) to be a
/// Sunday, i.e. `wday == 0` for `days_since_epoch == 0` under this
/// module's `0 == Sunday` convention with no extra offset needed.
const EPOCH_YEAR: u32 = 1978;

/// Converts a day count since [`EPOCH_YEAR`]-01-01 into `(year, month,
/// mday)` (`month`/`mday` both 1-based). Shared with
/// [`crate::dosdatestr`]'s `DateToStr`, which needs the same
/// day-count-to-calendar-date math for a `DateStamp`'s `ds_Days`.
pub(crate) fn days_to_ymd(days: u32) -> (u32, u32, u32) {
    let mut year = EPOCH_YEAR;
    let mut remaining = days;
    loop {
        let year_len = if is_leap_year(year) { 366 } else { 365 };
        if remaining < year_len {
            break;
        }
        remaining -= year_len;
        year += 1;
    }
    let mut month = 1;
    loop {
        let month_len = days_in_month(year, month);
        if remaining < month_len {
            break;
        }
        remaining -= month_len;
        month += 1;
    }
    (year, month, remaining + 1)
}

/// Converts `(year, month, mday)` back into a day count since
/// [`EPOCH_YEAR`]-01-01. Does not validate the inputs -- callers that need
/// validation (`CheckDate`) do it separately; this matches real
/// `Date2Amiga`'s own documented "does no sanity checking" behavior.
fn ymd_to_days(year: u32, month: u32, mday: u32) -> u32 {
    let mut days = 0u32;
    for y in EPOCH_YEAR..year {
        days += if is_leap_year(y) { 366 } else { 365 };
    }
    for m in 1..month.min(13) {
        days += days_in_month(year, m);
    }
    days + mday.saturating_sub(1)
}

/// Converts a `ClockData` (assumed valid) to seconds since the epoch.
fn clock_data_to_seconds(cd: &ClockData) -> u32 {
    let days = ymd_to_days(cd.year as u32, cd.month as u32, cd.mday as u32);
    days.wrapping_mul(86400)
        + (cd.hour as u32).wrapping_mul(3600)
        + (cd.min as u32).wrapping_mul(60)
        + cd.sec as u32
}

/// Converts a seconds-since-epoch count to a full `ClockData`, including
/// `wday` (`0 == Sunday`, see the module docs).
fn seconds_to_clock_data(seconds: u32) -> ClockData {
    let days = seconds / 86400;
    let rest = seconds % 86400;
    let hour = rest / 3600;
    let min = (rest % 3600) / 60;
    let sec = rest % 60;
    let (year, month, mday) = days_to_ymd(days);
    let wday = days % 7;
    ClockData {
        sec: sec as u16,
        min: min as u16,
        hour: hour as u16,
        mday: mday as u16,
        month: month as u16,
        year: year as u16,
        wday: wday as u16,
    }
}

/// Whether a `ClockData`'s `sec`/`min`/`hour`/`mday`/`month`/`year` fields
/// (everything except `wday` -- see the module docs' `CheckDate` note)
/// describe a legal date/time. `year` must be at or after [`EPOCH_YEAR`]
/// (this runtime, like real `Amiga2Date`, has no representation for a
/// pre-epoch date via this seconds-since-epoch encoding).
fn clock_data_is_valid(cd: &ClockData) -> bool {
    if (cd.year as u32) < EPOCH_YEAR {
        return false;
    }
    if cd.month == 0 || cd.month > 12 {
        return false;
    }
    let dim = days_in_month(cd.year as u32, cd.month as u32);
    if cd.mday == 0 || cd.mday as u32 > dim {
        return false;
    }
    if cd.hour > 23 {
        return false;
    }
    if cd.min > 59 {
        return false;
    }
    if cd.sec > 59 {
        return false;
    }
    true
}

/// `Amiga2Date` (LVO -120): `D0` = seconds since the epoch, `A0` = pointer
/// to a `ClockData` to fill in. No result register -- matches the real
/// `VOID Amiga2Date(ULONG, struct ClockData *)` signature (this runtime's
/// A-line dispatch doesn't need to special-case `VOID` LVOs; `D0` is
/// simply left whatever it already was).
fn amiga2date_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let seconds = ctx.cpu.data_register(DataRegister(0));
    let result_ptr = ctx.cpu.address_register(AddressRegister(0));
    let cd = seconds_to_clock_data(seconds);
    write_clock_data(ctx.mem, result_ptr, &cd);
    Ok(())
}

/// `Date2Amiga` (LVO -126): `A0` = pointer to a filled-in `ClockData`.
/// `D0` = seconds since the epoch, computed without validation (matching
/// the real Autodoc's `WARNING`: "does no sanity checking of the data in
/// the ClockData structure").
fn date2amiga_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let date_ptr = ctx.cpu.address_register(AddressRegister(0));
    let cd = read_clock_data(ctx.mem, date_ptr);
    let seconds = clock_data_to_seconds(&cd);
    ctx.cpu.set_data_register(DataRegister(0), seconds);
    Ok(())
}

/// `CheckDate` (LVO -132): `A0` = pointer to a `ClockData`. `D0` = seconds
/// since the epoch if the date is valid, or `0` if it's not (see the
/// module docs for the real API's documented `0`-means-both-things
/// ambiguity, faithfully reproduced here rather than resolved).
fn check_date_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let date_ptr = ctx.cpu.address_register(AddressRegister(0));
    let cd = read_clock_data(ctx.mem, date_ptr);
    let seconds = if clock_data_is_valid(&cd) {
        clock_data_to_seconds(&cd)
    } else {
        0
    };
    ctx.cpu.set_data_register(DataRegister(0), seconds);
    Ok(())
}

/// Registers every implemented `utility.library` handler onto
/// [`crate::dispatch::UTILITY_LIBRARY_BASE`], looked up by name through
/// [`UTILITY_LVOS`] (the T7-style generated table), following
/// [`crate::execmem::register_execmem_handlers`]'s registration pattern.
/// Called unconditionally from [`crate::dispatch::Runtime::new`].
pub fn register_utility_handlers<C: Cpu + 'static>(
    table: &mut LibraryTable<C>,
    mem: &mut C::Memory,
) {
    macro_rules! reg {
        ($name:literal, $handler:expr) => {
            table
                .register_by_name(
                    mem,
                    crate::dispatch::UTILITY_LIBRARY_BASE,
                    UTILITY_LVOS,
                    "utility.library",
                    $name,
                    $handler,
                )
                .unwrap_or_else(|e| panic!("{} should be in UTILITY_LVOS: {e}", $name));
        };
    }
    reg!("FindTagItem", find_tag_item_handler::<C>);
    reg!("GetTagData", get_tag_data_handler::<C>);
    reg!("NextTagItem", next_tag_item_handler::<C>);
    reg!("Stricmp", stricmp_handler::<C>);
    reg!("Strnicmp", strnicmp_handler::<C>);
    reg!("ToUpper", to_upper_handler::<C>);
    reg!("ToLower", to_lower_handler::<C>);
    reg!("Amiga2Date", amiga2date_handler::<C>);
    reg!("Date2Amiga", date2amiga_handler::<C>);
    reg!("CheckDate", check_date_handler::<C>);
}

#[cfg(test)]
#[allow(clippy::vec_init_then_push)]
mod tests {
    use super::*;
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{Runtime, StartConfig, UTILITY_LIBRARY_BASE};
    use crate::memory::FlatMemory;

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

    /// `move.l #imm32,An`.
    fn move_imm_to_a(n: u16) -> u16 {
        0x207C | (n << 9)
    }

    /// `movea.l #UTILITY_LIBRARY_BASE,a6`.
    fn movea_utility_base_to_a6() -> [u16; 3] {
        [
            move_imm_to_a(6),
            (UTILITY_LIBRARY_BASE >> 16) as u16,
            UTILITY_LIBRARY_BASE as u16,
        ]
    }

    /// `jsr <disp16>(a6)`.
    fn jsr_disp16_a6(disp: i32) -> [u16; 2] {
        [0x4EAE, disp as u16]
    }

    const RTS: u16 = 0x4E75;

    fn runtime_with_program(words: &[u16]) -> Runtime<M68kCpu> {
        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, words);
        let load_end = entry + 0x400;
        Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end,
                args: Vec::new(),
                ..StartConfig::default()
            },
        )
    }

    /// Prepends the A6-fixup to `words` before building the runtime.
    fn program(words: &[u16]) -> Runtime<M68kCpu> {
        let mut full = movea_utility_base_to_a6().to_vec();
        full.extend_from_slice(words);
        runtime_with_program(&full)
    }

    // --- tag-list traversal (host-side, no CPU program needed) ---

    #[test]
    fn get_tag_data_found_and_default() {
        let mut mem = FlatMemory::new(0x1000);
        // list: {10, 100}, {20, 200}, TAG_DONE
        let list = 0x100u32;
        mem.write_u32(list, 10);
        mem.write_u32(list + 4, 100);
        mem.write_u32(list + 8, 20);
        mem.write_u32(list + 12, 200);
        mem.write_u32(list + 16, TAG_DONE);
        mem.write_u32(list + 20, 0);

        assert_eq!(find_tag_item_impl(&mem, 20, list), list + 8);
        assert_eq!(find_tag_item_impl(&mem, 99, list), 0);

        let ctx_result_found = {
            // Directly exercise the same logic get_tag_data_handler uses.
            let found = find_tag_item_impl(&mem, 10, list);
            assert_ne!(found, 0);
            mem.read_u32(found + 4)
        };
        assert_eq!(ctx_result_found, 100);
    }

    #[test]
    fn find_tag_item_handles_tag_more_chain_and_skip_and_ignore() {
        let mut mem = FlatMemory::new(0x1000);
        // First array: {TAG_IGNORE, 0}, {TAG_SKIP, 1 (skip one)}, {999,
        // should be skipped}, {TAG_MORE, ptr to second array}
        let list = 0x100u32;
        let second = 0x200u32;

        mem.write_u32(list, TAG_IGNORE);
        mem.write_u32(list + 4, 0);
        mem.write_u32(list + 8, TAG_SKIP);
        mem.write_u32(list + 12, 1); // skip 1 entry after this one
        mem.write_u32(list + 16, 999); // skipped
        mem.write_u32(list + 20, 12345); // skipped's data, irrelevant
        mem.write_u32(list + 24, TAG_MORE);
        mem.write_u32(list + 28, second);

        // Second array: {42, 4242}, TAG_DONE
        mem.write_u32(second, 42);
        mem.write_u32(second + 4, 4242);
        mem.write_u32(second + 8, TAG_DONE);
        mem.write_u32(second + 12, 0);

        // 999 must NOT be findable (it was skipped over).
        assert_eq!(find_tag_item_impl(&mem, 999, list), 0);
        // 42 (in the continuation array) must be findable.
        assert_eq!(find_tag_item_impl(&mem, 42, list), second);
    }

    #[test]
    fn null_tag_list_is_a_legal_empty_list() {
        let mem = FlatMemory::new(0x100);
        assert_eq!(find_tag_item_impl(&mem, 1, 0), 0);
    }

    #[test]
    fn next_tag_item_iterates_to_completion() {
        let mut mem = FlatMemory::new(0x1000);
        let list = 0x100u32;
        mem.write_u32(list, 1000);
        mem.write_u32(list + 4, 1);
        mem.write_u32(list + 8, 2000);
        mem.write_u32(list + 12, 2);
        mem.write_u32(list + 16, TAG_DONE);
        mem.write_u32(list + 20, 0);

        let mut cur = list;
        let mut seen = Vec::new();
        loop {
            let (found, resume) = next_tag_item_impl(&mem, cur);
            if found == 0 {
                break;
            }
            seen.push((mem.read_u32(found), mem.read_u32(found + 4)));
            cur = resume;
        }
        assert_eq!(seen, vec![(1000, 1), (2000, 2)]);
    }

    // --- tag-list helpers via the actual jump-table dispatch ---

    #[test]
    fn get_tag_data_handler_finds_value_via_dispatch() {
        // Build a tag list right after the code, call GetTagData(20, 999,
        // list) via jsr -36(a6), exit code = D0.
        let entry = TRAP_TABLE_END;
        let mut words = movea_utility_base_to_a6().to_vec();
        // D0 = tagVal (20)
        words.push(move_imm_to_d(0));
        words.push(0);
        words.push(20);
        // D1 = default (999)
        words.push(move_imm_to_d(1));
        words.push(0);
        words.push(999);
        // A0 = list addr, patched below
        words.push(move_imm_to_a(0));
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-36)); // GetTagData
        words.push(RTS);

        let list_addr = entry + (words.len() as u32) * 2;
        words[10] = (list_addr >> 16) as u16;
        words[11] = list_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        mem.write_u32(list_addr, 10);
        mem.write_u32(list_addr + 4, 100);
        mem.write_u32(list_addr + 8, 20);
        mem.write_u32(list_addr + 12, 200);
        mem.write_u32(list_addr + 16, TAG_DONE);
        mem.write_u32(list_addr + 20, 0);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: list_addr + 24,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 200);
    }

    #[test]
    fn get_tag_data_handler_returns_default_when_not_found() {
        let entry = TRAP_TABLE_END;
        let mut words = movea_utility_base_to_a6().to_vec();
        words.push(move_imm_to_d(0)); // D0 = tagVal (999, absent)
        words.push(0);
        words.push(999);
        words.push(move_imm_to_d(1)); // D1 = default (777)
        words.push(0);
        words.push(777);
        words.push(move_imm_to_a(0)); // A0 = list, patched below
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-36)); // GetTagData
        words.push(RTS);

        let list_addr = entry + (words.len() as u32) * 2;
        words[10] = (list_addr >> 16) as u16;
        words[11] = list_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        mem.write_u32(list_addr, TAG_DONE);
        mem.write_u32(list_addr + 4, 0);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: list_addr + 8,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 777);
    }

    // --- Stricmp/Strnicmp ---

    #[test]
    fn stricmp_equal_ignores_case() {
        let mut mem = FlatMemory::new(0x100);
        crate::guestmem::write_c_string(&mut mem, 0x10, b"Hello");
        crate::guestmem::write_c_string(&mut mem, 0x20, b"hELLO");
        assert_eq!(amiga_str_compare(&mem, 0x10, 0x20, u32::MAX), 0);
    }

    #[test]
    fn stricmp_less_and_greater() {
        let mut mem = FlatMemory::new(0x100);
        crate::guestmem::write_c_string(&mut mem, 0x10, b"apple");
        crate::guestmem::write_c_string(&mut mem, 0x20, b"Banana");
        assert!(amiga_str_compare(&mem, 0x10, 0x20, u32::MAX) < 0);
        assert!(amiga_str_compare(&mem, 0x20, 0x10, u32::MAX) > 0);
    }

    #[test]
    fn strnicmp_length_limits_the_comparison() {
        let mut mem = FlatMemory::new(0x100);
        crate::guestmem::write_c_string(&mut mem, 0x10, b"HELLOworld");
        crate::guestmem::write_c_string(&mut mem, 0x20, b"helloEVERYONE");
        // First 5 chars equal case-insensitively; beyond that they
        // differ, but a length of 5 shouldn't look that far.
        assert_eq!(amiga_str_compare(&mem, 0x10, 0x20, 5), 0);
        assert_ne!(amiga_str_compare(&mem, 0x10, 0x20, 10), 0);
    }

    #[test]
    fn stricmp_via_dispatch() {
        let entry = TRAP_TABLE_END;
        let mut words = movea_utility_base_to_a6().to_vec();
        words.push(move_imm_to_a(0)); // A0 = str1, patched
        words.push(0);
        words.push(0);
        words.push(move_imm_to_a(1)); // A1 = str2, patched
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-162)); // Stricmp
        words.push(RTS);

        let str1 = entry + (words.len() as u32) * 2;
        let str2 = str1 + 8;
        words[4] = (str1 >> 16) as u16;
        words[5] = str1 as u16;
        words[7] = (str2 >> 16) as u16;
        words[8] = str2 as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        crate::guestmem::write_c_string(&mut mem, str1, b"AMIGA");
        crate::guestmem::write_c_string(&mut mem, str2, b"amiga");

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: str2 + 8,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 0);
    }

    // --- ToUpper/ToLower ---

    #[test]
    fn to_upper_and_to_lower_ascii_and_latin1() {
        assert_eq!(amiga_toupper(b'a'), b'A');
        assert_eq!(amiga_tolower(b'A'), b'a');
        assert_eq!(amiga_toupper(b'5'), b'5');
        // Latin-1: e-grave (lower 0xE8) <-> E-grave (upper 0xC8).
        assert_eq!(amiga_toupper(0xE8), 0xC8);
        assert_eq!(amiga_tolower(0xC8), 0xE8);
        // Multiplication/division signs are excluded from the mapping.
        assert_eq!(amiga_tolower(0xD7), 0xD7);
        assert_eq!(amiga_toupper(0xF7), 0xF7);
    }

    #[test]
    fn to_upper_via_dispatch() {
        let mut words = Vec::new();
        words.push(move_imm_to_d(0)); // D0 = 'a'
        words.push(0);
        words.push(b'a' as u16);
        words.extend_from_slice(&jsr_disp16_a6(-174)); // ToUpper
        words.push(RTS);
        let mut rt = program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code as u8, b'A');
    }

    // --- date helpers ---

    #[test]
    fn epoch_day_zero_is_a_sunday() {
        let cd = seconds_to_clock_data(0);
        assert_eq!((cd.year, cd.month, cd.mday), (1978, 1, 1));
        assert_eq!(cd.wday, 0, "1978-01-01 should be wday 0 (Sunday)");
    }

    #[test]
    fn amiga2date_date2amiga_round_trip_known_date() {
        // 1978-01-02 00:00:00 = 86400 seconds since the epoch.
        let seconds = 86400u32;
        let cd = seconds_to_clock_data(seconds);
        assert_eq!((cd.year, cd.month, cd.mday), (1978, 1, 2));
        assert_eq!(cd.wday, 1, "1978-01-02 should be wday 1 (Monday)");
        assert_eq!(clock_data_to_seconds(&cd), seconds);
    }

    #[test]
    fn amiga2date_date2amiga_round_trip_leap_year_date() {
        // 1980 is a leap year; Feb 29 1980 exists. Compute its day offset
        // by hand: 1978 (365) + 1979 (365) + Jan (31) + 28 days into Feb
        // = 730 + 31 + 28 = 789 days since the epoch.
        let days = 730 + 31 + 28;
        let seconds = days * 86400 + 3661; // + 01:01:01
        let cd = seconds_to_clock_data(seconds);
        assert_eq!((cd.year, cd.month, cd.mday), (1980, 2, 29));
        assert_eq!((cd.hour, cd.min, cd.sec), (1, 1, 1));
        assert_eq!(clock_data_to_seconds(&cd), seconds);
    }

    #[test]
    fn check_date_valid_and_invalid() {
        let valid = ClockData {
            sec: 0,
            min: 0,
            hour: 0,
            mday: 29,
            month: 2,
            year: 1980,
            wday: 0,
        };
        assert!(clock_data_is_valid(&valid));

        // 1979 was not a leap year: Feb 29 1979 doesn't exist.
        let invalid_mday = ClockData {
            year: 1979,
            month: 2,
            mday: 29,
            ..valid
        };
        assert!(!clock_data_is_valid(&invalid_mday));

        let invalid_month = ClockData { month: 13, ..valid };
        assert!(!clock_data_is_valid(&invalid_month));

        let invalid_hour = ClockData {
            hour: 24,
            mday: 1,
            month: 1,
            ..valid
        };
        assert!(!clock_data_is_valid(&invalid_hour));

        let pre_epoch = ClockData {
            year: 1977,
            ..valid
        };
        assert!(!clock_data_is_valid(&pre_epoch));
    }

    #[test]
    fn amiga2date_handler_via_dispatch() {
        let mut words = Vec::new();
        // D0 = 86400 (one day)
        words.push(move_imm_to_d(0));
        words.push((86400u32 >> 16) as u16);
        words.push(86400u32 as u16);
        // A0 = result buffer, patched
        words.push(move_imm_to_a(0));
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-120)); // Amiga2Date
        words.push(RTS);

        let entry = TRAP_TABLE_END;
        let full_len_before_patch = movea_utility_base_to_a6().len() + words.len();
        let result_addr = entry + (full_len_before_patch as u32) * 2;
        words[4] = (result_addr >> 16) as u16;
        words[5] = result_addr as u16;

        let mut full = movea_utility_base_to_a6().to_vec();
        full.extend_from_slice(&words);

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &full);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: result_addr + CLOCK_DATA_SIZE,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed");
        let cd = read_clock_data(rt.memory(), result_addr);
        assert_eq!((cd.year, cd.month, cd.mday), (1978, 1, 2));
        assert_eq!(cd.wday, 1);
    }

    #[test]
    fn date2amiga_handler_via_dispatch() {
        let mut words = Vec::new();
        // A0 = ClockData buffer, patched
        words.push(move_imm_to_a(0));
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-126)); // Date2Amiga
        words.push(RTS);

        let entry = TRAP_TABLE_END;
        let full_len_before_patch = movea_utility_base_to_a6().len() + words.len();
        let cd_addr = entry + (full_len_before_patch as u32) * 2;
        words[1] = (cd_addr >> 16) as u16;
        words[2] = cd_addr as u16;

        let mut full = movea_utility_base_to_a6().to_vec();
        full.extend_from_slice(&words);

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &full);
        write_clock_data(
            &mut mem,
            cd_addr,
            &ClockData {
                sec: 0,
                min: 0,
                hour: 0,
                mday: 2,
                month: 1,
                year: 1978,
                wday: 0,
            },
        );

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: cd_addr + CLOCK_DATA_SIZE,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code as u32, 86400);
    }

    #[test]
    fn check_date_handler_via_dispatch_valid_and_invalid() {
        // Valid: 1978-01-01 00:00:00 -> D0 == 0 too (epoch itself), so
        // use 1978-01-02 to disambiguate valid-and-nonzero from the
        // documented "0 means invalid" case.
        let mut words = Vec::new();
        words.push(move_imm_to_a(0)); // A0 = buffer, patched
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-132)); // CheckDate
        words.push(RTS);

        let entry = TRAP_TABLE_END;
        let full_len_before_patch = movea_utility_base_to_a6().len() + words.len();
        let cd_addr = entry + (full_len_before_patch as u32) * 2;
        words[1] = (cd_addr >> 16) as u16;
        words[2] = cd_addr as u16;

        let mut full = movea_utility_base_to_a6().to_vec();
        full.extend_from_slice(&words);

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &full);
        write_clock_data(
            &mut mem,
            cd_addr,
            &ClockData {
                sec: 0,
                min: 0,
                hour: 0,
                mday: 2,
                month: 1,
                year: 1978,
                wday: 0,
            },
        );

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: cd_addr + CLOCK_DATA_SIZE,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code as u32, 86400, "valid date should return its seconds");

        // Invalid: month 13.
        let mut mem2 = FlatMemory::new(0x2_0000);
        load_words(&mut mem2, entry, &full);
        write_clock_data(
            &mut mem2,
            cd_addr,
            &ClockData {
                sec: 0,
                min: 0,
                hour: 0,
                mday: 1,
                month: 13,
                year: 1978,
                wday: 0,
            },
        );
        let mut rt2 = Runtime::new(
            M68kCpu::new(),
            mem2,
            StartConfig {
                entry,
                load_end: cd_addr + CLOCK_DATA_SIZE,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out2 = Vec::new();
        let code2 = rt2.run(&mut out2, None).expect("run should succeed");
        assert_eq!(code2, 0, "invalid date should return 0");
    }
}
