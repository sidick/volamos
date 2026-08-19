//! `intuition.library`: a thin stub -- `DisplayAlert`, `AutoRequest`,
//! `EasyRequestArgs`, `CurrentTime`, matching vamos's own scope for this
//! library ("just enough that a console tool calling a stray Intuition
//! function doesn't crash. No real windowing/GUI"), added closing a gap
//! found comparing volamos's coverage against vamos's own
//! (`docs/plan.md`'s dated entry). This runtime has no display, no
//! windowing, no `Screen`/`Window`/`Gadget` model at all -- every
//! function here does the least a headless runtime honestly can, not a
//! real GUI implementation.
//!
//! # `DisplayAlert`: reuses `exec.library`'s own `Alert` handling
//!
//! Real `DisplayAlert` is Intuition's own presentation layer for the
//! same Guru-Meditation-style alert `exec.library`'s `Alert` (see
//! `crate::exectask::alert_handler`) triggers when Intuition is
//! available to draw it -- same `AT_DeadEnd` semantics (a dead-end
//! alert means the guest declared system integrity can't be
//! guaranteed), just with a custom message string and height instead of
//! a numeric code. This handler mirrors `alert_handler` exactly: always
//! logs to stderr unconditionally (a real alert is never silent on real
//! hardware), dead-end fails loudly via
//! [`DispatchError::HandlerFailed`], recoverable returns `TRUE`
//! (`D0`) as if the user dismissed it instantly (there's no display to
//! wait on input from).
//!
//! # `AutoRequest`/`EasyRequestArgs`: no display, no real choice to report
//!
//! Both real functions show a requester and report *which button the
//! user pressed*. With no display, there's no user input to honestly
//! report -- these return a fixed default (`AutoRequest`: `TRUE`,
//! matching its positive/default gadget; `EasyRequestArgs`: `0`, its
//! documented "leftmost gadget" result) rather than trying to guess
//! which choice a real user would have made. A caller that actually
//! branches on the result differently per button will see the wrong
//! branch -- an inherent limitation of a headless stub, not something
//! this runtime can resolve without a real display.
//!
//! # `CurrentTime`: the one genuinely real function here
//!
//! Unlike the requester functions, `CurrentTime` has an honest, correct
//! answer even headlessly: the real host wall-clock time. Reuses
//! [`crate::exectask::host_time_secs_micro`], the same AmigaOS-epoch
//! (1978-01-01) seconds-plus-microseconds source
//! `timer.device`'s`GetSysTime` already uses -- `CurrentTime`'s own
//! Autodoc documents the identical epoch/units.

use crate::cpu::{AddressRegister, Cpu, DataRegister};
use crate::dispatch::{DispatchError, HandlerContext, LibraryTable};
use crate::exectask::host_time_secs_micro;
use crate::lvos::intuition::INTUITION_LVOS;
use crate::memory::AddressSpace;

/// `AT_DeadEnd` (bit 31, `0x80000000`), per `<exec/alerts.h>` -- same
/// meaning and value as `crate::exectask::AT_DEAD_END`, duplicated here
/// (rather than made `pub(crate)` there and imported) since it's a
/// small, self-contained fact this module needs independently, not a
/// shared piece of exec-task state.
const AT_DEAD_END: u32 = 0x8000_0000;

/// `DisplayAlert` (`D0` = `alertNumber`, `A0` = `string` `CString*`,
/// `D1` = `height`, ignored -- a real display-layout hint this runtime
/// has no display to apply). `D0` = `TRUE`/`FALSE` (`BOOL`) -- see the
/// module docs.
fn display_alert_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let alert_num = ctx.cpu.data_register(DataRegister(0));
    let string_ptr = ctx.cpu.address_register(AddressRegister(0));
    let message = crate::guestmem::read_c_string(ctx.mem, string_ptr);
    let message = String::from_utf8_lossy(&message);
    let dead_end = alert_num & AT_DEAD_END != 0;
    eprintln!(
        "volamos: DisplayAlert({alert_num:#010x}): {} -- {message:?}",
        if dead_end {
            "AT_DeadEnd"
        } else {
            "AT_Recovery"
        }
    );
    if dead_end {
        return Err(DispatchError::HandlerFailed {
            library: "intuition.library".to_string(),
            lvo: -90,
            handler_name: "DisplayAlert".to_string(),
            message: format!(
                "DisplayAlert({alert_num:#010x}, {message:?}): AT_DeadEnd set -- the guest \
                 has declared system integrity can no longer be guaranteed; a real machine \
                 would reboot or hang here rather than return to the caller"
            ),
        });
    }
    ctx.cpu.set_data_register(DataRegister(0), 1);
    Ok(())
}

/// `AutoRequest` (`A0` = window, `A1`/`A2`/`A3` = body/positive/negative
/// `IntuiText*`, `D0`-`D3` = flags/width/height -- all ignored, no
/// display to show any of it on). `D0` = `TRUE` unconditionally -- see
/// the module docs.
fn auto_request_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    ctx.cpu.set_data_register(DataRegister(0), 1);
    Ok(())
}

/// `EasyRequestArgs` (`A0` = window, `A1` = `struct EasyStruct*`, `A2` =
/// `IDCMP` pointer, `A3` = args -- all ignored). `D0` = `0` (the
/// documented "leftmost gadget" result) unconditionally -- see the
/// module docs.
fn easy_request_args_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    ctx.cpu.set_data_register(DataRegister(0), 0);
    Ok(())
}

/// `CurrentTime` (`A0` = `ULONG*` seconds out-param, `A1` = `ULONG*`
/// micros out-param). No return value. See the module docs.
fn current_time_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let seconds_ptr = ctx.cpu.address_register(AddressRegister(0));
    let micros_ptr = ctx.cpu.address_register(AddressRegister(1));
    let (secs, micro) = host_time_secs_micro();
    ctx.mem.write_u32(seconds_ptr, secs);
    ctx.mem.write_u32(micros_ptr, micro);
    Ok(())
}

/// Registers this module's `intuition.library` handlers, looked up by
/// name through [`INTUITION_LVOS`], following
/// [`crate::execmem::register_execmem_handlers`]'s registration
/// pattern. Called unconditionally from
/// [`crate::dispatch::Runtime::new`] -- `intuition.library` is a
/// [`crate::dispatch::STANDARD_WORKBENCH_LIBRARIES`] member (always
/// present, matching real KS/WB 3.1 ROM-resident behavior).
pub fn register_intuition_handlers<C: Cpu + 'static>(
    table: &mut LibraryTable<C>,
    mem: &mut C::Memory,
) {
    macro_rules! reg {
        ($name:literal, $handler:expr) => {
            table
                .register_by_name(
                    mem,
                    crate::dispatch::INTUITION_LIBRARY_BASE,
                    INTUITION_LVOS,
                    "intuition.library",
                    $name,
                    $handler,
                )
                .unwrap_or_else(|e| panic!("{} should be in INTUITION_LVOS: {e}", $name));
        };
    }
    reg!("DisplayAlert", display_alert_handler::<C>);
    reg!("AutoRequest", auto_request_handler::<C>);
    reg!("EasyRequestArgs", easy_request_args_handler::<C>);
    reg!("CurrentTime", current_time_handler::<C>);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{INTUITION_LIBRARY_BASE, Runtime, StartConfig};
    use crate::memory::FlatMemory;

    fn load_words<M: AddressSpace>(mem: &mut M, addr: u32, words: &[u16]) {
        let mut offset = addr;
        for &w in words {
            mem.write_u16(offset, w);
            offset += 2;
        }
    }

    fn move_imm_to_a(n: u16) -> u16 {
        0x207C | (n << 9)
    }
    fn move_imm_to_d(n: u16) -> u16 {
        0x203C | (n << 9)
    }
    fn jsr_disp16_a6(disp: i32) -> [u16; 2] {
        [0x4EAE, disp as u16]
    }
    const RTS: u16 = 0x4E75;

    fn movea_intuition_base_to_a6() -> [u16; 3] {
        [
            move_imm_to_a(6),
            (INTUITION_LIBRARY_BASE >> 16) as u16,
            INTUITION_LIBRARY_BASE as u16,
        ]
    }

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

    fn intuition_program(words: &[u16]) -> Runtime<M68kCpu> {
        let mut full = movea_intuition_base_to_a6().to_vec();
        full.extend_from_slice(words);
        runtime_with_program(&full)
    }

    #[test]
    fn end_to_end_auto_request_always_returns_true() {
        let words = [RTS];
        let mut full = movea_intuition_base_to_a6().to_vec();
        full.extend_from_slice(&jsr_disp16_a6(-348));
        full.extend_from_slice(&words);
        let mut rt = runtime_with_program(&full);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 1);
    }

    #[test]
    fn end_to_end_easy_request_args_always_returns_zero() {
        let mut full = movea_intuition_base_to_a6().to_vec();
        full.extend_from_slice(&jsr_disp16_a6(-588));
        full.push(RTS);
        let mut rt = runtime_with_program(&full);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 0);
    }

    #[test]
    fn end_to_end_current_time_writes_plausible_values() {
        let secs_addr: u32 = 0x1_8000;
        let micro_addr: u32 = 0x1_8004;

        let mut words = vec![
            move_imm_to_a(0), // A0 = &seconds
            (secs_addr >> 16) as u16,
            secs_addr as u16,
            move_imm_to_a(1), // A1 = &micros
            (micro_addr >> 16) as u16,
            micro_addr as u16,
        ];
        words.extend_from_slice(&jsr_disp16_a6(-84)); // CurrentTime
        words.push(RTS);

        let mut rt = intuition_program(&words);
        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed");

        // Any real Amiga-epoch (1978-01-01) timestamp for "now" is a
        // large positive number of seconds; micros must be < 1_000_000.
        let secs = rt.memory().read_u32(secs_addr);
        let micro = rt.memory().read_u32(micro_addr);
        assert!(secs > 0);
        assert!(micro < 1_000_000);
    }

    #[test]
    fn end_to_end_display_alert_recoverable_returns_true() {
        let msg_addr: u32 = 0x1_8000;

        let mut words = vec![
            move_imm_to_d(0), // D0 = 0x00010000 (AG_NoMemory, AT_Recovery)
            0x0001,
            0x0000,
            move_imm_to_a(0), // A0 = message string
            (msg_addr >> 16) as u16,
            msg_addr as u16,
            move_imm_to_d(1), // D1 = height (ignored)
            0,
            0,
        ];
        words.extend_from_slice(&jsr_disp16_a6(-90)); // DisplayAlert
        words.push(RTS);

        let mut full = movea_intuition_base_to_a6().to_vec();
        full.extend_from_slice(&words);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &full);
        crate::guestmem::write_c_string(&mut mem, msg_addr, b"out of memory");
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
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 1);
    }

    #[test]
    fn end_to_end_display_alert_dead_end_fails_loudly() {
        let msg_addr: u32 = 0x1_8000;

        let mut words = vec![
            move_imm_to_d(0), // D0 = 0x80010000 (AG_NoMemory, AT_DeadEnd)
            0x8001,
            0x0000,
            move_imm_to_a(0), // A0 = message string
            (msg_addr >> 16) as u16,
            msg_addr as u16,
            move_imm_to_d(1), // D1 = height (ignored)
            0,
            0,
        ];
        words.extend_from_slice(&jsr_disp16_a6(-90)); // DisplayAlert
        words.push(RTS);

        let mut full = movea_intuition_base_to_a6().to_vec();
        full.extend_from_slice(&words);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &full);
        crate::guestmem::write_c_string(&mut mem, msg_addr, b"fatal");
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
        let mut out = Vec::new();
        let err = rt
            .run(&mut out, None)
            .expect_err("dead-end DisplayAlert should fail");
        match err {
            crate::dispatch::RuntimeError::Dispatch(DispatchError::HandlerFailed {
                library,
                lvo,
                handler_name,
                ..
            }) => {
                assert_eq!(library, "intuition.library");
                assert_eq!(lvo, -90);
                assert_eq!(handler_name, "DisplayAlert");
            }
            other => panic!("expected HandlerFailed, got {other:?}"),
        }
    }
}
