//! `exec.library`'s interrupt calls: loud stubs for the hardware-interrupt
//! registration calls (`SetIntVector`, `AddIntServer`, `RemIntServer`) and
//! a real, hardware-free `Cause()` (software interrupts).
//!
//! # Why the stubs fail instead of registering
//!
//! This runtime has no interrupt sources: no custom-chip model, no timer
//! tick, no VBlank. A server added with `AddIntServer(INTB_VERTB, ...)`
//! would never run, and a program that depends on it (a game loop paced
//! by a VBlank server, a library whose init hooks `INTB_PORTS`) would
//! hang or misbehave far from the real cause. None of the three calls has
//! an API-level failure return (`SetIntVector` returns the old vector, the
//! others return nothing), so the only honest answer is to stop the run
//! with a [`DispatchError::HandlerFailed`] that says what was asked for.
//! Accepting the registration and never firing it is a possible later
//! upgrade, to be decided per real program (see issue #136).
//!
//! # `Cause()`
//!
//! Software interrupts are deterministic and need no hardware, so they
//! are implemented for real, following the autodoc's IMPLEMENTATION
//! notes: a node whose `ln_Type` is already `NT_SOFTINT` is pending and is
//! ignored (no nest count); otherwise it is marked `NT_SOFTINT` and queued
//! on one of exec's five priority levels (`ln_Pri` of -32, -16, 0, +16,
//! +32; in-between values are truncated downwards, as the real
//! `asr #4` indexing does). A priority outside -32..=+32 is rejected
//! with a [`DispatchError::HandlerFailed`] (the autodoc says such values
//! "are not allowed"; real exec would index past its five-entry
//! `SoftIntList` array and corrupt `ExecBase`), as is a `NULL` node or a
//! node with `is_Code == NULL`. Validation happens before anything is
//! written, so a rejected `Cause` leaves the node untouched. The
//! node's type returns to `NT_INTERRUPT` when it is taken off the queue,
//! *before* its handler runs, so a handler may `Cause()` its own node
//! again.
//!
//! ## When the handler runs
//!
//! Real `Cause()` from task level with interrupts enabled preempts the
//! caller, i.e. the handler has run by the time `Cause` returns. This
//! runtime does exactly that. It does **not** defer while the guest is
//! inside `Disable()`: `Disable`/`Enable` keep no nest count here (see
//! `exectask.rs`), so there is nothing to defer against, and a guest that
//! `Cause`s inside a `Disable` section sees the handler run immediately
//! rather than at `Enable`. A handler that itself calls `Cause()` queues
//! the new node; it runs after the current handler returns, still inside
//! the outermost `Cause`, highest priority first and FIFO within a level.
//!
//! ## Registers
//!
//! The handler is entered with `A1` = `is_Data`, `A5` = `is_Code`,
//! `A6` = ExecBase. `D0`/`D1`/`A0`/`A1`/`A5` are scratch for the handler
//! and all other registers must be preserved. Unlike other interrupt
//! types, the `Cause` autodoc's BUGS section says a software interrupt
//! handler **must preserve `A6`**, and real hardware relies on that;
//! volamos merely tolerates a handler that trashes `A6` (or `A5`), since
//! it sets `A6` fresh for every handler and restores the caller's `A5`
//! and `A6` once the queue has drained (a real interrupt return is
//! transparent to the interrupted code). The `D0`/`D1`/`A0`/`A1` the
//! caller sees afterwards are ordinary library-call scratch. So a guest
//! that works here by trashing `A6` in a soft-interrupt handler is still
//! wrong on a real machine.
//!
//! Handlers here run in user mode on the caller's task stack, whereas
//! real software interrupts run in supervisor mode on the system stack,
//! so calls a real interrupt forbids (anything that can `Wait`) are not
//! caught.
//!
//! Each queued handler is run through
//! [`crate::dispatch::ContinuationStack::trampoline`]; the continuation
//! that follows one handler's return starts the next, so a whole queue
//! drains through one chain of continuations.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use crate::cpu::{AddressRegister, Cpu, DataRegister};
use crate::dispatch::{DispatchError, EXEC_LIBRARY_BASE, HandlerContext, LibraryTable};
use crate::lvos::exec::EXEC_LVOS;
use crate::memory::AddressSpace;

/// `ln_Type` offset within `struct Node`.
const LN_TYPE: u32 = 8;
/// `ln_Pri` offset within `struct Node`.
const LN_PRI: u32 = 9;
/// `is_Data` offset within `struct Interrupt` (after the 14-byte Node).
const IS_DATA: u32 = 14;
/// `is_Code` offset within `struct Interrupt`.
const IS_CODE: u32 = 18;
/// `NT_INTERRUPT` (`<exec/nodes.h>`).
const NT_INTERRUPT: u8 = 2;
/// `NT_SOFTINT` (`<exec/nodes.h>`): "internal flag used by SoftInts".
const NT_SOFTINT: u8 = 11;

/// Number of software-interrupt priority levels (-32, -16, 0, +16, +32).
const LEVELS: usize = 5;

/// Maps an `ln_Pri` byte to its queue index, 0 (lowest, -32) to 4
/// (highest, +32): `(pri >> 4) + 2` on the signed value. `None` for a
/// priority outside -32..=+32, which the autodoc disallows.
fn level_for_priority(pri: i8) -> Option<usize> {
    (-32..=32)
        .contains(&pri)
        .then(|| ((i32::from(pri) >> 4) + 2) as usize)
}

fn cause_error(message: String) -> DispatchError {
    DispatchError::HandlerFailed {
        library: "exec.library".to_string(),
        lvo: -180,
        handler_name: "Cause".to_string(),
        message,
    }
}

/// Names `INTB_*` bit numbers from `<hardware/intbits.h>` (0-14) plus
/// `INTB_NMI` (15, `<exec/interrupts.h>`) for the stub messages.
fn intb_name(num: u32) -> String {
    const NAMES: [&str; 16] = [
        "INTB_TBE",
        "INTB_DSKBLK",
        "INTB_SOFTINT",
        "INTB_PORTS",
        "INTB_COPER",
        "INTB_VERTB",
        "INTB_BLIT",
        "INTB_AUD0",
        "INTB_AUD1",
        "INTB_AUD2",
        "INTB_AUD3",
        "INTB_RBF",
        "INTB_DSKSYNC",
        "INTB_EXTER",
        "INTB_INTEN",
        "INTB_NMI",
    ];
    // The autodoc encodes intNum in D0 bits 0-4.
    let n = num & 0x1F;
    match NAMES.get(n as usize) {
        Some(name) => format!("{name} ({n})"),
        None => format!("unknown interrupt number {n}"),
    }
}

/// Pending software interrupts, shared by the `Cause` handler and the
/// continuations it chains.
#[derive(Default)]
struct SoftInts {
    /// One FIFO of node addresses per priority level, lowest level first.
    queues: [VecDeque<u32>; LEVELS],
    /// The caller's `(A5, A6)`, saved while a drain is in progress.
    /// `Some` is also the "a drain is already running" flag.
    saved: Option<(u32, u32)>,
}

/// Starts the next pending handler, or finishes the drain by restoring
/// the caller's `A5`/`A6`. Runs inside a `Cause` handler dispatch or a
/// continuation dispatch; either way it makes at most one trampoline call.
fn drain_step<C: Cpu + 'static>(
    ctx: &mut HandlerContext<'_, C>,
    state: Rc<RefCell<SoftInts>>,
) -> Result<(), DispatchError> {
    let next = state
        .borrow_mut()
        .queues
        .iter_mut()
        .rev()
        .find_map(VecDeque::pop_front);
    let Some(node) = next else {
        if let Some((a5, a6)) = state.borrow_mut().saved.take() {
            ctx.cpu.set_address_register(AddressRegister(5), a5);
            ctx.cpu.set_address_register(AddressRegister(6), a6);
        }
        return Ok(());
    };

    // Back to NT_INTERRUPT before the handler runs, as real exec does.
    ctx.mem.write_u8(node + LN_TYPE, NT_INTERRUPT);
    let data = ctx.mem.read_u32(node + IS_DATA);
    let code = ctx.mem.read_u32(node + IS_CODE); // non-NULL: checked at Cause time
    ctx.cpu.set_address_register(AddressRegister(1), data);
    ctx.cpu.set_address_register(AddressRegister(5), code);
    ctx.cpu
        .set_address_register(AddressRegister(6), EXEC_LIBRARY_BASE);
    ctx.continuations
        .trampoline(ctx.cpu, ctx.mem, code, move |ctx| drain_step(ctx, state));
    Ok(())
}

fn stub_error<C: Cpu>(
    ctx: &HandlerContext<'_, C>,
    lvo: i32,
    name: &str,
    what: &str,
) -> DispatchError {
    let int = intb_name(ctx.cpu.data_register(DataRegister(0)));
    let node = ctx.cpu.address_register(AddressRegister(1));
    DispatchError::HandlerFailed {
        library: "exec.library".to_string(),
        lvo,
        handler_name: name.to_string(),
        message: format!(
            "{name}({int}, {node:#010x}) requested {what}, but this runtime never delivers \
             hardware interrupts (no custom-chip model), so it would never run; \
             refusing instead of silently ignoring it"
        ),
    }
}

/// Registers `SetIntVector`/`AddIntServer`/`RemIntServer` (loud stubs)
/// and `Cause` (real). Called unconditionally from
/// [`crate::dispatch::Runtime::new`].
pub fn register_execint_handlers<C: Cpu + 'static>(
    table: &mut LibraryTable<C>,
    mem: &mut C::Memory,
) {
    macro_rules! reg_exec {
        ($name:literal, $handler:expr) => {
            table
                .register_by_name(
                    mem,
                    EXEC_LIBRARY_BASE,
                    EXEC_LVOS,
                    "exec.library",
                    $name,
                    $handler,
                )
                .unwrap_or_else(|e| panic!("{} should be in EXEC_LVOS: {e}", $name));
        };
    }
    reg_exec!("SetIntVector", |ctx: &mut HandlerContext<'_, C>| {
        Err(stub_error(
            ctx,
            -162,
            "SetIntVector",
            "replacing an interrupt vector",
        ))
    });
    reg_exec!("AddIntServer", |ctx: &mut HandlerContext<'_, C>| {
        Err(stub_error(ctx, -168, "AddIntServer", "an interrupt server"))
    });
    reg_exec!("RemIntServer", |ctx: &mut HandlerContext<'_, C>| {
        Err(stub_error(
            ctx,
            -174,
            "RemIntServer",
            "removing an interrupt server",
        ))
    });

    let state = Rc::new(RefCell::new(SoftInts::default()));
    reg_exec!("Cause", move |ctx: &mut HandlerContext<'_, C>| {
        let node = ctx.cpu.address_register(AddressRegister(1));
        if node == 0 {
            return Err(cause_error(
                "Cause(NULL): no struct Interrupt to run".to_string(),
            ));
        }
        if ctx.mem.read_u8(node + LN_TYPE) == NT_SOFTINT {
            return Ok(()); // already pending; no nest count
        }
        // Validate everything before touching the node.
        if ctx.mem.read_u32(node + IS_CODE) == 0 {
            return Err(cause_error(format!(
                "Cause({node:#010x}): software interrupt node has is_Code == NULL"
            )));
        }
        let pri = ctx.mem.read_u8(node + LN_PRI) as i8;
        let Some(level) = level_for_priority(pri) else {
            return Err(cause_error(format!(
                "Cause({node:#010x}): ln_Pri {pri} is outside the -32..=+32 range software \
                 interrupts allow (real exec would index past its five SoftIntList queues)"
            )));
        };
        ctx.mem.write_u8(node + LN_TYPE, NT_SOFTINT);
        let mut st = state.borrow_mut();
        st.queues[level].push_back(node);
        if st.saved.is_some() {
            // Called from inside a handler: the running drain picks it up.
            return Ok(());
        }
        st.saved = Some((
            ctx.cpu.address_register(AddressRegister(5)),
            ctx.cpu.address_register(AddressRegister(6)),
        ));
        drop(st);
        drain_step(ctx, state.clone())
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{Runtime, RuntimeError, StartConfig};
    use crate::memory::FlatMemory;

    const RTS: u16 = 0x4E75;
    const LOG_PTR: u32 = 0x1_0000;
    const RES_A5: u32 = 0x1_0010;
    const RES_A6: u32 = 0x1_0014;
    const RES_D2: u32 = 0x1_0018;
    const LOG: u32 = 0x1_0100;
    const H_LOG: u32 = 0x1_1000;
    const H_CAUSE: u32 = 0x1_1100;
    const H_CLOBBER: u32 = 0x1_1200;
    const NODE_STRIDE: u32 = 0x20;
    const NODES: u32 = 0x1_2000;
    const A5_SENTINEL: u32 = 0x5555_0005;
    const D2_SENTINEL: u32 = 0x2222_0002;

    fn node(i: u32) -> u32 {
        NODES + i * NODE_STRIDE
    }
    fn hi(v: u32) -> u16 {
        (v >> 16) as u16
    }
    fn lo(v: u32) -> u16 {
        v as u16
    }
    fn load(mem: &mut FlatMemory, addr: u32, words: &[u16]) {
        for (i, &w) in words.iter().enumerate() {
            mem.write_u16(addr + 2 * i as u32, w);
        }
    }

    /// Appends `(A1, A5, A6)` to the guest log at `*LOG_PTR`.
    fn log_words() -> Vec<u16> {
        vec![
            0x2079,
            hi(LOG_PTR),
            lo(LOG_PTR), // movea.l LOG_PTR,a0
            0x20C9,      // move.l a1,(a0)+
            0x20CD,      // move.l a5,(a0)+
            0x20CE,      // move.l a6,(a0)+
            0x23C8,
            hi(LOG_PTR),
            lo(LOG_PTR), // move.l a0,LOG_PTR
        ]
    }

    /// `Cause(node(i))`, with A6 = ExecBase.
    fn cause_words(i: u32) -> Vec<u16> {
        vec![
            0x227C,
            hi(node(i)),
            lo(node(i)),
            0x2C7C,
            hi(EXEC_LIBRARY_BASE),
            lo(EXEC_LIBRARY_BASE),
            0x4EAE,
            (-180i16) as u16,
        ]
    }

    /// Node `i`: `NT_INTERRUPT`, priority `pri`, `is_Data` = `i`.
    fn init_node(mem: &mut FlatMemory, i: u32, pri: i8, code: u32) {
        let n = node(i);
        mem.write_u8(n + LN_TYPE, NT_INTERRUPT);
        mem.write_u8(n + LN_PRI, pri as u8);
        mem.write_u32(n + IS_DATA, i);
        mem.write_u32(n + IS_CODE, code);
    }

    /// Main program: set sentinels, `Cause(node(first))`, record A5/A6/D2.
    fn main_program(first: u32) -> Vec<u16> {
        let mut w = vec![
            0x2C7C,
            hi(EXEC_LIBRARY_BASE),
            lo(EXEC_LIBRARY_BASE), // movea.l #exec,a6
            0x2A7C,
            hi(A5_SENTINEL),
            lo(A5_SENTINEL), // movea.l #sentinel,a5
            0x243C,
            hi(D2_SENTINEL),
            lo(D2_SENTINEL), // move.l #sentinel,d2
        ];
        w.extend(cause_words(first));
        w.extend([
            0x23CD,
            hi(RES_A5),
            lo(RES_A5),
            0x23CE,
            hi(RES_A6),
            lo(RES_A6),
            0x23C2,
            hi(RES_D2),
            lo(RES_D2),
            0x7000, // moveq #0,d0
            RTS,
        ]);
        w
    }

    fn run(
        program: &[u16],
        setup: impl FnOnce(&mut FlatMemory),
    ) -> (Result<i32, RuntimeError>, Runtime<M68kCpu>) {
        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load(&mut mem, entry, program);
        mem.write_u32(LOG_PTR, LOG);
        let mut h = log_words();
        h.push(RTS);
        load(&mut mem, H_LOG, &h);
        setup(&mut mem);
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
        let r = rt.run(&mut out, None);
        (r, rt)
    }

    /// The `(is_Data, A5, A6)` triples the log handlers recorded.
    fn log_entries(rt: &Runtime<M68kCpu>) -> Vec<(u32, u32, u32)> {
        let m = rt.memory();
        let end = m.read_u32(LOG_PTR);
        (LOG..end)
            .step_by(12)
            .map(|a| (m.read_u32(a), m.read_u32(a + 4), m.read_u32(a + 8)))
            .collect()
    }

    #[test]
    fn single_cause_runs_handler_once_with_documented_registers() {
        let (r, rt) = run(&main_program(0), |m| init_node(m, 0, 0, H_LOG));
        r.expect("run");
        assert_eq!(log_entries(&rt), vec![(0, H_LOG, EXEC_LIBRARY_BASE)]);
        assert_eq!(
            rt.memory().read_u8(node(0) + LN_TYPE),
            NT_INTERRUPT,
            "type returns to NT_INTERRUPT after the handler is taken off the queue"
        );
    }

    #[test]
    fn caller_registers_survive_a_handler_that_trashes_a5_a6() {
        let (r, rt) = run(&main_program(0), |m| {
            let mut h = log_words();
            h.extend([
                0x2C7C, 0xDEAD, 0x0006, // movea.l #$DEAD0006,a6
                0x2A7C, 0xDEAD, 0x0005, // movea.l #$DEAD0005,a5
                0x7055, // moveq #$55,d0
                RTS,
            ]);
            load(m, H_CLOBBER, &h);
            init_node(m, 0, 0, H_CLOBBER);
        });
        r.expect("run");
        let m = rt.memory();
        assert_eq!(m.read_u32(RES_A5), A5_SENTINEL);
        assert_eq!(m.read_u32(RES_A6), EXEC_LIBRARY_BASE);
        assert_eq!(m.read_u32(RES_D2), D2_SENTINEL);
        assert_eq!(log_entries(&rt).len(), 1);
    }

    #[test]
    fn already_queued_node_is_not_queued_again() {
        let (r, rt) = run(&main_program(0), |m| {
            init_node(m, 0, 0, H_LOG);
            m.write_u8(node(0) + LN_TYPE, NT_SOFTINT); // pending already
        });
        r.expect("run");
        assert!(log_entries(&rt).is_empty());
        assert_eq!(rt.memory().read_u8(node(0) + LN_TYPE), NT_SOFTINT);
    }

    #[test]
    fn nested_cause_queues_by_priority_then_fifo_and_guards_duplicates() {
        // Node 0 (pri 0) logs, then Causes: node 1 (-32), node 2 (+32),
        // node 2 again (already pending), node 3 (0), node 4 (+16).
        // Expected order: 0, 2 (+32), 4 (+16), 3 (0), 1 (-32).
        let (r, rt) = run(&main_program(0), |m| {
            let mut h = log_words();
            for i in [1, 2, 2, 3, 4] {
                h.extend(cause_words(i));
            }
            h.push(RTS);
            load(m, H_CAUSE, &h);
            init_node(m, 0, 0, H_CAUSE);
            init_node(m, 1, -32, H_LOG);
            init_node(m, 2, 32, H_LOG);
            init_node(m, 3, 0, H_LOG);
            init_node(m, 4, 16, H_LOG);
        });
        r.expect("run");
        let order: Vec<u32> = log_entries(&rt).iter().map(|e| e.0).collect();
        assert_eq!(order, vec![0, 2, 4, 3, 1]);
        let m = rt.memory();
        assert_eq!(m.read_u32(RES_A5), A5_SENTINEL);
        assert_eq!(m.read_u32(RES_A6), EXEC_LIBRARY_BASE);
        for i in 0..5 {
            assert_eq!(m.read_u8(node(i) + LN_TYPE), NT_INTERRUPT);
        }
    }

    #[test]
    fn same_level_runs_fifo() {
        let (r, rt) = run(&main_program(0), |m| {
            let mut h = log_words();
            for i in [3, 1, 2] {
                h.extend(cause_words(i));
            }
            h.push(RTS);
            load(m, H_CAUSE, &h);
            init_node(m, 0, 0, H_CAUSE);
            for i in 1..4 {
                init_node(m, i, 0, H_LOG);
            }
        });
        r.expect("run");
        let order: Vec<u32> = log_entries(&rt).iter().map(|e| e.0).collect();
        assert_eq!(order, vec![0, 3, 1, 2]);
    }

    #[test]
    fn priority_levels_truncate_and_reject_out_of_range() {
        assert_eq!(level_for_priority(-128), None);
        assert_eq!(level_for_priority(-33), None);
        assert_eq!(level_for_priority(-32), Some(0));
        assert_eq!(level_for_priority(-20), Some(0));
        assert_eq!(level_for_priority(-16), Some(1));
        assert_eq!(level_for_priority(15), Some(2));
        assert_eq!(level_for_priority(16), Some(3));
        assert_eq!(level_for_priority(32), Some(4));
        assert_eq!(level_for_priority(33), None);
        assert_eq!(level_for_priority(127), None);
    }

    fn cause_error_message(r: Result<i32, RuntimeError>) -> String {
        match r {
            Err(RuntimeError::Dispatch(DispatchError::HandlerFailed {
                library,
                lvo,
                handler_name,
                message,
            })) => {
                assert_eq!((library.as_str(), lvo), ("exec.library", -180));
                assert_eq!(handler_name, "Cause");
                message
            }
            other => panic!("expected HandlerFailed from Cause, got {other:?}"),
        }
    }

    #[test]
    fn null_is_code_is_rejected_and_node_left_untouched() {
        let (r, rt) = run(&main_program(0), |m| init_node(m, 0, 0, 0));
        let msg = cause_error_message(r);
        assert!(msg.contains("is_Code == NULL"), "{msg}");
        assert_eq!(rt.memory().read_u8(node(0) + LN_TYPE), NT_INTERRUPT);
    }

    #[test]
    fn cause_null_is_rejected_without_writing_low_memory() {
        let cause_null = [
            0x2C7C,
            hi(EXEC_LIBRARY_BASE),
            lo(EXEC_LIBRARY_BASE),
            0x93C9, // suba.l a1,a1
            0x4EAE,
            (-180i16) as u16,
            RTS,
        ];
        let (r, rt) = run(&cause_null, |_| {});
        let msg = cause_error_message(r);
        assert!(msg.contains("Cause(NULL)"), "{msg}");
        // Baseline: the same low memory after a run that does nothing.
        let (r0, rt0) = run(&[RTS], |_| {});
        r0.expect("baseline run");
        for a in 0..0x40 {
            assert_eq!(
                rt.memory().read_u8(a),
                rt0.memory().read_u8(a),
                "byte {a:#x}"
            );
        }
    }

    #[test]
    fn handler_that_recauses_its_own_node_runs_twice_and_terminates() {
        // Handler: log; if flag == 0 { flag += 1; Cause(self) }; rts.
        const FLAG: u32 = 0x1_0F00;
        let (r, rt) = run(&main_program(0), |m| {
            let recause_len = 6 + 2 * cause_words(0).len() as u16;
            let mut h = log_words();
            h.extend([0x4AB9, hi(FLAG), lo(FLAG)]); // tst.l FLAG
            h.push(0x6600 | recause_len); // bne.s over
            h.extend([0x52B9, hi(FLAG), lo(FLAG)]); // addq.l #1,FLAG
            h.extend(cause_words(0));
            h.push(RTS);
            load(m, H_CAUSE, &h);
            init_node(m, 0, 0, H_CAUSE);
        });
        r.expect("run");
        let m = rt.memory();
        assert_eq!(m.read_u32(FLAG), 1);
        assert_eq!(log_entries(&rt).len(), 2, "handler ran exactly twice");
        assert_eq!(m.read_u8(node(0) + LN_TYPE), NT_INTERRUPT);
        assert_eq!(m.read_u32(RES_A5), A5_SENTINEL);
    }

    #[test]
    fn second_top_level_cause_after_a_completed_drain_works() {
        let mut program = main_program(0);
        program.truncate(program.len() - 2); // drop `moveq #0,d0; rts`
        program.extend(cause_words(0));
        program.extend([0x7000, RTS]);
        let (r, rt) = run(&program, |m| init_node(m, 0, 0, H_LOG));
        r.expect("run");
        assert_eq!(log_entries(&rt).len(), 2);
        assert_eq!(rt.memory().read_u32(RES_A6), EXEC_LIBRARY_BASE);
    }

    #[test]
    fn out_of_range_priority_is_rejected_and_boundaries_accepted() {
        for pri in [33i8, -33] {
            let (r, rt) = run(&main_program(0), |m| init_node(m, 0, pri, H_LOG));
            let msg = cause_error_message(r);
            assert!(msg.contains(&format!("ln_Pri {pri}")), "{msg}");
            assert_eq!(rt.memory().read_u8(node(0) + LN_TYPE), NT_INTERRUPT);
        }
        for pri in [32i8, -32] {
            let (r, rt) = run(&main_program(0), |m| init_node(m, 0, pri, H_LOG));
            r.expect("boundary priority accepted");
            assert_eq!(log_entries(&rt).len(), 1);
        }
    }

    fn stub_message(lvo: i16, intnum: u16) -> String {
        let program = [
            0x2C7C,
            hi(EXEC_LIBRARY_BASE),
            lo(EXEC_LIBRARY_BASE),
            0x7000 | intnum,
            0x4EAE,
            lvo as u16,
            RTS,
        ];
        match run(&program, |_| {}).0 {
            Err(RuntimeError::Dispatch(DispatchError::HandlerFailed {
                library,
                lvo: l,
                message,
                ..
            })) => {
                assert_eq!(library, "exec.library");
                assert_eq!(l, i32::from(lvo));
                message
            }
            other => panic!("expected HandlerFailed, got {other:?}"),
        }
    }

    #[test]
    fn add_int_server_stub_names_the_interrupt() {
        let msg = stub_message(-168, 5);
        assert!(msg.contains("AddIntServer(INTB_VERTB (5)"), "{msg}");
        assert!(msg.contains("never delivers hardware interrupts"), "{msg}");
    }

    #[test]
    fn rem_int_server_stub_names_the_interrupt() {
        let msg = stub_message(-174, 3);
        assert!(msg.contains("RemIntServer(INTB_PORTS (3)"), "{msg}");
    }

    #[test]
    fn set_int_vector_stub_names_the_interrupt() {
        let msg = stub_message(-162, 15);
        assert!(msg.contains("SetIntVector(INTB_NMI (15)"), "{msg}");
    }
}
