//! Cooperative multi-task scheduling (issue #121).
//!
//! `volamos` was, until this module, single-tasking: exactly one guest
//! `struct Task`/`struct Process` ever existed (`crate::exectask`'s
//! "fake current task"), and every blocking primitive (`Wait` with
//! nothing satisfied, contended semaphores, an empty `WaitPort`) failed
//! loudly instead of hanging the host, on the reasoning that there was
//! never another task that could satisfy it anyway.
//!
//! `dos.library/CreateNewProc()` needs a second task to actually exist
//! and actually run. This module adds that -- but only the specific
//! slice issue #121's design settled on, not a general preemptive
//! scheduler:
//!
//! - **No preemption.** A switch only ever happens at a handler's own
//!   request, at one of a small set of trap-dispatch points (`Wait`
//!   blocking, `Signal` waking a blocked task, `Permit` after `Forbid`+
//!   `SysFlags |= SFB_SAR` -- `sched_yield()`'s real implementation --
//!   and `SetTaskPri` to a task's own current priority). A task that
//!   makes no such call and never returns runs forever, exactly as
//!   today.
//! - **`Forbid()` defers the voluntary ones** (issue #262). A per-task
//!   nest count ([`TaskScheduler::enter_forbid`]) holds back a
//!   `SetTaskPri` reschedule and `Permit`'s `SFB_SAR` yield until the
//!   outermost `Permit`, and `Wait`/`ObtainSemaphore`/`Delay` break the
//!   `Forbid()` while they sleep -- saved with the blocked task and
//!   restored when it resumes.
//! - **`Signal` never itself switches** -- it only marks a blocked
//!   target `Ready`. The next switch point (on either task) is what
//!   actually hands it the CPU.
//! - **Deadlock is reported, not hung.** If every task is blocked and
//!   none can be woken, blocking fails loudly with [`Deadlock`] --
//!   extending the same "fail loudly instead of hanging the host"
//!   posture every single-task stand-in already used before this
//!   module existed.
//! - **Out of scope for this slice**: semaphore contention (`Obtain
//!   Semaphore`/`WaitPort`) still fails loudly as before. The block/
//!   re-trap pattern below generalizes to them, but that's deliberately
//!   left for a follow-up once `Wait`/`Signal`/`CreateNewProc` are
//!   proven against real programs.
//!
//! # The switch mechanism
//!
//! [`RegisterSnapshot`](crate::cpu::RegisterSnapshot) holds everything a
//! suspended task needs to resume exactly where it left off. Only the
//! *running* task's registers are ever live in the actual [`Cpu`]; every
//! other known task's registers sit in its [`TaskRecord`] here.
//!
//! Blocking and voluntary yielding need different resume semantics, so
//! they're two different entry points rather than one:
//!
//! - [`TaskScheduler::block_current_on_signal`] is for `Wait` with
//!   nothing satisfied. Real hardware hasn't reached `Wait`'s own `rts`
//!   in this case -- the library call itself is suspended mid-flight.
//!   So the caller must **not** run the generic post-dispatch epilogue
//!   (pop the `JSR` return address, resume there) for this trap at all;
//!   this method captures the live registers with `pc` forced to the
//!   trap address itself (not wherever the CPU's `pc` naturally advanced
//!   to -- see [`crate::dispatch::HandlerContext::trap_pc`]'s doc), so
//!   that resuming this task later means re-trapping into the exact
//!   same `Wait` call, from scratch. There is deliberately no "remember
//!   to patch `D0` once this wakes" bookkeeping anywhere: a woken task
//!   just re-enters `wait_handler`, which re-reads `tc_SigRecvd` fresh
//!   and either completes for real this time or blocks again. The
//!   caller must check [`TaskScheduler::has_pending_switch`] *before*
//!   running its epilogue and skip it entirely if this method switched
//!   away.
//!
//! - [`TaskScheduler::yield_to_ready`] is for `Permit`+`SFB_SAR` and
//!   same-priority `SetTaskPri`. Here the library call genuinely has
//!   completed (real `sched_yield()` does return, eventually) -- so the
//!   caller runs its normal epilogue for the yielding task *first*
//!   (exactly as if no switch were happening), and only then calls this
//!   method, which captures the now-correct post-epilogue registers and
//!   switches if anyone else is `Ready`.
//!
//! Either way, a task switch also has to retarget the sanitizer's
//! tracked stack region ([`crate::sanitize::ShadowMap::
//! reset_stack_tracking`]) -- the same mechanism `crate::exectask`'s
//! `stack_swap_handler` already uses for a guest-initiated stack swap,
//! for the same reason: only one task's stack is ever *live* under
//! cooperative scheduling, so there's nothing to track concurrently,
//! but every switch must re-point the single tracked region or it ends
//! up checked against the wrong task's bounds. Callers are responsible
//! for this (see [`TaskScheduler::current`]'s callers in
//! `crate::dispatch`), since it needs the newly-current task's stack
//! bounds, which this module stores but the sanitizer lives outside of.
//!
//! # `tc_Switch`/`tc_Launch` (issue #237)
//!
//! Real `exec.library` calls the outgoing task's `tc_Switch` (if
//! `TF_SWITCH` is set in `tc_Flags`) and the incoming task's
//! `tc_Launch` (if `TF_LAUNCH` is set) at every switch -- real
//! thread-local storage and newlib's per-task `errno` rely on this to
//! point a global at the now-running task's own data. Both are real
//! guest subroutines that may themselves make further library calls,
//! so they run through [`crate::dispatch::ContinuationStack::
//! trampoline_now`] (the main dispatch loop, not a tight step loop),
//! exactly like any other guest code this runtime calls back into.
//!
//! This is why [`Self::current`] deliberately does *not* change the
//! instant a switch is decided ([`Self::block_current_on_signal`]/
//! [`Self::yield_to_ready`]/[`Self::terminate_current`] stage it, they
//! don't commit it) -- [`Self::commit`] is the one place that
//! actually happens, called by `crate::dispatch`'s switch-completion
//! logic only once the outgoing task's `tc_Switch` (if any) has
//! already run to completion on the outgoing task's own identity.
//! `tc_Launch` runs the other way around: after `commit`, on the
//! incoming task's newly-restored registers, with its real resume `pc`
//! stashed in the trampoline's continuation closure and restored once
//! `tc_Launch` itself returns.

use crate::cpu::{Cpu, RegisterSnapshot};
use crate::memory::AddressSpace;
use std::collections::BTreeMap;

/// Why a known, non-running task isn't running right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    /// Able to run; the next switch point may pick it.
    Ready,
    /// Blocked in `Wait`, for `wait_mask`. Becomes [`TaskState::Ready`]
    /// when [`TaskScheduler::wake_if_signal_matches`] sees this task's
    /// `tc_SigRecvd` (after whatever just OR'd bits into it) intersect
    /// `wait_mask`.
    BlockedOnSignal {
        /// The signal set this task is waiting for.
        wait_mask: u32,
    },
    /// Blocked in `ObtainSemaphore`/`ObtainSemaphoreShared` (issue
    /// #237's semaphore-contention follow-up), waiting for `sem` to
    /// become available. Becomes [`TaskState::Ready`] when
    /// [`TaskScheduler::wake_blocked_on_semaphore`] sees `ReleaseSemaphore`
    /// fully free that same semaphore -- not handed ownership directly;
    /// on resume it just re-enters the original `Obtain*` call from
    /// scratch (same re-trap model as [`TaskState::BlockedOnSignal`]),
    /// so a woken task that loses a race to another one simply blocks
    /// again. `wake_blocked_on_semaphore` wakes every task blocked on a
    /// given semaphore at once rather than tracking real FIFO queue
    /// order (`ss_WaitQueue` stays the empty, unused `MinList` it
    /// already was) -- correct either way, since cooperative scheduling
    /// means they still only re-attempt one at a time, just not
    /// necessarily in arrival order.
    BlockedOnSemaphore {
        /// The semaphore's guest address.
        sem: u32,
    },
    /// Blocked in `Delay` (issue #254), waiting for its own deadline
    /// (tracked in [`crate::exectask::PendingTimers`], not here --
    /// there's nothing guest-visible to key this on the way a
    /// semaphore's address or a signal mask works). Becomes
    /// [`TaskState::Ready`] when [`TaskScheduler::wake_task`] is
    /// called for this task, which [`crate::exectask::PendingTimers`]
    /// does once that deadline is reached; on resume, `delay_handler`
    /// re-enters from scratch and re-checks whether its deadline has
    /// genuinely passed yet (same re-trap model as every other
    /// `BlockedOn*` state).
    BlockedOnDelay,
}

/// Everything known about one non-running task: its suspended registers
/// and why it's suspended. Also carries its stack bounds (`tc_SPLower`/
/// `tc_SPUpper`, read once at creation -- a task's own stack bounds
/// never change after `CreateNewProc`/`create_current_task`, unlike its
/// signal state, so there's no staleness risk in caching them here)
/// purely so a switch *into* this task can retarget the sanitizer's
/// tracked stack region without a separate guest-memory read.
#[derive(Debug, Clone, Copy)]
struct TaskRecord {
    snapshot: RegisterSnapshot,
    state: TaskState,
    sp_lower: u32,
    sp_upper: u32,
    /// When this task last *became* [`TaskState::Ready`], as a ticket
    /// from [`TaskScheduler::ready_seq`] -- the ready queue's FIFO
    /// position within its priority (see [`TaskScheduler::pick_ready`]).
    /// Meaningless while the task is blocked; restamped the moment it
    /// becomes `Ready` again, so a just-woken or just-yielded task goes
    /// to the *back* of its priority's queue the way real exec's
    /// `AddTail` onto `TaskReady` puts it there.
    ready_since: u64,
    /// The task's `Forbid()` nest count at the moment it stopped
    /// running -- see [`TaskScheduler::forbid_nest`]. Restored by
    /// [`TaskScheduler::commit`] when it runs again.
    forbid_nest: u32,
}

/// A task is blocked and no other task can be woken to replace it --
/// every task is waiting on a signal nothing will ever set. Converted
/// by callers into the same loud [`crate::dispatch::DispatchError::
/// HandlerFailed`] idiom every other single-task stand-in in this
/// codebase already uses instead of hanging the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadlock;

/// Owns every task this runtime knows about except the one currently
/// running (whose registers are simply whatever's live in the actual
/// [`Cpu`] -- there would be nothing meaningful to store here for it
/// that isn't instantly stale).
///
/// Among every [`TaskState::Ready`] entry, the highest `ln_Pri` wins;
/// equal priorities round-robin in the order they became `Ready`
/// ([`TaskRecord::ready_since`]), matching real AmigaOS's FIFO
/// `TaskReady` list. Tasks are stored in a [`BTreeMap`], not a
/// `HashMap`, so everything here (including the ticket order two tasks
/// woken by one call end up with) stays deterministic.
///
/// This used to break ties by lowest task address instead, on the
/// reasoning that the handful of tasks this runtime's programs create
/// didn't need a real round-robin. Issue #262 found they do: with
/// three or more equal-priority tasks all yielding
/// (`std::this_thread::yield()`, i.e. `Forbid()`+`SFB_SAR`+`Permit()`,
/// as gcc's `__gthread_yield()` compiles to), an address-ordered pick
/// is a *fixed function of the candidate set*, so the two
/// lowest-addressed tasks ping-pong between themselves forever and
/// every other ready task -- including the main task polling for them
/// to finish -- starves.
pub struct TaskScheduler {
    /// The program's own task, the one [`Self::new`] started with.
    main: u32,
    current: u32,
    others: BTreeMap<u32, TaskRecord>,
    /// Set by a successful [`Self::block_current_on_signal`]/[`Self::
    /// yield_to_ready`]/[`Self::terminate_current`] call; taken (and
    /// cleared) by [`Self::take_pending_restore`]. Deliberately *not*
    /// reflected in [`Self::current`] yet at this point -- see that
    /// method's doc and [`Self::commit`]'s for why the two can
    /// disagree for a while (issue #237's `tc_Switch`).
    pending_restore: Option<(u32, TaskRecord)>,
    /// Set for the whole duration of a `tc_Switch`/`tc_Launch`
    /// trampoline (issue #237) -- see [`Self::begin_switch_hook`]'s doc
    /// for why: a guest `tc_Switch`/`tc_Launch` routine is documented
    /// as an ordinary subroutine that may make further library calls,
    /// but [`Self::current`] deliberately lags the real switch decision
    /// for exactly as long as one of these runs, which means a second,
    /// nested switch decision staged while this is set (the routine
    /// itself calling `Wait` and blocking, or `Permit`/`SetTaskPri` and
    /// yielding) would re-read the *same* outgoing task's still-set
    /// `TF_SWITCH` and re-enter its `tc_Switch` a second time, mid-
    /// flight, corrupting the continuation stack and the eventual
    /// commit. Checked (and refused) by [`Self::block_current_on_signal`]/
    /// [`Self::yield_to_ready`]/[`Self::terminate_current`] instead of
    /// letting that happen.
    switch_hook_active: bool,
    /// Set by [`Self::request_yield`] (`Permit`/`SetTaskPri`'s
    /// handlers), taken by [`Self::take_yield_request`]. Deliberately
    /// a flag rather than having those handlers call
    /// [`Self::yield_to_ready`] directly: a handler runs *before* the
    /// generic post-dispatch epilogue that actually completes its own
    /// library call (pops the `JSR` return address, sets `pc` to it),
    /// so it can't yet capture the correct post-epilogue registers to
    /// save. The caller (`crate::dispatch::Runtime::run`) runs that
    /// epilogue first, *then* checks this flag and calls
    /// [`Self::yield_to_ready`] itself.
    yield_requested: bool,
    /// Set by [`Self::request_reschedule_if_outranked`] (`SetTaskPri`'s
    /// handler), taken by [`Self::take_reschedule_request`]. A flag
    /// (carrying the new priority) for the same reason
    /// [`Self::yield_requested`] is: the handler runs *before* the
    /// generic post-dispatch epilogue that actually completes
    /// `SetTaskPri`'s own call, so it can't yet capture the correct
    /// post-epilogue registers to save if it switched immediately.
    reschedule_requested: Option<i8>,
    /// Hands out [`TaskRecord::ready_since`] tickets: bumped once per
    /// became-`Ready` transition, so a larger ticket means "joined the
    /// ready queue later".
    ready_seq: u64,
    /// The running task's `Forbid()` nest count: bumped by [`Self::
    /// enter_forbid`], dropped by [`Self::leave_forbid`]. Per-task, like
    /// real exec's `TDNestCnt`/`tc_TDNestCnt` pair: saved into the
    /// outgoing task's [`TaskRecord`] at every switch and restored from
    /// the incoming one's by [`Self::commit`], so a task that blocks in
    /// `Wait` inside a `Forbid()` gets that `Forbid()` back when it
    /// resumes, and one that exits inside one leaves no stray count
    /// behind for whoever runs next.
    ///
    /// While non-zero, [`Self::take_reschedule_request`] holds back a
    /// pending `SetTaskPri` reschedule, and `Permit` only acts on
    /// `SFB_SAR` once the count is back to zero -- see their docs.
    forbid_nest: u32,
    /// The incoming task's saved [`TaskRecord::forbid_nest`], parked by
    /// [`Self::take_pending_restore`] until [`Self::commit`] makes it
    /// the running task's. Kept separate from [`Self::forbid_nest`] for
    /// the same reason [`Self::current`] lags the switch decision: an
    /// outgoing task's `tc_Switch` hook (issue #237) still runs as that
    /// task, and must see *its* count, not the incoming task's.
    incoming_forbid_nest: u32,
}

impl TaskScheduler {
    /// Starts tracking with `main_task` as the sole, currently-running
    /// task -- called once from [`crate::dispatch::Runtime::new`].
    pub fn new(main_task: u32) -> Self {
        Self {
            main: main_task,
            current: main_task,
            others: BTreeMap::new(),
            pending_restore: None,
            switch_hook_active: false,
            yield_requested: false,
            reschedule_requested: None,
            ready_seq: 0,
            forbid_nest: 0,
            incoming_forbid_nest: 0,
        }
    }

    /// The next [`TaskRecord::ready_since`] ticket -- one per
    /// became-`Ready` transition.
    fn next_ready_ticket(&mut self) -> u64 {
        self.ready_seq += 1;
        self.ready_seq
    }

    /// Marks `task` [`TaskState::Ready`] and puts it at the back of its
    /// priority's ready queue, if it's a known non-running task
    /// currently in `blocked_in`. Returns whether it actually changed
    /// state, so callers that only wake one task can stop looking.
    /// Every became-`Ready` transition goes through here, so none can
    /// forget to take a fresh ticket.
    fn mark_ready_if(&mut self, task: u32, blocked_in: TaskState) -> bool {
        let ticket = self.next_ready_ticket();
        match self.others.get_mut(&task) {
            Some(record) if record.state == blocked_in => {
                record.state = TaskState::Ready;
                record.ready_since = ticket;
                true
            }
            _ => false,
        }
    }

    /// Requests a voluntary yield once the current library call
    /// finishes -- see [`Self::yield_requested`]'s doc for why this is
    /// a flag rather than switching immediately. Called by `Permit`'s
    /// handler when `SysFlags`'s `SFB_SAR` bit was set (real
    /// `sched_yield()`'s mechanism).
    pub fn request_yield(&mut self) {
        self.yield_requested = true;
    }

    /// Takes (and clears) a pending [`Self::request_yield`] request.
    pub fn take_yield_request(&mut self) -> bool {
        std::mem::take(&mut self.yield_requested)
    }

    /// Requests [`Self::reschedule_if_outranked`] once the current
    /// `SetTaskPri` call finishes -- see [`Self::reschedule_requested`]'s
    /// doc for why this is a flag rather than switching immediately.
    /// `new_pri` is the priority the task was just set to.
    pub fn request_reschedule_if_outranked(&mut self, new_pri: i8) {
        self.reschedule_requested = Some(new_pri);
    }

    /// Takes (and clears) a pending
    /// [`Self::request_reschedule_if_outranked`] request -- unless a
    /// `Forbid()` is in effect, in which case it stays pending and this
    /// returns `None`.
    ///
    /// Real exec doesn't reschedule while task switching is forbidden;
    /// it notes that one is due and does it at the outermost `Permit()`
    /// (issue #262). That matters to code that changes the running
    /// task's priority and then readies a higher-priority task inside
    /// the *same* `Forbid()` -- gcc's `<benaphore.h>` does exactly this
    /// when it releases a lock: restore the owner's priority, then
    /// signal the waiter. Deciding at the `SetTaskPri()` picks among
    /// whoever is ready *then*, and hands the CPU to a middle-priority
    /// task over the waiter that was about to become ready.
    ///
    /// The request is re-examined at `Permit()`'s own epilogue (the
    /// handler is where [`Self::leave_forbid`] drops the count to
    /// zero), so what it compares against is whoever is `Ready` *then*.
    pub fn take_reschedule_request(&mut self) -> Option<i8> {
        if self.forbid_nest > 0 {
            return None;
        }
        self.reschedule_requested.take()
    }

    /// `Forbid()`: one more level of task-switching prohibition for the
    /// running task.
    pub fn enter_forbid(&mut self) {
        self.forbid_nest += 1;
    }

    /// `Permit()`: one fewer level. Returns whether task switching is
    /// now permitted again (the outermost `Permit()`), which is the only
    /// point real exec acts on a reschedule that came due meanwhile.
    ///
    /// A `Permit()` with no `Forbid()` to match (a guest bug, or a
    /// count lost to a guest that inlines `TDNestCnt`) counts as
    /// outermost rather than underflowing -- the behavior every
    /// `Permit()` had before this count existed.
    pub fn leave_forbid(&mut self) -> bool {
        self.forbid_nest = self.forbid_nest.saturating_sub(1);
        self.forbid_nest == 0
    }

    /// The currently-running task's guest address -- **not** updated
    /// the moment a switch is decided, only once it's actually
    /// [`Self::commit`]ted. Between those two points (while a `tc_Switch`
    /// the outgoing task asked for, issue #237, runs through the
    /// trampoline), this still correctly reports the outgoing task:
    /// `tc_Switch` genuinely hasn't lost the CPU yet by any real
    /// hardware's definition, and anything it calls (a library
    /// function reading `current_task`, a stack-bounds check) needs to
    /// see *its* identity, not the task it's handing off to.
    pub fn current(&self) -> u32 {
        self.current
    }

    /// Whether `task` is the program's own task rather than one that
    /// `CreateNewProc` started.
    pub fn is_main(&self, task: u32) -> bool {
        task == self.main
    }

    /// Whether a switch has been decided ([`Self::
    /// block_current_on_signal`]/[`Self::yield_to_ready`]/[`Self::
    /// terminate_current`] succeeded) but not yet [`Self::commit`]ted.
    /// The caller checks this instead of comparing [`Self::current`]
    /// against its own cached copy of the previously-running task,
    /// since `current` deliberately doesn't change until `commit` does
    /// (see that method's doc).
    pub fn has_pending_switch(&self) -> bool {
        self.pending_restore.is_some()
    }

    /// Actually switches [`Self::current`] to `next` -- the one place
    /// that field changes. Called once the incoming task's registers
    /// are about to be restored into the live `Cpu`, which for most
    /// switches is immediately after [`Self::take_pending_restore`]
    /// but, when the outgoing task's `tc_Switch` (issue #237) is
    /// involved, only after that call returns (it runs through the
    /// trampoline on the *outgoing* task's own identity/stack, which
    /// must stay correct for its whole duration).
    pub fn commit(&mut self, next: u32) {
        self.current = next;
        self.forbid_nest = self.incoming_forbid_nest;
        // Whatever reschedule the outgoing task had pending was just
        // answered by this very switch. Left set, it would carry that
        // task's `SetTaskPri` value over to whoever runs next and be
        // judged against *their* priority.
        self.reschedule_requested = None;
    }

    /// Marks a `tc_Switch`/`tc_Launch` trampoline as in flight -- see
    /// [`Self::switch_hook_active`]'s doc. Call before trampolining
    /// into either hook; pair with [`Self::end_switch_hook`] once its
    /// own completion continuation actually runs (the hook's guest code
    /// has, by then, fully returned).
    pub fn begin_switch_hook(&mut self) {
        self.switch_hook_active = true;
    }

    /// Clears what [`Self::begin_switch_hook`] set.
    pub fn end_switch_hook(&mut self) {
        self.switch_hook_active = false;
    }

    /// Whether `task` is a known *non-running* task (created via
    /// [`Self::spawn_ready`] and not yet [`Self::remove`]d) -- used by
    /// `Signal`'s handler to decide whether a target other than the
    /// current task is a real, blockable task or a guest bug (a stale
    /// or uninitialized task pointer).
    pub fn is_known(&self, task: u32) -> bool {
        self.others.contains_key(&task)
    }

    /// Registers a freshly-created task (`dos.library/CreateNewProc`,
    /// issue #121) as `Ready` with `snapshot` as its initial register
    /// state. Never switches -- a newly created task doesn't run until
    /// some later switch point picks it, exactly like real hardware
    /// (`CreateNewProc` itself doesn't yield).
    pub fn spawn_ready(
        &mut self,
        task: u32,
        snapshot: RegisterSnapshot,
        sp_lower: u32,
        sp_upper: u32,
    ) {
        let ready_since = self.next_ready_ticket();
        self.others.insert(
            task,
            TaskRecord {
                snapshot,
                state: TaskState::Ready,
                sp_lower,
                sp_upper,
                ready_since,
                forbid_nest: 0,
            },
        );
    }

    /// Removes a task that has run to completion (fallen off the end of
    /// its `NP_Entry`, via the task-exit stub) from tracking entirely --
    /// it is neither `current` nor eligible to be switched to again.
    /// The caller is responsible for freeing its stack/`struct Process`
    /// back to the guest heap; this only stops the scheduler itself
    /// from knowing about it.
    pub fn remove(&mut self, task: u32) {
        self.others.remove(&task);
    }

    /// The stack bounds [`Self::spawn_ready`] recorded for `task`, if
    /// it's a known non-running task -- used by a switch's caller to
    /// retarget the sanitizer (see this module's doc).
    pub fn stack_bounds(&self, task: u32) -> Option<(u32, u32)> {
        self.others.get(&task).map(|r| (r.sp_lower, r.sp_upper))
    }

    /// Picks the highest-priority [`TaskState::Ready`] entry, the
    /// longest-waiting one ([`TaskRecord::ready_since`]) breaking a
    /// tie, reading each candidate's `ln_Pri` fresh from guest memory
    /// (a task's priority can change at any time via `SetTaskPri`, so
    /// nothing here caches it). See [`Self`]'s own doc for why the tie
    /// break has to be queue order rather than anything derived from
    /// the candidate set itself.
    fn pick_ready<M: AddressSpace>(&self, mem: &M) -> Option<u32> {
        self.others
            .iter()
            .filter(|(_, record)| record.state == TaskState::Ready)
            .map(|(&addr, record)| {
                let pri = mem.read_u8(addr + crate::execlist::LN_PRI) as i8;
                (pri, record.ready_since, addr)
            })
            .max_by_key(|&(pri, ready_since, _)| (pri, std::cmp::Reverse(ready_since)))
            .map(|(_, _, addr)| addr)
    }

    /// Blocks the currently-running task on `wait_mask` (`Wait` with
    /// nothing satisfied) and switches to the next `Ready` task, or
    /// fails with [`Deadlock`] if there isn't one -- in which case
    /// nothing is mutated at all, so the caller's own state (the task
    /// that tried to block) is left exactly as it was, free to retry or
    /// report the failure.
    ///
    /// `pc` must be the trap address itself (`crate::dispatch::
    /// HandlerContext::trap_pc`), not `cpu.pc()` -- see this module's
    /// doc for why. On success, the caller must skip its own generic
    /// post-dispatch epilogue entirely for this trap: this method's
    /// snapshot of the blocking task already *is* its correct resume
    /// state.
    pub fn block_current_on_signal<C: Cpu, M: AddressSpace>(
        &mut self,
        cpu: &C,
        mem: &M,
        pc: u32,
        wait_mask: u32,
    ) -> Result<(), Deadlock> {
        self.block_current(cpu, mem, pc, TaskState::BlockedOnSignal { wait_mask })
    }

    /// As [`Self::block_current_on_signal`], but for `ObtainSemaphore`/
    /// `ObtainSemaphoreShared` finding `sem` already held (issue #237's
    /// semaphore-contention follow-up): blocks the currently-running
    /// task on `sem` and switches to the next `Ready` task, or fails
    /// with [`Deadlock`] if there isn't one. Same resume contract as
    /// `block_current_on_signal` -- `pc` must be `ctx.trap_pc`, and the
    /// caller skips its own generic post-dispatch epilogue on success,
    /// since resuming re-enters the original `Obtain*` call from
    /// scratch rather than returning a remembered result.
    pub fn block_current_on_semaphore<C: Cpu, M: AddressSpace>(
        &mut self,
        cpu: &C,
        mem: &M,
        pc: u32,
        sem: u32,
    ) -> Result<(), Deadlock> {
        self.block_current(cpu, mem, pc, TaskState::BlockedOnSemaphore { sem })
    }

    /// As [`Self::block_current_on_signal`], but for `Delay` finding
    /// its own deadline not yet reached (issue #254): blocks the
    /// currently-running task and switches to the next `Ready` task,
    /// or fails with [`Deadlock`] if there isn't one. Same resume
    /// contract -- `pc` must be `ctx.trap_pc`, and the caller skips
    /// its own generic post-dispatch epilogue on success, since
    /// resuming re-enters `delay_handler` from scratch rather than
    /// returning a remembered result.
    pub fn block_current_on_delay<C: Cpu, M: AddressSpace>(
        &mut self,
        cpu: &C,
        mem: &M,
        pc: u32,
    ) -> Result<(), Deadlock> {
        self.block_current(cpu, mem, pc, TaskState::BlockedOnDelay)
    }

    /// Marks `target` [`TaskState::Ready`] if it's currently blocked on
    /// a `Delay` (issue #254) -- called by
    /// [`crate::exectask::PendingTimers`] once a `Delay`-blocked
    /// task's own deadline is reached, the same way
    /// [`Self::wake_blocked_on_semaphore`] reacts to `ReleaseSemaphore`.
    /// A no-op if `target` isn't currently `BlockedOnDelay` (it may
    /// already be `Ready`, or blocked on something else entirely, or
    /// not a known task at all) -- callers don't need to check first.
    pub fn wake_task(&mut self, target: u32) {
        self.mark_ready_if(target, TaskState::BlockedOnDelay);
    }

    /// Shared implementation behind [`Self::block_current_on_signal`]/
    /// [`Self::block_current_on_semaphore`]/[`Self::
    /// block_current_on_delay`]: the three differ only in which
    /// [`TaskState`] the blocking task ends up recorded with.
    fn block_current<C: Cpu, M: AddressSpace>(
        &mut self,
        cpu: &C,
        mem: &M,
        pc: u32,
        state: TaskState,
    ) -> Result<(), Deadlock> {
        // See `switch_hook_active`'s doc: a `tc_Switch`/`tc_Launch`
        // hook calling `Wait`/`ObtainSemaphore` and blocking here would
        // re-enter the trampoline mid-flight for the same outgoing
        // task. Refuse exactly like "nothing else is ready" rather
        // than risk that.
        if self.switch_hook_active {
            return Err(Deadlock);
        }
        let Some(next) = self.pick_ready(mem) else {
            return Err(Deadlock);
        };
        let mut snapshot = crate::cpu::capture(cpu);
        snapshot.pc = pc;
        let blocking = self.current;
        let (blocking_sp_lower, blocking_sp_upper) = Self::read_stack_bounds(mem, blocking);
        self.others.insert(
            blocking,
            TaskRecord {
                snapshot,
                state,
                sp_lower: blocking_sp_lower,
                sp_upper: blocking_sp_upper,
                // Blocked, so it holds no ready-queue position at all;
                // whichever `mark_ready_if` eventually wakes it stamps
                // the ticket that places it then.
                ready_since: 0,
                // `Wait` breaks `Forbid()` while it sleeps -- the next
                // task runs with its own count -- and the count comes
                // back when this task does, via `commit`.
                forbid_nest: self.forbid_nest,
            },
        );
        let next_record = self
            .others
            .remove(&next)
            .expect("pick_ready only returns a known task");
        // Deliberately *not* `self.current = next` here -- see
        // `Self::commit`'s doc. `Self::current` keeps reporting the
        // blocking task until the caller actually restores `next`'s
        // registers (immediately, or after `next`'s... no, after the
        // *blocking* task's `tc_Switch` runs, if it asked for one).
        self.pending_restore = Some((next, next_record));
        Ok(())
    }

    /// Takes the next task's address, register snapshot, and stack
    /// bounds a successful [`Self::block_current_on_signal`]/[`Self::
    /// yield_to_ready`]/[`Self::terminate_current`] call left for the
    /// caller to actually apply: restore the snapshot into the live
    /// `Cpu`, retarget the sanitizer, and -- the one thing this method
    /// does *not* do -- [`Self::commit`] the switch, since the caller
    /// may need to run the outgoing task's `tc_Switch` first (issue
    /// #237). `None` if the last call didn't switch.
    pub fn take_pending_restore(&mut self) -> Option<(u32, RegisterSnapshot, u32, u32)> {
        self.pending_restore.take().map(|(next, r)| {
            self.incoming_forbid_nest = r.forbid_nest;
            (next, r.snapshot, r.sp_lower, r.sp_upper)
        })
    }

    fn read_stack_bounds<M: AddressSpace>(mem: &M, task: u32) -> (u32, u32) {
        (
            mem.read_u32(task + crate::exectask::TC_SPLOWER),
            mem.read_u32(task + crate::exectask::TC_SPUPPER),
        )
    }

    /// Marks `target` [`TaskState::Ready`] if it's currently
    /// [`TaskState::BlockedOnSignal`] on a mask that intersects
    /// `recvd` (the target's `tc_SigRecvd` *after* `Signal`'s caller
    /// already OR'd the new bits in). Never switches -- `Signal` itself
    /// doesn't yield, matching real hardware; the woken task only
    /// actually runs once some later switch point picks it. A no-op if
    /// `target` isn't a known blocked task (either it's `current`, or
    /// it's `Ready` already, or `Signal`'s own "unknown task" check
    /// already rejected it before this is ever called).
    pub fn wake_if_signal_matches(&mut self, target: u32, recvd: u32) {
        let Some(record) = self.others.get(&target) else {
            return;
        };
        if let TaskState::BlockedOnSignal { wait_mask } = record.state
            && recvd & wait_mask != 0
        {
            self.mark_ready_if(target, TaskState::BlockedOnSignal { wait_mask });
        }
    }

    /// Marks every task [`TaskState::Ready`] that's currently
    /// [`TaskState::BlockedOnSemaphore`] on `sem` (issue #237's
    /// semaphore-contention follow-up) -- called once `ReleaseSemaphore`
    /// sees `sem` fully freed (nest count reached zero). Wakes *all*
    /// matching waiters at once rather than tracking real FIFO queue
    /// order through `ss_WaitQueue` (which stays the empty, unused
    /// `MinList` it already was); this stays behaviourally correct
    /// under cooperative scheduling, since the woken tasks still only
    /// re-attempt their original `Obtain*` call one at a time, just not
    /// necessarily in arrival order, and whichever loses the race to
    /// obtain `sem` first simply blocks again. Never switches, matching
    /// `wake_if_signal_matches`.
    pub fn wake_blocked_on_semaphore(&mut self, sem: u32) {
        let waiters: Vec<u32> = self
            .others
            .iter()
            .filter(|(_, record)| record.state == TaskState::BlockedOnSemaphore { sem })
            .map(|(&addr, _)| addr)
            .collect();
        for waiter in waiters {
            self.mark_ready_if(waiter, TaskState::BlockedOnSemaphore { sem });
        }
    }

    /// Voluntary yield (`Permit` after `Forbid`+`SysFlags |= SFB_SAR`,
    /// or `SetTaskPri` to a task's own current priority): if another
    /// task is `Ready`, switches to it; otherwise a no-op (there's
    /// nothing to give the timeslice to). Unlike [`Self::
    /// block_current_on_signal`], the caller must run its normal
    /// post-dispatch epilogue for the yielding task's library call
    /// *first* -- the call genuinely completed -- and only then call
    /// this, so `cpu`'s live registers already hold the correct
    /// post-epilogue resume state to capture.
    ///
    /// Returns whether a switch happened; the caller checks
    /// [`Self::take_pending_restore`] either way (empty if this
    /// returned `false`).
    pub fn yield_to_ready<C: Cpu, M: AddressSpace>(&mut self, cpu: &C, mem: &M) -> bool {
        // See `switch_hook_active`'s doc -- same reasoning as
        // `block_current_on_signal`'s matching check.
        if self.switch_hook_active {
            return false;
        }
        let Some(next) = self.pick_ready(mem) else {
            return false;
        };
        self.switch_to_ready(cpu, mem, next);
        true
    }

    /// `SetTaskPri` (issue #254): unlike [`Self::yield_to_ready`]
    /// (`Permit`+`SFB_SAR`'s unconditional "give up the timeslice if
    /// anything is ready"), real exec's scheduler only reschedules out
    /// of `SetTaskPri` when the calling task's new priority actually
    /// drops below some ready task's -- setting the *same* priority a
    /// task already had is a no-op (it keeps running), and a lower-
    /// then-restore pair yields only during the lowering call, to
    /// whichever ready task that lower priority no longer dominates.
    /// `new_pri` is the just-written priority the caller already
    /// stored into the task's own `ln_Pri` (read fresh rather than
    /// cached, in case something else changed it in between).
    ///
    /// Same calling convention as `yield_to_ready`: the caller runs its
    /// own post-dispatch epilogue first, then calls this with `cpu`
    /// holding that post-epilogue state. Returns whether a switch
    /// happened.
    pub fn reschedule_if_outranked<C: Cpu, M: AddressSpace>(
        &mut self,
        cpu: &C,
        mem: &M,
        new_pri: i8,
    ) -> bool {
        if self.switch_hook_active {
            return false;
        }
        let Some(next) = self.pick_ready(mem) else {
            return false;
        };
        let next_pri = mem.read_u8(next + crate::execlist::LN_PRI) as i8;
        if next_pri <= new_pri {
            // The lowered task still dominates (or ties) every ready
            // task's priority -- real exec never switches away on a
            // tie, so neither does this.
            return false;
        }
        self.switch_to_ready(cpu, mem, next);
        true
    }

    /// Shared staging logic behind [`Self::yield_to_ready`] and
    /// [`Self::reschedule_if_outranked`]: both have already decided
    /// (via [`Self::pick_ready`] plus their own condition) that `next`
    /// should run instead of [`Self::current`].
    fn switch_to_ready<C: Cpu, M: AddressSpace>(&mut self, cpu: &C, mem: &M, next: u32) {
        let yielding = self.current;
        let (sp_lower, sp_upper) = Self::read_stack_bounds(mem, yielding);
        // A fresh ticket: the yielding task rejoins its priority's
        // ready queue at the *back*, which is what makes repeated
        // yields rotate through every equal-priority task instead of
        // bouncing between the same two (issue #262).
        let ready_since = self.next_ready_ticket();
        self.others.insert(
            yielding,
            TaskRecord {
                snapshot: crate::cpu::capture(cpu),
                state: TaskState::Ready,
                sp_lower,
                sp_upper,
                ready_since,
                forbid_nest: self.forbid_nest,
            },
        );
        let next_record = self
            .others
            .remove(&next)
            .expect("pick_ready only returns a known task");
        // Not `self.current = next` here either -- see `Self::commit`'s
        // doc. A voluntary yield/reschedule has no outgoing `tc_Switch`
        // step (the yielding task's own library call already fully
        // completed, by the time this is called -- see the callers'
        // doc), but the caller still commits via the same path as every
        // other switch, for one reason: the *incoming* task may have
        // `TF_LAUNCH` set, and `tc_Launch` must see `current()` already
        // updated to it.
        self.pending_restore = Some((next, next_record));
    }

    /// The current task ran off the end of its `NP_Entry` (reached the
    /// task-exit stub, issue #121): it is gone, not suspended, so
    /// unlike every other switch here there is no snapshot to save for
    /// it. Switches to the next `Ready` task, or fails with
    /// [`Deadlock`] if there isn't one (an orphaned task left with
    /// nothing able to wake it).
    ///
    /// Doesn't free the exited task's `struct Process`/stack/name
    /// string itself -- this module never recorded their addresses
    /// for the *running* task (only non-running tasks are tracked in
    /// [`Self::others`]), so it has nothing to free them with. Issue
    /// #254 found this mattered after all (many `CreateNewProc`'d
    /// short-lived processes, not the one-time leak this used to
    /// assume): [`crate::exectask::task_exit_stub_handler`], the sole
    /// caller, frees them itself before calling this, since it still
    /// has the exiting task's own struct to read every address back
    /// out of.
    pub fn terminate_current<M: AddressSpace>(&mut self, mem: &M) -> Result<(), Deadlock> {
        // See `switch_hook_active`'s doc -- same reasoning as
        // `block_current_on_signal`'s matching check.
        if self.switch_hook_active {
            return Err(Deadlock);
        }
        let Some(next) = self.pick_ready(mem) else {
            return Err(Deadlock);
        };
        let next_record = self
            .others
            .remove(&next)
            .expect("pick_ready only returns a known task");
        // Not `self.current = next` -- see `Self::commit`'s doc. The
        // terminating task is gone either way, but real hardware still
        // calls its `tc_Switch` on this exact loss-of-CPU (the caller
        // checks the *current* task's `TF_SWITCH`/`tc_Flags`
        // generically for every switch, not specially for this one),
        // so `current()` must keep reporting it until that's run.
        self.pending_restore = Some((next, next_record));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::M68kCpu;
    use crate::memory::FlatMemory;

    fn zero_snapshot() -> RegisterSnapshot {
        RegisterSnapshot {
            d: [0; 8],
            a: [0; 8],
            pc: 0,
            sr: 0,
        }
    }

    #[test]
    fn switch_hook_active_refuses_block_current_on_signal_even_with_a_ready_task() {
        // Code review finding: without this guard, a tc_Switch/
        // tc_Launch hook (itself an ordinary subroutine that may call
        // Wait) calling Wait() and finding another Ready task would
        // stage a second, nested switch decision while current()
        // still lags the first one -- the caller would then re-read
        // the same outgoing task's still-set TF_SWITCH and re-enter
        // its tc_Switch a second time, mid-flight. The guard must
        // refuse exactly as if nothing were ready, leaving the
        // already-Ready task completely untouched.
        let main = 0x1000;
        let other = 0x2000;
        let mut tasks = TaskScheduler::new(main);
        let mem = FlatMemory::new(0x4000);
        let cpu = M68kCpu::new();
        tasks.spawn_ready(other, zero_snapshot(), 0, 0x1000);

        tasks.begin_switch_hook();
        assert_eq!(
            tasks.block_current_on_signal(&cpu, &mem, 0, 1),
            Err(Deadlock),
            "must refuse while a switch hook is in flight, even though `other` is Ready"
        );
        assert!(
            tasks.others.contains_key(&other),
            "the Ready task must be left completely untouched by the refused attempt"
        );
        assert!(
            !tasks.has_pending_switch(),
            "a refused attempt must not stage a switch"
        );

        tasks.end_switch_hook();
        assert!(
            tasks.block_current_on_signal(&cpu, &mem, 0, 1).is_ok(),
            "must work normally again once the hook finishes"
        );
    }

    #[test]
    fn switch_hook_active_refuses_yield_to_ready_and_terminate_current() {
        // Same guard, the other two switch-staging entry points.
        let main = 0x1000;
        let other = 0x2000;
        let mem = FlatMemory::new(0x4000);
        let cpu = M68kCpu::new();

        let mut tasks = TaskScheduler::new(main);
        tasks.spawn_ready(other, zero_snapshot(), 0, 0x1000);
        tasks.begin_switch_hook();
        assert!(
            !tasks.yield_to_ready(&cpu, &mem),
            "yield_to_ready must refuse while a switch hook is in flight"
        );

        let mut tasks = TaskScheduler::new(main);
        tasks.spawn_ready(other, zero_snapshot(), 0, 0x1000);
        tasks.begin_switch_hook();
        assert_eq!(
            tasks.terminate_current(&mem),
            Err(Deadlock),
            "terminate_current must refuse while a switch hook is in flight"
        );
    }

    #[test]
    fn repeated_yields_rotate_through_every_equal_priority_task() {
        // codewiz's issue #262 "yield-n.c" repro, at the scheduler
        // level: N equal-priority tasks that each yield must every one
        // of them get the CPU. `pick_ready` used to break a priority
        // tie by lowest task address -- a fixed function of the
        // candidate set, with no memory of who ran last -- so the two
        // lowest-addressed tasks ping-ponged between themselves and
        // every other ready task starved forever. Three tasks is the
        // smallest set that shows it (with two there's never a choice
        // to make).
        let main = 0x1000;
        let others = [0x2000, 0x3000, 0x4000];
        let mut mem = FlatMemory::new(0x8000);
        let cpu = M68kCpu::new();

        let mut tasks = TaskScheduler::new(main);
        for task in others {
            // Equal priority: `ln_Pri` 0 for all four, main included.
            mem.write_u8(task + crate::execlist::LN_PRI, 0);
            tasks.spawn_ready(task, zero_snapshot(), 0, 0x1000);
        }
        mem.write_u8(main + crate::execlist::LN_PRI, 0);

        // Yield round-robin style, committing each switch the way
        // `crate::dispatch` does, and record who ran.
        let mut ran = Vec::new();
        for _ in 0..8 {
            assert!(
                tasks.yield_to_ready(&cpu, &mem),
                "something is always Ready here, so every yield must switch"
            );
            let (next, ..) = tasks
                .take_pending_restore()
                .expect("a switch was just staged");
            tasks.commit(next);
            ran.push(tasks.current());
        }

        for task in others.iter().chain(std::iter::once(&main)) {
            assert!(
                ran.contains(task),
                "{task:#x} never ran across 8 yields among 4 equal-priority tasks: {ran:#x?}"
            );
        }
        // And it's a real rotation, not just eventual coverage: the
        // same task must not run twice before every other one has.
        assert_eq!(
            ran[..4]
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            4,
            "the first four yields must visit four distinct tasks: {ran:#x?}"
        );
    }

    #[test]
    fn a_woken_task_joins_the_back_of_its_priority_queue() {
        // The other half of issue #262's fix: becoming `Ready` again
        // takes a *fresh* ready-queue ticket, so a task that was
        // blocked and has just been woken doesn't jump ahead of tasks
        // that have been waiting their turn all along.
        let main = 0x1000;
        let early = 0x2000;
        let late = 0x4000;
        let mut mem = FlatMemory::new(0x8000);
        for task in [main, early, late] {
            mem.write_u8(task + crate::execlist::LN_PRI, 0);
        }

        let mut tasks = TaskScheduler::new(main);
        tasks.spawn_ready(late, zero_snapshot(), 0, 0x1000);
        tasks.spawn_ready(early, zero_snapshot(), 0, 0x1000);

        // `late` was spawned first, so it holds the older ticket and
        // must be picked first even though `early` has the lower
        // address (which the old tie break would have preferred).
        assert_eq!(tasks.pick_ready(&mem), Some(late));

        // Block `late` on a signal, then wake it: it now holds the
        // newest ticket, so `early` -- which has been Ready the whole
        // time -- goes first.
        tasks.others.get_mut(&late).expect("spawned above").state =
            TaskState::BlockedOnSignal { wait_mask: 1 };
        tasks.wake_if_signal_matches(late, 1);
        assert_eq!(
            tasks.others[&late].state,
            TaskState::Ready,
            "the signal matched its mask, so it must be Ready again"
        );
        assert_eq!(
            tasks.pick_ready(&mem),
            Some(early),
            "a just-woken task must rejoin at the back of its priority's queue"
        );
    }

    /// Stages and commits a switch away from the current task the way
    /// `crate::dispatch` does, for tests that only care about the
    /// scheduler's bookkeeping.
    fn switch_now(tasks: &mut TaskScheduler) {
        let (next, ..) = tasks
            .take_pending_restore()
            .expect("a switch was just staged");
        tasks.commit(next);
    }

    #[test]
    fn a_reschedule_request_waits_for_the_outermost_permit() {
        // Issue #262: inside Forbid() a SetTaskPri reschedule stays
        // pending, and is handed back only once every Forbid() has had
        // its Permit().
        let mut tasks = TaskScheduler::new(0x1000);
        tasks.enter_forbid();
        tasks.enter_forbid();
        tasks.request_reschedule_if_outranked(-1);

        assert_eq!(tasks.take_reschedule_request(), None, "two levels deep");
        assert!(!tasks.leave_forbid(), "inner Permit is not the outermost");
        assert_eq!(tasks.take_reschedule_request(), None, "one level deep");
        assert!(tasks.leave_forbid(), "outermost Permit");
        assert_eq!(
            tasks.take_reschedule_request(),
            Some(-1),
            "the request must survive the whole Forbid, not be dropped"
        );
        assert_eq!(tasks.take_reschedule_request(), None, "and is taken once");
    }

    #[test]
    fn an_unbalanced_permit_counts_as_outermost_instead_of_underflowing() {
        let mut tasks = TaskScheduler::new(0x1000);
        assert!(tasks.leave_forbid());
        tasks.enter_forbid();
        assert!(
            tasks.leave_forbid(),
            "a stray Permit must not leave a debt that swallows the next Forbid"
        );
    }

    #[test]
    fn wait_breaks_a_forbid_and_gets_it_back_on_resume() {
        // Real exec: a task that Wait()s inside Forbid() lets others
        // run, and finds its Forbid() back when it wakes. The nest
        // count is per task.
        let main = 0x1000;
        let other = 0x2000;
        let mut mem = FlatMemory::new(0x4000);
        let cpu = M68kCpu::new();
        for task in [main, other] {
            mem.write_u8(task + crate::execlist::LN_PRI, 0);
        }
        let mut tasks = TaskScheduler::new(main);
        tasks.spawn_ready(other, zero_snapshot(), 0, 0x1000);

        tasks.enter_forbid();
        tasks.enter_forbid();
        tasks
            .block_current_on_signal(&cpu, &mem, 0, 1)
            .expect("`other` is ready");
        switch_now(&mut tasks);
        assert_eq!(tasks.current(), other);

        // `other` is not Forbidden: a reschedule request is live.
        tasks.request_reschedule_if_outranked(0);
        assert_eq!(
            tasks.take_reschedule_request(),
            Some(0),
            "the incoming task must not inherit the blocked task's Forbid"
        );

        // Wake `main` and switch back to it.
        tasks.wake_if_signal_matches(main, 1);
        assert!(tasks.yield_to_ready(&cpu, &mem));
        switch_now(&mut tasks);
        assert_eq!(tasks.current(), main);

        assert!(!tasks.leave_forbid(), "first Permit: still one level in");
        assert!(tasks.leave_forbid(), "second Permit: now permitted");
    }

    #[test]
    fn a_task_that_exits_inside_a_forbid_leaves_no_count_for_the_next_task() {
        let main = 0x1000;
        let other = 0x2000;
        let mut mem = FlatMemory::new(0x4000);
        mem.write_u8(other + crate::execlist::LN_PRI, 0);
        let mut tasks = TaskScheduler::new(main);
        tasks.spawn_ready(other, zero_snapshot(), 0, 0x1000);

        tasks.enter_forbid();
        tasks.terminate_current(&mem).expect("`other` is ready");
        switch_now(&mut tasks);

        tasks.request_reschedule_if_outranked(0);
        assert_eq!(
            tasks.take_reschedule_request(),
            Some(0),
            "the exited task's Forbid must not outlive it"
        );
    }

    #[test]
    fn a_pending_reschedule_does_not_carry_over_to_the_next_task() {
        // `Wait` inside a Forbid, after a SetTaskPri: the switch away
        // answers the pending reschedule. Left set, it would be judged
        // against whoever runs next, using the blocked task's priority.
        let main = 0x1000;
        let other = 0x2000;
        let mut mem = FlatMemory::new(0x4000);
        let cpu = M68kCpu::new();
        for task in [main, other] {
            mem.write_u8(task + crate::execlist::LN_PRI, 0);
        }
        let mut tasks = TaskScheduler::new(main);
        tasks.spawn_ready(other, zero_snapshot(), 0, 0x1000);

        tasks.enter_forbid();
        tasks.request_reschedule_if_outranked(-1);
        tasks
            .block_current_on_signal(&cpu, &mem, 0, 1)
            .expect("`other` is ready");
        switch_now(&mut tasks);

        assert_eq!(tasks.take_reschedule_request(), None);
    }
}
