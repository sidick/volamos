//! `exec.library`'s `struct SignalSemaphore` API: `InitSemaphore`/
//! `ObtainSemaphore`/`ReleaseSemaphore`/`AttemptSemaphore`. Found needed
//! running the real `AmiSnap` binary (`~/src/amisnap`), which uses a
//! semaphore somewhere in its libnix-provided runtime support.
//!
//! # Design: single-tasking means "contention" is always a guest bug
//!
//! Real semaphore semantics (traced against AROS's `rom/exec/
//! semaphores.c`/`obtainsemaphore.c`/`releasesemaphore.c`/
//! `attemptsemaphore.c`, since the NDK headers document the struct
//! layout but not the exact nesting/ownership algorithm) exist to
//! arbitrate a resource between multiple *concurrently running* tasks:
//! `ObtainSemaphore` blocks (via `Wait`) if some other task already
//! holds it, and `ReleaseSemaphore` wakes the next waiter.
//!
//! This runtime only ever has one task ([`crate::exectask::TASK_STRUCT_SIZE`]'s
//! module docs -- see [`crate::dispatch::HandlerContext::current_task`]),
//! so the "some other task holds it" branch can never legitimately be
//! reached: the only way `ss_Owner` could differ from
//! [`crate::dispatch::HandlerContext::current_task`] while the
//! semaphore is held is guest memory corruption or an uninitialized/
//! garbage `SignalSemaphore*` -- a real bug worth surfacing loudly
//! (same philosophy as `exectask.rs`'s `Signal`-to-unknown-task and
//! `Wait`-with-nothing-pending checks), *except* for `AttemptSemaphore`,
//! whose whole documented contract is "never blocks, just tells you
//! whether it worked" -- callers are required to handle a `FALSE`
//! result, so returning `FALSE` there instead of erroring stays
//! faithful to the real function's contract.
//!
//! The waiter-queue side of the real algorithm (`ss_WaitQueue`,
//! `ss_MultipleLink`, shared-vs-exclusive `SM_SHARED` locks,
//! `ObtainSemaphoreShared`/`AttemptSemaphoreShared`) is consequently
//! never exercised in this runtime and isn't implemented -- only the
//! nesting/ownership bookkeeping a single task can actually observe.

use crate::cpu::{AddressRegister, Cpu, DataRegister};
use crate::dispatch::{DispatchError, EXEC_LIBRARY_BASE, HandlerContext, LibraryTable};
use crate::lvos::exec::EXEC_LVOS;
use crate::memory::AddressSpace;

/// `ss_Link`: `struct Node`, offset 0 (14 bytes: `ln_Succ`/`ln_Pred` 4
/// each, `ln_Type`/`ln_Pri` 1 each, `ln_Name` 4 -- same layout
/// `crate::execlist`'s `LN_*` constants already describe).
const SS_LINK_TYPE: u32 = 8; // ln_Type, per crate::execlist::LN_TYPE
/// `ss_NestCount`: `WORD`, offset 14.
const SS_NEST_COUNT: u32 = 14;
/// `ss_WaitQueue`: `struct MinList` (`mlh_Head`/`mlh_Tail`/
/// `mlh_TailPred`, 4 bytes each = 12), offset 16. Only initialized to a
/// real empty list by `InitSemaphore` -- see this module's docs for why
/// nothing ever queues on it in this runtime.
const SS_WAIT_QUEUE: u32 = 16;
const SS_WAIT_QUEUE_HEAD: u32 = SS_WAIT_QUEUE;
const SS_WAIT_QUEUE_TAIL: u32 = SS_WAIT_QUEUE + 4;
const SS_WAIT_QUEUE_TAILPRED: u32 = SS_WAIT_QUEUE + 8;
/// `ss_MultipleLink`: `struct SemaphoreRequest` (`sr_Link` `struct
/// MinNode` 8 bytes + `sr_Waiter` `struct Task*` 4 bytes = 12), offset
/// 28. Never populated -- part of the waiter-queue machinery this
/// module's docs explain is unreachable here.
/// `ss_Owner`: `struct Task*`, offset 40 (16 + 12 + 12).
const SS_OWNER: u32 = 40;
/// `ss_QueueCount`: `WORD`, offset 44. `-1` means "free"; `AROS`'s
/// `InternalObtainSemaphore` increments this *before* checking whether
/// the semaphore was free (`== 0` after increment means "I got it
/// uncontended").
const SS_QUEUE_COUNT: u32 = 44;

/// `NT_SIGNALSEM` (15), per `<exec/nodes.h>`, verified against a
/// primary NDK source.
const NT_SIGNALSEM: u8 = 15;

/// `InitSemaphore` (LVO -558): `A0` = `struct SignalSemaphore*`. No
/// return value. Prepares a semaphore for use -- every real caller must
/// call this before `ObtainSemaphore`/`ReleaseSemaphore`/
/// `AttemptSemaphore` (`ss_Link.ln_Type` is what
/// [`crate::execsem`]'s docs' "contention is always a bug" check relies
/// on being a real, initialized value, not leftover heap garbage).
fn init_semaphore_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let sem = ctx.cpu.address_register(AddressRegister(0));

    // Empty MinList: head points at the tail-sentinel slot, tail is
    // NULL, tailpred points back at the head slot -- same head/tail-
    // overlap convention crate::execlist's init_list_header uses for a
    // full struct List, just without that struct's trailing
    // lh_Type/lh_Pad bytes (a MinList doesn't have them, and this
    // semaphore's own ss_MultipleLink field starts immediately after
    // ss_WaitQueue, so writing past byte 12 here would corrupt it).
    ctx.mem
        .write_u32(sem + SS_WAIT_QUEUE_HEAD, sem + SS_WAIT_QUEUE_TAIL);
    ctx.mem.write_u32(sem + SS_WAIT_QUEUE_TAIL, 0);
    ctx.mem
        .write_u32(sem + SS_WAIT_QUEUE_TAILPRED, sem + SS_WAIT_QUEUE_HEAD);

    ctx.mem.write_u8(sem + SS_LINK_TYPE, NT_SIGNALSEM);
    ctx.mem.write_u16(sem + SS_NEST_COUNT, 0);
    ctx.mem.write_u32(sem + SS_OWNER, 0);
    ctx.mem.write_u16(sem + SS_QUEUE_COUNT, 0xFFFF); // -1: free

    Ok(())
}

/// `ObtainSemaphore` (LVO -564): `A0` = `struct SignalSemaphore*`. No
/// return value (real `ObtainSemaphore` preserves every register).
fn obtain_semaphore_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let sem = ctx.cpu.address_register(AddressRegister(0));
    let task = ctx.current_task;

    let queue_count = ctx.mem.read_u16(sem + SS_QUEUE_COUNT) as i16;
    let new_queue_count = queue_count.wrapping_add(1);
    ctx.mem
        .write_u16(sem + SS_QUEUE_COUNT, new_queue_count as u16);

    if new_queue_count == 0 {
        // Was free: we now own it.
        ctx.mem.write_u32(sem + SS_OWNER, task);
        let nest = ctx.mem.read_u16(sem + SS_NEST_COUNT);
        ctx.mem.write_u16(sem + SS_NEST_COUNT, nest.wrapping_add(1));
        return Ok(());
    }

    let owner = ctx.mem.read_u32(sem + SS_OWNER);
    if owner == task {
        // Recursive obtain by the same (only) task: just nest.
        let nest = ctx.mem.read_u16(sem + SS_NEST_COUNT);
        ctx.mem.write_u16(sem + SS_NEST_COUNT, nest.wrapping_add(1));
        return Ok(());
    }

    // Some other "owner" -- impossible in a single-tasking runtime
    // unless the semaphore is uninitialized/corrupt (see this module's
    // docs). Undo the speculative increment before failing, so a
    // caller that somehow recovers doesn't see permanently skewed
    // bookkeeping.
    ctx.mem.write_u16(sem + SS_QUEUE_COUNT, queue_count as u16);
    Err(DispatchError::HandlerFailed {
        library: "exec.library".to_string(),
        lvo: -564,
        handler_name: "ObtainSemaphore".to_string(),
        message: format!(
            "ObtainSemaphore({sem:#010x}) would block forever: already held by task \
             {owner:#010x}, not the current task {task:#010x} -- this is a single-tasking \
             runtime with no other task that could plausibly own it, so the semaphore is \
             either uninitialized or corrupt"
        ),
    })
}

/// `ReleaseSemaphore` (LVO -570): `A0` = `struct SignalSemaphore*`. No
/// return value.
fn release_semaphore_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let sem = ctx.cpu.address_register(AddressRegister(0));
    let task = ctx.current_task;
    let owner = ctx.mem.read_u32(sem + SS_OWNER);

    if owner != task {
        return Err(DispatchError::HandlerFailed {
            library: "exec.library".to_string(),
            lvo: -570,
            handler_name: "ReleaseSemaphore".to_string(),
            message: format!(
                "ReleaseSemaphore({sem:#010x}): owned by task {owner:#010x}, not the current \
                 task {task:#010x} -- an unmatched Release (never Obtained, or already fully \
                 released) or a corrupt semaphore"
            ),
        });
    }

    let nest = ctx.mem.read_u16(sem + SS_NEST_COUNT) as i16;
    let new_nest = nest.wrapping_sub(1);
    ctx.mem.write_u16(sem + SS_NEST_COUNT, new_nest as u16);

    let queue_count = ctx.mem.read_u16(sem + SS_QUEUE_COUNT) as i16;
    ctx.mem
        .write_u16(sem + SS_QUEUE_COUNT, queue_count.wrapping_sub(1) as u16);

    if new_nest == 0 {
        // Fully released, and (this runtime's single task being the
        // only possible waiter) nothing to hand ownership to.
        ctx.mem.write_u32(sem + SS_OWNER, 0);
    }

    Ok(())
}

/// `AttemptSemaphore` (LVO -576): `A0` = `struct SignalSemaphore*`.
/// `D0` = `TRUE`/`FALSE` (whether the lock was obtained). Unlike
/// `ObtainSemaphore`, a real contended attempt is documented to return
/// `FALSE` rather than block -- see this module's docs for why that
/// contract is honored here (return `FALSE`) instead of failing loudly
/// like [`obtain_semaphore_handler`] does for the same "someone else
/// owns it" condition.
fn attempt_semaphore_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let sem = ctx.cpu.address_register(AddressRegister(0));
    let task = ctx.current_task;

    let queue_count = ctx.mem.read_u16(sem + SS_QUEUE_COUNT) as i16;
    let new_queue_count = queue_count.wrapping_add(1);
    ctx.mem
        .write_u16(sem + SS_QUEUE_COUNT, new_queue_count as u16);

    let obtained = if new_queue_count == 0 {
        ctx.mem.write_u32(sem + SS_OWNER, task);
        let nest = ctx.mem.read_u16(sem + SS_NEST_COUNT);
        ctx.mem.write_u16(sem + SS_NEST_COUNT, nest.wrapping_add(1));
        true
    } else if ctx.mem.read_u32(sem + SS_OWNER) == task {
        let nest = ctx.mem.read_u16(sem + SS_NEST_COUNT);
        ctx.mem.write_u16(sem + SS_NEST_COUNT, nest.wrapping_add(1));
        true
    } else {
        ctx.mem.write_u16(sem + SS_QUEUE_COUNT, queue_count as u16);
        false
    };

    ctx.cpu
        .set_data_register(DataRegister(0), if obtained { 1 } else { 0 });
    Ok(())
}

/// Registers this module's `exec.library` semaphore handlers, looked up
/// by name through [`EXEC_LVOS`], following
/// [`crate::execmem::register_execmem_handlers`]'s registration
/// pattern. Called unconditionally from
/// [`crate::dispatch::Runtime::new`].
pub fn register_execsem_handlers<C: Cpu + 'static>(
    table: &mut LibraryTable<C>,
    mem: &mut C::Memory,
) {
    macro_rules! reg {
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
    reg!("InitSemaphore", init_semaphore_handler::<C>);
    reg!("ObtainSemaphore", obtain_semaphore_handler::<C>);
    reg!("ReleaseSemaphore", release_semaphore_handler::<C>);
    reg!("AttemptSemaphore", attempt_semaphore_handler::<C>);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{Runtime, StartConfig};
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

    /// `jsr <disp16>(a6)`.
    fn jsr_disp16_a6(disp: i32) -> [u16; 2] {
        [0x4EAE, disp as u16]
    }

    const RTS: u16 = 0x4E75;

    fn movea_exec_base_to_a6() -> [u16; 3] {
        [
            move_imm_to_a(6),
            (EXEC_LIBRARY_BASE >> 16) as u16,
            EXEC_LIBRARY_BASE as u16,
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

    fn exec_program(words: &[u16]) -> Runtime<M68kCpu> {
        let mut full = movea_exec_base_to_a6().to_vec();
        full.extend_from_slice(words);
        runtime_with_program(&full)
    }

    /// A scratch guest address for a `SignalSemaphore` -- just past the
    /// trap table / program area this test harness uses, comfortably
    /// inside the flat memory these tests allocate.
    const SEM_ADDR: u32 = 0x1_0000;

    #[test]
    fn end_to_end_init_semaphore_sets_type_and_free_state() {
        let mut words = Vec::new();
        words.push(move_imm_to_a(0));
        words.push((SEM_ADDR >> 16) as u16);
        words.push(SEM_ADDR as u16);
        words.extend_from_slice(&jsr_disp16_a6(-558)); // InitSemaphore
        words.push(RTS);

        let mut rt = exec_program(&words);
        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed");

        let mem = rt.memory();
        assert_eq!(mem.read_u8(SEM_ADDR + SS_LINK_TYPE), NT_SIGNALSEM);
        assert_eq!(mem.read_u16(SEM_ADDR + SS_NEST_COUNT), 0);
        assert_eq!(mem.read_u32(SEM_ADDR + SS_OWNER), 0);
        assert_eq!(mem.read_u16(SEM_ADDR + SS_QUEUE_COUNT) as i16, -1);
        assert_eq!(
            mem.read_u32(SEM_ADDR + SS_WAIT_QUEUE_HEAD),
            SEM_ADDR + SS_WAIT_QUEUE_TAIL
        );
        assert_eq!(mem.read_u32(SEM_ADDR + SS_WAIT_QUEUE_TAIL), 0);
    }

    #[test]
    fn end_to_end_obtain_release_round_trip() {
        let mut words = Vec::new();
        words.push(move_imm_to_a(0));
        words.push((SEM_ADDR >> 16) as u16);
        words.push(SEM_ADDR as u16);
        words.extend_from_slice(&jsr_disp16_a6(-558)); // InitSemaphore
        words.extend_from_slice(&jsr_disp16_a6(-564)); // ObtainSemaphore
        words.extend_from_slice(&jsr_disp16_a6(-570)); // ReleaseSemaphore
        words.push(RTS);

        let mut rt = exec_program(&words);
        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(rt.memory().read_u32(SEM_ADDR + SS_OWNER), 0);
        assert_eq!(rt.memory().read_u16(SEM_ADDR + SS_NEST_COUNT), 0);
    }

    #[test]
    fn end_to_end_nested_obtain_needs_matching_releases() {
        let mut words = Vec::new();
        words.push(move_imm_to_a(0));
        words.push((SEM_ADDR >> 16) as u16);
        words.push(SEM_ADDR as u16);
        words.extend_from_slice(&jsr_disp16_a6(-558)); // InitSemaphore
        words.extend_from_slice(&jsr_disp16_a6(-564)); // ObtainSemaphore (1)
        words.extend_from_slice(&jsr_disp16_a6(-564)); // ObtainSemaphore (2, nested)
        words.extend_from_slice(&jsr_disp16_a6(-570)); // ReleaseSemaphore (still held)
        words.push(RTS);

        let mut rt = exec_program(&words);
        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            rt.memory().read_u32(SEM_ADDR + SS_OWNER),
            rt.current_task(),
            "still held after only one of two matching Releases"
        );
        assert_eq!(rt.memory().read_u16(SEM_ADDR + SS_NEST_COUNT), 1);
    }

    #[test]
    fn end_to_end_attempt_semaphore_succeeds_when_free() {
        let mut words = Vec::new();
        words.push(move_imm_to_a(0));
        words.push((SEM_ADDR >> 16) as u16);
        words.push(SEM_ADDR as u16);
        words.extend_from_slice(&jsr_disp16_a6(-558)); // InitSemaphore
        words.extend_from_slice(&jsr_disp16_a6(-576)); // AttemptSemaphore -> D0
        words.push(RTS);

        let mut rt = exec_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 1, "AttemptSemaphore on a free semaphore returns TRUE");
    }

    #[test]
    fn release_without_obtain_fails_loudly() {
        let mut words = Vec::new();
        words.push(move_imm_to_a(0));
        words.push((SEM_ADDR >> 16) as u16);
        words.push(SEM_ADDR as u16);
        words.extend_from_slice(&jsr_disp16_a6(-558)); // InitSemaphore
        words.extend_from_slice(&jsr_disp16_a6(-570)); // ReleaseSemaphore, never Obtained
        words.push(RTS);

        let mut rt = exec_program(&words);
        let mut out = Vec::new();
        let err = rt
            .run(&mut out, None)
            .expect_err("release without a matching obtain should fail");
        match err {
            crate::dispatch::RuntimeError::Dispatch(DispatchError::HandlerFailed {
                library,
                lvo,
                handler_name,
                ..
            }) => {
                assert_eq!(library, "exec.library");
                assert_eq!(lvo, -570);
                assert_eq!(handler_name, "ReleaseSemaphore");
            }
            other => panic!("expected HandlerFailed, got {other:?}"),
        }
    }
}
