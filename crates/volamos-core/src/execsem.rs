//! `exec.library`'s `struct SignalSemaphore` API: `InitSemaphore`/
//! `ObtainSemaphore`/`ReleaseSemaphore`/`AttemptSemaphore` and their
//! `Shared` variants. Found needed
//! running the real `AmiSnap` binary (`~/src/amisnap`), which uses a
//! semaphore somewhere in its libnix-provided runtime support.
//!
//! # Design: real blocking since `CreateNewProc` (issue #121/#237)
//!
//! Real semaphore semantics (traced against AROS's `rom/exec/
//! semaphores.c`/`obtainsemaphore.c`/`releasesemaphore.c`/
//! `attemptsemaphore.c`, since the NDK headers document the struct
//! layout but not the exact nesting/ownership algorithm) exist to
//! arbitrate a resource between multiple *concurrently running* tasks:
//! `ObtainSemaphore` blocks (via `Wait`) if some other task already
//! holds it, and `ReleaseSemaphore` wakes the next waiter.
//!
//! Before `CreateNewProc` (issue #121) this runtime only ever had one
//! task, so that branch could never legitimately be reached and any
//! contention meant guest memory corruption or an uninitialized/garbage
//! `SignalSemaphore*`. A second real task changes that: codewiz's issue
//! #237 report (a `CreateNewProc`'d child contending a parent's held
//! semaphore -- the exact shape libpthread's mutex/thread-table
//! bookkeeping hits at its very first lock) showed contention is now a
//! completely ordinary, correct program's behavior. `obtain_semaphore_
//! handler`/`obtain_semaphore_shared_handler` now block the caller
//! (`TaskScheduler::block_current_on_semaphore`) and switch to another
//! runnable task, the same re-trap pattern `exectask::wait_handler`
//! uses for `Wait`.
//!
//! A waiter is queued the way real exec queues it (issue #263): a
//! `struct SemaphoreRequest` linked on `ss_WaitQueue`, naming the
//! waiting task, with its `ss_QueueCount` increment kept. Code that
//! checks whether a task is queued before it goes on -- libpthread's
//! `tlskeys-1`/`tlskeys-2` do -- sees what it would on exec. Real exec
//! builds that request on the waiter's own stack, which the re-trap
//! model has no frame for, so it lives in a guest-heap block instead,
//! freed once the request is granted.
//!
//! `release_semaphore_handler` hands a fully released semaphore
//! straight to the first queued waiter (or, for a run of shared
//! requests at the head, all of them), in arrival order, as real exec
//! does -- ownership passes at the release, so nothing can slip in
//! between the release and the waiter running
//! (`TaskScheduler::grant_semaphore`). The woken task re-enters its
//! `Obtain*` call from scratch and is told it already owns the
//! semaphore (`TaskScheduler::take_semaphore_grant`). Contention is
//! still surfaced loudly -- same philosophy as `exectask.rs`'s
//! `Signal`-to-unknown-task and `Wait`-with-nothing-pending checks --
//! if it happens with no other task runnable and no pending timer that
//! could ever change that either (a genuine deadlock, not just "no
//! scheduler"). `AttemptSemaphore`'s whole documented contract is
//! "never blocks, just tells you whether it worked" -- callers are
//! required to handle a `FALSE` result, so returning `FALSE` there
//! instead of blocking or erroring stays faithful to the real
//! function's contract.
//!
//! `ss_MultipleLink` (used by `ObtainSemaphoreList`'s own partial-
//! obtain dance, which this runtime still doesn't implement) is never
//! populated.
//!
//! # Shared locks
//!
//! AROS's `ObtainSemaphoreShared`/`AttemptSemaphoreShared` run the same
//! algorithm as their exclusive counterparts with a NULL owner: a free
//! semaphore becomes shared (`ss_Owner` = NULL), and a shared one just
//! nests. `ReleaseSemaphore` accepts a NULL owner. An exclusive
//! `ObtainSemaphore` on a semaphore the task holds shared blocks forever
//! on a real system, so it fails loudly here like any other contention.
//! libpthread's `pthread_getspecific`, which libstdc++ calls on every
//! `throw` when gcc is built with `--enable-threads=posix`, takes a
//! shared lock.

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
/// `mlh_TailPred`, 4 bytes each = 12), offset 16. Initialized to a real
/// empty list by `InitSemaphore`; a contended `Obtain*` links a
/// [`SemaphoreRequest`](SR_WAITER) on it.
const SS_WAIT_QUEUE: u32 = 16;
const SS_WAIT_QUEUE_HEAD: u32 = SS_WAIT_QUEUE;
const SS_WAIT_QUEUE_TAIL: u32 = SS_WAIT_QUEUE + 4;
const SS_WAIT_QUEUE_TAILPRED: u32 = SS_WAIT_QUEUE + 8;
/// `struct SemaphoreRequest`: `sr_Link` (`struct MinNode`, 8 bytes) +
/// `sr_Waiter` (`struct Task*`) = 12 bytes. One of these is what a
/// queued waiter links on `ss_WaitQueue`; `ss_MultipleLink`, the one
/// embedded in the semaphore itself at offset 28, is never populated.
const SR_WAITER: u32 = 8;
const SR_SIZE: u32 = 12;
/// A shared request is told apart from an exclusive one by this bit
/// ORed into `sr_Waiter` (tasks are word-aligned, so it is free);
/// `SM_SHARED` in `<exec/semaphores.h>`.
const SM_SHARED: u32 = 1;
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
    init_semaphore_impl(ctx.mem, sem);
    Ok(())
}

/// Shared init logic behind [`init_semaphore_handler`] and
/// [`add_semaphore_handler`] (real `AddSemaphore` calls `InitSemaphore`
/// on its argument first, per AROS's `rom/exec/addsemaphore.c`).
fn init_semaphore_impl<M: AddressSpace>(mem: &mut M, sem: u32) {
    // Empty MinList: head points at the tail-sentinel slot, tail is
    // NULL, tailpred points back at the head slot -- same head/tail-
    // overlap convention crate::execlist's init_list_header uses for a
    // full struct List, just without that struct's trailing
    // lh_Type/lh_Pad bytes (a MinList doesn't have them, and this
    // semaphore's own ss_MultipleLink field starts immediately after
    // ss_WaitQueue, so writing past byte 12 here would corrupt it).
    mem.write_u32(sem + SS_WAIT_QUEUE_HEAD, sem + SS_WAIT_QUEUE_TAIL);
    mem.write_u32(sem + SS_WAIT_QUEUE_TAIL, 0);
    mem.write_u32(sem + SS_WAIT_QUEUE_TAILPRED, sem + SS_WAIT_QUEUE_HEAD);

    mem.write_u8(sem + SS_LINK_TYPE, NT_SIGNALSEM);
    mem.write_u16(sem + SS_NEST_COUNT, 0);
    mem.write_u32(sem + SS_OWNER, 0);
    mem.write_u16(sem + SS_QUEUE_COUNT, 0xFFFF); // -1: free
}

/// Shared obtain algorithm behind [`obtain_semaphore_handler`] and
/// [`obtain_semaphore_list_handler`]: returns `Ok(())` if `task` now
/// (nestedly-)owns `sem`, or `Err(owner)` -- the conflicting owner --
/// if some other "task" already holds it (impossible in a single-
/// tasking runtime unless the semaphore is uninitialized/corrupt, see
/// this module's docs). On `Err`, the speculative `ss_QueueCount`
/// increment is undone first, so a caller that somehow recovers
/// doesn't see permanently skewed bookkeeping.
///
/// `owner` is what a free semaphore's `ss_Owner` becomes, as in AROS's
/// `InternalObtainSemaphore`: `task` for an exclusive lock, `0` for a
/// shared one. A held semaphore nests if its owner is `task` or `owner`.
fn obtain_semaphore_impl<M: AddressSpace>(
    mem: &mut M,
    sem: u32,
    task: u32,
    owner: u32,
) -> Result<(), u32> {
    let queue_count = mem.read_u16(sem + SS_QUEUE_COUNT) as i16;
    let new_queue_count = queue_count.wrapping_add(1);
    mem.write_u16(sem + SS_QUEUE_COUNT, new_queue_count as u16);

    if new_queue_count == 0 {
        // Was free: we now own it.
        mem.write_u32(sem + SS_OWNER, owner);
        let nest = mem.read_u16(sem + SS_NEST_COUNT);
        mem.write_u16(sem + SS_NEST_COUNT, nest.wrapping_add(1));
        return Ok(());
    }

    let current_owner = mem.read_u32(sem + SS_OWNER);
    if current_owner == task || current_owner == owner {
        // Recursive obtain by the same (only) task, or another shared
        // lock on a shared semaphore: just nest.
        let nest = mem.read_u16(sem + SS_NEST_COUNT);
        mem.write_u16(sem + SS_NEST_COUNT, nest.wrapping_add(1));
        return Ok(());
    }

    mem.write_u16(sem + SS_QUEUE_COUNT, queue_count as u16);
    Err(current_owner)
}

/// Shared release algorithm behind [`release_semaphore_handler`] and
/// [`release_semaphore_list_handler`]: returns `Ok(())` on success, or
/// `Err(owner)` -- the actual owner, which must differ from `task` --
/// if this is an unmatched release (never obtained, already fully
/// released, or a corrupt semaphore). A shared semaphore (`ss_Owner` =
/// NULL) is released by whoever holds it, as in AROS.
fn release_semaphore_impl<M: AddressSpace>(mem: &mut M, sem: u32, task: u32) -> Result<(), u32> {
    let owner = mem.read_u32(sem + SS_OWNER);
    let free = mem.read_u16(sem + SS_QUEUE_COUNT) as i16 == -1;
    if free || (owner != 0 && owner != task) {
        return Err(owner);
    }

    let nest = mem.read_u16(sem + SS_NEST_COUNT) as i16;
    let new_nest = nest.wrapping_sub(1);
    mem.write_u16(sem + SS_NEST_COUNT, new_nest as u16);

    let queue_count = mem.read_u16(sem + SS_QUEUE_COUNT) as i16;
    mem.write_u16(sem + SS_QUEUE_COUNT, queue_count.wrapping_sub(1) as u16);

    if new_nest == 0 {
        // Fully released, and (this runtime's single task being the
        // only possible waiter) nothing to hand ownership to.
        mem.write_u32(sem + SS_OWNER, 0);
    }

    Ok(())
}

/// Obtains `sem` for the current task, queuing and blocking on
/// contention -- the shared body of [`obtain_semaphore_handler`] and
/// [`obtain_semaphore_shared_handler`] (`shared` says which).
///
/// If `ReleaseSemaphore` handed the semaphore to this task while it
/// slept (this is then the re-trap into the original call), there is
/// nothing left to do. Otherwise, on contention a `SemaphoreRequest`
/// is allocated and the task blocked on `sem`; the request is linked,
/// and the speculative `ss_QueueCount` increment [`obtain_semaphore_impl`]
/// undid is reinstated, only once the block has actually succeeded -- a
/// failed block (nothing else runnable) leaves the semaphore exactly as
/// it found it.
fn obtain_or_block<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
    shared: bool,
) -> Result<(), DispatchError> {
    let sem = ctx.cpu.address_register(AddressRegister(0));
    let task = ctx.current_task;
    let (lvo, handler_name) = if shared {
        (-678, "ObtainSemaphoreShared")
    } else {
        (-564, "ObtainSemaphore")
    };

    if ctx.tasks.take_semaphore_grant(task, sem) {
        return Ok(());
    }

    loop {
        let Err(owner) = obtain_semaphore_impl(ctx.mem, sem, task, if shared { 0 } else { task })
        else {
            return Ok(());
        };

        let request =
            ctx.heap
                .alloc(ctx.mem, SR_SIZE)
                .map_err(|e| DispatchError::HandlerFailed {
                    library: "exec.library".to_string(),
                    lvo,
                    handler_name: handler_name.to_string(),
                    message: format!(
                        "{handler_name}({sem:#010x}): no memory for the SemaphoreRequest to queue \
                     on: {e}"
                    ),
                })?;

        if ctx
            .tasks
            .block_current_on_semaphore(ctx.cpu, ctx.mem, ctx.trap_pc, sem)
            .is_ok()
        {
            ctx.mem.write_u32(
                request + SR_WAITER,
                task | if shared { SM_SHARED } else { 0 },
            );
            crate::execlist::add_tail_impl(ctx.mem, sem + SS_WAIT_QUEUE, request);
            let queue_count = ctx.mem.read_u16(sem + SS_QUEUE_COUNT);
            ctx.mem
                .write_u16(sem + SS_QUEUE_COUNT, queue_count.wrapping_add(1));
            return Ok(());
        }
        ctx.heap
            .free(request)
            .expect("the SemaphoreRequest was just allocated");

        if !ctx.timers.fire_earliest(ctx.mem, ctx.tasks) {
            return Err(DispatchError::HandlerFailed {
                library: "exec.library".to_string(),
                lvo,
                handler_name: handler_name.to_string(),
                message: match (shared, owner) {
                    (false, 0) => format!(
                        "ObtainSemaphore({sem:#010x}) would deadlock: the current task \
                         {task:#010x} holds it shared, and an exclusive lock waits for \
                         every shared one to be released, but no other task is runnable \
                         and no pending timer could ever release it"
                    ),
                    (false, _) => format!(
                        "ObtainSemaphore({sem:#010x}) would deadlock: already held by task \
                         {owner:#010x}, not the current task {task:#010x}, but no other \
                         task is runnable and no pending timer could ever release it"
                    ),
                    (true, _) => format!(
                        "ObtainSemaphoreShared({sem:#010x}) would deadlock: held exclusively \
                         by task {owner:#010x}, not the current task {task:#010x}, but no \
                         other task is runnable and no pending timer could ever release it"
                    ),
                },
            });
        }
    }
}

/// `ObtainSemaphore` (LVO -564): `A0` = `struct SignalSemaphore*`. No
/// return value (real `ObtainSemaphore` preserves every register).
///
/// On contention (issue #237's semaphore-contention follow-up, reported
/// by codewiz with a minimal `CreateNewProc` repro): queues on `sem`'s
/// `ss_WaitQueue` and blocks the current task, switching to another
/// runnable one, the same `block_current_on_signal`/re-trap pattern
/// `exectask::wait_handler` already uses for `Wait` -- see
/// [`obtain_or_block`] and this module's docs. Only fails loudly if no
/// other task is runnable and no pending timer could ever change that
/// either, exactly `wait_handler`'s own deadlock posture.
fn obtain_semaphore_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    obtain_or_block(ctx, false)
}

/// `ObtainSemaphoreShared` (LVO -678): `A0` = `struct SignalSemaphore*`.
/// No return value. A free semaphore becomes shared (`ss_Owner` = NULL);
/// a shared one, or one the current task holds exclusively, nests. On
/// contention, queues and blocks exactly as [`obtain_semaphore_handler`]
/// does.
fn obtain_semaphore_shared_handler<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
) -> Result<(), DispatchError> {
    obtain_or_block(ctx, true)
}

/// Releases `sem` for `task` and, if that fully frees it with waiters
/// queued, hands it to them -- the shared body of
/// [`release_semaphore_handler`] and [`release_semaphore_list_handler`].
/// Returns `Err(owner)` for an unmatched release, as
/// [`release_semaphore_impl`] does.
///
/// Handing over mirrors real exec: the first waiter takes the semaphore
/// (exclusive: it becomes `ss_Owner` with nest count 1; shared: owner
/// NULL, and every shared request in an unbroken run at the head joins
/// it), is unlinked from `ss_WaitQueue`, and is made runnable. Its
/// `ss_QueueCount` increment, kept while it waited, now stands for the
/// lock it holds -- so the count needs no further adjustment.
fn release_and_hand_over<M: AddressSpace>(
    mem: &mut M,
    heap: &mut crate::guestmem::GuestHeap,
    tasks: &mut crate::taskswitch::TaskScheduler,
    sem: u32,
    task: u32,
) -> Result<(), u32> {
    release_semaphore_impl(mem, sem, task)?;
    if mem.read_u16(sem + SS_NEST_COUNT) != 0 {
        return Ok(());
    }

    let mut granted_shared = false;
    loop {
        let request = mem.read_u32(sem + SS_WAIT_QUEUE_HEAD);
        if mem.read_u32(request + crate::execlist::LN_SUCC) == 0 {
            break; // empty
        }
        let waiter_bits = mem.read_u32(request + SR_WAITER);
        let shared = waiter_bits & SM_SHARED != 0;
        let waiter = waiter_bits & !SM_SHARED;
        if granted_shared && !shared {
            break;
        }

        crate::execlist::remove_impl(mem, request);
        heap.free(request)
            .expect("a queued SemaphoreRequest is a live heap block");
        mem.write_u32(sem + SS_OWNER, if shared { 0 } else { waiter });
        let nest = mem.read_u16(sem + SS_NEST_COUNT);
        mem.write_u16(sem + SS_NEST_COUNT, nest.wrapping_add(1));
        tasks.grant_semaphore(waiter, sem);

        if !shared {
            break;
        }
        granted_shared = true;
    }
    Ok(())
}

/// `ReleaseSemaphore` (LVO -570): `A0` = `struct SignalSemaphore*`. No
/// return value. If this fully frees `sem` (nest count reaches zero)
/// and tasks are queued on it, hands it to the first of them -- see
/// [`release_and_hand_over`]. They resume inside their original
/// `ObtainSemaphore`/`ObtainSemaphoreShared` call already owning it.
fn release_semaphore_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let sem = ctx.cpu.address_register(AddressRegister(0));
    let task = ctx.current_task;

    release_and_hand_over(ctx.mem, ctx.heap, ctx.tasks, sem, task).map_err(|owner| {
        DispatchError::HandlerFailed {
            library: "exec.library".to_string(),
            lvo: -570,
            handler_name: "ReleaseSemaphore".to_string(),
            message: format!(
                "ReleaseSemaphore({sem:#010x}): owned by task {owner:#010x}, not the current \
                 task {task:#010x} -- an unmatched Release (never Obtained, or already fully \
                 released) or a corrupt semaphore"
            ),
        }
    })
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

    let obtained = obtain_semaphore_impl(ctx.mem, sem, task, task).is_ok();
    ctx.cpu
        .set_data_register(DataRegister(0), if obtained { 1 } else { 0 });
    Ok(())
}

/// `AttemptSemaphoreShared` (LVO -720): `A0` = `struct SignalSemaphore*`.
/// `D0` = `TRUE`/`FALSE`. As [`obtain_semaphore_shared_handler`], but
/// returns `FALSE` instead of failing when another task holds the
/// semaphore exclusively, as [`attempt_semaphore_handler`] does.
fn attempt_semaphore_shared_handler<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
) -> Result<(), DispatchError> {
    let sem = ctx.cpu.address_register(AddressRegister(0));
    let task = ctx.current_task;

    let obtained = obtain_semaphore_impl(ctx.mem, sem, task, 0).is_ok();
    ctx.cpu
        .set_data_register(DataRegister(0), if obtained { 1 } else { 0 });
    Ok(())
}

/// `ExecBase.SemaphoreList`'s guest address -- the public, named
/// semaphore list `FindSemaphore`/`AddSemaphore`/`RemSemaphore`/
/// `ObtainSemaphoreList`/`ReleaseSemaphoreList` (below) operate on, at
/// [`crate::dispatch::EXEC_BASE_SEMAPHORELIST_OFFSET`]. A real,
/// walkable `struct List` initialized by `Runtime::new`, same
/// reasoning as `ExecBase.LibList`.
fn semaphore_list_addr() -> u32 {
    EXEC_LIBRARY_BASE + crate::dispatch::EXEC_BASE_SEMAPHORELIST_OFFSET
}

/// `FindSemaphore` (LVO -594): `A1` = name (`CString*`). `D0` = the
/// matching `SignalSemaphore*` on [`semaphore_list_addr`], or `0` if
/// not found -- traced against AROS's `rom/exec/findsemaphore.c`
/// ("just look into the list", via `FindName`, which
/// [`crate::execlist::find_name_impl`] already implements).
fn find_semaphore_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let name_ptr = ctx.cpu.address_register(AddressRegister(1));
    let name = crate::guestmem::read_c_string(ctx.mem, name_ptr);
    let found = crate::execlist::find_name_impl(ctx.mem, semaphore_list_addr(), &name);
    ctx.cpu.set_data_register(DataRegister(0), found);
    Ok(())
}

/// `AddSemaphore` (LVO -600): `A1` = `struct SignalSemaphore*`. No
/// return value. Real `AddSemaphore` (traced against AROS's
/// `rom/exec/addsemaphore.c`) calls `InitSemaphore` on it first (see
/// [`init_semaphore_impl`]), then `Enqueue`s it onto
/// [`semaphore_list_addr`] (priority-ordered, per `ss_Link.ln_Pri` --
/// [`crate::execlist::enqueue_impl`]) -- reproduced faithfully here
/// rather than a plain `AddTail`.
fn add_semaphore_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let sem = ctx.cpu.address_register(AddressRegister(1));
    init_semaphore_impl(ctx.mem, sem);
    crate::execlist::enqueue_impl(ctx.mem, semaphore_list_addr(), sem);
    Ok(())
}

/// `RemSemaphore` (LVO -606): `A1` = `struct SignalSemaphore*`. No
/// return value -- unlinks it from [`semaphore_list_addr`] (a plain
/// `Remove`, per AROS's `rom/exec/remsemaphore.c`; determined purely
/// from the semaphore's own `ss_Link`, not by re-deriving which list
/// it's on).
fn rem_semaphore_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let sem = ctx.cpu.address_register(AddressRegister(1));
    crate::execlist::remove_impl(ctx.mem, sem);
    Ok(())
}

/// `ObtainSemaphoreList` (LVO -582): `A0` = `struct List*` of
/// `SignalSemaphore`s (any list, not necessarily [`semaphore_list_addr`]
/// -- real callers commonly pass their own custom list, per the
/// Autodoc's `EXAMPLE`). No return value. Real `ObtainSemaphoreList`
/// (traced against AROS's `rom/exec/obtainsemaphorelist.c`) obtains
/// every semaphore on the list, queuing a wait for any it can't get
/// immediately, releasing what it already holds, and retrying the
/// whole list once that one frees up. Unlike [`obtain_semaphore_handler`]/
/// [`obtain_semaphore_shared_handler`], this handler deliberately
/// doesn't implement that partial-obtain/release/retry dance -- nothing
/// in this runtime's corpus has needed it yet -- so contention here
/// still fails the whole call loudly at the first semaphore that can't
/// be obtained immediately, same posture this module used everywhere
/// before issue #237's semaphore-contention follow-up.
fn obtain_semaphore_list_handler<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
) -> Result<(), DispatchError> {
    let list = ctx.cpu.address_register(AddressRegister(0));
    let task = ctx.current_task;
    let mut node = ctx.mem.read_u32(list + crate::execlist::LH_HEAD);
    loop {
        let succ = ctx.mem.read_u32(node + crate::execlist::LN_SUCC);
        if succ == 0 {
            return Ok(());
        }
        obtain_semaphore_impl(ctx.mem, node, task, task).map_err(|owner| {
            DispatchError::HandlerFailed {
                library: "exec.library".to_string(),
                lvo: -582,
                handler_name: "ObtainSemaphoreList".to_string(),
                message: format!(
                    "ObtainSemaphoreList({list:#010x}): semaphore {node:#010x} already held \
                     by task {owner:#010x}, not the current task {task:#010x} -- \
                     ObtainSemaphoreList doesn't block-and-retry on contention in this \
                     runtime, unlike ObtainSemaphore/ObtainSemaphoreShared"
                ),
            }
        })?;
        node = succ;
    }
}

/// `ReleaseSemaphoreList` (LVO -588): `A0` = `struct List*` of
/// `SignalSemaphore`s. No return value -- releases every semaphore on
/// the list, one at a time (traced against AROS's
/// `rom/exec/releasesemaphorelist.c`: "we own all the semaphores, so
/// just go over the list and release them"). A mismatch (a semaphore
/// on the list this task doesn't actually own) fails loudly at that
/// semaphore rather than silently skipping it or continuing past a
/// corrupt state.
fn release_semaphore_list_handler<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
) -> Result<(), DispatchError> {
    let list = ctx.cpu.address_register(AddressRegister(0));
    let task = ctx.current_task;
    let mut node = ctx.mem.read_u32(list + crate::execlist::LH_HEAD);
    loop {
        let succ = ctx.mem.read_u32(node + crate::execlist::LN_SUCC);
        if succ == 0 {
            return Ok(());
        }
        release_and_hand_over(ctx.mem, ctx.heap, ctx.tasks, node, task).map_err(|owner| {
            DispatchError::HandlerFailed {
                library: "exec.library".to_string(),
                lvo: -588,
                handler_name: "ReleaseSemaphoreList".to_string(),
                message: format!(
                    "ReleaseSemaphoreList({list:#010x}): semaphore {node:#010x} owned by task \
                     {owner:#010x}, not the current task {task:#010x} -- an unmatched \
                     release or a corrupt semaphore"
                ),
            }
        })?;
        node = succ;
    }
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
    reg!(
        "ObtainSemaphoreShared",
        obtain_semaphore_shared_handler::<C>
    );
    reg!(
        "AttemptSemaphoreShared",
        attempt_semaphore_shared_handler::<C>
    );
    reg!("FindSemaphore", find_semaphore_handler::<C>);
    reg!("AddSemaphore", add_semaphore_handler::<C>);
    reg!("RemSemaphore", rem_semaphore_handler::<C>);
    reg!("ObtainSemaphoreList", obtain_semaphore_list_handler::<C>);
    reg!("ReleaseSemaphoreList", release_semaphore_list_handler::<C>);
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

    /// As [`exec_program`], but runs `init_mem` against the guest memory
    /// before execution starts -- for tests that need extra state (a
    /// name string, a second semaphore, a hand-built list) already in
    /// place before the program runs.
    fn exec_program_with(
        words: &[u16],
        init_mem: impl FnOnce(&mut FlatMemory),
    ) -> Runtime<M68kCpu> {
        let mut full = movea_exec_base_to_a6().to_vec();
        full.extend_from_slice(words);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &full);
        init_mem(&mut mem);
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

    /// A scratch guest address for a `SignalSemaphore` -- just past the
    /// trap table / program area this test harness uses, comfortably
    /// inside the flat memory these tests allocate.
    const SEM_ADDR: u32 = 0x1_0000;
    /// A second scratch `SignalSemaphore` address, for list-of-many
    /// tests.
    const SEM2_ADDR: u32 = 0x1_1000;
    /// A scratch address for a semaphore's name `CString`.
    const NAME_ADDR: u32 = 0x1_2000;
    /// A scratch address for a hand-built `struct List` header (for
    /// `ObtainSemaphoreList`/`ReleaseSemaphoreList` tests, which take
    /// an arbitrary caller list, not necessarily `ExecBase.SemaphoreList`).
    const LIST_ADDR: u32 = 0x1_3000;

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

    // --- Two-task semaphore contention (issue #237's semaphore-
    // contention follow-up, reported by codewiz) ---

    /// `move.l #imm32,Dn`.
    fn move_imm_to_d(n: u16) -> u16 {
        0x203C | (n << 9)
    }

    /// `move.l D0,(An)`.
    fn move_d0_to_a_indirect(an: u16) -> u16 {
        0x2080 | (an << 9)
    }

    /// `move.l (An),D0`.
    fn move_a_indirect_to_d0(an: u16) -> u16 {
        0x2010 | an
    }

    /// `move.l (disp,An),D0`.
    fn move_a_disp_to_d0(an: u16, disp: i16) -> [u16; 2] {
        [0x2028 | an, disp as u16]
    }

    #[test]
    fn obtain_semaphore_blocks_until_the_holder_releases_it() {
        // Mirrors codewiz's minimal repro on issue #237: a parent task
        // holds `sem`; a child, spawned via CreateNewProc, tries to
        // obtain the same semaphore while the parent still holds it.
        // Real hardware blocks the child until the parent releases it;
        // before this fix, this runtime instead failed loudly with
        // "would block forever ... single-tasking runtime", since a
        // second real task couldn't exist yet when this module's
        // original design was written.
        //
        // Since this runtime only switches tasks at an explicit
        // blocking/yielding point (no preemption), the parent must
        // voluntarily yield after spawning the child and before
        // releasing, so the child actually gets to run and attempt
        // (and block on) the contended Obtain before the parent
        // releases it. Real exec's `SetTaskPri` only reschedules when
        // the caller's priority actually drops below a ready task's
        // (issue #254) -- same priority is a no-op -- so the yield
        // here is the real "dip and restore" idiom
        // (`SetTaskPri(self, pri - 1); SetTaskPri(self, pri);`,
        // `__gthread_yield()`'s own pattern): the dip (to below the
        // child's priority) is what actually triggers the switch; the
        // restore afterward runs once the parent is given the CPU back
        // and is itself a no-op (raising priority never yields). The
        // order of operations below is deliberate -- see this test's
        // steps.
        const DOS_LIBRARY_BASE: u32 = crate::dispatch::DOS_LIBRARY_BASE;
        const NP_ENTRY: u32 = 0x8000_0000 + 1000 + 3; // TAG_USER + NP_Dummy + NP_Entry

        // --- parent ---
        let mut words = vec![
            move_imm_to_a(6), // A6 = EXEC_LIBRARY_BASE
            (EXEC_LIBRARY_BASE >> 16) as u16,
            EXEC_LIBRARY_BASE as u16,
        ];
        words.push(move_imm_to_a(0)); // A0 = SEM_ADDR
        words.push((SEM_ADDR >> 16) as u16);
        words.push(SEM_ADDR as u16);
        words.extend_from_slice(&jsr_disp16_a6(-558)); // InitSemaphore
        words.extend_from_slice(&jsr_disp16_a6(-564)); // ObtainSemaphore: parent now owns it

        words.push(move_imm_to_a(6)); // A6 = DOS_LIBRARY_BASE
        words.push((DOS_LIBRARY_BASE >> 16) as u16);
        words.push(DOS_LIBRARY_BASE as u16);
        let tags_imm_idx = words.len();
        words.push(move_imm_to_d(1)); // D1 = tags (patched below)
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-498)); // CreateNewProc: spawn child

        words.push(move_imm_to_a(6)); // A6 = EXEC_LIBRARY_BASE
        words.push((EXEC_LIBRARY_BASE >> 16) as u16);
        words.push(EXEC_LIBRARY_BASE as u16);
        let parent_task_imm_idx_pri_dip = words.len();
        words.push(move_imm_to_a(1)); // A1 = parent task (patched below)
        words.push(0);
        words.push(0);
        words.push(move_imm_to_d(0)); // D0 = -1 (dip below the child's priority)
        words.push(0xFFFF);
        words.push(0xFFFF);
        words.extend_from_slice(&jsr_disp16_a6(-300)); // SetTaskPri(self, -1): drops below
        // the child's priority, triggering a real reschedule -- the
        // child attempts ObtainSemaphore and blocks on the still-
        // parent-held `sem`, switching straight back here.

        words.push(move_imm_to_a(6)); // A6 = EXEC_LIBRARY_BASE (SetTaskPri doesn't clobber
        // A6, but reload for clarity/robustness like every other call site here)
        words.push((EXEC_LIBRARY_BASE >> 16) as u16);
        words.push(EXEC_LIBRARY_BASE as u16);
        let parent_task_imm_idx_pri_restore = words.len();
        words.push(move_imm_to_a(1)); // A1 = parent task (patched below)
        words.push(0);
        words.push(0);
        words.push(move_imm_to_d(0)); // D0 = 0 (restore the original priority)
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-300)); // SetTaskPri(self, 0): raising priority
        // never yields, so this is a plain no-op by the time it runs

        // Issue #263: the child is now blocked in ObtainSemaphore, and
        // must be visible in the semaphore the way it is on exec: its
        // request on ss_WaitQueue (naming it) and counted in
        // ss_QueueCount. Record all three for the assertions below.
        words.push(move_imm_to_a(0)); // A0 = SEM_ADDR
        words.push((SEM_ADDR >> 16) as u16);
        words.push(SEM_ADDR as u16);
        words.push(0x7000); // moveq #0,D0
        words.extend_from_slice(&[0x3028, SS_QUEUE_COUNT as u16]); // move.w ss_QueueCount(A0),D0
        let observed_qc_imm_idx = words.len();
        words.push(move_imm_to_a(1)); // A1 = observed_qc_addr (patched below)
        words.push(0);
        words.push(0);
        words.push(move_d0_to_a_indirect(1));
        words.extend_from_slice(&move_a_disp_to_d0(0, SS_WAIT_QUEUE_HEAD as i16)); // D0 = mlh_Head
        let observed_head_imm_idx = words.len();
        words.push(move_imm_to_a(1)); // A1 = observed_head_addr (patched below)
        words.push(0);
        words.push(0);
        words.push(move_d0_to_a_indirect(1));
        words.push(0x2440); // movea.l D0,A2
        words.extend_from_slice(&[0x202A, SR_WAITER as u16]); // move.l sr_Waiter(A2),D0
        let observed_waiter_imm_idx = words.len();
        words.push(move_imm_to_a(1)); // A1 = observed_waiter_addr (patched below)
        words.push(0);
        words.push(0);
        words.push(move_d0_to_a_indirect(1));

        words.push(move_imm_to_a(0)); // A0 = SEM_ADDR (SetTaskPri clobbered A0/D0/D1)
        words.push((SEM_ADDR >> 16) as u16);
        words.push(SEM_ADDR as u16);
        words.extend_from_slice(&jsr_disp16_a6(-570)); // ReleaseSemaphore: frees it, waking
        // the child (still blocked on its original ObtainSemaphore call)

        words.push(move_imm_to_d(0)); // D0 = 0x00010000 (the join signal)
        words.push(0x0001);
        words.push(0x0000);
        words.extend_from_slice(&jsr_disp16_a6(-318)); // Wait: nothing satisfied yet (the
        // child hasn't obtained+signaled), switches to the child

        let marker_imm_idx_parent = words.len();
        words.push(move_imm_to_a(0)); // A0 = marker_addr (patched below)
        words.push(0);
        words.push(0);
        words.push(move_a_indirect_to_d0(0)); // D0 = *marker_addr
        words.push(RTS); // -> EXIT_STUB_ADDR, exit code = the marker

        // --- child: ObtainSemaphore(sem) [blocks, then succeeds once
        // woken]; write the marker; Signal(parent, join signal); fall
        // off the end into the task-exit stub ---
        let child_entry = TRAP_TABLE_END + (words.len() as u32) * 2;

        words.push(move_imm_to_a(6)); // A6 = EXEC_LIBRARY_BASE
        words.push((EXEC_LIBRARY_BASE >> 16) as u16);
        words.push(EXEC_LIBRARY_BASE as u16);
        words.push(move_imm_to_a(0)); // A0 = SEM_ADDR
        words.push((SEM_ADDR >> 16) as u16);
        words.push(SEM_ADDR as u16);

        // Proves this is a genuine contention repro, not a degenerate
        // one where the parent already released before the child ever
        // ran (which would still coincidentally pass this test's final
        // assertions, by a different, uninteresting path): read
        // ss_Owner directly, the instant the child starts, before
        // calling anything -- the test below asserts this equals the
        // parent's own task address.
        words.extend_from_slice(&move_a_disp_to_d0(0, SS_OWNER as i16));
        let observed_owner_imm_idx = words.len();
        words.push(move_imm_to_a(1)); // A1 = observed_owner_addr (patched below)
        words.push(0);
        words.push(0);
        words.push(move_d0_to_a_indirect(1)); // *observed_owner_addr = D0

        words.extend_from_slice(&jsr_disp16_a6(-564)); // ObtainSemaphore: contended, blocks

        let marker_imm_idx_child = words.len();
        words.push(move_imm_to_a(0)); // A0 = marker_addr (patched below)
        words.push(0);
        words.push(0);
        words.push(move_imm_to_d(0)); // D0 = 0x2A
        words.push(0);
        words.push(0x2A);
        words.push(move_d0_to_a_indirect(0)); // *marker_addr = D0

        words.push(move_imm_to_a(6)); // A6 = EXEC_LIBRARY_BASE
        words.push((EXEC_LIBRARY_BASE >> 16) as u16);
        words.push(EXEC_LIBRARY_BASE as u16);
        let parent_task_imm_idx_signal = words.len();
        words.push(move_imm_to_a(1)); // A1 = parent task (patched below)
        words.push(0);
        words.push(0);
        words.push(move_imm_to_d(0)); // D0 = 0x00010000
        words.push(0x0001);
        words.push(0x0000);
        words.extend_from_slice(&jsr_disp16_a6(-324)); // Signal
        words.push(RTS); // -> TASK_EXIT_STUB_ADDR, pre-pushed by CreateNewProc

        let entry = TRAP_TABLE_END;
        let code_end = entry + (words.len() as u32) * 2;
        let marker_addr = code_end + 16;
        let observed_owner_addr = marker_addr + 16;
        let observed_qc_addr = observed_owner_addr + 16;
        let observed_head_addr = observed_qc_addr + 16;
        let observed_waiter_addr = observed_head_addr + 16;
        let tags_addr = observed_waiter_addr + 16;
        let load_end = tags_addr + 32;

        words[tags_imm_idx + 1] = (tags_addr >> 16) as u16;
        words[tags_imm_idx + 2] = tags_addr as u16;
        words[marker_imm_idx_parent + 1] = (marker_addr >> 16) as u16;
        words[marker_imm_idx_parent + 2] = marker_addr as u16;
        words[marker_imm_idx_child + 1] = (marker_addr >> 16) as u16;
        words[marker_imm_idx_child + 2] = marker_addr as u16;
        words[observed_owner_imm_idx + 1] = (observed_owner_addr >> 16) as u16;
        words[observed_owner_imm_idx + 2] = observed_owner_addr as u16;
        for (idx, addr) in [
            (observed_qc_imm_idx, observed_qc_addr),
            (observed_head_imm_idx, observed_head_addr),
            (observed_waiter_imm_idx, observed_waiter_addr),
        ] {
            words[idx + 1] = (addr >> 16) as u16;
            words[idx + 2] = addr as u16;
        }

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        mem.write_u32(marker_addr, 0);
        mem.write_u32(observed_owner_addr, 0);
        // tags: [NP_ENTRY, child_entry], [TAG_DONE(0), 0]
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
        let parent_task = rt.current_task();
        for idx in [
            parent_task_imm_idx_pri_dip,
            parent_task_imm_idx_pri_restore,
            parent_task_imm_idx_signal,
        ] {
            let patch_addr = entry + (idx as u32) * 2 + 2;
            rt.memory_mut().write_u32(patch_addr, parent_task);
        }

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, 0x2A,
            "the child must actually block on the contended semaphore, get woken by the \
             parent's Release, and successfully obtain it before writing the marker -- a \
             wrong value anywhere in that chain (the child never blocking, obtaining it \
             early, or never waking) fails this assertion"
        );
        let mem = rt.memory();
        assert_eq!(
            mem.read_u32(observed_owner_addr),
            parent_task,
            "this is the discriminating check: the child must observe ss_Owner still equal \
             to the parent's task address the instant it starts running, proving the \
             parent's SetTaskPri dip genuinely switched to it *before* the parent's \
             Release -- otherwise this test degenerates to a non-contended ObtainSemaphore \
             that happens to leave the same final state"
        );
        let owner = mem.read_u32(SEM_ADDR + SS_OWNER);
        assert_ne!(
            owner, 0,
            "the child obtained it exclusively, so it must have a non-NULL owner"
        );
        assert_ne!(
            owner, parent_task,
            "the parent released it before the child obtained it, so the owner must now be \
             the child task, not the parent"
        );
        assert_eq!(mem.read_u16(SEM_ADDR + SS_NEST_COUNT), 1);

        // Issue #263: while the child waited it was in the queue, and
        // counted.
        assert_eq!(
            mem.read_u16(observed_qc_addr + 2),
            1,
            "the owner's lock plus one waiter: ss_QueueCount = 1 (it was 0 with the owner alone)"
        );
        assert_ne!(
            mem.read_u32(observed_head_addr),
            SEM_ADDR + SS_WAIT_QUEUE_TAIL,
            "ss_WaitQueue must not be empty while the child waits"
        );
        assert_eq!(
            mem.read_u32(observed_waiter_addr),
            owner,
            "the queued SemaphoreRequest must name the waiting task (the one that went on to \
             own the semaphore)"
        );
        // ...and once it was handed the semaphore, it left the queue
        // and the count stands for the lock it now holds.
        assert_eq!(
            mem.read_u32(SEM_ADDR + SS_WAIT_QUEUE_HEAD),
            SEM_ADDR + SS_WAIT_QUEUE_TAIL,
            "the granted request must be unlinked"
        );
        assert_eq!(mem.read_u16(SEM_ADDR + SS_QUEUE_COUNT), 0);
    }

    /// A heap-allocated `SemaphoreRequest` linked on `sem`'s
    /// `ss_WaitQueue` the way [`obtain_or_block`] leaves one, for the
    /// hand-over tests below.
    fn queue_waiter(
        mem: &mut FlatMemory,
        heap: &mut crate::guestmem::GuestHeap,
        sem: u32,
        task: u32,
        shared: bool,
    ) {
        let request = heap.alloc(mem, SR_SIZE).expect("heap has room");
        mem.write_u32(
            request + SR_WAITER,
            task | if shared { SM_SHARED } else { 0 },
        );
        crate::execlist::add_tail_impl(mem, sem + SS_WAIT_QUEUE, request);
        let qc = mem.read_u16(sem + SS_QUEUE_COUNT);
        mem.write_u16(sem + SS_QUEUE_COUNT, qc.wrapping_add(1));
    }

    fn handover_fixture() -> (FlatMemory, crate::guestmem::GuestHeap) {
        let mut mem = FlatMemory::new(0x2_0000);
        let heap = crate::guestmem::GuestHeap::new(0x1_4000, 0x1_c000);
        init_semaphore_impl(&mut mem, SEM_ADDR);
        (mem, heap)
    }

    fn queue_len(mem: &FlatMemory, sem: u32) -> usize {
        let mut n = 0;
        let mut node = mem.read_u32(sem + SS_WAIT_QUEUE_HEAD);
        while mem.read_u32(node + crate::execlist::LN_SUCC) != 0 {
            n += 1;
            node = mem.read_u32(node + crate::execlist::LN_SUCC);
        }
        n
    }

    #[test]
    fn release_hands_the_semaphore_to_waiters_in_arrival_order() {
        // Issue #263: ownership passes straight to the first queued
        // waiter, not to whichever woken task happens to run first.
        let (mut mem, mut heap) = handover_fixture();
        let mut tasks = crate::taskswitch::TaskScheduler::new(0x1000);
        let (owner, first, second) = (0x1000, 0x2000, 0x3000);

        obtain_semaphore_impl(&mut mem, SEM_ADDR, owner, owner).unwrap();
        queue_waiter(&mut mem, &mut heap, SEM_ADDR, first, false);
        queue_waiter(&mut mem, &mut heap, SEM_ADDR, second, false);
        assert_eq!(mem.read_u16(SEM_ADDR + SS_QUEUE_COUNT), 2);

        release_and_hand_over(&mut mem, &mut heap, &mut tasks, SEM_ADDR, owner).unwrap();
        assert_eq!(
            mem.read_u32(SEM_ADDR + SS_OWNER),
            first,
            "first in, first served"
        );
        assert_eq!(mem.read_u16(SEM_ADDR + SS_NEST_COUNT), 1);
        assert_eq!(
            mem.read_u16(SEM_ADDR + SS_QUEUE_COUNT),
            1,
            "the new owner's lock plus the one still waiting, minus one"
        );
        assert_eq!(queue_len(&mem, SEM_ADDR), 1);
        assert!(tasks.take_semaphore_grant(first, SEM_ADDR));
        assert!(
            !tasks.take_semaphore_grant(second, SEM_ADDR),
            "the second waiter must keep waiting"
        );

        release_and_hand_over(&mut mem, &mut heap, &mut tasks, SEM_ADDR, first).unwrap();
        assert_eq!(mem.read_u32(SEM_ADDR + SS_OWNER), second);
        assert_eq!(queue_len(&mem, SEM_ADDR), 0);
        assert_eq!(mem.read_u16(SEM_ADDR + SS_QUEUE_COUNT), 0);

        release_and_hand_over(&mut mem, &mut heap, &mut tasks, SEM_ADDR, second).unwrap();
        assert_eq!(
            mem.read_u16(SEM_ADDR + SS_QUEUE_COUNT) as i16,
            -1,
            "nobody left: the semaphore is free again"
        );
        assert_eq!(mem.read_u32(SEM_ADDR + SS_OWNER), 0);
    }

    #[test]
    fn a_nested_release_does_not_hand_over() {
        let (mut mem, mut heap) = handover_fixture();
        let mut tasks = crate::taskswitch::TaskScheduler::new(0x1000);
        let owner = 0x1000;
        obtain_semaphore_impl(&mut mem, SEM_ADDR, owner, owner).unwrap();
        obtain_semaphore_impl(&mut mem, SEM_ADDR, owner, owner).unwrap(); // nested
        queue_waiter(&mut mem, &mut heap, SEM_ADDR, 0x2000, false);

        release_and_hand_over(&mut mem, &mut heap, &mut tasks, SEM_ADDR, owner).unwrap();
        assert_eq!(
            mem.read_u32(SEM_ADDR + SS_OWNER),
            owner,
            "still the owner's"
        );
        assert_eq!(queue_len(&mem, SEM_ADDR), 1, "the waiter is still queued");
        assert!(!tasks.take_semaphore_grant(0x2000, SEM_ADDR));

        release_and_hand_over(&mut mem, &mut heap, &mut tasks, SEM_ADDR, owner).unwrap();
        assert_eq!(mem.read_u32(SEM_ADDR + SS_OWNER), 0x2000);
    }

    #[test]
    fn release_hands_a_run_of_shared_waiters_the_semaphore_together() {
        // Shared requests at the head of the queue all get in at once;
        // an exclusive one behind them keeps waiting.
        let (mut mem, mut heap) = handover_fixture();
        let mut tasks = crate::taskswitch::TaskScheduler::new(0x1000);
        let owner = 0x1000;
        obtain_semaphore_impl(&mut mem, SEM_ADDR, owner, owner).unwrap();
        queue_waiter(&mut mem, &mut heap, SEM_ADDR, 0x2000, true);
        queue_waiter(&mut mem, &mut heap, SEM_ADDR, 0x3000, true);
        queue_waiter(&mut mem, &mut heap, SEM_ADDR, 0x4000, false);
        queue_waiter(&mut mem, &mut heap, SEM_ADDR, 0x5000, true);

        release_and_hand_over(&mut mem, &mut heap, &mut tasks, SEM_ADDR, owner).unwrap();
        assert_eq!(mem.read_u32(SEM_ADDR + SS_OWNER), 0, "held shared");
        assert_eq!(
            mem.read_u16(SEM_ADDR + SS_NEST_COUNT),
            2,
            "two shared locks"
        );
        assert!(tasks.take_semaphore_grant(0x2000, SEM_ADDR));
        assert!(tasks.take_semaphore_grant(0x3000, SEM_ADDR));
        assert!(!tasks.take_semaphore_grant(0x4000, SEM_ADDR));
        assert!(
            !tasks.take_semaphore_grant(0x5000, SEM_ADDR),
            "a shared request behind an exclusive one must not jump it"
        );
        assert_eq!(queue_len(&mem, SEM_ADDR), 2);

        // Both shared holders release; the exclusive waiter is next.
        release_and_hand_over(&mut mem, &mut heap, &mut tasks, SEM_ADDR, 0x2000).unwrap();
        assert_eq!(queue_len(&mem, SEM_ADDR), 2, "one shared lock still out");
        release_and_hand_over(&mut mem, &mut heap, &mut tasks, SEM_ADDR, 0x3000).unwrap();
        assert_eq!(mem.read_u32(SEM_ADDR + SS_OWNER), 0x4000);
        assert!(tasks.take_semaphore_grant(0x4000, SEM_ADDR));
    }

    // --- ObtainSemaphoreShared/AttemptSemaphoreShared ---

    /// `InitSemaphore` on `SEM_ADDR`, then each of `lvos` in turn, then
    /// `rts`: `A0` stays `SEM_ADDR` throughout (every semaphore call
    /// preserves it), and the last call's `D0` is the exit code.
    fn sem_program(lvos: &[i32]) -> Vec<u16> {
        let mut words = vec![move_imm_to_a(0), (SEM_ADDR >> 16) as u16, SEM_ADDR as u16];
        words.extend_from_slice(&jsr_disp16_a6(-558)); // InitSemaphore
        for &lvo in lvos {
            words.extend_from_slice(&jsr_disp16_a6(lvo));
        }
        words.push(RTS);
        words
    }

    #[test]
    fn end_to_end_shared_obtain_leaves_no_owner_and_nests() {
        // ObtainSemaphoreShared twice, ReleaseSemaphore once.
        let mut rt = exec_program(&sem_program(&[-678, -678, -570]));
        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed");
        let mem = rt.memory();
        assert_eq!(
            mem.read_u32(SEM_ADDR + SS_OWNER),
            0,
            "a shared lock has no owner"
        );
        assert_eq!(mem.read_u16(SEM_ADDR + SS_NEST_COUNT), 1);
        assert_eq!(
            mem.read_u16(SEM_ADDR + SS_QUEUE_COUNT) as i16,
            0,
            "still held"
        );
    }

    #[test]
    fn end_to_end_shared_obtain_release_round_trip() {
        let mut rt = exec_program(&sem_program(&[-678, -570]));
        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed");
        let mem = rt.memory();
        assert_eq!(mem.read_u16(SEM_ADDR + SS_NEST_COUNT), 0);
        assert_eq!(
            mem.read_u16(SEM_ADDR + SS_QUEUE_COUNT) as i16,
            -1,
            "free again"
        );
    }

    #[test]
    fn end_to_end_shared_obtain_nests_inside_an_exclusive_lock() {
        // ObtainSemaphore, then ObtainSemaphoreShared by the same task.
        let mut rt = exec_program(&sem_program(&[-564, -678]));
        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed");
        let mem = rt.memory();
        assert_eq!(mem.read_u32(SEM_ADDR + SS_OWNER), rt.current_task());
        assert_eq!(mem.read_u16(SEM_ADDR + SS_NEST_COUNT), 2);
    }

    #[test]
    fn end_to_end_attempt_semaphore_shared_succeeds_when_free_or_shared() {
        let mut rt = exec_program(&sem_program(&[-720, -720]));
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, 1,
            "AttemptSemaphoreShared on a shared semaphore returns TRUE"
        );
        assert_eq!(rt.memory().read_u16(SEM_ADDR + SS_NEST_COUNT), 2);
    }

    #[test]
    fn end_to_end_attempt_semaphore_fails_on_a_shared_lock() {
        // ObtainSemaphoreShared, then AttemptSemaphore (exclusive).
        let mut rt = exec_program(&sem_program(&[-678, -576]));
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, 0,
            "an exclusive attempt on a shared semaphore returns FALSE"
        );
        assert_eq!(rt.memory().read_u16(SEM_ADDR + SS_NEST_COUNT), 1);
    }

    #[test]
    fn exclusive_obtain_of_a_shared_lock_fails_loudly() {
        // ObtainSemaphoreShared, then ObtainSemaphore: a deadlock on AmigaOS.
        let mut rt = exec_program(&sem_program(&[-678, -564]));
        let mut out = Vec::new();
        let err = rt
            .run(&mut out, None)
            .expect_err("an exclusive obtain of a shared lock should fail");
        match err {
            crate::dispatch::RuntimeError::Dispatch(DispatchError::HandlerFailed {
                lvo,
                handler_name,
                ..
            }) => {
                assert_eq!(lvo, -564);
                assert_eq!(handler_name, "ObtainSemaphore");
            }
            other => panic!("expected HandlerFailed, got {other:?}"),
        }
    }

    // --- FindSemaphore/AddSemaphore/RemSemaphore ---

    #[test]
    fn end_to_end_add_then_find_semaphore() {
        let mut words = Vec::new();
        words.push(move_imm_to_a(1)); // A1 = SEM_ADDR
        words.push((SEM_ADDR >> 16) as u16);
        words.push(SEM_ADDR as u16);
        words.extend_from_slice(&jsr_disp16_a6(-600)); // AddSemaphore
        words.push(move_imm_to_a(1)); // A1 = NAME_ADDR
        words.push((NAME_ADDR >> 16) as u16);
        words.push(NAME_ADDR as u16);
        words.extend_from_slice(&jsr_disp16_a6(-594)); // FindSemaphore -> D0
        words.push(RTS);

        let mut rt = exec_program_with(&words, |mem| {
            crate::guestmem::write_c_string(mem, NAME_ADDR, b"MySem");
            mem.write_u32(SEM_ADDR + crate::execlist::LN_NAME, NAME_ADDR);
        });
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code as u32, SEM_ADDR);
    }

    #[test]
    fn end_to_end_rem_semaphore_then_find_returns_null() {
        let mut words = Vec::new();
        words.push(move_imm_to_a(1)); // A1 = SEM_ADDR
        words.push((SEM_ADDR >> 16) as u16);
        words.push(SEM_ADDR as u16);
        words.extend_from_slice(&jsr_disp16_a6(-600)); // AddSemaphore
        words.push(move_imm_to_a(1)); // A1 = SEM_ADDR again
        words.push((SEM_ADDR >> 16) as u16);
        words.push(SEM_ADDR as u16);
        words.extend_from_slice(&jsr_disp16_a6(-606)); // RemSemaphore
        words.push(move_imm_to_a(1)); // A1 = NAME_ADDR
        words.push((NAME_ADDR >> 16) as u16);
        words.push(NAME_ADDR as u16);
        words.extend_from_slice(&jsr_disp16_a6(-594)); // FindSemaphore -> D0
        words.push(RTS);

        let mut rt = exec_program_with(&words, |mem| {
            crate::guestmem::write_c_string(mem, NAME_ADDR, b"MySem");
            mem.write_u32(SEM_ADDR + crate::execlist::LN_NAME, NAME_ADDR);
        });
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 0);
    }

    // --- ObtainSemaphoreList/ReleaseSemaphoreList ---

    /// Hand-builds a `struct List` at `LIST_ADDR` containing `SEM_ADDR`
    /// then `SEM2_ADDR`, both freshly `InitSemaphore`d.
    fn init_two_semaphore_list(mem: &mut FlatMemory) {
        crate::execlist::init_list_header(mem, LIST_ADDR);
        init_semaphore_impl(mem, SEM_ADDR);
        init_semaphore_impl(mem, SEM2_ADDR);
        crate::execlist::add_tail_impl(mem, LIST_ADDR, SEM_ADDR);
        crate::execlist::add_tail_impl(mem, LIST_ADDR, SEM2_ADDR);
    }

    #[test]
    fn end_to_end_obtain_then_release_semaphore_list() {
        let mut words = Vec::new();
        words.push(move_imm_to_a(0)); // A0 = LIST_ADDR
        words.push((LIST_ADDR >> 16) as u16);
        words.push(LIST_ADDR as u16);
        words.extend_from_slice(&jsr_disp16_a6(-582)); // ObtainSemaphoreList
        words.push(move_imm_to_a(0)); // A0 = LIST_ADDR again
        words.push((LIST_ADDR >> 16) as u16);
        words.push(LIST_ADDR as u16);
        words.extend_from_slice(&jsr_disp16_a6(-588)); // ReleaseSemaphoreList
        words.push(RTS);

        let mut rt = exec_program_with(&words, init_two_semaphore_list);
        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed");

        let mem = rt.memory();
        assert_eq!(mem.read_u32(SEM_ADDR + SS_OWNER), 0);
        assert_eq!(mem.read_u16(SEM_ADDR + SS_NEST_COUNT), 0);
        assert_eq!(mem.read_u32(SEM2_ADDR + SS_OWNER), 0);
        assert_eq!(mem.read_u16(SEM2_ADDR + SS_NEST_COUNT), 0);
    }

    #[test]
    fn obtain_semaphore_list_fails_loudly_on_a_held_semaphore() {
        let mut words = Vec::new();
        words.push(move_imm_to_a(0)); // A0 = LIST_ADDR
        words.push((LIST_ADDR >> 16) as u16);
        words.push(LIST_ADDR as u16);
        words.extend_from_slice(&jsr_disp16_a6(-582)); // ObtainSemaphoreList
        words.push(RTS);

        let mut rt = exec_program_with(&words, |mem| {
            init_two_semaphore_list(mem);
            // SEM2_ADDR is already "held" by a bogus other owner.
            init_semaphore_impl(mem, SEM2_ADDR);
            mem.write_u16(SEM2_ADDR + SS_QUEUE_COUNT, 0);
            mem.write_u32(SEM2_ADDR + SS_OWNER, 0xDEAD_0000);
        });
        let mut out = Vec::new();
        let err = rt
            .run(&mut out, None)
            .expect_err("obtaining an already-held semaphore should fail");
        match err {
            crate::dispatch::RuntimeError::Dispatch(DispatchError::HandlerFailed {
                library,
                lvo,
                handler_name,
                ..
            }) => {
                assert_eq!(library, "exec.library");
                assert_eq!(lvo, -582);
                assert_eq!(handler_name, "ObtainSemaphoreList");
            }
            other => panic!("expected HandlerFailed, got {other:?}"),
        }
    }
}
