//! `dos.library` `DateToStr`/`StrToDate`: converts between a
//! `DateStamp` and human-readable day-of-week/date/time strings, in
//! both directions.
//!
//! `DateToStr` found missing while running the real Workbench 3.1.4
//! `C:/List` binary: it calls this for every directory entry's
//! timestamp. `StrToDate` found missing while running the real
//! `C:/Date` binary against an explicit `Date 15-mar-95 12:00:00`
//! argument (its no-argument "print the current date" form only needs
//! `DateToStr`, and already worked).
//!
//! # `struct DateTime` (`dos/datetime.h`)
//!
//! ```text
//! struct DateTime {
//!     struct DateStamp dat_Stamp;   // offset 0, 12 bytes
//!     UBYTE   dat_Format;           // offset 12
//!     UBYTE   dat_Flags;            // offset 13
//!     UBYTE   *dat_StrDay;          // offset 14
//!     UBYTE   *dat_StrDate;         // offset 18
//!     UBYTE   *dat_StrTime;         // offset 22
//! };
//! ```
//! 26 bytes total. Classic AmigaOS structs pack to even-byte (not
//! 4-byte) boundaries, so no padding falls between `dat_Flags` and
//! `dat_StrDay`.
//!
//! # Scope
//!
//! `dat_Format`'s `FORMAT_DEF` falls back to `FORMAT_DOS` (per the
//! RKRM: "Otherwise, it falls back to `FORMAT_DOS`" when no
//! `locale.library` is installed, which this runtime never has).
//! `DateToStr` honors `DTF_SUBST` (`List` enables it by default for
//! its own date column), comparing against
//! [`crate::dosdate::now_as_datestamp`]; `StrToDate` honors
//! `DTF_FUTURE` (weekday names resolve to a future vs. past date) --
//! each flag only applies to its own direction, matching the RKRM's
//! own documented split ("`DTF_SUBST` is only honored when converting
//! ... to human-readable strings", "`DTF_FUTURE` is only honored when
//! converting a string to the AmigaDOS representation").
//! `StrToDate`'s weekday-name resolution when *today* is already the
//! requested weekday (e.g. asking for "Wednesday" on a Wednesday) is
//! resolved to a full week away (7 days back/forward, matching the
//! everyday-language reading of "last/next Wednesday" said on a
//! Wednesday) rather than "today" -- not pinned down by the RKRM
//! prose, so documented here as this runtime's own choice.

use crate::cpu::{Cpu, DataRegister};
use crate::dispatch::{DOS_LIBRARY_BASE, DispatchError, HandlerContext, LibraryTable};
use crate::dosdate::now_as_datestamp;
use crate::guestmem::{read_c_string, write_c_string};
use crate::lvos::dos::DOS_LVOS;
use crate::memory::AddressSpace;
use crate::utility::{days_to_ymd, ymd_to_days};

const DOSTRUE: u32 = 0xFFFF_FFFF;
const DOSFALSE: u32 = 0;

const DAT_STAMP_OFFSET: u32 = 0;
const DAT_FORMAT_OFFSET: u32 = 12;
const DAT_FLAGS_OFFSET: u32 = 13;
const DAT_STRDAY_OFFSET: u32 = 14;
const DAT_STRDATE_OFFSET: u32 = 18;
const DAT_STRTIME_OFFSET: u32 = 22;

#[allow(dead_code)] // documents the value; format_date's fallback arm covers it
const FORMAT_DOS: u8 = 0;
const FORMAT_INT: u8 = 1;
const FORMAT_USA: u8 = 2;
const FORMAT_CDN: u8 = 3;

const DTF_SUBST: u8 = 0x01;
const DTF_FUTURE: u8 = 0x02;

const WEEKDAY_NAMES: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
const MONTH_ABBR: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Renders `(year, month, mday)` per `format` (falling back to
/// [`FORMAT_DOS`]'s style for `FORMAT_DEF` and any unrecognized value).
fn format_date(year: u32, month: u32, mday: u32, format: u8) -> String {
    let yy = year % 100;
    match format {
        FORMAT_INT => format!("{yy:02}-{month:02}-{mday:02}"),
        FORMAT_USA => format!("{month:02}-{mday:02}-{yy:02}"),
        FORMAT_CDN => format!("{mday:02}-{month:02}-{yy:02}"),
        // FORMAT_DOS, FORMAT_DEF (falls back to FORMAT_DOS with no
        // locale.library), and any unrecognized value.
        _ => format!(
            "{mday:02}-{}-{yy:02}",
            MONTH_ABBR[(month as usize - 1).min(11)]
        ),
    }
}

/// Core of `DateToStr`: `(day_name, date_str, time_str)`, or `None` if
/// `ds_Minute`/`ds_Tick` are out of range (real `DateToStr`'s only
/// documented failure case).
fn render_date_time(
    days: i32,
    minute: i32,
    tick: i32,
    format: u8,
    flags: u8,
) -> Option<(String, String, String)> {
    if !(0..1440).contains(&minute) || !(0..3000).contains(&tick) {
        return None;
    }

    let (year, month, mday) = days_to_ymd(days.max(0) as u32);
    let wday = (days as i64).rem_euclid(7) as usize;
    let day_name = WEEKDAY_NAMES[wday].to_string();

    let date_str = if flags & DTF_SUBST != 0 {
        let (today_days, _, _) = now_as_datestamp();
        let diff = i64::from(days) - i64::from(today_days);
        match diff {
            0 => "Today".to_string(),
            1 => "Tomorrow".to_string(),
            -1 => "Yesterday".to_string(),
            -7..=-2 => day_name.clone(),
            d if d > 1 => "Future".to_string(),
            _ => format_date(year, month, mday, format),
        }
    } else {
        format_date(year, month, mday, format)
    };

    let hour = minute / 60;
    let min = minute % 60;
    let sec = tick / 50;
    let time_str = format!("{hour:02}:{min:02}:{sec:02}");

    Some((day_name, date_str, time_str))
}

/// Interprets a two-digit year per the RKRM's documented `StrToDate`
/// rule: `78..=99` -> `1978..=1999`, `00..=45` -> `2000..=2045`, any
/// other value is refused (real `StrToDate`'s ROM code, before any
/// `locale.library` four-digit-year patch, which this runtime never
/// has -- see the module docs).
fn expand_two_digit_year(yy: u32) -> Option<u32> {
    match yy {
        78..=99 => Some(1900 + yy),
        0..=45 => Some(2000 + yy),
        _ => None,
    }
}

/// Core of `StrToDate`'s date-string half: parses `s` per `format`
/// (`FORMAT_INT`/`USA`/`CDN`, or `FORMAT_DOS`/`DEF`'s `dd-Mon-yy` for
/// anything else), or a relative word ("Today"/"Tomorrow"/"Yesterday"/
/// a weekday name, honored for every format, matching real
/// `StrToDate`). Returns a day count since the AmigaOS epoch, or
/// `None` if `s` isn't a recognized date. `flags & DTF_FUTURE`
/// controls which direction a bare weekday name resolves -- see the
/// module docs for the "today is that weekday" edge case.
fn parse_date_string(s: &str, format: u8, flags: u8, today_days: i32) -> Option<i32> {
    let s = s.trim();
    match s.to_ascii_lowercase().as_str() {
        "today" => return Some(today_days),
        "tomorrow" => return Some(today_days + 1),
        "yesterday" => return Some(today_days - 1),
        _ => {}
    }
    if let Some(target_wday) = WEEKDAY_NAMES.iter().position(|w| w.eq_ignore_ascii_case(s)) {
        let today_wday = (today_days as i64).rem_euclid(7);
        let target_wday = target_wday as i64;
        return Some(if flags & DTF_FUTURE != 0 {
            let ahead = (target_wday - today_wday).rem_euclid(7);
            today_days + if ahead == 0 { 7 } else { ahead as i32 }
        } else {
            let ago = (today_wday - target_wday).rem_euclid(7);
            today_days - if ago == 0 { 7 } else { ago as i32 }
        });
    }

    let parts: Vec<&str> = s.split('-').collect();
    let [p0, p1, p2] = parts[..] else {
        return None;
    };
    let (day, month, yy) = match format {
        FORMAT_INT => (p2.parse().ok()?, p1.parse().ok()?, p0.parse().ok()?),
        FORMAT_USA => (p1.parse().ok()?, p0.parse().ok()?, p2.parse().ok()?),
        FORMAT_CDN => (p0.parse().ok()?, p1.parse().ok()?, p2.parse().ok()?),
        // FORMAT_DOS, FORMAT_DEF, and any unrecognized value.
        _ => {
            let month = MONTH_ABBR.iter().position(|m| m.eq_ignore_ascii_case(p1))? as u32 + 1;
            (p0.parse().ok()?, month, p2.parse().ok()?)
        }
    };
    let year = expand_two_digit_year(yy)?;
    if !(1..=12).contains(&month) || day < 1 {
        return None;
    }
    Some(ymd_to_days(year, month, day) as i32)
}

/// Core of `StrToDate`'s time-string half: `"HH:MM:SS"`, 24h clock,
/// always this format regardless of `dat_Format` (per the RKRM).
/// Returns `(ds_Minute, ds_Tick)`, or `None` if `s` isn't well-formed
/// or any component is out of range.
fn parse_time_string(s: &str) -> Option<(i32, i32)> {
    let parts: Vec<&str> = s.trim().split(':').collect();
    let [h, m, sec] = parts[..] else {
        return None;
    };
    let h: i32 = h.parse().ok()?;
    let m: i32 = m.parse().ok()?;
    let sec: i32 = sec.parse().ok()?;
    if !(0..24).contains(&h) || !(0..60).contains(&m) || !(0..60).contains(&sec) {
        return None;
    }
    Some((h * 60 + m, sec * 50))
}

/// `DateToStr` (`D1` = `struct DateTime*`). `D0` = `DOSTRUE`/`DOSFALSE`.
/// Fills whichever of `dat_StrDay`/`dat_StrDate`/`dat_StrTime` are
/// non-`NULL`. Doesn't touch `IoErr()`, matching the real function.
fn date_to_str_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let dt_addr = ctx.cpu.data_register(DataRegister(1));

    let days = ctx.mem.read_u32(dt_addr.wrapping_add(DAT_STAMP_OFFSET)) as i32;
    let minute = ctx.mem.read_u32(dt_addr.wrapping_add(DAT_STAMP_OFFSET + 4)) as i32;
    let tick = ctx.mem.read_u32(dt_addr.wrapping_add(DAT_STAMP_OFFSET + 8)) as i32;
    let format = ctx.mem.read_u8(dt_addr.wrapping_add(DAT_FORMAT_OFFSET));
    let flags = ctx.mem.read_u8(dt_addr.wrapping_add(DAT_FLAGS_OFFSET));

    let Some((day_name, date_str, time_str)) = render_date_time(days, minute, tick, format, flags)
    else {
        ctx.cpu.set_data_register(DataRegister(0), DOSFALSE);
        return Ok(());
    };

    let day_ptr = ctx.mem.read_u32(dt_addr.wrapping_add(DAT_STRDAY_OFFSET));
    if day_ptr != 0 {
        write_c_string(ctx.mem, day_ptr, day_name.as_bytes());
    }
    let date_ptr = ctx.mem.read_u32(dt_addr.wrapping_add(DAT_STRDATE_OFFSET));
    if date_ptr != 0 {
        write_c_string(ctx.mem, date_ptr, date_str.as_bytes());
    }
    let time_ptr = ctx.mem.read_u32(dt_addr.wrapping_add(DAT_STRTIME_OFFSET));
    if time_ptr != 0 {
        write_c_string(ctx.mem, time_ptr, time_str.as_bytes());
    }

    ctx.cpu.set_data_register(DataRegister(0), DOSTRUE);
    Ok(())
}

/// `StrToDate` (`D1` = `struct DateTime*`). `D0` = `DOSTRUE`/`DOSFALSE`.
/// Parses whichever of `dat_StrDate`/`dat_StrTime` are non-`NULL` into
/// `dat_Stamp`'s `ds_Days`/(`ds_Minute`,`ds_Tick`) respectively,
/// leaving the corresponding field(s) of the `dat_Stamp` passed in
/// unaltered when the pointer is `NULL` (per the RKRM). Doesn't touch
/// `IoErr()`, matching the real function.
fn str_to_date_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let dt_addr = ctx.cpu.data_register(DataRegister(1));
    let format = ctx.mem.read_u8(dt_addr.wrapping_add(DAT_FORMAT_OFFSET));
    let flags = ctx.mem.read_u8(dt_addr.wrapping_add(DAT_FLAGS_OFFSET));

    let mut days = ctx.mem.read_u32(dt_addr.wrapping_add(DAT_STAMP_OFFSET)) as i32;
    let mut minute = ctx.mem.read_u32(dt_addr.wrapping_add(DAT_STAMP_OFFSET + 4)) as i32;
    let mut tick = ctx.mem.read_u32(dt_addr.wrapping_add(DAT_STAMP_OFFSET + 8)) as i32;

    let date_ptr = ctx.mem.read_u32(dt_addr.wrapping_add(DAT_STRDATE_OFFSET));
    if date_ptr != 0 {
        let s = String::from_utf8_lossy(&read_c_string(ctx.mem, date_ptr)).into_owned();
        let (today_days, _, _) = now_as_datestamp();
        match parse_date_string(&s, format, flags, today_days) {
            Some(d) => days = d,
            None => {
                ctx.cpu.set_data_register(DataRegister(0), DOSFALSE);
                return Ok(());
            }
        }
    }

    let time_ptr = ctx.mem.read_u32(dt_addr.wrapping_add(DAT_STRTIME_OFFSET));
    if time_ptr != 0 {
        let s = String::from_utf8_lossy(&read_c_string(ctx.mem, time_ptr)).into_owned();
        match parse_time_string(&s) {
            Some((m, t)) => {
                minute = m;
                tick = t;
            }
            None => {
                ctx.cpu.set_data_register(DataRegister(0), DOSFALSE);
                return Ok(());
            }
        }
    }

    ctx.mem
        .write_u32(dt_addr.wrapping_add(DAT_STAMP_OFFSET), days as u32);
    ctx.mem
        .write_u32(dt_addr.wrapping_add(DAT_STAMP_OFFSET + 4), minute as u32);
    ctx.mem
        .write_u32(dt_addr.wrapping_add(DAT_STAMP_OFFSET + 8), tick as u32);
    ctx.cpu.set_data_register(DataRegister(0), DOSTRUE);
    Ok(())
}

/// Registers `DateToStr`/`StrToDate` onto [`DOS_LIBRARY_BASE`], looked
/// up by name through [`DOS_LVOS`]. Called from
/// [`crate::dispatch::Runtime::new`] alongside the other `dos.library`
/// registrations.
pub fn register_dosdatestr_handlers<C: Cpu + 'static>(
    table: &mut LibraryTable<C>,
    mem: &mut C::Memory,
) {
    table
        .register_by_name(
            mem,
            DOS_LIBRARY_BASE,
            DOS_LVOS,
            "dos.library",
            "DateToStr",
            date_to_str_handler::<C>,
        )
        .unwrap_or_else(|e| panic!("DateToStr should be in DOS_LVOS: {e}"));
    table
        .register_by_name(
            mem,
            DOS_LIBRARY_BASE,
            DOS_LVOS,
            "dos.library",
            "StrToDate",
            str_to_date_handler::<C>,
        )
        .unwrap_or_else(|e| panic!("StrToDate should be in DOS_LVOS: {e}"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{Runtime, StartConfig};
    use crate::memory::FlatMemory;

    // --- render_date_time: unit-level ---

    #[test]
    fn format_dos_style_matches_the_rkrm_example() {
        // 2023-09-30 -> "30-Sep-23" per the RKRM's own worked example.
        assert_eq!(format_date(2023, 9, 30, FORMAT_DOS), "30-Sep-23");
    }

    #[test]
    fn format_int_style_matches_the_rkrm_example() {
        assert_eq!(format_date(2023, 9, 30, FORMAT_INT), "23-09-30");
    }

    #[test]
    fn format_usa_style_matches_the_rkrm_example() {
        assert_eq!(format_date(2023, 9, 30, FORMAT_USA), "09-30-23");
    }

    #[test]
    fn render_date_time_out_of_range_minute_is_none() {
        assert!(render_date_time(0, 1440, 0, FORMAT_DOS, 0).is_none());
        assert!(render_date_time(0, 0, 3000, FORMAT_DOS, 0).is_none());
    }

    #[test]
    fn render_date_time_without_subst_uses_the_formatted_date() {
        // Day 0 = 1978-01-01, a Sunday.
        let (day, date, time) = render_date_time(0, 90, 100, FORMAT_DOS, 0).unwrap();
        assert_eq!(day, "Sunday");
        assert_eq!(date, "01-Jan-78");
        assert_eq!(time, "01:30:02");
    }

    #[test]
    fn render_date_time_subst_today_and_tomorrow() {
        let (today_days, _, _) = now_as_datestamp();
        let (_, date_today, _) = render_date_time(today_days, 0, 0, FORMAT_DOS, DTF_SUBST).unwrap();
        assert_eq!(date_today, "Today");
        let (_, date_tomorrow, _) =
            render_date_time(today_days + 1, 0, 0, FORMAT_DOS, DTF_SUBST).unwrap();
        assert_eq!(date_tomorrow, "Tomorrow");
        let (_, date_yesterday, _) =
            render_date_time(today_days - 1, 0, 0, FORMAT_DOS, DTF_SUBST).unwrap();
        assert_eq!(date_yesterday, "Yesterday");
    }

    #[test]
    fn render_date_time_subst_past_week_uses_weekday_name() {
        let (today_days, _, _) = now_as_datestamp();
        let (day_name, date_str, _) =
            render_date_time(today_days - 3, 0, 0, FORMAT_DOS, DTF_SUBST).unwrap();
        assert_eq!(
            date_str, day_name,
            "3 days ago should show as its weekday name"
        );
    }

    #[test]
    fn render_date_time_subst_future_beyond_tomorrow() {
        let (today_days, _, _) = now_as_datestamp();
        let (_, date_str, _) =
            render_date_time(today_days + 5, 0, 0, FORMAT_DOS, DTF_SUBST).unwrap();
        assert_eq!(date_str, "Future");
    }

    // --- parse_date_string / parse_time_string: unit-level ---

    #[test]
    fn expand_two_digit_year_covers_both_ranges_and_refuses_the_gap() {
        assert_eq!(expand_two_digit_year(78), Some(1978));
        assert_eq!(expand_two_digit_year(99), Some(1999));
        assert_eq!(expand_two_digit_year(0), Some(2000));
        assert_eq!(expand_two_digit_year(45), Some(2045));
        assert_eq!(expand_two_digit_year(60), None);
    }

    #[test]
    fn parse_date_string_format_dos_round_trips_with_format_date() {
        // 15-Mar-95 -> the same day count format_date would render it
        // from, going through days_to_ymd/ymd_to_days.
        let days = parse_date_string("15-Mar-95", FORMAT_DOS, 0, 0).unwrap();
        let (year, month, mday) = days_to_ymd(days as u32);
        assert_eq!((year, month, mday), (1995, 3, 15));
    }

    #[test]
    fn parse_date_string_is_case_insensitive_on_the_month_abbreviation() {
        let a = parse_date_string("15-mar-95", FORMAT_DOS, 0, 0).unwrap();
        let b = parse_date_string("15-MAR-95", FORMAT_DOS, 0, 0).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn parse_date_string_format_int_usa_cdn_agree_on_the_same_date() {
        let dos = parse_date_string("15-Mar-95", FORMAT_DOS, 0, 0).unwrap();
        let int = parse_date_string("95-03-15", FORMAT_INT, 0, 0).unwrap();
        let usa = parse_date_string("03-15-95", FORMAT_USA, 0, 0).unwrap();
        let cdn = parse_date_string("15-03-95", FORMAT_CDN, 0, 0).unwrap();
        assert_eq!(dos, int);
        assert_eq!(dos, usa);
        assert_eq!(dos, cdn);
    }

    #[test]
    fn parse_date_string_relative_words() {
        let today = 20000;
        assert_eq!(
            parse_date_string("Today", FORMAT_DOS, 0, today),
            Some(today)
        );
        assert_eq!(
            parse_date_string("tomorrow", FORMAT_DOS, 0, today),
            Some(today + 1)
        );
        assert_eq!(
            parse_date_string("YESTERDAY", FORMAT_DOS, 0, today),
            Some(today - 1)
        );
    }

    #[test]
    fn parse_date_string_weekday_past_matches_date_to_str_own_substitution() {
        // today_days - 3's weekday name, fed back through
        // parse_date_string with the *past* direction (no DTF_FUTURE),
        // should resolve back to today_days - 3 -- the same value
        // render_date_time's own DTF_SUBST path would have produced it
        // from.
        let today_days = 20000;
        let (day_name, _, _) = render_date_time(today_days - 3, 0, 0, FORMAT_DOS, 0).unwrap();
        let resolved = parse_date_string(&day_name, FORMAT_DOS, 0, today_days).unwrap();
        assert_eq!(resolved, today_days - 3);
    }

    #[test]
    fn parse_date_string_weekday_today_resolves_a_full_week_away() {
        // Asking for today's own weekday name should mean "a week ago"
        // (past) / "a week from now" (future), not "today" -- see the
        // module docs' documented edge-case choice.
        let today_days = 20000;
        let today_wday = (today_days as i64).rem_euclid(7) as usize;
        let name = WEEKDAY_NAMES[today_wday];
        assert_eq!(
            parse_date_string(name, FORMAT_DOS, 0, today_days),
            Some(today_days - 7)
        );
        assert_eq!(
            parse_date_string(name, FORMAT_DOS, DTF_FUTURE, today_days),
            Some(today_days + 7)
        );
    }

    #[test]
    fn parse_date_string_rejects_garbage() {
        assert_eq!(parse_date_string("not a date", FORMAT_DOS, 0, 0), None);
        assert_eq!(parse_date_string("15-Xyz-95", FORMAT_DOS, 0, 0), None); // bad month name
        assert_eq!(parse_date_string("15-Mar-1995", FORMAT_DOS, 0, 0), None); // 4-digit year refused
    }

    #[test]
    fn parse_date_string_does_not_validate_the_day_of_month() {
        // Matches ymd_to_days's own documented "no sanity checking"
        // behavior (same convention as Date2Amiga).
        assert!(parse_date_string("32-Mar-95", FORMAT_DOS, 0, 0).is_some());
    }

    #[test]
    fn parse_time_string_valid() {
        assert_eq!(parse_time_string("12:00:00"), Some((720, 0)));
        assert_eq!(parse_time_string("00:00:00"), Some((0, 0)));
        assert_eq!(parse_time_string("23:59:59"), Some((1439, 2950)));
    }

    #[test]
    fn parse_time_string_rejects_out_of_range_and_malformed() {
        assert_eq!(parse_time_string("24:00:00"), None);
        assert_eq!(parse_time_string("12:60:00"), None);
        assert_eq!(parse_time_string("12:00:60"), None);
        assert_eq!(parse_time_string("12:00"), None);
        assert_eq!(parse_time_string("garbage"), None);
    }

    // --- End-to-end: real A-line trap dispatch ---

    fn load_words(mem: &mut FlatMemory, addr: u32, words: &[u16]) {
        let mut offset = addr;
        for &w in words {
            mem.write_u16(offset, w);
            offset += 2;
        }
    }

    fn move_imm_to_d(n: u16) -> u16 {
        0x203C | (n << 9)
    }

    fn jsr_disp16(an: u16) -> u16 {
        0x4EA8 | an
    }

    const RTS: u16 = 0x4E75;

    #[test]
    fn end_to_end_date_to_str_fills_all_three_buffers() {
        let dt_addr = TRAP_TABLE_END + 0x100;
        let day_buf = dt_addr + 0x40;
        let date_buf = day_buf + 0x20;
        let time_buf = date_buf + 0x20;

        let words = [
            move_imm_to_d(1),
            (dt_addr >> 16) as u16,
            dt_addr as u16,
            jsr_disp16(6),
            (-744i16) as u16, // DateToStr(a6)
            RTS,
        ];

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);

        // dat_Stamp: day 0 (1978-01-01, a Sunday), minute 90, tick 100.
        mem.write_u32(dt_addr + DAT_STAMP_OFFSET, 0);
        mem.write_u32(dt_addr + DAT_STAMP_OFFSET + 4, 90);
        mem.write_u32(dt_addr + DAT_STAMP_OFFSET + 8, 100);
        mem.write_u8(dt_addr + DAT_FORMAT_OFFSET, FORMAT_DOS);
        mem.write_u8(dt_addr + DAT_FLAGS_OFFSET, 0); // no DTF_SUBST
        mem.write_u32(dt_addr + DAT_STRDAY_OFFSET, day_buf);
        mem.write_u32(dt_addr + DAT_STRDATE_OFFSET, date_buf);
        mem.write_u32(dt_addr + DAT_STRTIME_OFFSET, time_buf);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: time_buf + 0x40,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, DOSTRUE as i32);

        assert_eq!(
            crate::guestmem::read_c_string(rt.memory(), day_buf),
            b"Sunday"
        );
        assert_eq!(
            crate::guestmem::read_c_string(rt.memory(), date_buf),
            b"01-Jan-78"
        );
        assert_eq!(
            crate::guestmem::read_c_string(rt.memory(), time_buf),
            b"01:30:02"
        );
    }

    #[test]
    fn end_to_end_date_to_str_null_pointers_are_skipped() {
        let dt_addr = TRAP_TABLE_END + 0x100;

        let words = [
            move_imm_to_d(1),
            (dt_addr >> 16) as u16,
            dt_addr as u16,
            jsr_disp16(6),
            (-744i16) as u16,
            RTS,
        ];

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);
        mem.write_u32(dt_addr + DAT_STAMP_OFFSET, 0);
        mem.write_u32(dt_addr + DAT_STAMP_OFFSET + 4, 0);
        mem.write_u32(dt_addr + DAT_STAMP_OFFSET + 8, 0);
        mem.write_u8(dt_addr + DAT_FORMAT_OFFSET, FORMAT_DOS);
        mem.write_u8(dt_addr + DAT_FLAGS_OFFSET, 0);
        mem.write_u32(dt_addr + DAT_STRDAY_OFFSET, 0);
        mem.write_u32(dt_addr + DAT_STRDATE_OFFSET, 0);
        mem.write_u32(dt_addr + DAT_STRTIME_OFFSET, 0);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: dt_addr + 0x40,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, DOSTRUE as i32,
            "no NULL-pointer buffer should crash or fail the call"
        );
    }

    #[test]
    fn end_to_end_str_to_date_parses_date_and_time() {
        let dt_addr = TRAP_TABLE_END + 0x100;
        let date_str_addr = dt_addr + 0x40;
        let time_str_addr = date_str_addr + 0x20;

        let words = [
            move_imm_to_d(1),
            (dt_addr >> 16) as u16,
            dt_addr as u16,
            jsr_disp16(6),
            (-750i16) as u16, // StrToDate(a6)
            RTS,
        ];

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);
        mem.write_u32(dt_addr + DAT_STAMP_OFFSET, 0);
        mem.write_u32(dt_addr + DAT_STAMP_OFFSET + 4, 0);
        mem.write_u32(dt_addr + DAT_STAMP_OFFSET + 8, 0);
        mem.write_u8(dt_addr + DAT_FORMAT_OFFSET, FORMAT_DOS);
        mem.write_u8(dt_addr + DAT_FLAGS_OFFSET, 0);
        mem.write_u32(dt_addr + DAT_STRDAY_OFFSET, 0);
        mem.write_u32(dt_addr + DAT_STRDATE_OFFSET, date_str_addr);
        mem.write_u32(dt_addr + DAT_STRTIME_OFFSET, time_str_addr);
        crate::guestmem::write_c_string(&mut mem, date_str_addr, b"15-Mar-95");
        crate::guestmem::write_c_string(&mut mem, time_str_addr, b"12:00:00");

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: time_str_addr + 0x40,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, DOSTRUE as i32);

        let days = rt.memory().read_u32(dt_addr + DAT_STAMP_OFFSET);
        let minute = rt.memory().read_u32(dt_addr + DAT_STAMP_OFFSET + 4);
        let tick = rt.memory().read_u32(dt_addr + DAT_STAMP_OFFSET + 8);
        assert_eq!(days_to_ymd(days), (1995, 3, 15));
        assert_eq!(minute, 720);
        assert_eq!(tick, 0);
    }

    #[test]
    fn end_to_end_str_to_date_bad_input_fails_cleanly() {
        let dt_addr = TRAP_TABLE_END + 0x100;
        let date_str_addr = dt_addr + 0x40;

        let words = [
            move_imm_to_d(1),
            (dt_addr >> 16) as u16,
            dt_addr as u16,
            jsr_disp16(6),
            (-750i16) as u16,
            RTS,
        ];

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);
        mem.write_u32(dt_addr + DAT_STAMP_OFFSET, 0);
        mem.write_u32(dt_addr + DAT_STAMP_OFFSET + 4, 0);
        mem.write_u32(dt_addr + DAT_STAMP_OFFSET + 8, 0);
        mem.write_u8(dt_addr + DAT_FORMAT_OFFSET, FORMAT_DOS);
        mem.write_u8(dt_addr + DAT_FLAGS_OFFSET, 0);
        mem.write_u32(dt_addr + DAT_STRDAY_OFFSET, 0);
        mem.write_u32(dt_addr + DAT_STRDATE_OFFSET, date_str_addr);
        mem.write_u32(dt_addr + DAT_STRTIME_OFFSET, 0);
        crate::guestmem::write_c_string(&mut mem, date_str_addr, b"not a date");

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: date_str_addr + 0x40,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, DOSFALSE as i32);
    }
}
