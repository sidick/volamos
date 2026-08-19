//! `dos.library` `DateToStr`: renders a `DateStamp` as human-readable
//! day-of-week/date/time strings.
//!
//! Found missing while running the real Workbench 3.1.4 `C:/List`
//! binary: it calls this for every directory entry's timestamp.
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
//! Only `DateToStr` is implemented (not `StrToDate`, which parses
//! free-form human-entered text -- much larger scope, and no corpus
//! binary has needed it yet). `dat_Format`'s `FORMAT_DEF` falls back to
//! `FORMAT_DOS` (per the RKRM: "Otherwise, it falls back to
//! `FORMAT_DOS`" when no `locale.library` is installed, which this
//! runtime never has). `DTF_SUBST` is implemented (`List` enables it by
//! default for its own date column), comparing against
//! [`crate::dosdate::now_as_datestamp`]; `DTF_FUTURE` only affects
//! `StrToDate`, so it's ignored here.

use crate::cpu::{Cpu, DataRegister};
use crate::dispatch::{DOS_LIBRARY_BASE, DispatchError, HandlerContext, LibraryTable};
use crate::dosdate::now_as_datestamp;
use crate::guestmem::write_c_string;
use crate::lvos::dos::DOS_LVOS;
use crate::memory::AddressSpace;
use crate::utility::days_to_ymd;

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

/// Registers `DateToStr` onto [`DOS_LIBRARY_BASE`], looked up by name
/// through [`DOS_LVOS`]. Called from [`crate::dispatch::Runtime::new`]
/// alongside the other `dos.library` registrations.
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
}
