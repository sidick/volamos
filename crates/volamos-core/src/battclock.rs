//! `battclock.resource` -- the battery-backed clock chip, backed by the
//! host clock (issue #278).
//!
//! # Why a resource at all
//!
//! `exec.library`'s resource list has been real since Phase 3 (see
//! [`crate::execlist`]'s `AddResource`/`RemResource`/`OpenResource`),
//! but nothing was ever *on* it, so `OpenResource` of any system
//! resource returned `NULL`. That is fine for a program that probes for
//! one and copes, and fatal for the m68k-amigaos-gcc startup code:
//! `libstubs` puts `{ base, "name" }` pairs on `__LIB_LIST__` and crt0
//! calls `OpenResource()` for every name ending in `.resource` before
//! `main()`, with no NULL check in the stock version -- so a program
//! merely *referencing* `BattClockBase` jumped through `0` and died
//! with volamos's PC-out-of-bounds report before reaching `main`.
//!
//! `battclock.resource` is the one system resource that needs no
//! hardware emulation at all: it is a clock, and volamos already has a
//! clock. (`ciaa.resource`/`ciab.resource`, the other pair guest code
//! commonly opens, would mean emulating the CIAs' timers and interrupt
//! wiring -- a different undertaking, and not one a CLI runtime needs.)
//!
//! # The three functions
//!
//! Per the NDK 3.2 autodocs (`battclock.doc`, all three `(V36)`) and
//! `include_i/lvo/battclock_lib.i` for the offsets:
//!
//! | LVO | Function | Contract |
//! | ---: | --- | --- |
//! | -6 | `ResetBattClock()` | "put the clock chip into a working and usable state and also set the date on the clock chip to 01-Jan-1978" |
//! | -12 | `ReadBattClock()` | `D0` = "the number of seconds from 01-Jan-1978 that the clock chip thinks it is" |
//! | -18 | `WriteBattClock(time)` | `D0` = the seconds-since-1978 value "that should be written to the clock chip" |
//!
//! There is no public `struct` for the resource: guest code holds the
//! base `OpenResource` returned (`BattClockBase`, which `proto/
//! battclock.h` declares as a `struct Library *`) and calls through its
//! negative offsets, exactly like a library. So this is built the same
//! way [`crate::dispatch::TIMER_DEVICE_BASE`] is -- a real base inside
//! the reserved trap-table region, a [`write_library_node`]-style
//! header on it, handlers registered at the real LVOs -- with two
//! additions a resource needs: `ln_Type` is `NT_RESOURCE`, and the node
//! is linked onto `ExecBase.ResourceList` at startup so `OpenResource`
//! finds it by name like any guest-added resource.
//!
//! # A writable clock, without a second clock to keep
//!
//! `ReadBattClock` reports the same host wall clock
//! [`crate::exectask::host_time_secs_micro`] gives `GetSysTime` and
//! [`crate::dosdate::now_as_datestamp`] gives `DateStamp`, in the same
//! AmigaOS epoch (1978-01-01) those already use -- so an unwritten
//! chip and the system clock read the same time, which is what makes a
//! `SetClock SAVE` / `SetClock LOAD` round trip a no-op rather than a
//! time shift. (That also means it reports what the host reports,
//! which on a host configured for UTC is UTC, not local time --
//! volamos's existing `DateStamp`/`GetSysTime` already behave that
//! way, and matching them matters more here than matching a real
//! Amiga's local-time-only world view.)
//!
//! The agreement is one-directional, though, and only until something
//! writes the chip: `timer.device`'s `TR_SETSYSTIME` is a no-op in
//! this runtime (there is no virtual clock to set -- see
//! [`crate::exectask`]'s `do_io_handler`), so `SetClock LOAD` cannot
//! push a written RTC value back into the system clock. After a
//! `WriteBattClock(t)` with `t` far from now, `ReadBattClock` and
//! `DateStamp` genuinely disagree, by design, for the rest of the run.
//!
//! A real clock chip can be *set*, though, and then keeps ticking from
//! there. Rather than carry a second clock, this keeps one signed
//! second offset between what the chip reports and what the host says,
//! so the chip always ticks at the host's rate:
//!
//! - `ReadBattClock` -> `host + offset`
//! - `WriteBattClock(t)` -> `offset = t - host`
//! - `ResetBattClock` -> `offset = -host`, i.e. the chip now reads
//!   `0` = 01-Jan-1978, exactly as the autodoc specifies, and counts up
//!   from there
//!
//! The offset lives in guest memory, in a private longword just past
//! the resource's header (see [`BC_OFFSET_FIELD`]) rather than in a
//! host-side `struct`, following the same "guest memory is the single
//! source of truth" convention [`crate::exectask`]'s module docs
//! establish for task and signal state. It is **not** a field of any
//! real `battclock.resource` -- there is no documented struct to
//! conflict with, and nothing but this module reads it.
//!
//! Each run starts with the offset at `0` (the chip agrees with the
//! host); nothing persists across runs, as volamos is one process per
//! invocation with no boot state to keep.
//!
//! # The simpler modelling, deliberately not taken
//!
//! A real clock is not the only honest answer here. Plenty of real
//! Amigas -- a stock A500, A600 or A1200 -- have no battery-backed
//! clock fitted at all, and the autodoc already says what the resource
//! does then: "if the clock chip returns an invalid date, the clock
//! chip is reset and 0 is returned". So always reporting `0` would be
//! a legitimate implementation of this resource, not a stub, and
//! guests have to cope with it regardless.
//!
//! That would also be enough for what prompted the resource: the gcc
//! startup code needs `OpenResource` to *succeed*, and doesn't care
//! what time comes back. A working clock is the better answer while it
//! stays this cheap, and is what this module implements -- but if some
//! future interaction (a timezone question, a `--clock-*` flag, the
//! `TR_SETSYSTIME` asymmetry above) ever makes a live clock genuinely
//! awkward, degrading to "no clock fitted" is a sanctioned
//! simplification rather than a regression. It would change what
//! guests see, so it belongs in `userdocs/Supported-Libraries.md` if
//! it ever happens.

use crate::cpu::{Cpu, DataRegister};
use crate::dispatch::{BATTCLOCK_RESOURCE_BASE, DispatchError, HandlerContext, LibraryTable};
use crate::memory::AddressSpace;

/// `battclock.resource`'s name, as `resources/battclock.i`'s
/// `BATTCLOCKNAME` macro spells it -- the string `OpenResource` matches
/// on, so it has to be exact.
pub const BATTCLOCK_NAME: &str = "battclock.resource";

/// `exec/nodes.h`'s `NT_RESOURCE`. Real `battclock.resource`'s node
/// carries this rather than `NT_LIBRARY`; nothing in this runtime reads
/// it, but guest code walking `ExecBase.ResourceList` can.
const NT_RESOURCE: u8 = 8;

/// Byte offset, from [`BATTCLOCK_RESOURCE_BASE`], of this runtime's
/// private "how far the clock chip has been set from the host clock"
/// longword, read as a signed delta in seconds (see the module docs).
/// Sits immediately past the `struct Library`-shaped header
/// [`crate::dispatch::write_library_node`] writes (34 bytes), inside
/// the positive-offset room the base's chunk reserves for exactly this
/// kind of thing. Private to this module -- real `battclock.resource`
/// has no public structure at all.
const BC_OFFSET_FIELD: u32 = 34;

/// Byte offset, from [`BATTCLOCK_RESOURCE_BASE`], of the resource's own
/// `NUL`-terminated name string, which its node's `ln_Name` points at:
/// just past [`BC_OFFSET_FIELD`]'s longword. 19 bytes including the
/// terminator, well inside the chunk's positive-offset room.
const BC_NAME_FIELD: u32 = BC_OFFSET_FIELD + 4;

/// The private fields above have to stay inside the positive-offset
/// room this base's chunk reserves -- past it sit
/// [`crate::dispatch::EXIT_STUB_ADDR`] and
/// [`crate::dispatch::FPU_DISABLED_060_DECLINE_STUB_ADDR`], the two
/// end-anchored A-line stub words, and overwriting the first of those
/// would stop the guest's outermost `rts` trapping at all, i.e. break
/// program termination with no compile- or test-time signal. Adding a
/// field (or a longer name string) past the chunk's end is therefore a
/// compile error rather than a silent clobber.
///
/// First byte past the last private field (the name string's `NUL`
/// included) -- split out so the bound below reads as one comparison.
const BC_FIELDS_END: u32 = BC_NAME_FIELD + BATTCLOCK_NAME.len() as u32 + 1;
const _: () = assert!(
    BC_FIELDS_END <= crate::dispatch::EXIT_STUB_ADDR - BATTCLOCK_RESOURCE_BASE,
    "battclock.resource's private fields must fit below EXIT_STUB_ADDR"
);

/// `ResetBattClock`'s LVO, per `include_i/lvo/battclock_lib.i`.
const LVO_RESET_BATT_CLOCK: i32 = -6;
/// `ReadBattClock`'s LVO.
const LVO_READ_BATT_CLOCK: i32 = -12;
/// `WriteBattClock`'s LVO.
const LVO_WRITE_BATT_CLOCK: i32 = -18;

/// What the clock chip currently reads, in AmigaOS-epoch seconds: the
/// host clock plus however far the chip has been set away from it.
/// Wrapping, like the real chip's `ULONG`, so a guest that writes an
/// absurd value gets consistent arithmetic rather than a host panic.
fn chip_secs<M: AddressSpace + ?Sized>(mem: &M) -> u32 {
    let (host_secs, _micro) = crate::exectask::host_time_secs_micro();
    let offset = mem.read_u32(BATTCLOCK_RESOURCE_BASE + BC_OFFSET_FIELD);
    host_secs.wrapping_add(offset)
}

/// Sets the chip to read `secs` from now on, by storing the delta from
/// the current host clock (see the module docs).
fn set_chip_secs<M: AddressSpace + ?Sized>(mem: &mut M, secs: u32) {
    let (host_secs, _micro) = crate::exectask::host_time_secs_micro();
    mem.write_u32(
        BATTCLOCK_RESOURCE_BASE + BC_OFFSET_FIELD,
        secs.wrapping_sub(host_secs),
    );
}

/// `ResetBattClock` (LVO -6): no inputs, no result. "Put the clock chip
/// into a working and usable state and also set the date on the clock
/// chip to 01-Jan-1978" (autodoc) -- there is no chip to bring up, so
/// the whole of it here is setting the time to `0`, the epoch itself.
fn reset_batt_clock_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    set_chip_secs(ctx.mem, 0);
    Ok(())
}

/// `ReadBattClock` (LVO -12): `D0` = seconds since 01-Jan-1978 that the
/// clock chip thinks it is. The autodoc's "if the clock chip returns an
/// invalid date, the clock chip is reset and 0 is returned" case can't
/// arise here -- the host clock is the chip -- except that
/// [`crate::exectask::host_time_secs_micro`] already reports `0` for a
/// host clock set before 1978, which is the same answer.
fn read_batt_clock_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let secs = chip_secs(ctx.mem);
    ctx.cpu.set_data_register(DataRegister(0), secs);
    Ok(())
}

/// `WriteBattClock` (LVO -18): `D0` = the seconds-since-1978 value to
/// write to the clock chip. Kept for the rest of the run (see the
/// module docs on the offset), so a `ReadBattClock` right after reads
/// back what was written, and a later one reads it plus the elapsed
/// time -- as a real chip would. Never touches the host clock, or
/// volamos's own `DateStamp`/`GetSysTime`: on real AmigaOS, writing the
/// RTC is `SetClock SAVE`, which doesn't change the system time either.
fn write_batt_clock_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let secs = ctx.cpu.data_register(DataRegister(0));
    set_chip_secs(ctx.mem, secs);
    Ok(())
}

/// Builds the resource at [`BATTCLOCK_RESOURCE_BASE`] and links it onto
/// `ExecBase.ResourceList` so `OpenResource("battclock.resource")`
/// finds it. Called once from [`crate::dispatch::Runtime::new`], after
/// the resource list's own header is initialized (an `Enqueue` onto an
/// uninitialized list would corrupt it) -- the same ordering
/// `write_library_list_nodes` relies on for `LibList`.
///
/// Uses [`crate::dispatch::write_library_node`] for the header and
/// [`crate::execlist`]'s own `Enqueue` primitive for the linking, so
/// the node this installs is indistinguishable from one a guest added
/// itself with `AddResource` -- `RemResource` can even unlink it, which
/// is exactly what a real resource list allows.
pub fn install_battclock_resource<M: AddressSpace>(mem: &mut M) {
    crate::dispatch::write_library_node(mem, BATTCLOCK_RESOURCE_BASE);
    mem.write_u8(
        BATTCLOCK_RESOURCE_BASE + crate::dispatch::LIB_NODE_TYPE_OFFSET,
        NT_RESOURCE,
    );
    // The chip starts out agreeing with the host clock.
    mem.write_u32(BATTCLOCK_RESOURCE_BASE + BC_OFFSET_FIELD, 0);

    let name_addr = BATTCLOCK_RESOURCE_BASE + BC_NAME_FIELD;
    crate::guestmem::write_c_string(mem, name_addr, BATTCLOCK_NAME.as_bytes());
    mem.write_u32(
        BATTCLOCK_RESOURCE_BASE + crate::execlist::LN_NAME,
        name_addr,
    );
    crate::execlist::enqueue_impl(
        mem,
        crate::dispatch::EXEC_LIBRARY_BASE + crate::dispatch::EXEC_BASE_RESOURCELIST_OFFSET,
        BATTCLOCK_RESOURCE_BASE,
    );
}

/// Registers the resource's three vectors on
/// [`BATTCLOCK_RESOURCE_BASE`]. Registered directly by offset rather
/// than through a generated LVO table, exactly as `crate::exectask`
/// does for `timer.device`'s library-style vectors: three well-known,
/// NDK-confirmed offsets don't warrant one. Called unconditionally from
/// [`crate::dispatch::Runtime::new`].
pub fn register_battclock_handlers<C: Cpu + 'static>(
    table: &mut LibraryTable<C>,
    mem: &mut C::Memory,
) {
    macro_rules! reg {
        ($lvo:expr, $name:literal, $handler:expr) => {
            table.register(
                mem,
                BATTCLOCK_RESOURCE_BASE,
                $lvo,
                BATTCLOCK_NAME,
                $name,
                $handler,
            );
        };
    }
    reg!(
        LVO_RESET_BATT_CLOCK,
        "ResetBattClock",
        reset_batt_clock_handler::<C>
    );
    reg!(
        LVO_READ_BATT_CLOCK,
        "ReadBattClock",
        read_batt_clock_handler::<C>
    );
    reg!(
        LVO_WRITE_BATT_CLOCK,
        "WriteBattClock",
        write_batt_clock_handler::<C>
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{EXEC_LIBRARY_BASE, Runtime, StartConfig};
    use crate::memory::FlatMemory;

    fn load_words<M: AddressSpace>(mem: &mut M, addr: u32, words: &[u16]) {
        let mut offset = addr;
        for &w in words {
            mem.write_u16(offset, w);
            offset += 2;
        }
    }

    /// `move.l #imm32,An`.
    fn move_imm_to_a(n: u16) -> u16 {
        0x207C | (n << 9)
    }
    /// `move.l #imm32,Dn`.
    fn move_imm_to_d(n: u16) -> u16 {
        0x203C | (n << 9)
    }
    /// `jsr <disp16>(a6)`.
    fn jsr_disp16_a6(disp: i32) -> [u16; 2] {
        [0x4EAE, disp as u16]
    }
    const RTS: u16 = 0x4E75;

    fn movea_imm_a6(base: u32) -> [u16; 3] {
        [move_imm_to_a(6), (base >> 16) as u16, base as u16]
    }

    /// Where [`run_code`] writes the resource's name string, for the
    /// `OpenResource` test's `A1`.
    const NAME_ADDR: u32 = 0x1_8000;

    /// Runs `words` -- which inherit nothing, so each program sets its
    /// own `A6` -- and returns the exit code, i.e. whatever is in `D0`
    /// at the final `rts`.
    fn run_code(words: &[u16]) -> i32 {
        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, words);
        crate::guestmem::write_c_string(&mut mem, NAME_ADDR, BATTCLOCK_NAME.as_bytes());
        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: entry + 0x400,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed")
    }

    #[test]
    fn open_resource_finds_battclock_on_the_resource_list() {
        // Issue #278: exactly what the m68k-amigaos-gcc startup code
        // does for every `__LIB_LIST__` name ending in ".resource" --
        // and what returned NULL (and then crashed the stock crt0,
        // which doesn't check) before this resource existed.
        let mut words = movea_imm_a6(EXEC_LIBRARY_BASE).to_vec();
        words.push(move_imm_to_a(1)); // A1 = "battclock.resource"
        words.push((NAME_ADDR >> 16) as u16);
        words.push(NAME_ADDR as u16);
        words.extend_from_slice(&jsr_disp16_a6(-498)); // OpenResource
        words.push(RTS);

        assert_eq!(
            run_code(&words) as u32,
            BATTCLOCK_RESOURCE_BASE,
            "OpenResource(\"battclock.resource\") should return the real base"
        );
    }

    #[test]
    fn installed_resource_node_is_named_and_typed_as_a_resource() {
        let rt = {
            let mut mem = FlatMemory::new(0x2_0000);
            let entry = TRAP_TABLE_END;
            load_words(&mut mem, entry, &[RTS]);
            Runtime::new(
                M68kCpu::new(),
                mem,
                StartConfig {
                    entry,
                    load_end: entry + 0x100,
                    args: Vec::new(),
                    ..StartConfig::default()
                },
            )
        };
        let mem = rt.memory();
        assert_eq!(
            mem.read_u8(BATTCLOCK_RESOURCE_BASE + crate::dispatch::LIB_NODE_TYPE_OFFSET),
            NT_RESOURCE,
            "a resource's ln_Type is NT_RESOURCE, not NT_LIBRARY"
        );
        let name_ptr = mem.read_u32(BATTCLOCK_RESOURCE_BASE + crate::execlist::LN_NAME);
        assert_eq!(
            crate::guestmem::read_c_string(mem, name_ptr),
            BATTCLOCK_NAME.as_bytes(),
            "ln_Name must be the exact BATTCLOCKNAME string OpenResource matches on"
        );
    }

    #[test]
    fn read_batt_clock_agrees_with_the_system_clock() {
        // Both are the same host clock in the same (1978) epoch, so
        // they must not disagree by more than the moment between the
        // two calls -- the property SetClock SAVE/LOAD depends on.
        let mut words = movea_imm_a6(BATTCLOCK_RESOURCE_BASE).to_vec();
        words.extend_from_slice(&jsr_disp16_a6(LVO_READ_BATT_CLOCK));
        words.push(RTS);
        let chip = run_code(&words) as u32;

        let (sys_secs, _micro) = crate::exectask::host_time_secs_micro();
        let skew = sys_secs.abs_diff(chip);
        assert!(
            skew <= 2,
            "ReadBattClock ({chip}) should match GetSysTime/DateStamp's clock ({sys_secs})"
        );

        // Pin the epoch independently. Comparing against
        // host_time_secs_micro alone can't: both sides would move
        // together if the epoch offset were ever dropped. So derive
        // the expected value straight from the host's Unix clock, with
        // the 1970 -> 1978 day count (8 years, 1972 and 1976 leap)
        // spelled out here rather than taken from
        // dosdate::UNIX_TO_AMIGA_EPOCH_DAYS -- an error in that
        // constant should fail this test, not be inherited by it.
        let unix_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("host clock is past the Unix epoch")
            .as_secs();
        let expected = unix_secs - (8 * 365 + 2) * 86_400;
        assert!(
            u64::from(chip).abs_diff(expected) <= 2,
            "ReadBattClock ({chip}) should be seconds since 1978-01-01, i.e. about \
             {expected} -- a Unix-epoch value would be {unix_secs}"
        );
    }

    #[test]
    fn write_batt_clock_is_read_back_and_keeps_ticking() {
        // WriteBattClock(1_000_000) then ReadBattClock: the chip reads
        // back what was written (plus at most the second or two the
        // host clock advanced in between, since a real chip keeps
        // running -- see the module docs on the offset model).
        const WRITTEN: u32 = 1_000_000;
        let mut words = movea_imm_a6(BATTCLOCK_RESOURCE_BASE).to_vec();
        words.push(move_imm_to_d(0)); // D0 = the time to write
        words.push((WRITTEN >> 16) as u16);
        words.push(WRITTEN as u16);
        words.extend_from_slice(&jsr_disp16_a6(LVO_WRITE_BATT_CLOCK));
        words.extend_from_slice(&jsr_disp16_a6(LVO_READ_BATT_CLOCK));
        words.push(RTS);

        let read_back = run_code(&words) as u32;
        assert!(
            (WRITTEN..=WRITTEN + 2).contains(&read_back),
            "ReadBattClock after WriteBattClock({WRITTEN}) gave {read_back}"
        );
    }

    #[test]
    fn reset_batt_clock_sets_the_chip_to_the_amiga_epoch() {
        // "...also sets the date on the clock chip to 01-Jan-1978"
        // (NDK battclock.doc/ResetBattClock), i.e. 0 seconds -- even
        // after a WriteBattClock put it somewhere else.
        let mut words = movea_imm_a6(BATTCLOCK_RESOURCE_BASE).to_vec();
        words.push(move_imm_to_d(0));
        words.push(0x1234);
        words.push(0x5678);
        words.extend_from_slice(&jsr_disp16_a6(LVO_WRITE_BATT_CLOCK));
        words.extend_from_slice(&jsr_disp16_a6(LVO_RESET_BATT_CLOCK));
        words.extend_from_slice(&jsr_disp16_a6(LVO_READ_BATT_CLOCK));
        words.push(RTS);

        let after_reset = run_code(&words) as u32;
        assert!(
            after_reset <= 2,
            "ResetBattClock should put the chip back at the 1978 epoch, not {after_reset}"
        );
    }
}
