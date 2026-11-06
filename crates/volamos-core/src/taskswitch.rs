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
/// Ready/blocked candidates are chosen by scanning in task-address
/// order (a [`BTreeMap`], not a `HashMap`, specifically so this is
/// deterministic): among every [`TaskState::Ready`] entry, the highest
/// `ln_Pri` wins; ties break by lowest address. This is a simplified
/// stand-in for real AmigaOS's round-robin-among-equal-priority
/// scheduling (good enough for the handful of tasks any program this
/// runtime targets actually creates; a real round-robin would need to
/// remember *which* equal-priority task ran last, which nothing here
/// currently needs).
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
        }
    }

    /// Requests a voluntary yield once the current library call
    /// finishes -- see [`Self::yield_requested`]'s doc for why this is
    /// a flag rather than switching immediately. Called by `Permit`'s
    /// handler when `SysFlags`'s `SFB_SAR` bit was set (real
    /// `sched_yield()`'s mechanism) and by `SetTaskPri`'s handler when
    /// the new priority equals the old one.
    pub fn request_yield(&mut self) {
        self.yield_requested = true;
    }

    /// Takes (and clears) a pending [`Self::request_yield`] request.
    pub fn take_yield_request(&mut self) -> bool {
        std::mem::take(&mut self.yield_requested)
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
        self.others.insert(
            task,
            TaskRecord {
                snapshot,
                state: TaskState::Ready,
                sp_lower,
                sp_upper,
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

    /// Picks the highest-priority [`TaskState::Ready`] entry (lowest
    /// address breaking a tie), reading each candidate's `ln_Pri`
    /// fresh from guest memory (a task's priority can change at any
    /// time via `SetTaskPri`, so nothing here caches it).
    fn pick_ready<M: AddressSpace>(&self, mem: &M) -> Option<u32> {
        self.others
            .iter()
            .filter(|(_, record)| record.state == TaskState::Ready)
            .map(|(&addr, _)| {
                let pri = mem.read_u8(addr + crate::execlist::LN_PRI) as i8;
                (pri, addr)
            })
            .max_by_key(|&(pri, addr)| (pri, std::cmp::Reverse(addr)))
            .map(|(_, addr)| addr)
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
        // See `switch_hook_active`'s doc: a `tc_Switch`/`tc_Launch`
        // hook calling `Wait` and blocking here would re-enter the
        // trampoline mid-flight for the same outgoing task. Refuse
        // exactly like "nothing else is ready" rather than risk that.
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
                state: TaskState::BlockedOnSignal { wait_mask },
                sp_lower: blocking_sp_lower,
                sp_upper: blocking_sp_upper,
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
        self.pending_restore
            .take()
            .map(|(next, r)| (next, r.snapshot, r.sp_lower, r.sp_upper))
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
        if let Some(record) = self.others.get_mut(&target)
            && let TaskState::BlockedOnSignal { wait_mask } = record.state
            && recvd & wait_mask != 0
        {
            record.state = TaskState::Ready;
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
        let yielding = self.current;
        let (sp_lower, sp_upper) = Self::read_stack_bounds(mem, yielding);
        self.others.insert(
            yielding,
            TaskRecord {
                snapshot: crate::cpu::capture(cpu),
                state: TaskState::Ready,
                sp_lower,
                sp_upper,
            },
        );
        let next_record = self
            .others
            .remove(&next)
            .expect("pick_ready only returns a known task");
        // Not `self.current = next` here either -- see `Self::commit`'s
        // doc. A voluntary yield has no outgoing `tc_Switch` step (the
        // yielding task's own library call already fully completed, by
        // the time this is called -- see this method's doc), but the
        // caller still commits via the same path as every other switch,
        // for one reason: the *incoming* task may have `TF_LAUNCH` set,
        // and `tc_Launch` must see `current()` already updated to it.
        self.pending_restore = Some((next, next_record));
        true
    }

    /// The current task ran off the end of its `NP_Entry` (reached the
    /// task-exit stub, issue #121): it is gone, not suspended, so
    /// unlike every other switch here there is no snapshot to save for
    /// it. Switches to the next `Ready` task, or fails with
    /// [`Deadlock`] if there isn't one (an orphaned task left with
    /// nothing able to wake it).
    ///
    /// Deliberately leaks the exited task's `struct Process`/stack
    /// back in the guest heap rather than freeing them: this module
    /// never recorded their addresses for the *running* task (only
    /// non-running tasks are tracked in [`Self::others`]), and
    /// recovering them would mean plumbing the allocation sizes back
    /// in from `crate::exectask::create_task`/`CreateNewProc`'s
    /// handler for what is, for every program this runtime targets (one
    /// short-lived CLI run, not a long-lived server spawning threads
    /// indefinitely), a harmless one-time leak.
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
}
