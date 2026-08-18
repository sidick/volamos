//! `exec.library` task/signal basics, `dos.library` `CheckSignal`, and
//! host `SIGINT`/`SIGTERM` -> `SIGBREAKF_CTRL_C` delivery (Phase 3 stage
//! 5).
//!
//! # A fake "current task"
//!
//! `volamos` is single-threaded and single-tasking: there is exactly one
//! guest "task" ever running, and it never actually blocks or context
//! switches. Per `docs/plan.md`'s Phase 3 scope
//! ("`FindTask(NULL)`, `SetSignal`, `Wait`, `SetExcept` minimally"),
//! [`create_current_task`] allocates one real, guest-visible `struct
//! Task` (`<exec/tasks.h>`, `sizeof(struct Task)` = 92 bytes) on the
//! guest heap at [`crate::dispatch::Runtime::new`] time, and every
//! handler in this module reads/writes it directly. Its address is
//! threaded through [`crate::dispatch::HandlerContext::current_task`]
//! (alongside `heap`/`registry`/`dos`, the same pattern every other
//! per-run host state already uses).
//!
//! ## State authority: guest memory is the single source of truth
//!
//! Unlike `dos.library`'s [`crate::dosfile::DosState`] (host-side state
//! mirrored into guest structs), this module keeps **no** separate
//! host-side copy of signal state at all -- `tc_SigAlloc`/`tc_SigWait`/
//! `tc_SigRecvd`/`tc_SigExcept` living in guest memory *are* the state.
//! Every handler below reads the current value straight out of guest
//! memory, computes the new value, and writes it straight back. This is
//! simpler than a host-mirrored design (no synchronization to get
//! wrong), and it means a guest program that peeks at its own task
//! struct's signal fields directly (rather than going exclusively
//! through `SetSignal`/`Wait`/...) sees exactly the same bytes a real
//! kernel would have left there -- the same "guest memory is
//! authoritative" choice `execlist.rs` already makes for `struct List`/
//! `struct MsgPort`.
//!
//! ## `struct Task` fields this module maintains
//!
//! ```text
//! struct Task {
//!     struct Node tc_Node;        +0   (14 bytes; only ln_Type/ln_Name set)
//!     UBYTE  tc_Flags;            +14  (never written; stays 0)
//!     UBYTE  tc_State;            +15  (never written; stays 0)
//!     BYTE   tc_IDNestCnt;        +16  (never written; stays 0)
//!     BYTE   tc_TDNestCnt;        +17  (never written; stays 0)
//!     ULONG  tc_SigAlloc;         +18  MAINTAINED (AllocSignal/FreeSignal)
//!     ULONG  tc_SigWait;          +22  MAINTAINED (Wait, best-effort)
//!     ULONG  tc_SigRecvd;         +26  MAINTAINED (SetSignal/Wait/Signal/
//!                                       CheckSignal/host-break folding)
//!     ULONG  tc_SigExcept;        +30  MAINTAINED (SetExcept; never
//!                                       actually delivered -- see below)
//!     ...                          92  (sizeof(struct Task); everything
//!                                       past tc_SigExcept -- tc_TrapAlloc,
//!                                       tc_TrapAble, tc_ExceptData/Code,
//!                                       tc_TrapData, tc_SPReg/SPLower/
//!                                       SPUpper, tc_Trap, tc_Switch/
//!                                       Launch/Suspend/Resume hooks,
//!                                       tc_UserData -- is zeroed once at
//!                                       creation and never touched again;
//!                                       nothing in this runtime reads it)
//! };
//! ```
//!
//! `tc_Node.ln_Type` is set to [`NT_TASK`] and `tc_Node.ln_Name` points to
//! a heap-allocated, NUL-terminated process name string -- both set once
//! at creation and never changed. `tc_Node.ln_Succ`/`ln_Pred`/`ln_Pri` stay
//! `0`: this fake task is never linked onto any exec task list (there is
//! no such list in this runtime).
//!
//! # `FindTask`
//!
//! `FindTask(NULL)` (`A1` = 0) returns the fake current task's guest
//! address. `FindTask("some name")` (`A1` != 0) always returns `0` (not
//! found): this runtime doesn't maintain a named-task list at all --
//! there is only ever the one, unnamed-from-the-caller's-perspective,
//! current task, matching real `FindTask`'s "name of task to be found, or
//! NULL for current task" contract in the degenerate single-task case.
//!
//! # `Wait`: trapping unrunnable blocking waits
//!
//! In a real, multitasking AmigaOS, `Wait()` suspends the calling task
//! until another task or an interrupt signals one of the requested bits.
//! There is no other task here, and no interrupt delivery except the host
//! break folded in from [`fold_pending_host_break`] -- so a `Wait` on
//! signals that aren't (and can never become) pending would deadlock the
//! host process forever. [`wait_handler`] therefore: folds in any pending
//! host break first, then if any requested signal is already pending,
//! clears and returns exactly that subset (matching real `Wait`'s own
//! contract -- it returns the *satisfied* subset, not the full request);
//! otherwise it fails loudly with [`DispatchError::HandlerFailed`] rather
//! than hanging. This mirrors vamos's own approach of trapping unrunnable
//! blocking waits instead of silently deadlocking the emulator.
//!
//! # `SetExcept`: tracked but never delivered
//!
//! [`set_except_handler`] maintains `tc_SigExcept` (the mask of signals
//! that would trigger the task's exception handler, `tc_ExceptCode`, on a
//! real kernel) and returns the old value, matching real `SetExcept`'s
//! contract -- but this runtime has no exception-handler-invocation
//! mechanism at all, so exceptions are simply never delivered. Tracked
//! anyway (rather than being a pure no-op) so a guest program that reads
//! `tc_SigExcept` back sees a consistent value.
//!
//! # Host `SIGINT`/`SIGTERM` -> `SIGBREAKF_CTRL_C`
//!
//! Real AmigaOS Shells map Ctrl-C at the keyboard to
//! `SIGBREAKF_CTRL_C` (bit 12, `0x00001000`) delivered to the running
//! process; `volamos` maps the host equivalent (`SIGINT`/`SIGTERM`,
//! e.g. an actual Ctrl-C at the host terminal, or `kill`) onto the same
//! guest signal. [`install_host_break_handler`] installs a minimal
//! host signal handler -- gated `#[cfg(unix)]`, a no-op on any other
//! platform -- that does nothing but set an atomic flag
//! ([`PENDING_HOST_BREAK`]); it is *not* installed automatically by
//! [`crate::dispatch::Runtime::new`] (that would hijack the test
//! runner's own `SIGINT` handling for every unit test in this crate),
//! only by an explicit call, which `crates/volamos/src/main.rs` makes
//! once at CLI startup.
//!
//! [`fold_pending_host_break`] is the only thing that ever consults the
//! flag: it atomically takes-and-clears it, and if it was set, ORs
//! [`SIGBREAKF_CTRL_C`] into `tc_SigRecvd`. [`crate::dispatch::Runtime::
//! run`] calls it once per dispatched library call (right before
//! building the [`crate::dispatch::HandlerContext`] for the trapped
//! call), and [`wait_handler`]/[`check_signal_handler`] each also call it
//! themselves before consulting `tc_SigRecvd`, so a `Wait`/`CheckSignal`
//! invoked directly (bypassing the run loop, as this module's own tests
//! do) still sees a break that was set on the flag directly.
//!
//! ## Polling granularity
//!
//! Since the flag is only ever folded in at a dispatched library-call
//! boundary, a guest program running a tight, call-free compute loop
//! (no library calls at all) will not observe a host break until its
//! *next* library call, however long that takes -- there is no
//! instruction-level or timer-driven check. This is a known, accepted
//! limitation: real hardware interrupts preempt at any instruction, this
//! runtime only checks at trap-dispatch time.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::cpu::{AddressRegister, Cpu, DataRegister};
use crate::dispatch::{
    DOS_LIBRARY_BASE, DispatchError, EXEC_LIBRARY_BASE, HandlerContext, LibraryTable,
};
use crate::execlist::{LN_NAME, LN_TYPE};
use crate::guestmem::{GuestHeap, write_c_string};
use crate::lvos::dos::DOS_LVOS;
use crate::lvos::exec::EXEC_LVOS;
use crate::memory::AddressSpace;

// --- struct Task field offsets (bytes from the task's own address) ---
// tc_Node (struct Node, offset 0..14) reuses execlist.rs's LN_* offsets
// -- struct Task literally embeds struct Node as its first member.

/// `tc_Flags`: `UBYTE`, offset 14. Never written by this module.
pub const TC_FLAGS: u32 = 14;
/// `tc_State`: `UBYTE`, offset 15. Never written by this module.
pub const TC_STATE: u32 = 15;
/// `tc_IDNestCnt`: `BYTE`, offset 16. Never written by this module.
pub const TC_IDNESTCNT: u32 = 16;
/// `tc_TDNestCnt`: `BYTE`, offset 17. Never written by this module.
pub const TC_TDNESTCNT: u32 = 17;
/// `tc_SigAlloc`: `ULONG`, offset 18. Maintained by [`alloc_signal_handler`]/
/// [`free_signal_handler`].
pub const TC_SIGALLOC: u32 = 18;
/// `tc_SigWait`: `ULONG`, offset 22. Maintained (best-effort) by
/// [`wait_handler`].
pub const TC_SIGWAIT: u32 = 22;
/// `tc_SigRecvd`: `ULONG`, offset 26. Maintained by [`set_signal_handler`]/
/// [`wait_handler`]/[`signal_handler`]/[`check_signal_handler`]/
/// [`fold_pending_host_break`].
pub const TC_SIGRECVD: u32 = 26;
/// `tc_SigExcept`: `ULONG`, offset 30. Maintained by
/// [`set_except_handler`]; never actually delivered -- see module docs.
pub const TC_SIGEXCEPT: u32 = 30;

/// `sizeof(struct Task)` per `<exec/tasks.h>`.
pub const TASK_STRUCT_SIZE: u32 = 92;

/// `NT_TASK` (1), per `<exec/nodes.h>`.
pub const NT_TASK: u8 = 1;

/// `SIGBREAKF_CTRL_C` (bit 12, `0x00001000`), per `<exec/exec.h>` --
/// what a real AmigaOS Shell maps a host Ctrl-C onto, and what
/// [`fold_pending_host_break`] folds a host `SIGINT`/`SIGTERM` into.
pub const SIGBREAKF_CTRL_C: u32 = 1 << 12;

/// Initial value of `tc_SigAlloc`: signal bits 0-15 are reserved for
/// system use and start out pre-allocated on a real kernel (`AllocSignal`
/// only ever hands out bits 16-31 to well-behaved callers, though this
/// module's `AllocSignal(-1)` search -- see [`alloc_signal_handler`] --
/// will fall through to a low bit too if every high one is exhausted,
/// same as real `AllocSignal` would).
pub const SIG_SYSTEM_RESERVED: u32 = 0x0000_FFFF;

/// Fixed process name written to the fake current task's `ln_Name`.
/// This runtime doesn't model AmigaOS's real process-naming rules
/// (derived from the program's own filename); a constant, recognizable
/// name is enough for anything that just wants *a* name to print.
const PROCESS_NAME: &[u8] = b"volamos";

/// Set by the host `SIGINT`/`SIGTERM` handler (see
/// [`install_host_break_handler`]) when a host break has occurred but
/// hasn't yet been folded into any task's `tc_SigRecvd`. Only ever
/// written from a signal handler (`store`, async-signal-safe) or read
/// by [`fold_pending_host_break`] (`swap`); tests may also set it
/// directly to exercise the folding logic without raising a real
/// signal.
static PENDING_HOST_BREAK: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
unsafe extern "C" {
    /// The POSIX `signal(2)` function: installs `handler` for `signum`,
    /// returning the previous handler (ignored here). Declaring this
    /// prototype ourselves (rather than depending on the `libc` crate)
    /// adds no new dependency -- `libc` is linked by `std` on every
    /// platform this project targets anyway; this is just the minimal
    /// FFI surface needed to reach it.
    fn signal(signum: i32, handler: extern "C" fn(i32)) -> extern "C" fn(i32);
}

/// `SIGINT`, per POSIX (`<signal.h>`) -- the same numeric value on every
/// unix this project targets (Linux, macOS).
#[cfg(unix)]
const SIGINT: i32 = 2;
/// `SIGTERM`, per POSIX.
#[cfg(unix)]
const SIGTERM: i32 = 15;

/// The actual host signal handler: does nothing except set
/// [`PENDING_HOST_BREAK`], which is the only async-signal-safe operation
/// this module needs (a plain atomic store) -- everything else (folding
/// the flag into guest memory) happens later, synchronously, from
/// [`fold_pending_host_break`], never from within the signal handler
/// itself.
#[cfg(unix)]
extern "C" fn handle_host_break(_signum: i32) {
    PENDING_HOST_BREAK.store(true, Ordering::SeqCst);
}

/// Installs the host `SIGINT`/`SIGTERM` -> [`PENDING_HOST_BREAK`] handler
/// (see the module docs' "Host `SIGINT`/`SIGTERM`" section). A no-op on
/// any non-`unix` target.
///
/// Deliberately **not** called from [`crate::dispatch::Runtime::new`] --
/// installing a process-wide signal handler as a side effect of
/// constructing a `Runtime` would hijack the test runner's own `SIGINT`
/// handling for every unit test in this crate (including this module's
/// own). Callers that actually want host-break delivery (i.e.
/// `crates/volamos/src/main.rs`) call this explicitly, once, at startup.
pub fn install_host_break_handler() {
    #[cfg(unix)]
    unsafe {
        signal(SIGINT, handle_host_break);
        signal(SIGTERM, handle_host_break);
    }
}

/// Atomically takes-and-clears [`PENDING_HOST_BREAK`]; if it was set,
/// ORs [`SIGBREAKF_CTRL_C`] into `task`'s `tc_SigRecvd`. See the module
/// docs' "Host `SIGINT`/`SIGTERM`" section for when this is called and
/// its polling-granularity limitation.
pub fn fold_pending_host_break<M: AddressSpace>(mem: &mut M, task: u32) {
    if PENDING_HOST_BREAK.swap(false, Ordering::SeqCst) {
        let recvd = mem.read_u32(task + TC_SIGRECVD);
        mem.write_u32(task + TC_SIGRECVD, recvd | SIGBREAKF_CTRL_C);
    }
}

/// Allocates and minimally initializes a fake "current task" `struct
/// Task` on the guest heap (see the module docs for exactly which
/// fields are set), returning its address. Called once from
/// [`crate::dispatch::Runtime::new`].
///
/// # Panics
///
/// Panics if the guest heap doesn't have room for the 92-byte struct
/// plus the process name string -- both allocations happen while the
/// heap is still essentially empty (this runs before the command-line
/// buffer is allocated), so this should never happen in practice; a
/// panic here is a clear, immediate signal of a misconfigured
/// (implausibly tiny) heap rather than a subtle downstream `NULL`
/// dereference.
pub fn create_current_task<M: AddressSpace>(mem: &mut M, heap: &mut GuestHeap) -> u32 {
    let task = heap
        .alloc(TASK_STRUCT_SIZE)
        .expect("guest heap has room for the fake current task struct");

    // Zero the whole struct first -- every field this module doesn't
    // maintain (tc_Flags, tc_State, tc_IDNestCnt, tc_TDNestCnt, and
    // everything past tc_SigExcept) stays zeroed, matching a freshly
    // allocated block; nothing in this runtime reads those fields.
    for i in 0..TASK_STRUCT_SIZE {
        mem.write_u8(task.wrapping_add(i), 0);
    }

    // tc_Node: ln_Type = NT_TASK, ln_Name -> a heap-allocated process
    // name string. ln_Succ/ln_Pred/ln_Pri stay 0 -- this fake task is
    // never linked onto any exec task list (there is no such list
    // here).
    mem.write_u8(task + LN_TYPE, NT_TASK);
    let name_addr = heap
        .alloc(PROCESS_NAME.len() as u32 + 1)
        .expect("guest heap has room for the process name string");
    write_c_string(mem, name_addr, PROCESS_NAME);
    mem.write_u32(task + LN_NAME, name_addr);

    // tc_SigAlloc: the 16 system-reserved low signal bits start
    // pre-allocated, matching real AmigaOS's initial state.
    mem.write_u32(task + TC_SIGALLOC, SIG_SYSTEM_RESERVED);

    task
}

/// `FindTask` (LVO -294): `A1` = task name, or `NULL` for the current
/// task. See the module docs' "`FindTask`" section.
fn find_task_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let name_ptr = ctx.cpu.address_register(AddressRegister(1));
    let result = if name_ptr == 0 { ctx.current_task } else { 0 };
    ctx.cpu.set_data_register(DataRegister(0), result);
    Ok(())
}

/// `SetSignal` (LVO -306): `D0` = new signal bits, `D1` = mask of which
/// bits to change. Returns the *old* `tc_SigRecvd` in `D0`, then applies
/// `(tc_SigRecvd & !mask) | (newSignals & mask)` -- the standard
/// AmigaOS "read-modify-write under a mask" `SetSignal` contract.
fn set_signal_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let new_signals = ctx.cpu.data_register(DataRegister(0));
    let signal_mask = ctx.cpu.data_register(DataRegister(1));
    let task = ctx.current_task;

    let old = ctx.mem.read_u32(task + TC_SIGRECVD);
    let updated = (old & !signal_mask) | (new_signals & signal_mask);
    ctx.mem.write_u32(task + TC_SIGRECVD, updated);
    ctx.cpu.set_data_register(DataRegister(0), old);
    Ok(())
}

/// `SetExcept` (LVO -312): same `newSignals`/`mask` contract as
/// [`set_signal_handler`], but against `tc_SigExcept`. See the module
/// docs' "`SetExcept`" section -- tracked, never delivered.
fn set_except_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let new_signals = ctx.cpu.data_register(DataRegister(0));
    let signal_mask = ctx.cpu.data_register(DataRegister(1));
    let task = ctx.current_task;

    let old = ctx.mem.read_u32(task + TC_SIGEXCEPT);
    let updated = (old & !signal_mask) | (new_signals & signal_mask);
    ctx.mem.write_u32(task + TC_SIGEXCEPT, updated);
    ctx.cpu.set_data_register(DataRegister(0), old);
    Ok(())
}

/// `AllocSignal` (LVO -330): `D0` = requested signal number, or `-1` for
/// "any free bit". Searches `tc_SigAlloc` from bit 31 down to bit 0 (so
/// an "any" request prefers the application-range high bits over the
/// system-reserved low ones, matching real `AllocSignal`'s own
/// preference, without hard-excluding the low bits if every high one is
/// exhausted). Returns the allocated bit number in `D0`, or `-1` if the
/// specific bit requested was already allocated (or, for "any", if
/// every bit is allocated).
fn alloc_signal_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let requested = ctx.cpu.data_register(DataRegister(0)) as i32;
    let task = ctx.current_task;
    let alloc = ctx.mem.read_u32(task + TC_SIGALLOC);

    let found = if requested == -1 {
        (0..32).rev().find(|&bit| alloc & (1 << bit) == 0)
    } else if (0..32).contains(&requested) {
        let bit = requested as u32;
        (alloc & (1 << bit) == 0).then_some(bit)
    } else {
        None
    };

    match found {
        Some(bit) => {
            ctx.mem.write_u32(task + TC_SIGALLOC, alloc | (1 << bit));
            ctx.cpu.set_data_register(DataRegister(0), bit);
        }
        None => {
            ctx.cpu.set_data_register(DataRegister(0), u32::MAX); // -1
        }
    }
    Ok(())
}

/// `FreeSignal` (LVO -336): `D0` = signal number to free, or `-1`
/// (a documented no-op -- real `FreeSignal(-1)` does nothing). Out-of-
/// range values (not `-1` and not `0..32`) are also treated as a no-op
/// rather than an error, since there's no bit to clear.
fn free_signal_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let signal_num = ctx.cpu.data_register(DataRegister(0)) as i32;
    if !(0..32).contains(&signal_num) {
        return Ok(());
    }
    let task = ctx.current_task;
    let alloc = ctx.mem.read_u32(task + TC_SIGALLOC);
    ctx.mem
        .write_u32(task + TC_SIGALLOC, alloc & !(1 << signal_num));
    Ok(())
}

/// `Wait` (LVO -318): `D0` = signal set to wait for. See the module
/// docs' "`Wait`: trapping unrunnable blocking waits" section.
fn wait_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let signal_set = ctx.cpu.data_register(DataRegister(0));
    let task = ctx.current_task;
    fold_pending_host_break(ctx.mem, task);

    let recvd = ctx.mem.read_u32(task + TC_SIGRECVD);
    let satisfied = recvd & signal_set;
    if satisfied != 0 {
        ctx.mem.write_u32(task + TC_SIGRECVD, recvd & !satisfied);
        ctx.mem.write_u32(task + TC_SIGWAIT, 0);
        ctx.cpu.set_data_register(DataRegister(0), satisfied);
        return Ok(());
    }

    // Nothing satisfied: record what we were waiting for (best-effort
    // fidelity -- a real kernel leaves tc_SigWait set exactly like this
    // while the task is actually suspended) and fail loudly instead of
    // hanging the host process forever.
    ctx.mem.write_u32(task + TC_SIGWAIT, signal_set);
    Err(DispatchError::HandlerFailed {
        library: "exec.library".to_string(),
        lvo: -318,
        handler_name: "Wait".to_string(),
        message: format!(
            "Wait({signal_set:#010x}) would block forever: this is a single-tasking \
             runtime with no other task to deliver a signal, and none of the requested \
             signals are currently pending in tc_SigRecvd -- mirrors vamos's approach of \
             trapping unrunnable blocking waits rather than deadlocking the host process"
        ),
    })
}

/// `Signal` (LVO -324): `A1` = target task, `D0` = signal bits to set.
/// Only the fake current task exists in this runtime, so a `Signal` to
/// any other address fails loudly (there's no other task it could
/// plausibly refer to -- almost certainly a guest bug, e.g. a stale or
/// uninitialized task pointer, worth surfacing rather than silently
/// dropping).
fn signal_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let target = ctx.cpu.address_register(AddressRegister(1));
    let signals = ctx.cpu.data_register(DataRegister(0));

    if target != ctx.current_task {
        return Err(DispatchError::HandlerFailed {
            library: "exec.library".to_string(),
            lvo: -324,
            handler_name: "Signal".to_string(),
            message: format!(
                "Signal({target:#010x}, {signals:#010x}): unknown task -- this runtime is \
                 single-tasking, the only task that exists is the fake current task at \
                 {:#010x}",
                ctx.current_task
            ),
        });
    }

    let recvd = ctx.mem.read_u32(target + TC_SIGRECVD);
    ctx.mem.write_u32(target + TC_SIGRECVD, recvd | signals);
    Ok(())
}

/// `dos.library`'s `CheckSignal` (LVO -792): `D1` = mask of signals to
/// check. Returns (and clears) the intersection of `tc_SigRecvd` and the
/// mask, folding in any pending host break first -- see the module
/// docs.
fn check_signal_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let mask = ctx.cpu.data_register(DataRegister(1));
    let task = ctx.current_task;
    fold_pending_host_break(ctx.mem, task);

    let recvd = ctx.mem.read_u32(task + TC_SIGRECVD);
    let hit = recvd & mask;
    ctx.mem.write_u32(task + TC_SIGRECVD, recvd & !hit);
    ctx.cpu.set_data_register(DataRegister(0), hit);
    Ok(())
}

/// Registers every implemented task/signal handler: `exec.library`'s
/// `FindTask`/`SetSignal`/`SetExcept`/`Wait`/`Signal`/`AllocSignal`/
/// `FreeSignal`, plus `dos.library`'s `CheckSignal` (registered from
/// here rather than `dosfile.rs`, so that file needs no edits at all --
/// see the module docs). Called unconditionally from
/// [`crate::dispatch::Runtime::new`].
pub fn register_exectask_handlers<C: Cpu + 'static>(
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
    reg_exec!("FindTask", find_task_handler::<C>);
    reg_exec!("SetSignal", set_signal_handler::<C>);
    reg_exec!("SetExcept", set_except_handler::<C>);
    reg_exec!("Wait", wait_handler::<C>);
    reg_exec!("Signal", signal_handler::<C>);
    reg_exec!("AllocSignal", alloc_signal_handler::<C>);
    reg_exec!("FreeSignal", free_signal_handler::<C>);

    table
        .register_by_name(
            mem,
            DOS_LIBRARY_BASE,
            DOS_LVOS,
            "dos.library",
            "CheckSignal",
            check_signal_handler::<C>,
        )
        .expect("CheckSignal should be in DOS_LVOS");
}

#[cfg(test)]
#[allow(clippy::vec_init_then_push)]
mod tests {
    use super::*;
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{Runtime, StartConfig};
    use crate::memory::FlatMemory;

    fn load_words(mem: &mut FlatMemory, addr: u32, words: &[u16]) {
        let mut offset = addr;
        for &w in words {
            mem.write_u16(offset, w);
            offset += 2;
        }
    }

    /// `move.l #imm32,Dn`.
    fn move_imm_to_d(n: u16) -> u16 {
        0x203C | (n << 9)
    }

    /// `move.l #imm32,An`.
    fn move_imm_to_a(n: u16) -> u16 {
        0x207C | (n << 9)
    }

    /// `move.l D0,Dn`.
    fn move_d0_to_d(n: u16) -> u16 {
        0x2000 | (n << 9)
    }

    /// `movea.l #EXEC_LIBRARY_BASE,a6` -- `Runtime::new` seeds A6 with
    /// `DOS_LIBRARY_BASE` (a Phase 1 compatibility shim), but these
    /// tests need A6 = `EXEC_LIBRARY_BASE` to call exec.library LVOs.
    fn movea_exec_base_to_a6() -> [u16; 3] {
        [
            move_imm_to_a(6),
            (EXEC_LIBRARY_BASE >> 16) as u16,
            EXEC_LIBRARY_BASE as u16,
        ]
    }

    /// `movea.l #DOS_LIBRARY_BASE,a6`, for dos.library calls
    /// (`CheckSignal`).
    fn movea_dos_base_to_a6() -> [u16; 3] {
        [
            move_imm_to_a(6),
            (DOS_LIBRARY_BASE >> 16) as u16,
            DOS_LIBRARY_BASE as u16,
        ]
    }

    /// `jsr <disp16>(a6)`.
    fn jsr_disp16_a6(disp: i32) -> [u16; 2] {
        [0x4EAE, disp as u16]
    }

    const RTS: u16 = 0x4E75;

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
            },
        )
    }

    /// Prepends the exec-base A6 fixup to `words`.
    fn exec_program(words: &[u16]) -> Runtime<M68kCpu> {
        let mut full = movea_exec_base_to_a6().to_vec();
        full.extend_from_slice(words);
        runtime_with_program(&full)
    }

    /// Prepends the dos-base A6 fixup to `words`.
    fn dos_program(words: &[u16]) -> Runtime<M68kCpu> {
        let mut full = movea_dos_base_to_a6().to_vec();
        full.extend_from_slice(words);
        runtime_with_program(&full)
    }

    // --- FindTask ---

    #[test]
    fn find_task_null_returns_current_task_with_readable_type_and_name() {
        let mut words = Vec::new();
        words.push(move_imm_to_a(1)); // A1 = 0 (NULL)
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-294)); // FindTask(NULL) -> D0
        words.push(RTS);

        let mut rt = exec_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        let task_addr = code as u32;
        assert_ne!(task_addr, 0, "FindTask(NULL) should return a real address");
        assert_eq!(task_addr, rt.current_task());

        assert_eq!(rt.memory().read_u8(task_addr + LN_TYPE), NT_TASK);
        let name_ptr = rt.memory().read_u32(task_addr + LN_NAME);
        assert_ne!(name_ptr, 0);
        assert_eq!(
            crate::guestmem::read_c_string(rt.memory(), name_ptr),
            PROCESS_NAME
        );
    }

    #[test]
    fn find_task_by_name_always_returns_null() {
        let entry = TRAP_TABLE_END;
        let name = b"somename\0";

        let mut words = Vec::new();
        words.push(move_imm_to_a(1)); // A1 placeholder, patched below
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-294)); // FindTask("somename")
        words.push(RTS);
        let str_addr = entry + (movea_exec_base_to_a6().len() + words.len()) as u32 * 2;
        words[1] = (str_addr >> 16) as u16;
        words[2] = str_addr as u16;

        let mut full = movea_exec_base_to_a6().to_vec();
        full.extend_from_slice(&words);
        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &full);
        crate::guestmem::write_c_string(&mut mem, str_addr, name);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: str_addr + name.len() as u32 + 4,
                args: Vec::new(),
            },
        );
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 0, "FindTask of a name should always return NULL");
    }

    // --- SetSignal ---

    #[test]
    fn set_signal_sets_bits_returns_old_value() {
        let mut words = Vec::new();
        words.push(move_imm_to_d(0)); // D0 = 0x30 (new signals)
        words.push(0);
        words.push(0x30);
        words.push(move_imm_to_d(1)); // D1 = 0xF0 (mask)
        words.push(0);
        words.push(0xF0);
        words.extend_from_slice(&jsr_disp16_a6(-306)); // SetSignal -> D0 = old (0)
        words.push(RTS);

        let mut rt = exec_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 0, "first SetSignal call's old value should be 0");

        let task = rt.current_task();
        assert_eq!(
            rt.memory().read_u32(task + TC_SIGRECVD),
            0x30,
            "bits in the mask should now read the new value"
        );
    }

    #[test]
    fn set_signal_clears_bits_and_leaves_unmasked_bits_untouched() {
        // First call: set 0xFF under mask 0xFF (recvd = 0xFF).
        // Second call: set 0x00 under mask 0x0F (clears low nibble only).
        let mut words = Vec::new();
        words.push(move_imm_to_d(0));
        words.push(0);
        words.push(0xFF);
        words.push(move_imm_to_d(1));
        words.push(0);
        words.push(0xFF);
        words.extend_from_slice(&jsr_disp16_a6(-306)); // recvd = 0xFF

        words.push(move_imm_to_d(0)); // D0 = 0 (new)
        words.push(0);
        words.push(0);
        words.push(move_imm_to_d(1)); // D1 = 0x0F (mask)
        words.push(0);
        words.push(0x0F);
        words.extend_from_slice(&jsr_disp16_a6(-306)); // recvd should become 0xF0
        words.push(RTS);

        let mut rt = exec_program(&words);
        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed");
        let task = rt.current_task();
        assert_eq!(rt.memory().read_u32(task + TC_SIGRECVD), 0xF0);
    }

    // --- AllocSignal / FreeSignal ---

    #[test]
    fn alloc_signal_specific_bit_then_double_alloc_fails() {
        let mut words = Vec::new();
        words.push(move_imm_to_d(0)); // D0 = 20
        words.push(0);
        words.push(20);
        words.extend_from_slice(&jsr_disp16_a6(-330)); // AllocSignal(20) -> D0 = 20
        words.push(move_d0_to_d(2)); // D2 = 20 (kept)

        words.push(move_imm_to_d(0)); // D0 = 20 again
        words.push(0);
        words.push(20);
        words.extend_from_slice(&jsr_disp16_a6(-330)); // AllocSignal(20) again -> D0 = -1
        // exit code = D0 (should be -1, i.e. 0xFFFFFFFF)
        words.push(RTS);

        let mut rt = exec_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, -1,
            "re-allocating an already-allocated bit should fail"
        );
    }

    #[test]
    fn alloc_signal_any_prefers_high_bits() {
        let mut words = Vec::new();
        words.push(move_imm_to_d(0)); // D0 = -1 (any)
        words.push(0xFFFF);
        words.push(0xFFFF);
        words.extend_from_slice(&jsr_disp16_a6(-330)); // AllocSignal(-1)
        words.push(RTS);

        let mut rt = exec_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, 31,
            "AllocSignal(-1) should hand out the highest free bit first"
        );
    }

    #[test]
    fn alloc_signal_exhaustion_returns_minus_one() {
        let mut words = Vec::new();
        // Allocate every one of the 16 application-range bits (16..32)
        // one at a time via AllocSignal(-1), then try one more.
        for _ in 0..16 {
            words.push(move_imm_to_d(0));
            words.push(0xFFFF);
            words.push(0xFFFF);
            words.extend_from_slice(&jsr_disp16_a6(-330));
        }
        // 16 more, exhausting the low (system) range too.
        for _ in 0..16 {
            words.push(move_imm_to_d(0));
            words.push(0xFFFF);
            words.push(0xFFFF);
            words.extend_from_slice(&jsr_disp16_a6(-330));
        }
        // One more: every bit is now allocated -- should fail.
        words.push(move_imm_to_d(0));
        words.push(0xFFFF);
        words.push(0xFFFF);
        words.extend_from_slice(&jsr_disp16_a6(-330));
        words.push(RTS);

        let mut rt = exec_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, -1, "AllocSignal(-1) with every bit taken should fail");
    }

    #[test]
    fn free_signal_clears_bit_allowing_realloc() {
        let mut words = Vec::new();
        words.push(move_imm_to_d(0)); // AllocSignal(20)
        words.push(0);
        words.push(20);
        words.extend_from_slice(&jsr_disp16_a6(-330));

        words.push(move_imm_to_d(0)); // FreeSignal(20)
        words.push(0);
        words.push(20);
        words.extend_from_slice(&jsr_disp16_a6(-336));

        words.push(move_imm_to_d(0)); // AllocSignal(20) again -- should succeed now
        words.push(0);
        words.push(20);
        words.extend_from_slice(&jsr_disp16_a6(-330));
        words.push(RTS);

        let mut rt = exec_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, 20,
            "AllocSignal(20) should succeed again after FreeSignal(20)"
        );
    }

    #[test]
    fn free_signal_minus_one_is_a_no_op() {
        let mut words = Vec::new();
        words.push(move_imm_to_d(0)); // D0 = -1
        words.push(0xFFFF);
        words.push(0xFFFF);
        words.extend_from_slice(&jsr_disp16_a6(-336)); // FreeSignal(-1)
        words.push(0x7000); // moveq #0,d0
        words.push(RTS);

        let mut rt = exec_program(&words);
        let mut out = Vec::new();
        let code = rt
            .run(&mut out, None)
            .expect("FreeSignal(-1) should be a no-op, not an error");
        assert_eq!(code, 0);
    }

    // --- Signal + Wait ---

    #[test]
    fn signal_current_task_then_wait_returns_satisfied_subset() {
        let mut words = Vec::new();
        // Signal(current_task, 0x30): A1 = current task addr, D0 = 0x30.
        // FindTask(NULL) first, to get the address into A1.
        words.push(move_imm_to_a(1)); // A1 = 0 (NULL) for FindTask
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-294)); // FindTask(NULL) -> D0
        words.push(0x2240); // movea.l d0,a1 (A1 = task addr)

        words.push(move_imm_to_d(0)); // D0 = 0x30 (signals)
        words.push(0);
        words.push(0x30);
        words.extend_from_slice(&jsr_disp16_a6(-324)); // Signal(task, 0x30)

        words.push(move_imm_to_d(0)); // D0 = 0xF0 (wait for a superset)
        words.push(0);
        words.push(0xF0);
        words.extend_from_slice(&jsr_disp16_a6(-318)); // Wait(0xF0) -> D0 = 0x30 (satisfied subset)
        words.push(RTS);

        let mut rt = exec_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, 0x30,
            "Wait should return exactly the satisfied subset of the requested signals"
        );

        // The satisfied bits should have been cleared from tc_SigRecvd.
        let task = rt.current_task();
        assert_eq!(rt.memory().read_u32(task + TC_SIGRECVD), 0);
    }

    #[test]
    fn signal_to_unknown_task_fails_loudly() {
        let mut words = Vec::new();
        words.push(move_imm_to_a(1)); // A1 = some bogus address
        words.push((TRAP_TABLE_END >> 16) as u16);
        words.push((TRAP_TABLE_END + 0x500) as u16);
        words.push(move_imm_to_d(0)); // D0 = 1
        words.push(0);
        words.push(1);
        words.extend_from_slice(&jsr_disp16_a6(-324)); // Signal(bogus, 1)
        words.push(RTS);

        let mut rt = exec_program(&words);
        let mut out = Vec::new();
        let err = rt.run(&mut out, None).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown task"),
            "expected an unknown-task diagnostic, got: {msg}"
        );
    }

    #[test]
    fn wait_on_nothing_pending_fails_loudly_instead_of_blocking() {
        let mut words = Vec::new();
        words.push(move_imm_to_d(0)); // D0 = 1 (nothing pending)
        words.push(0);
        words.push(1);
        words.extend_from_slice(&jsr_disp16_a6(-318)); // Wait(1)
        words.push(RTS);

        let mut rt = exec_program(&words);
        let mut out = Vec::new();
        let err = rt.run(&mut out, None).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("would block forever"),
            "expected a would-block diagnostic, got: {msg}"
        );
    }

    // --- SetExcept ---

    #[test]
    fn set_except_returns_old_value_and_updates_masked_bits() {
        let mut words = Vec::new();
        words.push(move_imm_to_d(0)); // D0 = 0xFF
        words.push(0);
        words.push(0xFF);
        words.push(move_imm_to_d(1)); // D1 = 0xFF (mask)
        words.push(0);
        words.push(0xFF);
        words.extend_from_slice(&jsr_disp16_a6(-312)); // SetExcept -> D0 = old (0)
        words.push(move_d0_to_d(2)); // D2 = old value (0)

        words.push(move_imm_to_d(0)); // D0 = 0
        words.push(0);
        words.push(0);
        words.push(move_imm_to_d(1)); // D1 = 0x0F
        words.push(0);
        words.push(0x0F);
        words.extend_from_slice(&jsr_disp16_a6(-312)); // SetExcept -> D0 = old (0xFF)
        words.push(RTS);

        let mut rt = exec_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code as u32, 0xFF, "second call's old value should be 0xFF");
        let task = rt.current_task();
        assert_eq!(rt.memory().read_u32(task + TC_SIGEXCEPT), 0xF0);
    }

    // --- CheckSignal (dos.library) ---

    #[test]
    fn check_signal_clears_and_returns_intersection() {
        let mut words = Vec::new();
        // SetSignal(0x30, 0x30) via exec.library requires A6 =
        // EXEC_LIBRARY_BASE; CheckSignal requires A6 = DOS_LIBRARY_BASE.
        // Set up exec.library A6, call SetSignal, then swap A6 to
        // dos.library for CheckSignal.
        words.extend_from_slice(&movea_exec_base_to_a6());
        words.push(move_imm_to_d(0)); // D0 = 0x30
        words.push(0);
        words.push(0x30);
        words.push(move_imm_to_d(1)); // D1 = 0x30 (mask)
        words.push(0);
        words.push(0x30);
        words.extend_from_slice(&jsr_disp16_a6(-306)); // SetSignal

        words.extend_from_slice(&movea_dos_base_to_a6());
        words.push(move_imm_to_d(1)); // D1 = 0xF0 (mask; only 0x30 & 0xF0 = 0x30 hits)
        words.push(0);
        words.push(0xF0);
        words.extend_from_slice(&jsr_disp16_a6(-792)); // CheckSignal(0xF0) -> D0 = 0x30
        words.push(RTS);

        let mut rt = runtime_with_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 0x30);

        let task = rt.current_task();
        assert_eq!(
            rt.memory().read_u32(task + TC_SIGRECVD),
            0,
            "CheckSignal should clear the bits it returned"
        );
    }

    #[test]
    fn check_signal_no_match_returns_zero_and_does_not_clear_other_bits() {
        let mut words = Vec::new();
        words.extend_from_slice(&movea_exec_base_to_a6());
        words.push(move_imm_to_d(0)); // D0 = 0x01
        words.push(0);
        words.push(0x01);
        words.push(move_imm_to_d(1)); // D1 = 0x01
        words.push(0);
        words.push(0x01);
        words.extend_from_slice(&jsr_disp16_a6(-306)); // SetSignal(0x01, 0x01)

        words.extend_from_slice(&movea_dos_base_to_a6());
        words.push(move_imm_to_d(1)); // D1 = 0x02 (doesn't overlap 0x01)
        words.push(0);
        words.push(0x02);
        words.extend_from_slice(&jsr_disp16_a6(-792)); // CheckSignal(0x02) -> D0 = 0
        words.push(RTS);

        let mut rt = runtime_with_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 0);
        let task = rt.current_task();
        assert_eq!(
            rt.memory().read_u32(task + TC_SIGRECVD),
            0x01,
            "an unmatched CheckSignal shouldn't touch bits outside its mask"
        );
    }

    // --- Host-break folding (set the atomic directly, per the task
    // brief -- no real signal is raised in these tests) ---

    #[test]
    fn pending_host_break_folds_into_check_signal() {
        let mut rt = dos_program(&{
            let mut words = Vec::new();
            words.push(move_imm_to_d(1)); // D1 = SIGBREAKF_CTRL_C
            words.push((SIGBREAKF_CTRL_C >> 16) as u16);
            words.push(SIGBREAKF_CTRL_C as u16);
            words.extend_from_slice(&jsr_disp16_a6(-792)); // CheckSignal
            words.push(RTS);
            words
        });

        PENDING_HOST_BREAK.store(true, Ordering::SeqCst);

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code as u32, SIGBREAKF_CTRL_C,
            "a pending host break should be folded into CheckSignal's result"
        );
        assert!(
            !PENDING_HOST_BREAK.load(Ordering::SeqCst),
            "folding should clear the pending flag"
        );
    }

    #[test]
    fn pending_host_break_folds_into_wait() {
        let mut rt = exec_program(&{
            let mut words = Vec::new();
            words.push(move_imm_to_d(0)); // D0 = SIGBREAKF_CTRL_C
            words.push((SIGBREAKF_CTRL_C >> 16) as u16);
            words.push(SIGBREAKF_CTRL_C as u16);
            words.extend_from_slice(&jsr_disp16_a6(-318)); // Wait
            words.push(RTS);
            words
        });

        PENDING_HOST_BREAK.store(true, Ordering::SeqCst);

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code as u32, SIGBREAKF_CTRL_C);
    }

    #[test]
    fn no_pending_break_leaves_check_signal_unaffected() {
        PENDING_HOST_BREAK.store(false, Ordering::SeqCst);
        let mut rt = dos_program(&{
            let mut words = Vec::new();
            words.push(move_imm_to_d(1));
            words.push((SIGBREAKF_CTRL_C >> 16) as u16);
            words.push(SIGBREAKF_CTRL_C as u16);
            words.extend_from_slice(&jsr_disp16_a6(-792));
            words.push(RTS);
            words
        });
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 0);
    }

    #[test]
    fn install_host_break_handler_is_a_no_op_when_called_without_a_signal() {
        // Just exercises the function for coverage -- doesn't (and
        // can't, without actually raising a signal, which would upset
        // the test runner) assert anything about delivery.
        install_host_break_handler();
    }
}
