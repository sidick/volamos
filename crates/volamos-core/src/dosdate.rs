//! `dos.library` `DateStamp`/`Delay`/`CompareDates`: current date/time,
//! suspending the calling process for a tick count, and comparing two
//! `DateStamp`s.
//!
//! `DateStamp` found missing while running the real Workbench 3.1.4
//! `C:/List` binary: it calls `DateStamp()` (presumably to timestamp
//! its own run, or to compare against file dates for `SINCE`/`UPTO`-
//! style filtering) before it ever gets to printing a single directory
//! entry. `Delay` found missing while running the real `C:/Wait`
//! binary.
//!
//! # `Delay` blocks like `Wait`, not like a host sleep (issue #254)
//!
//! Before `CreateNewProc` (issue #121), this runtime was single-
//! tasking, so `Delay` just blocked the host thread for the requested
//! duration via `std::thread::sleep`: real `Delay` is "system-
//! friendly" (per the RKRM: the process is suspended, not busy-
//! waiting), but with no other guest task that could ever run in the
//! meantime anyway, a flat host sleep had the same observable effect.
//! codewiz's issue #254 report showed that reasoning breaks once a
//! second real task exists: a task spinning on `Delay(1)` waiting for
//! another one (`pthread_join`'s polling idiom when no signal is
//! free) hung forever, since the host sleep gave that other task no
//! chance to run at all; and a pending real timer request
//! (`SendIO(TR_ADDREQUEST)`) in one task overshot its own deadline
//! whenever another task happened to be delaying, since nothing ever
//! checked [`crate::exectask::PendingTimers`] while the sleep blocked
//! everything.
//!
//! [`delay_handler`] now behaves like [`crate::exectask::wait_handler`]:
//! it registers its own deadline in `PendingTimers` (shared with real
//! timer requests, so both are considered together, each correctly
//! able to wake or be woken by the other) and blocks
//! (`TaskScheduler::block_current_on_delay`), switching to another
//! runnable task instead of sleeping the host thread, until that
//! deadline is reached. Only once no other task is runnable either
//! does it fall back to a real, bounded sleep -- and even then, only
//! up to whichever is sooner: its own deadline, or some other
//! pending entry that might wake a task first
//! (`PendingTimers::fire_earliest_bounded`) and change the picture
//! before this `Delay` would otherwise have to finish sleeping blind.
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
pub(crate) const UNIX_TO_AMIGA_EPOCH_DAYS: i64 = 8 * 365 + 2;

/// Computes `(ds_Days, ds_Minute, ds_Tick)` for the current host wall
/// clock. Saturates to `0` if the host clock is somehow before the
/// Amiga epoch (never happens in practice, but avoids an underflow
/// panic on a misconfigured host clock). Shared with
/// [`crate::dosdatestr`]'s `DateToStr`, for its `DTF_SUBST`
/// "Today"/"Tomorrow"/"Yesterday" substitution.
pub(crate) fn now_as_datestamp() -> (i32, i32, i32) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let amiga_secs = now.as_secs() as i64 - UNIX_TO_AMIGA_EPOCH_DAYS * 86_400;
    if amiga_secs < 0 {
        return (0, 0, 0);
    }
    let days = amiga_secs / 86_400;
    let secs_of_day = amiga_secs % 86_400;
    let minute = secs_of_day / 60;
    let secs_of_minute = secs_of_day % 60;
    // Keep the fractional second: dropping it (as an earlier version
    // did, via `.as_secs()` alone) made `tick` always an exact multiple
    // of `TICKS_PER_SECOND`. `GetSysTime` and `ReadEClock` were built on
    // this triple then, and that pinned `tv_micro` to 0 forever; they
    // now read the host clock directly (see `host_time_secs_micro` in
    // `crate::exectask`) to get microseconds.
    let tick =
        secs_of_minute * i64::from(TICKS_PER_SECOND) + i64::from(now.subsec_nanos()) / 20_000_000;
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
    let task = ctx.current_task;

    ctx.timers.fire_due(ctx.mem, ctx.tasks);

    let deadline = ctx
        .timers
        .delay_deadline(task, std::time::Duration::from_millis(millis));

    loop {
        if std::time::Instant::now() >= deadline {
            ctx.timers.remove_delay(task);
            return Ok(());
        }

        // Nothing here "remembers" to return once woken -- re-trapping
        // into this exact call re-checks the deadline fresh, same as
        // `wait_handler`'s re-trap model (see this module's doc).
        if ctx
            .tasks
            .block_current_on_delay(ctx.cpu, ctx.mem, ctx.trap_pc)
            .is_ok()
        {
            return Ok(());
        }

        // No other task is runnable. Some other pending entry (another
        // task's real timer request, or another task's own Delay)
        // might still be due before this one and could make a task
        // Ready -- give it the chance rather than sleeping blind for
        // the rest of this Delay's own duration.
        if !ctx
            .timers
            .fire_earliest_bounded(ctx.mem, ctx.tasks, deadline)
        {
            // Nothing else pending before our own deadline either:
            // sleep out the remainder for real, the same reasoning
            // `fire_earliest`'s own fallback sleep uses -- nothing else
            // could possibly make progress while this sleeps anyway.
            let now = std::time::Instant::now();
            if deadline > now {
                std::thread::sleep(deadline - now);
            }
            ctx.timers.remove_delay(task);
            return Ok(());
        }
        // Something else fired -- possibly waking a task -- loop back
        // and recheck.
    }
}

/// Reads a `struct DateStamp` at `addr` as `(ds_Days, ds_Minute, ds_Tick)`.
fn read_datestamp<C: Cpu>(ctx: &HandlerContext<'_, C>, addr: u32) -> (i32, i32, i32) {
    let days = ctx.mem.read_u32(addr.wrapping_add(DS_DAYS_OFFSET)) as i32;
    let minute = ctx.mem.read_u32(addr.wrapping_add(DS_MINUTE_OFFSET)) as i32;
    let tick = ctx.mem.read_u32(addr.wrapping_add(DS_TICK_OFFSET)) as i32;
    (days, minute, tick)
}

/// `CompareDates` (`D1` = `struct DateStamp* date1`, `D2` = `struct
/// DateStamp* date2`). `D0` = negative if `date1` is earlier than
/// `date2`, 0 if equal, positive if later -- compared field by field
/// (`ds_Days`, then `ds_Minute`, then `ds_Tick`), matching the field
/// order's own significance. Cannot fail.
fn compare_dates_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let date1_ptr = ctx.cpu.data_register(DataRegister(1));
    let date2_ptr = ctx.cpu.data_register(DataRegister(2));
    let date1 = read_datestamp(ctx, date1_ptr);
    let date2 = read_datestamp(ctx, date2_ptr);

    let diff = match date1.0.cmp(&date2.0) {
        std::cmp::Ordering::Equal => match date1.1.cmp(&date2.1) {
            std::cmp::Ordering::Equal => date1.2 - date2.2,
            other => other as i32,
        },
        other => other as i32,
    };
    ctx.cpu.set_data_register(DataRegister(0), diff as u32);
    Ok(())
}

/// Registers `DateStamp`/`Delay`/`CompareDates` onto
/// [`DOS_LIBRARY_BASE`], looked up by name through [`DOS_LVOS`]. Called
/// from [`crate::dispatch::Runtime::new`] alongside the other
/// `dos.library` registrations.
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
    table
        .register_by_name(
            mem,
            DOS_LIBRARY_BASE,
            DOS_LVOS,
            "dos.library",
            "CompareDates",
            compare_dates_handler::<C>,
        )
        .unwrap_or_else(|e| panic!("CompareDates should be in DOS_LVOS: {e}"));
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

    /// Regression test for a real bug: an earlier version derived `tick`
    /// from `SystemTime::now().duration_since(UNIX_EPOCH).as_secs()`
    /// alone, discarding the sub-second component before ever computing
    /// `tick` -- so `tick` was always an exact multiple of
    /// `TICKS_PER_SECOND` and `GetSysTime`'s `tv_micro` (derived from
    /// `tick % TICKS_PER_SECOND`, see `crate::exectask::
    /// host_time_secs_micro`) was permanently pinned to 0. That broke
    /// every real caller measuring sub-second elapsed time via
    /// `GetSysTime` (found via `bsdsocktest`'s `WaitSelect(): all NULL
    /// fdsets + timeout = delay` test reporting a genuine 250ms host
    /// delay as `elapsed: 0ms`). Two samples ~100ms apart, straddling a
    /// whole-second boundary or not, must show *some* sub-tick movement
    /// unless caught in the 1-in-50 chance both land on the exact same
    /// tick.
    #[test]
    fn now_as_datestamp_has_real_sub_second_precision() {
        let mut saw_change = false;
        for _ in 0..20 {
            let (_, _, tick_before) = now_as_datestamp();
            std::thread::sleep(std::time::Duration::from_millis(30));
            let (_, _, tick_after) = now_as_datestamp();
            if tick_before != tick_after {
                saw_change = true;
                break;
            }
        }
        assert!(
            saw_change,
            "tick never advanced across 20 samples 30ms apart -- \
             sub-second precision is broken"
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

    fn run_compare_dates(date1: (i32, i32, i32), date2: (i32, i32, i32)) -> i32 {
        let date1_addr = TRAP_TABLE_END + 0x100;
        let date2_addr = date1_addr + 12;

        let words = [
            0x223C, // move.l #imm32,D1
            (date1_addr >> 16) as u16,
            date1_addr as u16,
            0x243C, // move.l #imm32,D2
            (date2_addr >> 16) as u16,
            date2_addr as u16,
            0x4EAE,           // jsr disp16(a6)
            (-738i16) as u16, // CompareDates
            0x4E75,           // rts
        ];

        let mut mem = FlatMemory::new(0x2_0000);
        let mut offset = TRAP_TABLE_END;
        for &w in &words {
            mem.write_u16(offset, w);
            offset += 2;
        }
        mem.write_u32(date1_addr + DS_DAYS_OFFSET, date1.0 as u32);
        mem.write_u32(date1_addr + DS_MINUTE_OFFSET, date1.1 as u32);
        mem.write_u32(date1_addr + DS_TICK_OFFSET, date1.2 as u32);
        mem.write_u32(date2_addr + DS_DAYS_OFFSET, date2.0 as u32);
        mem.write_u32(date2_addr + DS_MINUTE_OFFSET, date2.1 as u32);
        mem.write_u32(date2_addr + DS_TICK_OFFSET, date2.2 as u32);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: date2_addr + 0x20,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );

        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed")
    }

    #[test]
    fn end_to_end_compare_dates_equal() {
        assert_eq!(run_compare_dates((100, 200, 300), (100, 200, 300)), 0);
    }

    #[test]
    fn end_to_end_compare_dates_date1_earlier_by_days() {
        assert!(run_compare_dates((100, 200, 300), (101, 0, 0)) < 0);
    }

    #[test]
    fn end_to_end_compare_dates_date1_later_by_minute() {
        assert!(run_compare_dates((100, 201, 0), (100, 200, 999)) > 0);
    }

    #[test]
    fn end_to_end_compare_dates_date1_earlier_by_tick_only() {
        assert!(run_compare_dates((100, 200, 300), (100, 200, 301)) < 0);
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

    /// `move.l #imm32,An`.
    fn move_imm_to_a(n: u16) -> u16 {
        0x207C | (n << 9)
    }

    /// `move.l D0,(An)`.
    fn move_d0_to_a_indirect(an: u16) -> u16 {
        0x2080 | (an << 9)
    }

    /// `move.l (An),D0`.
    fn move_a_indirect_to_d0(an: u16) -> u16 {
        0x2010 | an
    }

    /// `jsr <disp16>(a6)`.
    fn jsr_disp16_a6(disp: i32) -> [u16; 2] {
        [0x4EAE, disp as u16]
    }

    const RTS: u16 = 0x4E75;

    fn load_words(mem: &mut FlatMemory, addr: u32, words: &[u16]) {
        let mut offset = addr;
        for &w in words {
            mem.write_u16(offset, w);
            offset += 2;
        }
    }

    #[test]
    fn delay_lets_another_ready_task_run() {
        // codewiz's issue #254 "delay-switch.c" repro: a task spinning
        // on Delay(1) waiting for another one to make progress (e.g.
        // pthread_join's polling idiom when no signal is free) must
        // actually let that other task run during the Delay, not hang
        // the whole runtime in a host sleep that starves it. A
        // CreateNewProc child, spawned right before the parent's own
        // Delay, writes a marker and exits; if the child got to run
        // at all during the parent's Delay (this fix), the marker is
        // already set by the time Delay returns. Before this fix, the
        // child never got the CPU -- Delay was a flat host sleep that
        // gave nothing else a chance to run -- so the marker would
        // still read 0.
        const NP_ENTRY: u32 = 0x8000_0000 + 1000 + 3; // TAG_USER + NP_Dummy + NP_Entry

        let entry = TRAP_TABLE_END;
        let mut words = Vec::new();

        // --- parent: CreateNewProc(tags); Delay(5) [100ms]; read marker ---
        words.push(move_imm_to_a(6)); // A6 = DOS_LIBRARY_BASE
        words.push((DOS_LIBRARY_BASE >> 16) as u16);
        words.push(DOS_LIBRARY_BASE as u16);
        let tags_imm_idx = words.len();
        words.push(0x223C); // move.l #imm32,D1 = tags (patched below)
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-498)); // CreateNewProc
        words.push(0x223C); // move.l #imm32,D1 (ticks)
        words.push(0);
        words.push(5); // 5 ticks == 100ms
        words.extend_from_slice(&jsr_disp16_a6(-198)); // Delay

        let marker_imm_idx = words.len();
        words.push(move_imm_to_a(0)); // A0 = marker_addr (patched below)
        words.push(0);
        words.push(0);
        words.push(move_a_indirect_to_d0(0)); // D0 = *marker_addr
        words.push(RTS); // -> EXIT_STUB_ADDR, exit code = the marker

        // --- child: write the marker; fall off the end into the
        // task-exit stub ---
        let child_entry = TRAP_TABLE_END + (words.len() as u32) * 2;

        let marker_imm_idx_child = words.len();
        words.push(move_imm_to_a(0)); // A0 = marker_addr (patched below)
        words.push(0);
        words.push(0);
        words.push(0x7000 | 0x2A); // moveq #0x2A,d0
        words.push(move_d0_to_a_indirect(0)); // *marker_addr = D0
        words.push(RTS); // -> TASK_EXIT_STUB_ADDR, pre-pushed by CreateNewProc

        let code_end = entry + (words.len() as u32) * 2;
        let marker_addr = code_end + 16;
        let tags_addr = marker_addr + 16;
        let load_end = tags_addr + 32;

        words[tags_imm_idx + 1] = (tags_addr >> 16) as u16;
        words[tags_imm_idx + 2] = tags_addr as u16;
        words[marker_imm_idx + 1] = (marker_addr >> 16) as u16;
        words[marker_imm_idx + 2] = marker_addr as u16;
        words[marker_imm_idx_child + 1] = (marker_addr >> 16) as u16;
        words[marker_imm_idx_child + 2] = marker_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        mem.write_u32(marker_addr, 0);
        mem.write_u32(tags_addr, NP_ENTRY);
        mem.write_u32(tags_addr + 4, child_entry);
        mem.write_u32(tags_addr + 8, 0);
        mem.write_u32(tags_addr + 12, 0);

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

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, 0x2A,
            "the child must have run and written the marker during the parent's Delay, not \
             been starved of the CPU by a flat host sleep"
        );
    }

    #[test]
    fn a_delay_still_ends_while_another_task_polls_with_its_own_delay() {
        // codewiz's issue #262 "delay-delay.c" repro: a child's
        // Delay(3) must genuinely end while the main task polls with
        // Delay(1) -- libpthread's pthread_join() does exactly this
        // when no signal is free.
        //
        // What used to go wrong is the re-trap model meeting
        // `PendingTimers`' bookkeeping: a `Delay` entry is both the
        // wake event *and* the deadline memo `delay_deadline` hands
        // back on re-entry, and firing it used to remove it. So every
        // time the main task's own blocking wait fired the child's
        // elapsed entry, the woken child re-entered `delay_handler`,
        // found no memo, and started its full 3 ticks over again --
        // forever, for as long as anything kept waking it. The child's
        // Delay must now return after roughly its own duration, not
        // after the main task gives up polling.
        //
        // Parent: spawn the child, then poll Delay(1) up to 50 times
        // (a second, far longer than the child's 60ms) for the child's
        // marker, and exit with whatever the marker holds -- 0 if the
        // child's Delay never returned.
        const NP_ENTRY: u32 = 0x8000_0000 + 1000 + 3; // TAG_USER + NP_Dummy + NP_Entry
        const POLLS: u16 = 50;

        let entry = TRAP_TABLE_END;
        let mut words = Vec::new();

        // --- parent ---
        words.push(move_imm_to_a(6)); // A6 = DOS_LIBRARY_BASE
        words.push((DOS_LIBRARY_BASE >> 16) as u16);
        words.push(DOS_LIBRARY_BASE as u16);
        let tags_imm_idx = words.len();
        words.push(0x223C); // move.l #imm32,D1 = tags (patched below)
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-498)); // CreateNewProc
        words.push(0x7400 | POLLS); // moveq #POLLS,D2 -- the poll budget

        // poll: Delay(1); D0 = *marker; stop as soon as it's non-zero,
        // or once the budget runs out.
        let poll_idx = words.len();
        words.push(0x223C); // move.l #imm32,D1
        words.push(0);
        words.push(1); // 1 tick == 20ms
        words.extend_from_slice(&jsr_disp16_a6(-198)); // Delay
        let marker_imm_idx = words.len();
        words.push(move_imm_to_a(0)); // A0 = marker_addr (patched below)
        words.push(0);
        words.push(0);
        words.push(move_a_indirect_to_d0(0)); // D0 = *marker_addr, sets Z
        words.push(0x6600 | 8); // bne.s done (4 words further on)
        words.push(0x5382); // subq.l #1,D2
        let back = (poll_idx as i32 - (words.len() as i32 + 1)) * 2;
        words.push(0x6600 | (back as u8 as u16)); // bne.s poll
        words.push(0x7000); // moveq #0,D0 -- budget exhausted
        words.push(RTS); // -> EXIT_STUB_ADDR
        // done: exit with the *unused* poll budget, so the assertion
        // can tell "ended promptly" from "ended on the last poll".
        words.push(0x2002); // move.l D2,D0
        words.push(RTS); // -> EXIT_STUB_ADDR

        // --- child: Delay(3) [60ms], then write the marker ---
        let child_entry = TRAP_TABLE_END + (words.len() as u32) * 2;
        words.push(move_imm_to_a(6)); // A6 = DOS_LIBRARY_BASE
        words.push((DOS_LIBRARY_BASE >> 16) as u16);
        words.push(DOS_LIBRARY_BASE as u16);
        words.push(0x223C); // move.l #imm32,D1
        words.push(0);
        words.push(3); // 3 ticks == 60ms
        words.extend_from_slice(&jsr_disp16_a6(-198)); // Delay
        let marker_imm_idx_child = words.len();
        words.push(move_imm_to_a(0)); // A0 = marker_addr (patched below)
        words.push(0);
        words.push(0);
        words.push(0x7000 | 0x2A); // moveq #0x2A,d0
        words.push(move_d0_to_a_indirect(0)); // *marker_addr = D0
        words.push(RTS); // -> TASK_EXIT_STUB_ADDR

        let code_end = entry + (words.len() as u32) * 2;
        let marker_addr = code_end + 16;
        let tags_addr = marker_addr + 16;
        let load_end = tags_addr + 32;

        words[tags_imm_idx + 1] = (tags_addr >> 16) as u16;
        words[tags_imm_idx + 2] = tags_addr as u16;
        words[marker_imm_idx + 1] = (marker_addr >> 16) as u16;
        words[marker_imm_idx + 2] = marker_addr as u16;
        words[marker_imm_idx_child + 1] = (marker_addr >> 16) as u16;
        words[marker_imm_idx_child + 2] = marker_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        mem.write_u32(marker_addr, 0);
        mem.write_u32(tags_addr, NP_ENTRY);
        mem.write_u32(tags_addr + 4, child_entry);
        mem.write_u32(tags_addr + 8, 0);
        mem.write_u32(tags_addr + 12, 0);

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

        let mut out = Vec::new();
        // The exit code is the parent's unused poll budget: 0 means it
        // polled all 50 times (a second) and the child's Delay(3)
        // never returned at all.
        let polls_left = rt.run(&mut out, None).expect("run should succeed");
        assert_ne!(
            polls_left, 0,
            "the child's own Delay must have ended during the parent's Delay(1) polling \
             rather than restarting its full duration every time the parent's wait woke it"
        );
        let polls_used = i32::from(POLLS) - polls_left;
        assert!(
            polls_used <= 10,
            "the child's Delay(3) should end within a handful of the parent's 20ms polls; \
             it took {polls_used} of {POLLS}"
        );
    }
}
