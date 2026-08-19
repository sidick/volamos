//! `dos.library` `DateStamp`/`Delay`: current date/time, and
//! suspending the calling process for a tick count.
//!
//! `DateStamp` found missing while running the real Workbench 3.1.4
//! `C:/List` binary: it calls `DateStamp()` (presumably to timestamp
//! its own run, or to compare against file dates for `SINCE`/`UPTO`-
//! style filtering) before it ever gets to printing a single directory
//! entry. `Delay` found missing while running the real `C:/Wait`
//! binary -- unlike `exec.library`'s device-I/O primitives
//! (`crate::exectask`'s `OpenDevice`/`CloseDevice`, always failing
//! since this runtime models no real device drivers), `Delay` has
//! simple, fully-specifiable semantics this runtime *can* implement
//! faithfully: it just blocks the host thread for the requested
//! duration via `std::thread::sleep`. Real `Delay` is "system-friendly"
//! (per the RKRM: the process is suspended, not busy-waiting) --
//! irrelevant here since this runtime is single-threaded and there's
//! no other guest task that could run in the meantime anyway, but a
//! real sleep is still the correct, literal behavior a corpus binary
//! (or a human watching `Wait` actually wait) expects.
//!
//! # `struct DateStamp` (`dos/dos.h`)
//!
//! ```text
//! struct DateStamp {
//!     LONG ds_Days;   // days since 1978-01-01 (offset 0)
//!     LONG ds_Minute; // minutes past midnight (offset 4)
//!     LONG ds_Tick;   // ticks (1/50s) since the start of that minute (offset 8)
//! };
//! ```
//! 12 bytes total, no particular alignment required (per the RKRM:
//! "Unlike many other dos.library functions, there is no requirement to
//! align `ds` to a long-word boundary").
//!
//! Converted from the host wall clock (`std::time::SystemTime::now()`)
//! via a fixed day offset from the Unix epoch (1970-01-01) to the Amiga
//! epoch (1978-01-01): 8 full years, 2 of them (1972, 1976) leap, so
//! `2922` days -- independently cross-checked against `crate::utility`'s
//! own epoch-anchoring derivation (`EPOCH_YEAR = 1978`, module docs)
//! rather than just trusting one calculation.

use crate::cpu::{Cpu, DataRegister};
use crate::dispatch::{DOS_LIBRARY_BASE, DispatchError, HandlerContext, LibraryTable};
use crate::lvos::dos::DOS_LVOS;
use crate::memory::AddressSpace;
use std::time::{SystemTime, UNIX_EPOCH};

const DS_DAYS_OFFSET: u32 = 0;
const DS_MINUTE_OFFSET: u32 = 4;
const DS_TICK_OFFSET: u32 = 8;

/// Ticks (1/50s units) per second, per `dos/dos.h`'s `TICKS_PER_SECOND`.
const TICKS_PER_SECOND: u32 = 50;

/// Days from the Unix epoch (1970-01-01) to the Amiga epoch
/// (1978-01-01): 8 full years, 2 of them leap (1972, 1976).
const UNIX_TO_AMIGA_EPOCH_DAYS: i64 = 8 * 365 + 2;

/// Computes `(ds_Days, ds_Minute, ds_Tick)` for the current host wall
/// clock. Saturates to `0` if the host clock is somehow before the
/// Amiga epoch (never happens in practice, but avoids an underflow
/// panic on a misconfigured host clock). Shared with
/// [`crate::dosdatestr`]'s `DateToStr`, for its `DTF_SUBST`
/// "Today"/"Tomorrow"/"Yesterday" substitution.
pub(crate) fn now_as_datestamp() -> (i32, i32, i32) {
    let unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let amiga_secs = unix_secs - UNIX_TO_AMIGA_EPOCH_DAYS * 86_400;
    if amiga_secs < 0 {
        return (0, 0, 0);
    }
    let days = amiga_secs / 86_400;
    let secs_of_day = amiga_secs % 86_400;
    let minute = secs_of_day / 60;
    let secs_of_minute = secs_of_day % 60;
    let tick = secs_of_minute * i64::from(TICKS_PER_SECOND);
    (days as i32, minute as i32, tick as i32)
}

/// `DateStamp` (`D1` = `struct DateStamp*`). `D0` = `D1`, unchanged --
/// real `DateStamp()` returns the same pointer it was given. Cannot
/// fail.
fn datestamp_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let addr = ctx.cpu.data_register(DataRegister(1));
    let (days, minute, tick) = now_as_datestamp();
    ctx.mem
        .write_u32(addr.wrapping_add(DS_DAYS_OFFSET), days as u32);
    ctx.mem
        .write_u32(addr.wrapping_add(DS_MINUTE_OFFSET), minute as u32);
    ctx.mem
        .write_u32(addr.wrapping_add(DS_TICK_OFFSET), tick as u32);
    ctx.cpu.set_data_register(DataRegister(0), addr);
    Ok(())
}

/// `Delay` (`D1` = ticks, 1/50s units). No return value. Blocks the
/// host thread for the equivalent wall-clock duration -- see the
/// module docs.
fn delay_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let ticks = ctx.cpu.data_register(DataRegister(1));
    let millis = u64::from(ticks) * 1000 / u64::from(TICKS_PER_SECOND);
    std::thread::sleep(std::time::Duration::from_millis(millis));
    Ok(())
}

/// Registers `DateStamp`/`Delay` onto [`DOS_LIBRARY_BASE`], looked up
/// by name through [`DOS_LVOS`]. Called from
/// [`crate::dispatch::Runtime::new`] alongside the other `dos.library`
/// registrations.
pub fn register_dosdate_handlers<C: Cpu + 'static>(
    table: &mut LibraryTable<C>,
    mem: &mut C::Memory,
) {
    table
        .register_by_name(
            mem,
            DOS_LIBRARY_BASE,
            DOS_LVOS,
            "dos.library",
            "DateStamp",
            datestamp_handler::<C>,
        )
        .unwrap_or_else(|e| panic!("DateStamp should be in DOS_LVOS: {e}"));
    table
        .register_by_name(
            mem,
            DOS_LIBRARY_BASE,
            DOS_LVOS,
            "dos.library",
            "Delay",
            delay_handler::<C>,
        )
        .unwrap_or_else(|e| panic!("Delay should be in DOS_LVOS: {e}"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{Runtime, StartConfig};
    use crate::memory::FlatMemory;

    #[test]
    fn now_as_datestamp_is_plausibly_recent() {
        let (days, minute, tick) = now_as_datestamp();
        // Any real host clock is well past the year 2000 (day ~8035
        // since the Amiga epoch) and well before the DateStamp-overflow
        // horizon the RKRM documents (2045).
        assert!(days > 8_000, "days since 1978 should be large: {days}");
        assert!(
            (0..1440).contains(&minute),
            "minute-of-day out of range: {minute}"
        );
        assert!(
            (0..3000).contains(&tick),
            "tick-of-minute out of range: {tick}"
        );
    }

    #[test]
    fn end_to_end_datestamp_fills_struct_and_returns_same_pointer() {
        let mut mem = FlatMemory::new(0x2_0000);
        let ds_addr = TRAP_TABLE_END + 0x100;

        // D1 = ds_addr; jsr DateStamp(a6); rts (exit code = D0 == ds_addr)
        let words = [
            0x223C, // move.l #imm32,D1
            (ds_addr >> 16) as u16,
            ds_addr as u16,
            0x4EAE, // jsr disp16(a6)
            (-192i16) as u16,
            0x4E75, // rts
        ];
        let mut offset = TRAP_TABLE_END;
        for &w in &words {
            mem.write_u16(offset, w);
            offset += 2;
        }

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: ds_addr + 0x20,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, ds_addr as i32,
            "D0 should be the same DateStamp* passed in"
        );

        let days = rt.memory().read_u32(ds_addr + DS_DAYS_OFFSET) as i32;
        let minute = rt.memory().read_u32(ds_addr + DS_MINUTE_OFFSET) as i32;
        let tick = rt.memory().read_u32(ds_addr + DS_TICK_OFFSET) as i32;
        assert!(days > 8_000);
        assert!((0..1440).contains(&minute));
        assert!((0..3000).contains(&tick));
    }

    #[test]
    fn end_to_end_delay_blocks_for_roughly_the_requested_duration() {
        // 2 ticks == 40ms -- small enough to keep the test suite fast,
        // large enough to reliably distinguish "slept" from "didn't".
        let ticks: u16 = 2;
        let mut mem = FlatMemory::new(0x2_0000);
        let words = [
            0x223C, // move.l #imm32,D1
            0,
            ticks,
            0x4EAE,           // jsr disp16(a6)
            (-198i16) as u16, // Delay
            0x7000,           // moveq #0,d0 (Delay doesn't touch D0 itself)
            0x4E75,           // rts
        ];
        let mut offset = TRAP_TABLE_END;
        for &w in &words {
            mem.write_u16(offset, w);
            offset += 2;
        }

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: TRAP_TABLE_END + 0x100,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );

        let start = std::time::Instant::now();
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        let elapsed = start.elapsed();
        assert_eq!(code, 0);
        assert!(
            elapsed >= std::time::Duration::from_millis(30),
            "Delay(2) should block for roughly 40ms, only blocked {elapsed:?}"
        );
    }
}
