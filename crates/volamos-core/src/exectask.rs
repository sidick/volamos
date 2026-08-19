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
//!     UWORD  tc_TrapAlloc;        +34  (never written; stays 0)
//!     UWORD  tc_TrapAble;         +36  (never written; stays 0)
//!     APTR   tc_ExceptData;       +38  (never written; stays 0)
//!     APTR   tc_ExceptCode;       +42  (never written; stays 0)
//!     APTR   tc_TrapData;         +46  (never written; stays 0)
//!     APTR   tc_TrapCode;         +50  (never written; stays 0)
//!     APTR   tc_SPReg;            +54  (never written; stays 0 -- only
//!                                       meaningful across a real
//!                                       context switch, which this
//!                                       single-tasking runtime never
//!                                       performs)
//!     APTR   tc_SPLower;          +58  MAINTAINED (Phase 3 stage 6: set
//!                                       at task creation to the run's
//!                                       stack region, updated by
//!                                       StackSwap)
//!     ULONG  tc_SPUpper;          +62  MAINTAINED (ditto)
//!     ...                          92  (sizeof(struct Task); everything
//!                                       past tc_SPUpper -- tc_Trap,
//!                                       tc_Switch/Launch/Suspend/Resume
//!                                       hooks, tc_UserData -- is zeroed
//!                                       once at creation and never
//!                                       touched again; nothing in this
//!                                       runtime reads it)
//! };
//! ```
//!
//! Offsets above were verified field-by-field against the real
//! `<exec/tasks.h>` `struct Task` layout (`struct Node tc_Node` (14
//! bytes) + `UBYTE`/`UBYTE`/`BYTE`/`BYTE`/`ULONG`/`ULONG`/`ULONG`/`ULONG`
//! running 0..34, then `UWORD tc_TrapAlloc`/`UWORD tc_TrapAble` at
//! 34/36, `APTR tc_ExceptData`/`tc_ExceptCode`/`tc_TrapData`/`tc_TrapCode`
//! at 38/42/46/50, `APTR tc_SPReg` at 54, `APTR tc_SPLower` at 58,
//! `ULONG tc_SPUpper` at 62) -- not just inferred from padding.
//!
//! # `StackSwap`
//!
//! [`stack_swap_handler`] implements `exec.library`'s `StackSwap` (LVO
//! -732): `A0` points to a guest `struct StackSwapStruct { APTR
//! stk_Lower; ULONG stk_Upper; APTR stk_Pointer; }` (offsets 0/4/8). Real
//! `StackSwap` does a plain swap: the task's `A7`/`tc_SPLower`/
//! `tc_SPUpper` exchange places with the struct's `stk_Pointer`/
//! `stk_Lower`/`stk_Upper`, so the struct ends up holding the *old*
//! stack -- calling `StackSwap` again with the same struct swaps back.
//!
//! There's a wrinkle specific to how this runtime dispatches library
//! calls (see `crate::dispatch`'s module docs): the guest's `jsr
//! -732(a6)` pushed a return address onto the *old* stack before
//! trapping into [`stack_swap_handler`], and [`crate::dispatch::Runtime::
//! run`] performs the matching `rts` itself, generically, *after* the
//! handler returns -- by popping a return address off of whatever `A7`
//! happens to be at that point. If [`stack_swap_handler`] simply set
//! `A7` to the new stack's pointer and returned, that generic `rts`
//! would pop garbage off the *new* stack (which has no return address on
//! it) instead of the real one. So the handler does what the real
//! assembly-language `StackSwap` implementation does: it reads the
//! return address off the *old* `A7` itself before swapping anything,
//! then -- after updating the struct and the task's `tc_SPLower`/
//! `tc_SPUpper` -- pushes that same return address onto the *new* stack
//! and sets `A7` to point at it. The generic post-handler `rts` in
//! `Runtime::run` then pops it from the new stack exactly as it would
//! any other call's return address, landing the guest back at its
//! caller with `A7` now equal to the new stack's `stk_Pointer` (matching
//! real `StackSwap`'s visible effect on the caller).
//!
//! One more detail matters for a *second* `StackSwap` call (the
//! swap-back) to work: every `stk_Pointer` this handler reads or writes
//! is kept in "nothing pending" form -- the same convention the guest
//! itself uses when it first hands `StackSwap` a fresh stack (just its
//! plain top address, with no return address sitting there yet). `A7`
//! at handler-entry time, by contrast, still addresses the *just-popped*
//! return-address slot from this very call's own `jsr` -- so the value
//! saved into the struct is `A7 + 4`, not `A7` itself. Saving the
//! unadjusted `A7` would leave that slot's now-stale word behind for a
//! *later* `StackSwap` call to misread as if it were still a pending
//! return address, corrupting the swap-back (caught by this module's own
//! `stack_swap_switches_stack_*` tests during development). With the `+
//! 4` adjustment, both halves of a round trip and the caller-supplied
//! initial `stk_Pointer` all agree on the same invariant, and pushing a
//! fresh return address always means "decrement by 4 from a
//! nothing-pending pointer, whichever stack it names."
//!
//! # Stack-overflow detection
//!
//! [`check_stack_bounds`] is `Runtime::run`'s guard against the "stack-
//! overflow bug class vamos is known to hit" (`docs/plan.md`'s Phase 3
//! scope): it compares `A7` against the current task's `tc_SPLower`/
//! `tc_SPUpper`, read fresh from guest memory (so it sees whatever
//! `StackSwap` last set them to), and fails loudly via
//! [`crate::dispatch::DispatchError::StackOverflow`] if `A7` has run
//! outside those bounds, instead of letting the guest silently corrupt
//! whatever memory happens to sit past its stack. Like the host-break
//! poll above, this is only checked once per dispatched trap -- a tight,
//! call-free guest loop that blows its stack and never calls a library
//! function again won't be caught. It's also, by construction, never
//! confused by the brief window *inside* [`stack_swap_handler`] itself
//! where `A7` and `tc_SPLower`/`tc_SPUpper` are momentarily about to
//! change: the check only ever runs at the top of `Runtime::run`'s loop,
//! before a handler is invoked, at which point both the stack pointer
//! and the bounds always describe the *same* stack (either the old one,
//! pre-swap, or the new one, post-swap) -- never a torn mix of the two.
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
    TIMER_DEVICE_BASE,
};
use crate::execlist::{LN_NAME, LN_TYPE, init_msg_port_fields};
use crate::guestmem::{GuestHeap, bptr_from_addr, write_c_string};
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
/// `tc_SPLower`: `APTR`, offset 58 -- verified against `<exec/tasks.h>`,
/// see the module docs' "`struct Task` fields this module maintains"
/// section. The current task's stack region's lowest valid address.
/// Maintained by [`create_current_task`] and [`stack_swap_handler`].
pub const TC_SPLOWER: u32 = 58;
/// `tc_SPUpper`: `ULONG`, offset 62 (per `<exec/tasks.h>`; despite the
/// name, it's declared `ULONG` there, not `APTR`, though this module
/// treats it as a plain address either way). The current task's stack
/// region's address one past the highest valid address. Maintained by
/// [`create_current_task`] and [`stack_swap_handler`].
pub const TC_SPUPPER: u32 = 62;

/// `sizeof(struct Task)` per `<exec/tasks.h>`.
pub const TASK_STRUCT_SIZE: u32 = 92;

// --- struct Process fields this module maintains beyond struct Task
// (`<dos/dosextens.h>`: `struct Process { struct Task pr_Task; struct
// MsgPort pr_MsgPort; WORD pr_Pad; BPTR pr_SegList; ... BPTR pr_CLI;
// ... }`) -- offsets verified field-by-field against a primary NDK
// source, the same methodology `crate::dispatch`'s `EXEC_BASE_*`
// offsets use.

/// `pr_MsgPort`: `struct MsgPort`, immediately after the embedded
/// `struct Task` -- offset [`TASK_STRUCT_SIZE`] (92). Found needed
/// while running the real `AmiSnap` binary (Simon's own project,
/// `~/src/amisnap`, linked with libnix): its startup code's classic
/// Workbench-vs-CLI detection idiom reads `pr_CLI` (see
/// [`PR_CLI_OFFSET`]) and, only if that's `NULL` (a real Workbench
/// launch), does `WaitPort(&pr_MsgPort)` for the `WBStartup` message --
/// this runtime never delivers one, so `pr_CLI` must be non-`NULL` for
/// any libnix/SAS-C-style CLI-launched program (which is what running
/// a binary through this runtime actually represents) to avoid that
/// branch and never call `WaitPort` on this port at all. Still
/// initialized as a real, valid (if perpetually empty) `MsgPort` via
/// [`init_msg_port_fields`] regardless, in case some other real binary
/// legitimately posts to or waits on its own process port.
pub const PR_MSGPORT_OFFSET: u32 = TASK_STRUCT_SIZE;
/// `pr_CLI`: `BPTR`, offset 172 (`pr_Task` 92 + `pr_MsgPort` 34 +
/// `pr_Pad` 2 + `pr_SegList` 4 + `pr_StackSize` 4 + `pr_GlobVec` 4 +
/// `pr_TaskNum` 4 + `pr_StackBase` 4 + `pr_Result2` 4 + `pr_CurrentDir`
/// 4 + `pr_CIS` 4 + `pr_COS` 4 + `pr_ConsoleTask` 4 +
/// `pr_FileSystemTask` 4 = 172). `0` means "not running under a CLI"
/// (a real Workbench launch); this runtime sets it to a real,
/// heap-allocated (if otherwise empty) `struct CommandLineInterface`
/// instead -- see [`PR_MSGPORT_OFFSET`]'s doc for why, and
/// [`crate::dosfile::cli_handler`] (`dos.library`'s `Cli()`, which
/// just returns this same field) for the guest-visible read path.
pub const PR_CLI_OFFSET: u32 = 172;
/// `sizeof(struct Process)` per `<dos/dosextens.h>` -- `pr_CLI`'s own
/// offset (172) plus every field after it (`pr_ReturnAddr`/
/// `pr_PktWait`/`pr_WindowPtr`/`pr_HomeDir` 4 each = 16, `pr_Flags` 4,
/// `pr_ExitCode`/`pr_ExitData`/`pr_Arguments` 4 each = 12,
/// `pr_LocalVars` (`struct MinList`) 14, `pr_ShellPrivate` 4, `pr_CES`
/// 4) = 172 + 16 + 4 + 12 + 14 + 4 + 4 = 226... plus `pr_CLI` itself
/// (4) = 230. This runtime's fake current task is allocated at this
/// full size (not just [`TASK_STRUCT_SIZE`]) so real guest code that
/// treats it as a full `struct Process` -- as any libnix/SAS-C startup
/// does -- reads real, in-bounds (if mostly zeroed) memory rather than
/// running off the end of a bare `struct Task`-sized block.
pub const PROCESS_STRUCT_SIZE: u32 = 230;
/// `sizeof(struct CommandLineInterface)` per `<dos/dosextens.h>`: 16
/// fields, all 4 bytes each (`LONG`/`BPTR`/`BSTR`, every one
/// pointer-or-longword-sized) = 64.
const CLI_STRUCT_SIZE: u32 = 64;

// --- struct StackSwapStruct field offsets (bytes from the struct's own
// address) -- per <exec/execbase.h>: `struct StackSwapStruct { APTR
// stk_Lower; ULONG stk_Upper; APTR stk_Pointer; };`. All three fields
// are plain 4-byte values, so the offsets are simply 0/4/8.

/// `stk_Lower`: `APTR`, offset 0.
const STK_LOWER: u32 = 0;
/// `stk_Upper`: `ULONG`, offset 4.
const STK_UPPER: u32 = 4;
/// `stk_Pointer`: `APTR`, offset 8.
const STK_POINTER: u32 = 8;

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
///
/// `sp_lower`/`sp_upper` are this run's actual stack region bounds
/// (`[sp_lower, sp_upper)`, per [`crate::dispatch::Runtime::new`]'s
/// `StartConfig::stack_size`-derived layout), written into `tc_SPLower`/
/// `tc_SPUpper` (Phase 3 stage 6) so [`stack_swap_handler`] and
/// [`check_stack_bounds`] have real bounds to work with from the start.
pub fn create_current_task<M: AddressSpace>(
    mem: &mut M,
    heap: &mut GuestHeap,
    sp_lower: u32,
    sp_upper: u32,
) -> u32 {
    let task = heap
        .alloc(PROCESS_STRUCT_SIZE)
        .expect("guest heap has room for the fake current task struct");

    // Zero the whole struct first -- every field this module doesn't
    // maintain (tc_Flags, tc_State, tc_IDNestCnt, tc_TDNestCnt, and
    // everything past tc_SPUpper, plus every pr_* field beyond
    // pr_MsgPort/pr_CLI) stays zeroed, matching a freshly allocated
    // block; nothing in this runtime reads those fields. tc_SPLower/
    // tc_SPUpper/pr_MsgPort/pr_CLI are overwritten with real values
    // below.
    for i in 0..PROCESS_STRUCT_SIZE {
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

    // tc_SPLower/tc_SPUpper: this run's actual stack region bounds (see
    // this function's doc).
    mem.write_u32(task + TC_SPLOWER, sp_lower);
    mem.write_u32(task + TC_SPUPPER, sp_upper);

    // pr_MsgPort: a real, valid (if perpetually empty) MsgPort owned by
    // this task -- see PR_MSGPORT_OFFSET's doc.
    init_msg_port_fields(mem, task + PR_MSGPORT_OFFSET, task);

    // pr_CLI: a real, heap-allocated (if otherwise empty) struct
    // CommandLineInterface, written as a BPTR -- see PR_CLI_OFFSET's doc
    // for why this must be non-NULL.
    let cli_addr = heap
        .alloc(CLI_STRUCT_SIZE)
        .expect("guest heap has room for the fake CLI struct");
    for i in 0..CLI_STRUCT_SIZE {
        mem.write_u8(cli_addr.wrapping_add(i), 0);
    }
    mem.write_u32(task + PR_CLI_OFFSET, bptr_from_addr(cli_addr));

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

/// `AT_DeadEnd` (bit 31, `0x80000000`), per `<exec/alerts.h>` -- set in
/// `alertNum` when the condition is fatal (system integrity can no
/// longer be guaranteed; a real kernel typically reboots or hangs
/// rather than returning to the caller). Clear means "recoverable":
/// real `Alert()` flashes the power light / shows a requester and
/// returns to the caller so execution continues.
const AT_DEAD_END: u32 = 0x8000_0000;

/// `Alert` (LVO -108): `D7` = `alertNum` (per [`crate::lvos::exec::EXEC_LVOS`],
/// already verified against a primary source). This runtime has no
/// Guru Meditation display to show, so both cases are handled as
/// honestly as a headless runtime can: a recoverable alert
/// ([`AT_DEAD_END`] clear) is impossible to usefully act on either, so
/// it's simply logged (via the handler's own `CallInfo` -- see
/// `--verbose`) and returned from, matching real `Alert()`'s documented
/// "flashes and returns" behavior for this case. A dead-end alert
/// ([`AT_DEAD_END`] set) means the guest itself has declared system
/// integrity can no longer be guaranteed, so this fails loudly instead
/// of pretending to continue past it -- the real machine wouldn't
/// either. Found needed while running the real `AmiSnap` binary
/// (`~/src/amisnap`): `PC - EXEC_LIBRARY_BASE == -108` exactly matched
/// this LVO in the `UnknownCall` diagnostic's candidate list, i.e. the
/// guest really was calling `Alert()`, not some math/AmiSSL library (a
/// wrong initial guess -- `amisslmaster.library` is opened
/// conditionally and this runtime never even attempts it).
fn alert_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let alert_num = ctx.cpu.data_register(DataRegister(7));
    if alert_num & AT_DEAD_END != 0 {
        return Err(DispatchError::HandlerFailed {
            library: "exec.library".to_string(),
            lvo: -108,
            handler_name: "Alert".to_string(),
            message: format!(
                "Alert({alert_num:#010x}): AT_DeadEnd set -- the guest has declared system \
                 integrity can no longer be guaranteed; a real machine would reboot or hang \
                 here rather than return to the caller"
            ),
        });
    }
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

/// `exec.library`'s `StackSwap` (LVO -732): `A0` = pointer to a guest
/// `struct StackSwapStruct`. See the module docs' "`StackSwap`" section
/// for the full explanation, including why this handler pushes the
/// return address onto the new stack instead of just swapping `A7`.
fn stack_swap_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let struct_ptr = ctx.cpu.address_register(AddressRegister(0));
    let task = ctx.current_task;

    // A7 still points at the return address the guest's own `jsr`
    // pushed onto the *old* stack: Runtime::run's generic post-dispatch
    // `rts` hasn't run yet (it always runs after the handler returns),
    // so this is exactly the value a real CPU's SP would hold at this
    // point too.
    let old_sp = ctx.cpu.address_register(AddressRegister(7));
    let return_addr = ctx.mem.read_u32(old_sp);

    let new_lower = ctx.mem.read_u32(struct_ptr + STK_LOWER);
    let new_upper = ctx.mem.read_u32(struct_ptr + STK_UPPER);
    let new_pointer = ctx.mem.read_u32(struct_ptr + STK_POINTER);

    let old_lower = ctx.mem.read_u32(task + TC_SPLOWER);
    let old_upper = ctx.mem.read_u32(task + TC_SPUPPER);

    // The struct now holds the *old* stack -- calling StackSwap again
    // with the same struct swaps back, matching real StackSwap's
    // documented contract. Every `stk_Pointer`/`new_pointer` this
    // handler ever reads or writes is kept in "nothing pending" form
    // (the same convention the guest itself uses when it first hands
    // StackSwap a fresh stack's plain top address): `old_sp + 4`, not
    // `old_sp` -- `old_sp` still addresses the *just-popped* return-
    // address slot (its content is stale, not logically part of the
    // stack anymore), so saving the unadjusted value would leave that
    // stale word behind for a *later* swap-back to misread as if it
    // were still a pending return address (this was, in fact, a real
    // bug caught by this module's own `stack_swap_switches_stack_*`
    // test before the `+ 4` was added). Adjusted so both halves of a
    // swap round-trip and the caller-supplied initial `stk_Pointer`
    // (an empty stack's plain top address) all agree on the same
    // "nothing pending" invariant.
    ctx.mem.write_u32(struct_ptr + STK_LOWER, old_lower);
    ctx.mem.write_u32(struct_ptr + STK_UPPER, old_upper);
    ctx.mem
        .write_u32(struct_ptr + STK_POINTER, old_sp.wrapping_add(4));

    ctx.mem.write_u32(task + TC_SPLOWER, new_lower);
    ctx.mem.write_u32(task + TC_SPUPPER, new_upper);

    // Push the saved return address onto the new stack and point A7 at
    // it, so Runtime::run's generic post-dispatch `rts` (which pops
    // whatever word A7 addresses once this handler returns) lands the
    // guest back at its caller -- now running on the new stack, with A7
    // ending up equal to new_pointer after that pop, exactly matching
    // real StackSwap's visible effect. `new_pointer` is always in
    // "nothing pending" form (see above), so unconditionally
    // decrementing by 4 before writing is correct whether this is a
    // fresh caller-supplied stack or one a previous StackSwap call left
    // behind.
    let new_sp = new_pointer.wrapping_sub(4);
    ctx.mem.write_u32(new_sp, return_addr);
    ctx.cpu.set_address_register(AddressRegister(7), new_sp);

    Ok(())
}

/// `exec.library`'s `Forbid`/`Permit` (LVO -132/-138, no args, no
/// return value): real `Forbid` disables task switching until a
/// matching `Permit`, protecting a critical section (e.g. walking a
/// shared list) from being preempted mid-scan. This runtime is
/// single-threaded and never preempts the running guest task for
/// anything (see the module docs' "fake current task" section), so
/// there's nothing a critical section could ever be preempted by --
/// both are true no-ops. Found missing running the real Workbench
/// 3.1.4 `C:/Avail` binary, which calls `Forbid` before walking exec's
/// memory-pool list and `Permit` after.
fn forbid_handler<C: Cpu>(_ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    Ok(())
}

/// See [`forbid_handler`].
fn permit_handler<C: Cpu>(_ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    Ok(())
}

/// Checks `a7` against the current task's `tc_SPLower`/`tc_SPUpper`
/// bounds, read fresh from guest memory (so this sees whatever
/// [`stack_swap_handler`] last set them to). Called once per dispatched
/// trap by [`crate::dispatch::Runtime::run`], at the same point as
/// [`fold_pending_host_break`] -- see the module docs' "Stack-overflow
/// detection" section for the full rationale and its granularity/
/// `StackSwap`-safety caveats.
pub fn check_stack_bounds<M: AddressSpace>(
    mem: &M,
    task: u32,
    a7: u32,
) -> Result<(), DispatchError> {
    let lower = mem.read_u32(task + TC_SPLOWER);
    let upper = mem.read_u32(task + TC_SPUPPER);
    if a7 < lower || a7 > upper {
        return Err(DispatchError::StackOverflow { a7, lower, upper });
    }
    Ok(())
}

/// `IOERR_OPENFAIL` (`exec/errors.h`): "device/unit failed to open".
const IOERR_OPENFAIL: i8 = -1;
/// `IOERR_NOCMD` (`exec/errors.h`): "command not supported by device".
const IOERR_NOCMD: i8 = -3;

/// `struct IORequest.io_Device` byte offset.
const IO_DEVICE_OFFSET: u32 = 20;
/// `struct IORequest.io_Command` byte offset (`UWORD`).
const IO_COMMAND_OFFSET: u32 = 28;
/// `struct IORequest.io_Error` byte offset (`io_Message` is 20 bytes:
/// `mn_Node` 14 + `mn_ReplyPort` 4 + `mn_Length` 2; then `io_Device` 4,
/// `io_Unit` 4, `io_Command` 2, `io_Flags` 1, `io_Error` 1 -- 32 bytes
/// total, the standard `sizeof(struct IORequest)`).
const IO_ERROR_OFFSET: u32 = 31;
/// `struct timerequest.tr_time` (a `struct timeval`: `tv_secs`/
/// `tv_micro`, both `ULONG`) sits right after the 32-byte `IORequest`.
const TR_TIME_SECS_OFFSET: u32 = 32;
const TR_TIME_MICRO_OFFSET: u32 = 36;

/// `timer.device`'s three documented commands (`devices/timer.h`,
/// numerically `CMD_NONSTD` (9) + 0/1/2 -- confirmed against the
/// AmiBlitz3 `timer.ab3`/`io.ab3` includes, the RKRM Devices book's own
/// "Timer Device" chapter doesn't give literal numbers).
const TR_ADDREQUEST: u16 = 9;
const TR_GETSYSTIME: u16 = 10;
const TR_SETSYSTIME: u16 = 11;

/// The E-Clock's count rate in ticks per second, as [`ReadEClock`]
/// reports in `D0`: 709379 Hz on a PAL machine (the system master clock
/// divided by 10), the rate every real PAL Amiga reports and the value
/// this runtime's fixed PAL-like identity uses.
const ECLOCK_PAL_HZ: u32 = 709_379;

/// `exec.library`'s `OpenDevice` (LVO -444: `A0` = device name
/// `CString*`, `D0` = unit, `A1` = `struct IORequest*`, `D1` = flags).
/// `D0` = an error code (`0` on success).
///
/// Found missing while running the real Workbench 3.1.4 `C:/Date`
/// binary, which opens `timer.device` unconditionally at startup, and
/// the real `PhxAss` assembler (Aminet), which requires it to actually
/// open -- unlike `Date`'s no-argument form, `PhxAss` treats a failed
/// open as fatal ("Can't open timer.device (Init)."), so making every
/// device fail unconditionally (this runtime's original position) was
/// too broad. `timer.device` -- specifically, only its `TR_GETSYSTIME`/
/// `TR_SETSYSTIME`/`TR_ADDREQUEST` commands ([`do_io_handler`]) -- has
/// simple, fully host-implementable semantics (wall clock + sleep),
/// unlike every other device (`console.device`/hardware devices/...),
/// which would need real drivers this runtime doesn't have (same "no
/// real handler processes" scope boundary `crate::dospkt`'s `DoPkt`
/// and `crate::dosdevproc`'s `GetDeviceProc` establish for
/// `dos.library` handlers) -- so `timer.device` succeeds, with
/// `io_Device` set to the real [`TIMER_DEVICE_BASE`] (whose jump table
/// backs the RKRM-documented `TimerBase = io_Device` library-call idiom
/// -- see that constant's doc), and everything else still fails with
/// `IOERR_OPENFAIL`, matching the real, documented "device/unit failed
/// to open" convention for a device this runtime genuinely can't back.
fn open_device_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let name_ptr = ctx.cpu.address_register(AddressRegister(0));
    let ioreq = ctx.cpu.address_register(AddressRegister(1));
    let name = crate::guestmem::read_c_string(ctx.mem, name_ptr);

    if name.eq_ignore_ascii_case(b"timer.device") {
        ctx.mem
            .write_u32(ioreq + IO_DEVICE_OFFSET, TIMER_DEVICE_BASE);
        ctx.mem.write_u8(ioreq + IO_ERROR_OFFSET, 0);
        ctx.cpu.set_data_register(DataRegister(0), 0);
        return Ok(());
    }

    ctx.mem
        .write_u8(ioreq + IO_ERROR_OFFSET, IOERR_OPENFAIL as u8);
    ctx.cpu
        .set_data_register(DataRegister(0), IOERR_OPENFAIL as i32 as u32);
    Ok(())
}

/// `exec.library`'s `CloseDevice` (LVO -450: `A1` = `struct
/// IORequest*`). No return value. A true no-op -- this runtime doesn't
/// refcount device opens (matching [`crate::dispatch`]'s
/// `CloseLibrary`), and even for `timer.device` there's no real
/// resource to release. Real callers may call this even after a
/// *failed* `OpenDevice` (confirmed via the real `C:/Date` binary,
/// which does exactly that on its own failure path), so this must not
/// assume `io_Device` is ever non-`NULL`.
fn close_device_handler<C: Cpu>(_ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    Ok(())
}

/// Core of `DoIO`/`SendIO` for a `timer.device` request (`io_Device` ==
/// [`TIMER_DEVICE_BASE`]): dispatches on `io_Command`, writes the
/// result into the request, and returns the `io_Error` value. Any
/// `io_Device` this runtime doesn't recognize (shouldn't happen for a
/// well-behaved caller, since only `timer.device` ever opens
/// successfully) fails with [`IOERR_NOCMD`] rather than panicking.
fn run_io_request(mem: &mut dyn AddressSpace, ioreq: u32) -> i8 {
    let device = mem.read_u32(ioreq + IO_DEVICE_OFFSET);
    if device != TIMER_DEVICE_BASE {
        return IOERR_NOCMD;
    }

    let command = mem.read_u16(ioreq + IO_COMMAND_OFFSET);
    match command {
        TR_GETSYSTIME => {
            // tv_secs/tv_micro are both since the AmigaOS epoch
            // (1978-01-01) -- see host_time_secs_micro, shared with the
            // library-call GetSysTime vector.
            let (secs, micro) = host_time_secs_micro();
            mem.write_u32(ioreq + TR_TIME_SECS_OFFSET, secs);
            mem.write_u32(ioreq + TR_TIME_MICRO_OFFSET, micro);
            0
        }
        TR_SETSYSTIME => {
            // No separate virtual clock to set -- accept and no-op,
            // same "can't meaningfully change the host clock" stance
            // as this runtime takes elsewhere.
            0
        }
        TR_ADDREQUEST => {
            // A (synchronous, since DoIO/SendIO both resolve
            // immediately here) delay -- same host sleep
            // crate::dosdate's Delay() uses, just from a timeval
            // instead of a tick count.
            let secs = mem.read_u32(ioreq + TR_TIME_SECS_OFFSET);
            let micro = mem.read_u32(ioreq + TR_TIME_MICRO_OFFSET);
            let millis = u64::from(secs) * 1000 + u64::from(micro) / 1000;
            std::thread::sleep(std::time::Duration::from_millis(millis));
            0
        }
        _ => IOERR_NOCMD,
    }
}

/// `exec.library`'s `DoIO` (LVO -456: `A1` = `struct IORequest*`).
/// `D0` = `io_Error` (also written into the request itself, matching
/// real `DoIO`). Synchronous by construction here (this runtime is
/// single-threaded, so "queue and wait" and "do it now" are the same
/// thing) -- see [`run_io_request`] for the timer commands this
/// answers for real. Found missing while running the real `PhxAss`
/// assembler.
fn do_io_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let ioreq = ctx.cpu.address_register(AddressRegister(1));
    let error = run_io_request(ctx.mem, ioreq);
    ctx.mem.write_u8(ioreq + IO_ERROR_OFFSET, error as u8);
    ctx.cpu
        .set_data_register(DataRegister(0), error as i32 as u32);
    Ok(())
}

/// `exec.library`'s `SendIO` (LVO -462: `A1` = `struct IORequest*`).
/// No return value (real `SendIO` is asynchronous and doesn't report
/// success/failure directly -- the caller finds out via `WaitIO`/
/// `CheckIO`/the reply message). Since this runtime completes every
/// request synchronously (see [`do_io_handler`]), this just runs it
/// immediately and replies the message so a caller that does
/// `SendIO`+`WaitIO` (rather than `DoIO`) still gets a correctly
/// completed request back without ever actually blocking.
fn send_io_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let ioreq = ctx.cpu.address_register(AddressRegister(1));
    let error = run_io_request(ctx.mem, ioreq);
    ctx.mem.write_u8(ioreq + IO_ERROR_OFFSET, error as u8);
    Ok(())
}

/// `exec.library`'s `WaitIO` (LVO -474: `A1` = `struct IORequest*`).
/// `D0` = `io_Error`. Every request this runtime hands out is already
/// complete by the time `SendIO`/`DoIO` returns (see their doc
/// comments), so this just reads back `io_Error` -- there's never
/// anything left to actually wait for.
fn wait_io_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let ioreq = ctx.cpu.address_register(AddressRegister(1));
    let error = ctx.mem.read_u8(ioreq + IO_ERROR_OFFSET);
    ctx.cpu
        .set_data_register(DataRegister(0), error as i8 as i32 as u32);
    Ok(())
}

/// `exec.library`'s `CheckIO` (LVO -468: `A1` = `struct IORequest*`).
/// `D0` = the request pointer (matching real `CheckIO`'s "still
/// non-`NULL` `A1` on completion" contract) -- every request here is
/// always already complete, so this never returns `0` ("still
/// pending").
fn check_io_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let ioreq = ctx.cpu.address_register(AddressRegister(1));
    ctx.cpu.set_data_register(DataRegister(0), ioreq);
    Ok(())
}

/// `exec.library`'s `AbortIO` (LVO -480: `A1` = `struct IORequest*`).
/// No return value. A no-op -- every request here already completed
/// synchronously by the time `AbortIO` could possibly be called on it,
/// matching real `AbortIO`'s documented "no effect on an
/// already-completed request" behavior.
fn abort_io_handler<C: Cpu>(_ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    Ok(())
}

/// The current wall-clock time as AmigaOS-epoch (1978-01-01) seconds
/// plus microseconds -- the value `GetSysTime`/`TR_GETSYSTIME` report,
/// per the RKRM Devices book's "Timer Device" chapter ("By convention,
/// it tells how many seconds have passed since midnight, January 1,
/// 1978"). Built on [`crate::dosdate::now_as_datestamp`], the same
/// clock every other date-facing call here uses.
fn host_time_secs_micro() -> (u32, u32) {
    let (days, minute, tick) = crate::dosdate::now_as_datestamp();
    let secs = (days as i64) * 86_400 + (minute as i64) * 60 + (tick as i64) / 50;
    let micro = (tick as i64 % 50) * 20_000;
    (secs as u32, micro as u32)
}

/// Reads a `struct timeval` (`tv_secs`/`tv_micro`, both `ULONG`) at
/// `addr`.
fn read_timeval(mem: &dyn AddressSpace, addr: u32) -> (u32, u32) {
    (mem.read_u32(addr), mem.read_u32(addr.wrapping_add(4)))
}

/// Writes a `struct timeval` at `addr`.
fn write_timeval(mem: &mut dyn AddressSpace, addr: u32, secs: u32, micro: u32) {
    mem.write_u32(addr, secs);
    mem.write_u32(addr.wrapping_add(4), micro);
}

/// `timer.device`'s `AddTime` (LVO -42: `A0` = destination `struct
/// timeval*`, `A1` = source). `*A0 += *A1`, microseconds carrying into
/// seconds, result stored back into `*A0` (no meaningful `D0`). One of
/// the time-arithmetic functions the RKRM documents calling as library
/// vectors off `TimerBase = io_Device` -- see
/// [`crate::dispatch::TIMER_DEVICE_BASE`].
fn add_time_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let dest = ctx.cpu.address_register(AddressRegister(0));
    let src = ctx.cpu.address_register(AddressRegister(1));
    let (dsecs, dmicro) = read_timeval(ctx.mem, dest);
    let (ssecs, smicro) = read_timeval(ctx.mem, src);
    let mut micro = u64::from(dmicro) + u64::from(smicro);
    let mut secs = dsecs.wrapping_add(ssecs);
    if micro >= 1_000_000 {
        micro -= 1_000_000;
        secs = secs.wrapping_add(1);
    }
    write_timeval(ctx.mem, dest, secs, micro as u32);
    Ok(())
}

/// `timer.device`'s `SubTime` (LVO -48: `A0` = destination, `A1` =
/// source). `*A0 -= *A1`, borrowing from seconds, result stored back
/// into `*A0`. This exact call (`jsr -48(A6)` with `A6` fetched from
/// `io_Device`) is how the real `PhxAss` assembler computes its "N
/// lines in X sec" stats line -- the previously-unimplemented vector
/// behind the long-open `PcOutOfBounds`-after-Pass-2 crash.
fn sub_time_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let dest = ctx.cpu.address_register(AddressRegister(0));
    let src = ctx.cpu.address_register(AddressRegister(1));
    let (dsecs, dmicro) = read_timeval(ctx.mem, dest);
    let (ssecs, smicro) = read_timeval(ctx.mem, src);
    let mut secs = dsecs.wrapping_sub(ssecs);
    let micro = if dmicro < smicro {
        secs = secs.wrapping_sub(1);
        dmicro + 1_000_000 - smicro
    } else {
        dmicro - smicro
    };
    write_timeval(ctx.mem, dest, secs, micro);
    Ok(())
}

/// `timer.device`'s `CmpTime` (LVO -54: `A0` = `dest`, `A1` = `src`).
/// `D0` = `0` if equal, `-1` if `*A0` is later than `*A1`, `+1` if
/// `*A0` is earlier -- the documented (inverted-looking) convention,
/// confirmed by the RKRM Devices chapter's own worked example ("-1 ...
/// means the first parameter has greater time value than second").
fn cmp_time_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let dest = ctx.cpu.address_register(AddressRegister(0));
    let src = ctx.cpu.address_register(AddressRegister(1));
    let d = read_timeval(ctx.mem, dest);
    let s = read_timeval(ctx.mem, src);
    let result: i32 = match d.cmp(&s) {
        std::cmp::Ordering::Greater => -1,
        std::cmp::Ordering::Less => 1,
        std::cmp::Ordering::Equal => 0,
    };
    ctx.cpu.set_data_register(DataRegister(0), result as u32);
    Ok(())
}

/// `timer.device`'s `GetSysTime` (LVO -66: `A0` = destination `struct
/// timeval*`). Fills `*A0` with the current system time -- the direct
/// library-call form of `TR_GETSYSTIME` (V36+), same clock, same
/// AmigaOS-epoch convention (see [`host_time_secs_micro`]).
fn get_sys_time_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let dest = ctx.cpu.address_register(AddressRegister(0));
    let (secs, micro) = host_time_secs_micro();
    write_timeval(ctx.mem, dest, secs, micro);
    Ok(())
}

/// `timer.device`'s `ReadEClock` (LVO -60: `A0` = destination `struct
/// EClockVal*`, `ev_hi`/`ev_lo` -- the upper and lower halves of the
/// 64-bit E-Clock register). `D0` = the E-Clock's count rate in ticks
/// per second ([`ECLOCK_PAL_HZ`]). The RKRM documents the register's
/// absolute value as having "no direct relationship to actual time" --
/// only *differences* between two readings, divided by the rate, are
/// meaningful -- so deriving the tick count from the same wall clock as
/// [`host_time_secs_micro`] (seconds-since-1978 at 709379 ticks/sec)
/// gives correct interval arithmetic, which is the only documented use.
fn read_eclock_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let dest = ctx.cpu.address_register(AddressRegister(0));
    let (secs, micro) = host_time_secs_micro();
    let ticks = u64::from(secs) * u64::from(ECLOCK_PAL_HZ)
        + u64::from(micro) * u64::from(ECLOCK_PAL_HZ) / 1_000_000;
    ctx.mem.write_u32(dest, (ticks >> 32) as u32);
    ctx.mem.write_u32(dest.wrapping_add(4), ticks as u32);
    ctx.cpu.set_data_register(DataRegister(0), ECLOCK_PAL_HZ);
    Ok(())
}

/// Registers every implemented task/signal handler: `exec.library`'s
/// `FindTask`/`SetSignal`/`SetExcept`/`Wait`/`Signal`/`AllocSignal`/
/// `FreeSignal`/`StackSwap`/`Forbid`/`Permit`/`OpenDevice`/
/// `CloseDevice`/`DoIO`/`SendIO`/`WaitIO`/`CheckIO`/`AbortIO`, plus
/// `dos.library`'s `CheckSignal` (registered from here rather than
/// `dosfile.rs`, so that file needs no edits at all -- see the module
/// docs), plus `timer.device`'s library-style time functions
/// (`AddTime`/`SubTime`/`CmpTime`/`ReadEClock`/`GetSysTime`, registered
/// onto [`TIMER_DEVICE_BASE`] at their real device LVOs -- the six
/// standard device vectors `-6`..`-36` come first, so the extended
/// functions start at `-42`, order confirmed against AROS's own
/// `rom/timer/timer.conf` functionlist). Called unconditionally from
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
    reg_exec!("Alert", alert_handler::<C>);
    reg_exec!("SetSignal", set_signal_handler::<C>);
    reg_exec!("SetExcept", set_except_handler::<C>);
    reg_exec!("Wait", wait_handler::<C>);
    reg_exec!("Signal", signal_handler::<C>);
    reg_exec!("AllocSignal", alloc_signal_handler::<C>);
    reg_exec!("FreeSignal", free_signal_handler::<C>);
    reg_exec!("StackSwap", stack_swap_handler::<C>);
    reg_exec!("Forbid", forbid_handler::<C>);
    reg_exec!("Permit", permit_handler::<C>);
    reg_exec!("OpenDevice", open_device_handler::<C>);
    reg_exec!("CloseDevice", close_device_handler::<C>);
    reg_exec!("DoIO", do_io_handler::<C>);
    reg_exec!("SendIO", send_io_handler::<C>);
    reg_exec!("WaitIO", wait_io_handler::<C>);
    reg_exec!("CheckIO", check_io_handler::<C>);
    reg_exec!("AbortIO", abort_io_handler::<C>);

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

    // timer.device's library-style vectors, at their real device LVOs
    // (see this function's doc comment). Registered directly by offset
    // rather than through a generated LVO table -- five well-known,
    // AROS-confirmed vectors don't warrant one.
    macro_rules! reg_timer {
        ($lvo:literal, $name:literal, $handler:expr) => {
            table.register(
                mem,
                TIMER_DEVICE_BASE,
                $lvo,
                "timer.device",
                $name,
                $handler,
            );
        };
    }
    reg_timer!(-42, "AddTime", add_time_handler::<C>);
    reg_timer!(-48, "SubTime", sub_time_handler::<C>);
    reg_timer!(-54, "CmpTime", cmp_time_handler::<C>);
    reg_timer!(-60, "ReadEClock", read_eclock_handler::<C>);
    reg_timer!(-66, "GetSysTime", get_sys_time_handler::<C>);
}

#[cfg(test)]
#[allow(clippy::vec_init_then_push)]
mod tests {
    use super::*;
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{EXIT_STUB_ADDR, Runtime, StartConfig};
    use crate::memory::FlatMemory;
    use std::sync::Mutex;

    /// Serializes every test in this module that calls `Runtime::run`
    /// against [`PENDING_HOST_BREAK`] (a process-global `static`): the
    /// Rust test runner executes tests in parallel threads within one
    /// process, and `fold_pending_host_break` is an unconditional,
    /// unscoped `swap` against that one global flag on *every* dispatched
    /// trap -- so without serialization, one test setting the flag can
    /// have it folded away by a completely unrelated test's `Runtime::run`
    /// racing on another thread (observed: `pending_host_break_folds_into_wait`
    /// losing its own flag to a concurrent `signal_current_task_then_wait_
    /// returns_satisfied_subset`, and that same test spuriously gaining
    /// `SIGBREAKF_CTRL_C` from a concurrent break-setting test). Held for
    /// a locked test's *entire* body -- not just the moment it touches
    /// the flag -- with the flag cleared immediately after acquiring the
    /// lock, so a stray `true` set by a test that raced in just before
    /// the lock was acquired can't leak into the locked section.
    /// `unwrap_or_else` recovers from a poisoned lock (an earlier locked
    /// test panicking mid-section) instead of cascading that failure into
    /// every subsequent locked test.
    static HOST_BREAK_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Acquires [`HOST_BREAK_TEST_LOCK`] and clears
    /// [`PENDING_HOST_BREAK`]; every test below that calls `Runtime::run`
    /// holds the returned guard for its whole body (a `let _guard = ...`
    /// at the top of the test, dropped implicitly at the end of the
    /// function).
    fn lock_host_break() -> std::sync::MutexGuard<'static, ()> {
        let guard = HOST_BREAK_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        PENDING_HOST_BREAK.store(false, Ordering::SeqCst);
        guard
    }

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
                ..StartConfig::default()
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
        let _guard = lock_host_break();
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
    fn create_current_task_populates_pr_msgport_and_pr_cli() {
        let rt = runtime_with_program(&[RTS]);
        let task = rt.current_task();

        // pr_CLI is a real, non-NULL BPTR (see PR_CLI_OFFSET's doc).
        let cli_bptr = rt.memory().read_u32(task + PR_CLI_OFFSET);
        assert_ne!(cli_bptr, 0, "pr_CLI must be non-NULL for a CLI-style run");

        // pr_MsgPort is a real, valid, empty MsgPort owned by this task.
        let port = task + PR_MSGPORT_OFFSET;
        assert_eq!(
            rt.memory().read_u8(port + crate::execlist::LN_TYPE),
            crate::execlist::NT_MSGPORT
        );
        assert_eq!(
            rt.memory().read_u32(port + crate::execlist::MP_SIGTASK),
            task
        );
        let list_head = rt
            .memory()
            .read_u32(port + crate::execlist::MP_MSGLIST + crate::execlist::LH_HEAD);
        // An empty list's head node has a NULL ln_Succ (real AmigaOS
        // empty-list-header convention: head -> tail sentinel, whose own
        // ln_Succ is NULL).
        assert_eq!(
            rt.memory().read_u32(list_head + crate::execlist::LN_SUCC),
            0
        );
    }

    #[test]
    fn execbase_thistask_matches_current_task() {
        let rt = runtime_with_program(&[RTS]);
        let this_task = rt.memory().read_u32(
            crate::dispatch::EXEC_LIBRARY_BASE + crate::dispatch::EXEC_BASE_THISTASK_OFFSET,
        );
        assert_eq!(this_task, rt.current_task());
    }

    #[test]
    fn find_task_by_name_always_returns_null() {
        let _guard = lock_host_break();
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
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 0, "FindTask of a name should always return NULL");
    }

    // --- Alert ---

    #[test]
    fn alert_recoverable_returns_and_execution_continues() {
        let mut words = Vec::new();
        words.push(move_imm_to_d(7)); // D7 = 0x01234567 (AT_DeadEnd clear)
        words.push(0x0123);
        words.push(0x4567);
        words.extend_from_slice(&jsr_disp16_a6(-108)); // Alert(a6)
        words.push(move_imm_to_d(0)); // D0 = 99, proving control returned here
        words.push(0);
        words.push(99);
        words.push(RTS);

        let mut rt = exec_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 99, "a recoverable alert should return to the caller");
    }

    #[test]
    fn alert_dead_end_fails_loudly_instead_of_pretending_to_continue() {
        let mut words = Vec::new();
        words.push(move_imm_to_d(7)); // D7 = 0x80000001 (AT_DeadEnd set)
        words.push(0x8000);
        words.push(0x0001);
        words.extend_from_slice(&jsr_disp16_a6(-108)); // Alert(a6)
        words.push(RTS);

        let mut rt = exec_program(&words);
        let mut out = Vec::new();
        let err = rt
            .run(&mut out, None)
            .expect_err("dead-end alert should fail");
        match err {
            crate::dispatch::RuntimeError::Dispatch(DispatchError::HandlerFailed {
                library,
                lvo,
                handler_name,
                ..
            }) => {
                assert_eq!(library, "exec.library");
                assert_eq!(lvo, -108);
                assert_eq!(handler_name, "Alert");
            }
            other => panic!("expected HandlerFailed, got {other:?}"),
        }
    }

    // --- SetSignal ---

    #[test]
    fn set_signal_sets_bits_returns_old_value() {
        let _guard = lock_host_break();
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
        let _guard = lock_host_break();
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
        let _guard = lock_host_break();
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
        let _guard = lock_host_break();
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
        let _guard = lock_host_break();
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
        let _guard = lock_host_break();
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
        let _guard = lock_host_break();
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
        let _guard = lock_host_break();
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
        let _guard = lock_host_break();
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
        let _guard = lock_host_break();
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
        let _guard = lock_host_break();
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
        let _guard = lock_host_break();
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
        let _guard = lock_host_break();
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
        let _guard = lock_host_break();
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
        let _guard = lock_host_break();
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
        // lock_host_break() already clears the flag on entry.
        let _guard = lock_host_break();
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

    // --- StackSwap ---

    /// Swaps to a heap-adjacent (but manually placed, not
    /// `GuestHeap`-tracked) new stack region, makes a real library call
    /// (`PutStr`) while running on it, swaps back to the original stack,
    /// and exits normally -- proving both the stack switch and continued
    /// correct execution (including the generic post-dispatch `rts`
    /// mechanism) survive a `StackSwap` round trip. See the module docs'
    /// "`StackSwap`" section for why the handler has to push the return
    /// address onto the new stack itself.
    #[test]
    fn stack_swap_switches_stack_survives_a_library_call_then_swaps_back() {
        let _guard = lock_host_break();
        let entry = TRAP_TABLE_END;

        let mut words = Vec::new();
        words.extend_from_slice(&movea_exec_base_to_a6());

        words.push(move_imm_to_a(0)); // A0 = struct_addr (patched below)
        let a0_patch_1 = words.len();
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-732)); // StackSwap -- switch

        words.extend_from_slice(&movea_dos_base_to_a6());
        words.push(move_imm_to_d(1)); // D1 = str_addr (patched below)
        let d1_patch = words.len();
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-948)); // PutStr, on the new stack

        words.extend_from_slice(&movea_exec_base_to_a6());
        words.push(move_imm_to_a(0)); // A0 = struct_addr again (patched below)
        let a0_patch_2 = words.len();
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-732)); // StackSwap -- swap back

        words.push(0x7000); // moveq #0,d0
        words.push(RTS);

        // Place the string, the StackSwapStruct, and the new stack
        // region at fixed offsets past the code -- deliberately *not*
        // GuestHeap-tracked (nothing in this test calls AllocMem/
        // AllocVec), with `load_end` set past all of them so the real
        // heap (task struct + command-line buffer) never overlaps.
        let code_len = words.len() as u32 * 2;
        let str_addr = entry + code_len;
        let struct_addr = str_addr + 8;
        let new_stack_block = struct_addr + 32;
        let new_stack_size = 0x1000u32;
        let new_stack_top = new_stack_block + new_stack_size;
        let load_end = new_stack_top;

        words[a0_patch_1] = (struct_addr >> 16) as u16;
        words[a0_patch_1 + 1] = struct_addr as u16;
        words[d1_patch] = (str_addr >> 16) as u16;
        words[d1_patch + 1] = str_addr as u16;
        words[a0_patch_2] = (struct_addr >> 16) as u16;
        words[a0_patch_2 + 1] = struct_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        write_c_string(&mut mem, str_addr, b"hi");
        // struct StackSwapStruct { stk_Lower, stk_Upper, stk_Pointer }
        mem.write_u32(struct_addr, new_stack_block);
        mem.write_u32(struct_addr + 4, new_stack_top);
        mem.write_u32(struct_addr + 8, new_stack_top);

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

        let task = rt.current_task();
        let original_lower = rt.memory().read_u32(task + TC_SPLOWER);
        let original_upper = rt.memory().read_u32(task + TC_SPUPPER);

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");

        assert_eq!(code, 0);
        assert_eq!(
            out, b"hi",
            "PutStr should have run correctly while on the swapped-to stack"
        );
        assert_eq!(
            rt.memory().read_u32(task + TC_SPLOWER),
            original_lower,
            "tc_SPLower should be restored by the second StackSwap"
        );
        assert_eq!(
            rt.memory().read_u32(task + TC_SPUPPER),
            original_upper,
            "tc_SPUpper should be restored by the second StackSwap"
        );
    }

    /// A single `StackSwap` (no swap-back) immediately updates
    /// `tc_SPLower`/`tc_SPUpper` to the new stack's bounds -- checked
    /// directly, without going back through a second `StackSwap`, by
    /// pre-arranging the exit sentinel at the top of the new stack so
    /// the guest's own final `rts` (on the new stack) still lands
    /// cleanly on [`EXIT_STUB_ADDR`].
    #[test]
    fn stack_swap_updates_tc_sp_lower_upper_immediately() {
        let _guard = lock_host_break();
        let entry = TRAP_TABLE_END;

        let mut words = Vec::new();
        words.extend_from_slice(&movea_exec_base_to_a6());
        words.push(move_imm_to_a(0)); // A0 = struct_addr (patched below)
        let a0_patch = words.len();
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-732)); // StackSwap
        words.push(0x7000); // moveq #0,d0
        words.push(RTS);

        let code_len = words.len() as u32 * 2;
        let struct_addr = entry + code_len;
        let new_stack_block = struct_addr + 32;
        let new_stack_size = 0x1000u32;
        let new_stack_top = new_stack_block + new_stack_size;
        // Leave margin past new_stack_top before the heap starts: its
        // first allocation (the fake current task struct) would
        // otherwise land exactly at new_stack_top and zero it, wiping
        // out the exit sentinel this test pre-writes there below.
        let load_end = new_stack_top + 0x200;

        words[a0_patch] = (struct_addr >> 16) as u16;
        words[a0_patch + 1] = struct_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        mem.write_u32(struct_addr, new_stack_block); // stk_Lower
        mem.write_u32(struct_addr + 4, new_stack_top); // stk_Upper
        mem.write_u32(struct_addr + 8, new_stack_top); // stk_Pointer
        // The guest's own final `rts` (executed for real by the CPU,
        // not by Runtime::run's synthetic post-handler one) will pop
        // whatever is at the top of the *new* stack once StackSwap's
        // own synthetic rts has run -- pre-arrange the exit sentinel
        // there since this test deliberately never swaps back to the
        // original stack (which is where the sentinel normally lives).
        mem.write_u32(new_stack_top, EXIT_STUB_ADDR);

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
        assert_eq!(code, 0);

        let task = rt.current_task();
        assert_eq!(rt.memory().read_u32(task + TC_SPLOWER), new_stack_block);
        assert_eq!(rt.memory().read_u32(task + TC_SPUPPER), new_stack_top);
    }

    // --- Forbid/Permit ---

    #[test]
    fn forbid_then_permit_is_a_harmless_no_op() {
        let _guard = lock_host_break();
        let mut words = Vec::new();
        words.extend_from_slice(&jsr_disp16_a6(-132)); // Forbid
        words.extend_from_slice(&jsr_disp16_a6(-138)); // Permit
        words.push(0x7000 | 0x2A); // moveq #42,d0
        words.push(RTS);

        let mut rt = exec_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, 42,
            "Forbid/Permit should leave D0 untouched by anyone else"
        );
    }

    // --- OpenDevice ---

    #[test]
    fn open_device_always_fails_with_ioerr_openfail() {
        let _guard = lock_host_break();
        let entry = TRAP_TABLE_END;
        let name = b"some-other.device\0";

        let mut words = Vec::new();
        words.push(move_imm_to_a(0)); // A0 = name (patched below)
        let name_idx = words.len();
        words.push(0);
        words.push(0);
        words.push(0x7000); // moveq #0,d0 (unit)
        words.push(move_imm_to_a(1)); // A1 = ioreq (patched below)
        let ioreq_idx = words.len();
        words.push(0);
        words.push(0);
        words.push(0x7200); // moveq #0,d1 (flags)
        words.extend_from_slice(&jsr_disp16_a6(-444)); // OpenDevice
        words.push(RTS);

        let name_addr = entry + (movea_exec_base_to_a6().len() + words.len()) as u32 * 2;
        let ioreq_addr = (name_addr + name.len() as u32 + 3) & !3;
        words[name_idx] = (name_addr >> 16) as u16;
        words[name_idx + 1] = name_addr as u16;
        words[ioreq_idx] = (ioreq_addr >> 16) as u16;
        words[ioreq_idx + 1] = ioreq_addr as u16;

        let mut full = movea_exec_base_to_a6().to_vec();
        full.extend_from_slice(&words);
        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &full);
        crate::guestmem::write_c_string(&mut mem, name_addr, name);
        for i in 0..32 {
            mem.write_u8(ioreq_addr + i, 0);
        }

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: ioreq_addr + 64,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, IOERR_OPENFAIL as i32);
        assert_eq!(
            rt.memory().read_u8(ioreq_addr + IO_ERROR_OFFSET),
            IOERR_OPENFAIL as u8
        );
    }

    // --- timer.device: OpenDevice/DoIO ---

    /// Builds and runs: `OpenDevice("timer.device", 0, ioreq, 0)`, then
    /// (if `command` is `Some`) sets `io_Command`/`tr_time` on the same
    /// `ioreq` and calls `DoIO(ioreq)`. Returns the exit code (`D0` at
    /// the final `RTS`, i.e. `DoIO`'s `io_Error` if a command was run,
    /// or `OpenDevice`'s own result otherwise) and the `Runtime` for
    /// inspecting `ioreq`'s final contents.
    fn run_open_timer_device_and_do_io(
        command: Option<(u16, u32, u32)>,
    ) -> (i32, Runtime<M68kCpu>, u32) {
        let entry = TRAP_TABLE_END;
        let name = b"timer.device\0";

        let mut words = Vec::new();
        words.push(move_imm_to_a(0)); // A0 = name (patched below)
        let name_idx = words.len();
        words.push(0);
        words.push(0);
        words.push(0x7000); // moveq #0,d0 (unit)
        words.push(move_imm_to_a(1)); // A1 = ioreq (patched below)
        let ioreq_idx = words.len();
        words.push(0);
        words.push(0);
        words.push(0x7200); // moveq #0,d1 (flags)
        words.extend_from_slice(&jsr_disp16_a6(-444)); // OpenDevice
        if command.is_some() {
            words.push(move_imm_to_a(1)); // A1 = ioreq again
            words.push(0); // patched to the same value below
            words.push(0);
            words.extend_from_slice(&jsr_disp16_a6(-456)); // DoIO
        }
        words.push(RTS);

        let name_addr = entry + (movea_exec_base_to_a6().len() + words.len()) as u32 * 2;
        let ioreq_addr = (name_addr + name.len() as u32 + 3) & !3;
        words[name_idx] = (name_addr >> 16) as u16;
        words[name_idx + 1] = name_addr as u16;
        words[ioreq_idx] = (ioreq_addr >> 16) as u16;
        words[ioreq_idx + 1] = ioreq_addr as u16;
        if command.is_some() {
            // ioreq_idx/+1 (OpenDevice's A1 immediate) -> +2 moveq d1 ->
            // +3/+4 OpenDevice's jsr -> +5 DoIO's move_imm_to_a(1)
            // opcode -> +6/+7 its immediate, the one patched here.
            let a1_again_idx = ioreq_idx + 6;
            words[a1_again_idx] = (ioreq_addr >> 16) as u16;
            words[a1_again_idx + 1] = ioreq_addr as u16;
        }

        let mut full = movea_exec_base_to_a6().to_vec();
        full.extend_from_slice(&words);
        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &full);
        crate::guestmem::write_c_string(&mut mem, name_addr, name);
        for i in 0..40 {
            mem.write_u8(ioreq_addr + i, 0);
        }
        if let Some((cmd, secs, micro)) = command {
            mem.write_u16(ioreq_addr + IO_COMMAND_OFFSET, cmd);
            mem.write_u32(ioreq_addr + TR_TIME_SECS_OFFSET, secs);
            mem.write_u32(ioreq_addr + TR_TIME_MICRO_OFFSET, micro);
        }

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: ioreq_addr + 64,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        (code, rt, ioreq_addr)
    }

    #[test]
    fn open_device_timer_device_succeeds() {
        let _guard = lock_host_break();
        let (code, rt, ioreq_addr) = run_open_timer_device_and_do_io(None);
        assert_eq!(code, 0, "OpenDevice(timer.device) should succeed");
        assert_eq!(
            rt.memory().read_u32(ioreq_addr + IO_DEVICE_OFFSET),
            TIMER_DEVICE_BASE
        );
        assert_eq!(rt.memory().read_u8(ioreq_addr + IO_ERROR_OFFSET), 0);
    }

    #[test]
    fn do_io_get_systime_fills_tv_secs_and_tv_micro() {
        let _guard = lock_host_break();
        let (code, rt, ioreq_addr) = run_open_timer_device_and_do_io(Some((TR_GETSYSTIME, 0, 0)));
        assert_eq!(code, 0);
        let secs = rt.memory().read_u32(ioreq_addr + TR_TIME_SECS_OFFSET);
        let micro = rt.memory().read_u32(ioreq_addr + TR_TIME_MICRO_OFFSET);
        // Any real host clock is well past 8000 days since the Amiga
        // epoch (matching crate::dosdate's own "plausibly recent" test).
        assert!(secs > 8_000 * 86_400, "tv_secs should be large: {secs}");
        assert!(
            micro < 1_000_000,
            "tv_micro should be a sub-second value: {micro}"
        );
    }

    #[test]
    fn do_io_add_request_sleeps_for_the_requested_duration() {
        let _guard = lock_host_break();
        // 40ms -- small enough to keep the suite fast, large enough to
        // reliably distinguish "slept" from "didn't" (same tick count
        // reasoning as crate::dosdate's own Delay test).
        let start = std::time::Instant::now();
        let (code, _rt, _ioreq_addr) =
            run_open_timer_device_and_do_io(Some((TR_ADDREQUEST, 0, 40_000)));
        let elapsed = start.elapsed();
        assert_eq!(code, 0);
        assert!(
            elapsed >= std::time::Duration::from_millis(30),
            "TR_ADDREQUEST should have blocked for roughly 40ms, only blocked {elapsed:?}"
        );
    }

    #[test]
    fn do_io_unknown_command_returns_ioerr_nocmd() {
        let _guard = lock_host_break();
        let (code, rt, ioreq_addr) = run_open_timer_device_and_do_io(Some((999, 0, 0)));
        assert_eq!(code, IOERR_NOCMD as i32);
        assert_eq!(
            rt.memory().read_u8(ioreq_addr + IO_ERROR_OFFSET),
            IOERR_NOCMD as u8
        );
    }

    #[test]
    fn run_io_request_unit_level_get_systime() {
        let mut mem = FlatMemory::new(0x1000);
        let ioreq = 0x100u32;
        mem.write_u32(ioreq + IO_DEVICE_OFFSET, TIMER_DEVICE_BASE);
        mem.write_u16(ioreq + IO_COMMAND_OFFSET, TR_GETSYSTIME);
        assert_eq!(run_io_request(&mut mem, ioreq), 0);
        assert!(mem.read_u32(ioreq + TR_TIME_SECS_OFFSET) > 8_000 * 86_400);
    }

    #[test]
    fn run_io_request_unrecognized_device_returns_nocmd() {
        let mut mem = FlatMemory::new(0x1000);
        let ioreq = 0x100u32;
        mem.write_u32(ioreq + IO_DEVICE_OFFSET, 0xDEAD_0000); // not TIMER_DEVICE_BASE
        mem.write_u16(ioreq + IO_COMMAND_OFFSET, TR_GETSYSTIME);
        assert_eq!(run_io_request(&mut mem, ioreq), IOERR_NOCMD);
    }

    #[test]
    fn send_io_wait_io_check_io_abort_io_via_dispatch() {
        // SendIO(ioreq) [TR_GETSYSTIME] then WaitIO(ioreq) -> D0 =
        // io_Error (0); separately CheckIO/AbortIO never block or fail
        // since every request here already completed synchronously.
        let _guard = lock_host_break();
        let entry = TRAP_TABLE_END;
        let name = b"timer.device\0";

        let mut words = Vec::new();
        words.push(move_imm_to_a(0)); // A0 = name (patched)
        let name_idx = words.len();
        words.push(0);
        words.push(0);
        words.push(0x7000); // moveq #0,d0
        words.push(move_imm_to_a(1)); // A1 = ioreq (patched)
        let ioreq_idx1 = words.len();
        words.push(0);
        words.push(0);
        words.push(0x7200); // moveq #0,d1
        words.extend_from_slice(&jsr_disp16_a6(-444)); // OpenDevice
        words.push(move_imm_to_a(1)); // A1 = ioreq again (patched)
        let ioreq_idx2 = words.len();
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-462)); // SendIO
        words.push(move_imm_to_a(1)); // A1 = ioreq again (patched)
        let ioreq_idx3 = words.len();
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-468)); // CheckIO -> D0 = ioreq
        words.push(0x2600); // move.l d0,d3 (stash CheckIO's result)
        words.push(move_imm_to_a(1)); // A1 = ioreq again (patched)
        let ioreq_idx4 = words.len();
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-480)); // AbortIO (no-op)
        words.push(move_imm_to_a(1)); // A1 = ioreq again (patched)
        let ioreq_idx5 = words.len();
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-474)); // WaitIO -> D0 = io_Error
        words.push(RTS);

        let name_addr = entry + (movea_exec_base_to_a6().len() + words.len()) as u32 * 2;
        let ioreq_addr = (name_addr + name.len() as u32 + 3) & !3;
        for idx in [ioreq_idx1, ioreq_idx2, ioreq_idx3, ioreq_idx4, ioreq_idx5] {
            words[idx] = (ioreq_addr >> 16) as u16;
            words[idx + 1] = ioreq_addr as u16;
        }
        words[name_idx] = (name_addr >> 16) as u16;
        words[name_idx + 1] = name_addr as u16;

        let mut full = movea_exec_base_to_a6().to_vec();
        full.extend_from_slice(&words);
        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &full);
        crate::guestmem::write_c_string(&mut mem, name_addr, name);
        for i in 0..40 {
            mem.write_u8(ioreq_addr + i, 0);
        }
        mem.write_u16(ioreq_addr + IO_COMMAND_OFFSET, TR_GETSYSTIME);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: ioreq_addr + 64,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 0, "WaitIO should report io_Error == 0");
    }

    // --- timer.device's library-style vectors (TimerBase idiom) ---

    #[test]
    fn timer_base_idiom_fetches_io_device_and_calls_sub_time() {
        // The full RKRM-documented sequence, byte-for-byte the shape
        // that crashed with the old sentinel io_Device: OpenDevice,
        // then `movea.l (0x14,a0),a6` (A6 = io_Device = "TimerBase"),
        // then `jsr -48(a6)` (SubTime) -- exactly what the real PhxAss
        // assembler executes for its stats line. Uses a borrow-needing
        // input pair so the micro-underflow path is covered too:
        // (10s, 200000us) - (3s, 500000us) = (6s, 700000us).
        let _guard = lock_host_break();
        let entry = TRAP_TABLE_END;
        let name = b"timer.device\0";

        let mut words = Vec::new();
        words.push(move_imm_to_a(0)); // A0 = name (patched)
        let name_idx = words.len();
        words.push(0);
        words.push(0);
        words.push(0x7000); // moveq #0,d0 (unit)
        words.push(move_imm_to_a(1)); // A1 = ioreq (patched)
        let ioreq_idx = words.len();
        words.push(0);
        words.push(0);
        words.push(0x7200); // moveq #0,d1 (flags)
        words.extend_from_slice(&jsr_disp16_a6(-444)); // OpenDevice
        words.push(move_imm_to_a(0)); // A0 = ioreq again (patched)
        let ioreq_idx2 = words.len();
        words.push(0);
        words.push(0);
        words.push(0x2C68); // movea.l (0x14,a0),a6 -- A6 = io_Device
        words.push(0x0014);
        words.push(move_imm_to_a(0)); // A0 = dest timeval (patched)
        let dest_idx = words.len();
        words.push(0);
        words.push(0);
        words.push(move_imm_to_a(1)); // A1 = src timeval (patched)
        let src_idx = words.len();
        words.push(0);
        words.push(0);
        words.push(0x4EAE); // jsr -48(a6) -- SubTime
        words.push(0xFFD0);
        words.push(0x4EAE); // jsr -54(a6) -- CmpTime(dest, src): dest is
        words.push(0xFFCA); // now later than src, so D0 = -1
        words.push(RTS);

        let name_addr = entry + (movea_exec_base_to_a6().len() + words.len()) as u32 * 2;
        let ioreq_addr = (name_addr + name.len() as u32 + 3) & !3;
        let dest_addr = ioreq_addr + 64;
        let src_addr = dest_addr + 8;
        for (idx, value) in [
            (name_idx, name_addr),
            (ioreq_idx, ioreq_addr),
            (ioreq_idx2, ioreq_addr),
            (dest_idx, dest_addr),
            (src_idx, src_addr),
        ] {
            words[idx] = (value >> 16) as u16;
            words[idx + 1] = value as u16;
        }

        let mut full = movea_exec_base_to_a6().to_vec();
        full.extend_from_slice(&words);
        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &full);
        crate::guestmem::write_c_string(&mut mem, name_addr, name);
        for i in 0..40 {
            mem.write_u8(ioreq_addr + i, 0);
        }
        mem.write_u32(dest_addr, 10);
        mem.write_u32(dest_addr + 4, 200_000);
        mem.write_u32(src_addr, 3);
        mem.write_u32(src_addr + 4, 500_000);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: src_addr + 16,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, -1, "CmpTime(later, earlier) should return -1");
        assert_eq!(rt.memory().read_u32(dest_addr), 6, "SubTime tv_secs");
        assert_eq!(
            rt.memory().read_u32(dest_addr + 4),
            700_000,
            "SubTime tv_micro (borrow path)"
        );
    }

    #[test]
    fn add_time_carries_micro_overflow_into_seconds() {
        // AddTime via its real LVO with A6 pointed straight at
        // TIMER_DEVICE_BASE (skipping OpenDevice -- the vector table is
        // populated unconditionally at Runtime::new time):
        // (1s, 600000us) + (2s, 700000us) = (4s, 300000us).
        let _guard = lock_host_break();
        let entry = TRAP_TABLE_END;

        let mut words = Vec::new();
        words.push(move_imm_to_a(6)); // A6 = TIMER_DEVICE_BASE
        words.push((TIMER_DEVICE_BASE >> 16) as u16);
        words.push(TIMER_DEVICE_BASE as u16);
        words.push(move_imm_to_a(0)); // A0 = dest (patched)
        let dest_idx = words.len();
        words.push(0);
        words.push(0);
        words.push(move_imm_to_a(1)); // A1 = src (patched)
        let src_idx = words.len();
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-42)); // AddTime
        words.push(0x7000); // moveq #0,d0
        words.push(RTS);

        let dest_addr = entry + (words.len() as u32) * 2 + 4;
        let src_addr = dest_addr + 8;
        words[dest_idx] = (dest_addr >> 16) as u16;
        words[dest_idx + 1] = dest_addr as u16;
        words[src_idx] = (src_addr >> 16) as u16;
        words[src_idx + 1] = src_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        mem.write_u32(dest_addr, 1);
        mem.write_u32(dest_addr + 4, 600_000);
        mem.write_u32(src_addr, 2);
        mem.write_u32(src_addr + 4, 700_000);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: src_addr + 16,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 0);
        assert_eq!(rt.memory().read_u32(dest_addr), 4, "AddTime tv_secs");
        assert_eq!(
            rt.memory().read_u32(dest_addr + 4),
            300_000,
            "AddTime tv_micro (carry path)"
        );
    }

    #[test]
    fn get_sys_time_and_read_eclock_vectors_report_plausible_values() {
        let _guard = lock_host_break();
        let entry = TRAP_TABLE_END;

        let mut words = Vec::new();
        words.push(move_imm_to_a(6)); // A6 = TIMER_DEVICE_BASE
        words.push((TIMER_DEVICE_BASE >> 16) as u16);
        words.push(TIMER_DEVICE_BASE as u16);
        words.push(move_imm_to_a(0)); // A0 = timeval dest (patched)
        let tv_idx = words.len();
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-66)); // GetSysTime
        words.push(move_imm_to_a(0)); // A0 = EClockVal dest (patched)
        let ev_idx = words.len();
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-60)); // ReadEClock -> D0 = rate
        words.push(RTS);

        let tv_addr = entry + (words.len() as u32) * 2 + 4;
        let ev_addr = tv_addr + 8;
        words[tv_idx] = (tv_addr >> 16) as u16;
        words[tv_idx + 1] = tv_addr as u16;
        words[ev_idx] = (ev_addr >> 16) as u16;
        words[ev_idx + 1] = ev_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: ev_addr + 16,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code as u32, ECLOCK_PAL_HZ,
            "ReadEClock should return the PAL E-Clock rate in D0"
        );
        // Same "plausibly recent" bound as the TR_GETSYSTIME test.
        assert!(rt.memory().read_u32(tv_addr) > 8_000 * 86_400);
        // The E-Clock tick count for any recent time exceeds 32 bits
        // (8000 days * 86400 * 709379 >> 2^32), so ev_hi must be
        // non-zero -- catches an accidental 32-bit truncation.
        assert!(
            rt.memory().read_u32(ev_addr) > 0,
            "ev_hi should be non-zero"
        );
    }

    // --- CloseDevice ---

    #[test]
    fn close_device_after_a_failed_open_is_a_harmless_no_op() {
        let _guard = lock_host_break();
        let mut words = Vec::new();
        words.push(move_imm_to_a(1)); // A1 = 0 (never-opened ioreq)
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-450)); // CloseDevice
        words.push(0x7000 | 0x2A); // moveq #42,d0
        words.push(RTS);

        let mut rt = exec_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 42);
    }
}
