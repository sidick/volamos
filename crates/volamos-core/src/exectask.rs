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
//! [`stack_swap_handler`] sanity-checks `stk_Lower`/`stk_Upper`/
//! `stk_Pointer` before touching anything, and fails loudly right at
//! the call site on a corrupted or never-initialized
//! `StackSwapStruct` -- found worth adding after a separate tool built
//! on this runtime hit exactly that confusion: a bad struct otherwise
//! runs on silently (A7 lands on garbage, the generic post-dispatch
//! `rts` pops whatever happens to be there) until the failure surfaces
//! as something far removed from this call site, most often
//! [`crate::dispatch::DispatchError::EmptyContinuationStack`].
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
//! `tc_Node.ln_Type` is set to [`NT_PROCESS`] -- the current task is a
//! CLI process, and startup code such as DICE's checks for that before
//! reading the `pr_*` fields -- and `tc_Node.ln_Name` points to a
//! heap-allocated, NUL-terminated process name string -- both set once
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
//! process; `volamos` maps the host equivalent (an actual Ctrl-C/SIGINT
//! at the host terminal) onto the same guest signal.
//! [`install_host_break_handler`] installs minimal host signal handlers
//! -- gated `#[cfg(unix)]`, a no-op on any other platform -- that are
//! *not* installed automatically by [`crate::dispatch::Runtime::new`]
//! (that would hijack the test runner's own `SIGINT` handling for every
//! unit test in this crate), only by an explicit call, which
//! `crates/volamos/src/main.rs` makes once at CLI startup.
//!
//! The SIGINT handler does nothing but set an atomic flag
//! ([`PENDING_HOST_BREAK`]). [`fold_pending_host_break`] is the only
//! thing that ever consults it: it atomically takes-and-clears it, and
//! if it was set, ORs [`SIGBREAKF_CTRL_C`] into `tc_SigRecvd`.
//! [`crate::dispatch::Runtime::run`] calls it once per dispatched
//! library call (right before building the
//! [`crate::dispatch::HandlerContext`] for the trapped call), and
//! [`wait_handler`]/[`check_signal_handler`] each also call it
//! themselves before consulting `tc_SigRecvd`, so a `Wait`/`CheckSignal`
//! invoked directly (bypassing the run loop, as this module's own tests
//! do) still sees a break that was set on the flag directly.
//!
//! ## Polling granularity, and forced termination
//!
//! Since the flag is only ever folded in at a dispatched library-call
//! boundary, a guest program running a tight, call-free compute loop
//! (no library calls at all) will not observe a host break until its
//! *next* library call, however long that takes -- there is no
//! instruction-level or timer-driven check. Real hardware interrupts
//! preempt at any instruction; this runtime only checks at
//! trap-dispatch time.
//!
//! That polling gap means a guest that never makes a library call can
//! never be asked to break cooperatively, so two signals bypass the
//! flag entirely and terminate the host process immediately via `_exit`
//! (async-signal-safe, unlike [`std::process::exit`], and consistent
//! with what an already-hung guest gets from `SIGKILL` today -- no
//! guest or host cleanup runs either way):
//!
//! - `SIGTERM` always does this on receipt. It means "terminate", not
//!   "ask nicely" -- there is no cooperative delivery for it at all.
//! - A *second* `SIGINT` does this if it arrives while a first one is
//!   still pending (i.e. [`PENDING_HOST_BREAK`] is already set, meaning
//!   it hasn't been folded into any task yet) -- the double-Ctrl-C
//!   "I mean it" convention most interpreters use. A single `SIGINT`
//!   still only requests a cooperative break, as before.
//!
//! Both exit with the shell's usual `128 + signum` code for a
//! signal-terminated process (143 for `SIGTERM`, 130 for a forced
//! `SIGINT`).

use std::sync::atomic::{AtomicBool, Ordering};

use crate::cpu::{AddressRegister, Cpu, DataRegister};
use crate::dispatch::{
    DOS_LIBRARY_BASE, DispatchError, EXEC_LIBRARY_BASE, HandlerContext, LibraryTable,
    TIMER_DEVICE_BASE,
};
use crate::execlist::{LN_NAME, LN_TYPE, init_msg_port_fields};
use crate::guestmem::{GuestHeap, bptr_from_addr, write_bstr, write_c_string};
use crate::lvos::dos::DOS_LVOS;
use crate::lvos::exec::EXEC_LVOS;
use crate::memory::AddressSpace;
use crate::taskswitch::TaskScheduler;

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
/// `tc_Switch`: `APTR` (a function pointer), offset 66 -- immediately
/// after `tc_SPUpper`. Called on the task *losing* the CPU at a switch
/// (issue #237), if [`TF_SWITCH`] is set in `tc_Flags`. Never written
/// by this module -- only read, by `crate::dispatch`'s switch
/// machinery.
pub const TC_SWITCH_OFFSET: u32 = 66;
/// `tc_Launch`: `APTR`, offset 70 -- immediately after `tc_Switch`.
/// Called on the task *getting* the CPU at a switch, if [`TF_LAUNCH`]
/// is set. Same read-only treatment as `tc_Switch`.
pub const TC_LAUNCH_OFFSET: u32 = 70;

/// `TF_SWITCH` (bit 6, `0x40`) in `tc_Flags`, per `<exec/tasks.h>`:
/// "call `tc_Switch` when this task loses the CPU." Real thread-local
/// storage and newlib's per-task `errno` rely on this and
/// [`TF_LAUNCH`] (issue #237) -- a task switch that doesn't honor them
/// leaves a thread's `tc_Switch`/`tc_Launch` never called, so whatever
/// global pointer they update (e.g. the active task's TLS block) keeps
/// pointing at the previous task's.
pub const TF_SWITCH: u8 = 1 << 6;
/// `TF_LAUNCH` (bit 7, `0x80`): "call `tc_Launch` when this task gets
/// the CPU." See [`TF_SWITCH`]'s doc.
pub const TF_LAUNCH: u8 = 1 << 7;

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
/// `pr_CIS`: `BPTR`, offset 156 (`pr_CLI` 172 - `pr_FileSystemTask` 4 -
/// `pr_ConsoleTask` 4 - `pr_COS` 4 - `pr_CIS` 4). The process's current
/// input handle, the same one `Input()` returns: startup code that reads
/// it directly (DICE's) instead of calling `Input()` gets no stdin when
/// it is `0`. Written at creation and by `SelectInput`.
pub const PR_CIS_OFFSET: u32 = 156;
/// `pr_COS`: `BPTR`, offset 160. As [`PR_CIS_OFFSET`], for `Output()` and
/// `SelectOutput`.
pub const PR_COS_OFFSET: u32 = 160;
/// `pr_ConsoleTask`: `APTR`, offset 164, the console handler's port, which
/// `Open("*")` reaches. Set to the port the standard handles' `fh_Type`
/// already names: startup code that opens its own stderr (DICE's) only
/// does so when this is non-`NULL`.
pub const PR_CONSOLE_TASK_OFFSET: u32 = 164;
/// `pr_ReturnAddr`: `APTR`, offset 176 (`pr_CLI` 172 + 4). Per the
/// `RunCommand`/`CreateNewProc` convention real AmigaOS uses (and
/// `ixemul.library`'s startup code, `ix_init.c`, relies on): it points
/// at a longword holding the process's reserved stack size in bytes,
/// which sits **above** the return address the process's outermost
/// `rts` lands on -- that return address is at `pr_ReturnAddr - 4`,
/// which is also the process's initial `sp`:
///
/// ```text
/// pr_ReturnAddr - 4   [return address]            <- sp at entry
/// pr_ReturnAddr       [reserved stack size]
/// pr_ReturnAddr + 4   [caller's stack pointer]
/// ```
///
/// Readers derive the process's stack bounds from it two ways:
/// `lower = pr_ReturnAddr - size` exactly (libnix's `__init_stk`,
/// `sources/stack/init_stk.c`), or `upper = pr_ReturnAddr + 8`,
/// `lower = upper - size` -- 8 bytes conservative, as on real AmigaOS,
/// where those two longwords above the size are part of the stack
/// allocation too. Left at `0`, ixemul's `get_stack_size()`/
/// stack-extension code computed both bounds from address `0` instead
/// and its `CopyMem` of the live frame onto the "extended" stack
/// overwrote the real one.
///
/// The ordering is load-bearing in the other direction too: vbcc's
/// resident startup (`minres.o`) stashes its data block at entry
/// `sp - 4` and reads it back from `pr_ReturnAddr - 8` on exit (and
/// `_geta4` finds it the same way), so the inverted layout this
/// runtime built before issue #279 -- size below the return address --
/// made it `FreeMem` an address 8 bytes off. AROS's m68k
/// `AOS_CallEntry` (`arch/m68k-all/dos/callentry.S`) builds the frame
/// documented above; the same program runs clean on real AmigaOS 3.x
/// under MuForce.
///
/// Written once, at creation, by [`crate::dispatch::Runtime::new`]
/// (not this function -- it's part of the stack region's own layout,
/// not the fake task struct's static fields, so it's naturally set
/// alongside the exit-stub write, see
/// [`crate::dispatch::EXIT_STUB_ADDR`]). Found needed running a real
/// `ixemul.library` 80.1-linked program (issue #178); frame order
/// fixed for vbcc `minres.o` (issue #279).
pub const PR_RETURNADDR_OFFSET: u32 = 176;
/// `pr_HomeDir`: `BPTR`, offset 188 (`pr_CLI` 172 + 4, `pr_ReturnAddr`
/// 176, `pr_PktWait` 180, `pr_WindowPtr` 184, `pr_HomeDir` 188). A lock
/// on the directory the running program was loaded from, or `0` if
/// none exists (resident/ROM commands, shell built-ins) -- used to
/// resolve the `PROGDIR:` pseudo-assign and returned by
/// `GetProgramDir()`/set by `SetProgramDir()` (RKRM `processes.md`).
/// Left `0` by [`create_current_task`] itself (set later, once a `Vfs`
/// exists to resolve the launched binary's own host directory back to
/// an Amiga path -- see [`crate::dispatch::Runtime::set_program_dir`],
/// called once from `crates/volamos/src/main.rs` after `set_vfs`).
/// Found needed running the real SAS/C `sc` compiler (issue #17).
pub const PR_HOMEDIR_OFFSET: u32 = 188;
/// `pr_WindowPtr`: `APTR`, offset 184 (`pr_PktWait` 180 + 4). The window
/// DOS puts its requesters on; `-1` means none.
pub const PR_WINDOW_PTR_OFFSET: u32 = 184;
/// `pr_Flags`: `LONG`, offset 192 (`pr_HomeDir` 188 + 4). DOS's own flags
/// for the process: the `PRF_CLOSE*` bits say which standard handles it
/// closes when the process ends.
pub const PR_FLAGS_OFFSET: u32 = 192;
/// `pr_CES`: `BPTR`, offset 224 (`pr_LocalVars`, a 12-byte `struct
/// MinList`, at 208, then `pr_ShellPrivate` 220). The error stream, or
/// `0` to use `pr_COS`.
pub const PR_CES_OFFSET: u32 = 224;
/// `PRF_CLOSEINPUT`, per `<dos/dosextens.h>`: close `pr_CIS` at exit.
pub const PRF_CLOSEINPUT: u32 = 1 << 3;
/// `PRF_CLOSEOUTPUT`: close `pr_COS` at exit.
pub const PRF_CLOSEOUTPUT: u32 = 1 << 4;
/// `PRF_CLOSEERROR`: close `pr_CES` at exit.
pub const PRF_CLOSEERROR: u32 = 1 << 6;
/// `pr_ExitCode`: `APTR`, offset 196 (`pr_Flags` 192 + 4): code DOS calls
/// when the process ends (`NP_ExitCode`), with the process's return code
/// in `D0` and `pr_ExitData` in `D1`.
pub const PR_EXITCODE_OFFSET: u32 = 196;
/// `pr_ExitData`: `ULONG`, offset 200: the `NP_ExitData` argument for
/// [`PR_EXITCODE_OFFSET`]'s routine.
pub const PR_EXITDATA_OFFSET: u32 = 200;
/// `cli_ReturnCode`: `LONG`, offset 12 within `struct
/// CommandLineInterface` (`cli_Result2` 0, `cli_SetName` 4,
/// `cli_CommandDir` 8): the return code of the last command the CLI ran.
/// A process with its own CLI has it set to its return code as it ends.
pub(crate) const CLI_RETURN_CODE_OFFSET: u32 = 12;
/// `pr_Arguments`: `STRPTR`, offset 204 (`pr_HomeDir` 188 + 4, `pr_Flags`
/// 192, `pr_ExitCode` 196, `pr_ExitData` 200, `pr_Arguments` 204). Per
/// `<dos/dosextens.h>`: "Arguments passed to the process at start".
/// This runtime already builds the exact same command-line buffer for
/// `A0`/`D0` at process entry (see `crate::dispatch::Runtime::new`) --
/// `pr_Arguments` just needs to point at it too, since a real
/// AmigaOS process (via `RunCommand`/`CreateNewProc`) gets both: A0/D0
/// are the entry-time convention a `crt0` reads once at startup, but
/// `pr_Arguments` is the same string surviving in the process struct
/// for any code to re-read later (e.g. after `A0`/`D0` have long since
/// been clobbered). Found running the real `sidick/micropython`
/// Amiga port (its own startup reads argv from `pr_Arguments`, not
/// A0/D0): left at 0/NULL, it always decided "no arguments" and fell
/// back to its REPL even when a script path was passed, regardless of
/// A0/D0 being correct -- confirmed by cross-checking against vamos,
/// which does set this field (`amitools`' `Process.init_args()`).
pub const PR_ARGUMENTS_OFFSET: u32 = 204;
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
pub(crate) const CLI_STRUCT_SIZE: u32 = 64;
/// `cli_SetName`: `BSTR`, offset 4 within `struct CommandLineInterface`
/// (after `cli_Result2`): the buffer holding the current directory's
/// name. Written by `crate::dosfile`'s `AllocDosObject(DOS_CLI)`, along
/// with the other three buffers below.
pub(crate) const CLI_SET_NAME_OFFSET: u32 = 4;
/// `cli_FailLevel`: `LONG`, offset 20 (`cli_CommandName` 16 + 4).
pub(crate) const CLI_FAIL_LEVEL_OFFSET: u32 = 20;
/// `cli_Prompt`: `BSTR`, offset 24 (`cli_FailLevel` 20 + 4).
pub(crate) const CLI_PROMPT_OFFSET: u32 = 24;
/// `cli_CommandFile`: `BSTR`, offset 36 (`cli_StandardInput` 28,
/// `cli_CurrentInput` 32, then this).
pub(crate) const CLI_COMMAND_FILE_OFFSET: u32 = 36;
/// `cli_Background`: `LONG`, offset 44 (`cli_CommandFile` 36,
/// `cli_Interactive` 40, then this).
pub(crate) const CLI_BACKGROUND_OFFSET: u32 = 44;
/// `cli_CommandName`: `BSTR`, offset 16 within `struct
/// CommandLineInterface` (`cli_Result2`/`cli_SetName`/`cli_CommandDir`/
/// `cli_ReturnCode` 4 each = 16, `cli_CommandName` is the 5th field).
/// Real `dos.library`'s `GetProgramName()` reads exactly this field
/// (see [`crate::dosfile::get_program_name_handler`]) -- found needed
/// running the real `AmiSnap` binary.
pub(crate) const CLI_COMMAND_NAME_OFFSET: u32 = 16;
/// `cli_StandardInput`: `BPTR`, offset 28 within `struct
/// CommandLineInterface` (`cli_Result2`/`cli_SetName`/`cli_CommandDir`/
/// `cli_ReturnCode`/`cli_CommandName`/`cli_FailLevel`/`cli_Prompt` 4
/// each = 28, `cli_StandardInput` is the 8th field). Real programs
/// (e.g. `C:Search`) read this directly -- not via `Input()` -- to
/// decide whether their input has been redirected, by comparing it
/// against `Input()`'s own return value and `IsInteractive()`. Found
/// via disassembling the real `Search` binary: leaving this at 0
/// (unset) makes it look, to any guest program that checks, as if
/// input was redirected from a non-interactive, non-`Input()` source
/// even on a plain unredirected CLI invocation.
pub(crate) const CLI_STANDARD_INPUT_OFFSET: u32 = 28;
/// `cli_DefaultStack`: `LONG`, offset 52 within `struct
/// CommandLineInterface` (`cli_StandardInput` 28, `cli_CurrentInput` 4,
/// `cli_CommandFile` 4, `cli_Interactive` 4, `cli_Background` 4,
/// `cli_CurrentOutput` 4, summing to 48; `cli_DefaultStack` is the 14th
/// field). Per `<dos/dosextens.h>`: "stack size to use, in longwords";
/// real `NewCLI`/`RunCommand` fill it from the `Stack` shell command,
/// dividing bytes by 4. `ixemul.library`'s startup code
/// (`get_stack_size()` in `library/stackextend.c`) compares a
/// program's own `__stack` against `cli_DefaultStack * 4` to decide
/// whether to extend the stack -- left at 0 (unset), every program
/// looks stack-starved and ixemul always extends, using
/// [`PR_RETURNADDR_OFFSET`]'s (also-then-unset) bounds to decide where
/// the "current" frame is, with disastrous results (see that offset's
/// doc). Found needed running a real `ixemul.library` 80.1-linked
/// program (issue #178).
pub(crate) const CLI_DEFAULT_STACK_OFFSET: u32 = 52;

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
/// `NT_PROCESS` (13), per `<exec/nodes.h>`: the current task's type.
pub const NT_PROCESS: u8 = 13;

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
    /// The POSIX `_exit(2)` function: terminates the process immediately,
    /// without running destructors, `atexit` hooks, or anything else --
    /// unlike [`std::process::exit`], this is async-signal-safe and so
    /// the only thing this module calls from within a signal handler.
    fn _exit(status: i32) -> !;
}

/// `SIGINT`, per POSIX (`<signal.h>`) -- the same numeric value on every
/// unix this project targets (Linux, macOS).
#[cfg(unix)]
const SIGINT: i32 = 2;
/// `SIGTERM`, per POSIX.
#[cfg(unix)]
const SIGTERM: i32 = 15;

/// The shell's usual exit code for a process terminated by `signum`
/// (`128 + signum`), used for this module's own forced-`_exit` paths --
/// see the module docs' "Polling granularity, and forced termination"
/// section.
#[cfg(unix)]
const fn signal_exit_code(signum: i32) -> i32 {
    128 + signum
}

/// The `SIGINT` handler: sets [`PENDING_HOST_BREAK`] so the next
/// dispatched library call delivers `SIGBREAKF_CTRL_C` to the guest --
/// unless it was *already* set (a prior `SIGINT` the guest hasn't
/// observed yet), in which case this is a second Ctrl-C and the guest
/// isn't cooperating, so terminate immediately instead. Both the atomic
/// swap and `_exit` are async-signal-safe; nothing else runs here.
#[cfg(unix)]
extern "C" fn handle_host_break(_signum: i32) {
    if PENDING_HOST_BREAK.swap(true, Ordering::SeqCst) {
        unsafe { _exit(signal_exit_code(SIGINT)) };
    }
}

/// The `SIGTERM` handler: `SIGTERM` means "terminate", not "ask the
/// guest nicely" -- there is no cooperative delivery for it, so this
/// terminates the process immediately on every delivery. See the module
/// docs' "Polling granularity, and forced termination" section.
#[cfg(unix)]
extern "C" fn handle_host_term(_signum: i32) {
    unsafe { _exit(signal_exit_code(SIGTERM)) };
}

/// Installs the host `SIGINT`/`SIGTERM` handlers (see the module docs'
/// "Host `SIGINT`/`SIGTERM`" section). A no-op on any non-`unix` target.
///
/// Deliberately **not** called from [`crate::dispatch::Runtime::new`] --
/// installing process-wide signal handlers as a side effect of
/// constructing a `Runtime` would hijack the test runner's own `SIGINT`
/// handling for every unit test in this crate (including this module's
/// own). Callers that actually want host-break delivery (i.e.
/// `crates/volamos/src/main.rs`) call this explicitly, once, at startup.
pub fn install_host_break_handler() {
    #[cfg(unix)]
    unsafe {
        signal(SIGINT, handle_host_break);
        signal(SIGTERM, handle_host_term);
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
///
/// `input_addr`/`output_addr` are the guest addresses of the handles
/// `Input()`/`Output()` return, stored in `pr_CIS`/`pr_COS` as BPTRs.
///
/// `return_addr_footer` is the guest address of the reserved-stack-size
/// longword in the entry frame [`crate::dispatch::Runtime::new`]
/// already wrote at the top of the stack region, one longword above
/// the initial `sp` (per [`PR_RETURNADDR_OFFSET`]'s doc), written here
/// into `pr_ReturnAddr` unchanged; `cli_DefaultStack` (see
/// [`CLI_DEFAULT_STACK_OFFSET`]) is derived from the same `[sp_lower,
/// sp_upper)` region as `tc_SPLower`/`tc_SPUpper` below, so both
/// fields describe one consistent reserved-stack size.
#[allow(clippy::too_many_arguments)] // internal setup helper; one param per real struct field it fills
pub fn create_current_task<M: AddressSpace>(
    mem: &mut M,
    heap: &mut GuestHeap,
    sp_lower: u32,
    sp_upper: u32,
    program_name: &str,
    input_addr: u32,
    output_addr: u32,
    return_addr_footer: u32,
) -> u32 {
    let task = heap
        .alloc(&mut *mem, PROCESS_STRUCT_SIZE)
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

    // tc_Node: ln_Type = NT_PROCESS, ln_Name -> a heap-allocated process
    // name string. ln_Succ/ln_Pred/ln_Pri stay 0 -- this fake task is
    // never linked onto any exec task list (there is no such list
    // here).
    mem.write_u8(task + LN_TYPE, NT_PROCESS);
    let name_addr = heap
        .alloc(&mut *mem, PROCESS_NAME.len() as u32 + 1)
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

    // pr_ReturnAddr: see PR_RETURNADDR_OFFSET's doc and this
    // function's own doc for `return_addr_footer`.
    mem.write_u32(task + PR_RETURNADDR_OFFSET, return_addr_footer);

    // pr_MsgPort: a real, valid (if perpetually empty) MsgPort owned by
    // this task -- see PR_MSGPORT_OFFSET's doc.
    init_msg_port_fields(mem, task + PR_MSGPORT_OFFSET, task);

    // pr_CIS/pr_COS: the same handles Input()/Output() return;
    // pr_ConsoleTask: the handler port those handles name.
    mem.write_u32(task + PR_CIS_OFFSET, bptr_from_addr(input_addr));
    mem.write_u32(task + PR_COS_OFFSET, bptr_from_addr(output_addr));
    let console_port = mem.read_u32(output_addr + crate::dosfile::FH_TYPE_OFFSET);
    mem.write_u32(task + PR_CONSOLE_TASK_OFFSET, console_port);

    // pr_CLI: a real, heap-allocated (if otherwise empty) struct
    // CommandLineInterface, written as a BPTR -- see PR_CLI_OFFSET's doc
    // for why this must be non-NULL.
    let cli_addr = heap
        .alloc(&mut *mem, CLI_STRUCT_SIZE)
        .expect("guest heap has room for the fake CLI struct");
    for i in 0..CLI_STRUCT_SIZE {
        mem.write_u8(cli_addr.wrapping_add(i), 0);
    }
    mem.write_u32(task + PR_CLI_OFFSET, bptr_from_addr(cli_addr));

    // cli_CommandName: a real BSTR with the guest program's own name
    // (an empty BSTR if the caller didn't supply one -- see
    // CLI_COMMAND_NAME_OFFSET's doc), so GetProgramName() has real
    // data to read instead of NUL/zero bytes.
    let name_bytes = program_name.as_bytes();
    let cli_command_name_addr = heap
        .alloc(&mut *mem, 1 + (name_bytes.len().min(255) as u32))
        .expect("guest heap has room for the fake cli_CommandName BSTR");
    write_bstr(mem, cli_command_name_addr, name_bytes);
    mem.write_u32(
        cli_addr + CLI_COMMAND_NAME_OFFSET,
        bptr_from_addr(cli_command_name_addr),
    );

    // cli_StandardInput: the same handle this process's own Input()
    // returns -- see CLI_STANDARD_INPUT_OFFSET's doc.
    mem.write_u32(
        cli_addr + CLI_STANDARD_INPUT_OFFSET,
        bptr_from_addr(input_addr),
    );

    // cli_DefaultStack: this run's reserved stack size, in longwords
    // -- see CLI_DEFAULT_STACK_OFFSET's doc.
    mem.write_u32(
        cli_addr + CLI_DEFAULT_STACK_OFFSET,
        (sp_upper - sp_lower) / 4,
    );

    task
}

/// `TAG_USER`, per `<utility/tagitem.h>`.
const TAG_USER: u32 = 0x8000_0000;
/// `NP_Dummy`, per `<dos/dostags.h>`: `TAG_USER + 1000`. Every other
/// `NP_*` tag is defined relative to this.
const NP_DUMMY: u32 = TAG_USER + 1000;
/// `NP_Entry` (`NP_Dummy + 3`): the child task's entry point. Required
/// -- see [`create_new_proc_handler`]'s doc for why `NP_Seglist`
/// (the other, mutually exclusive way real `CreateNewProc` accepts)
/// isn't supported.
const NP_ENTRY: u32 = NP_DUMMY + 3;
/// `NP_Input` (`NP_Dummy + 4`): `pr_CIS`, stored as given. Default: a
/// new `NIL:` handle.
const NP_INPUT: u32 = NP_DUMMY + 4;
/// `NP_Output` (`NP_Dummy + 5`): `pr_COS`, as `NP_Input`.
const NP_OUTPUT: u32 = NP_DUMMY + 5;
/// `NP_CloseInput` (`NP_Dummy + 6`): close `pr_CIS` at exit. Default
/// TRUE.
const NP_CLOSEINPUT: u32 = NP_DUMMY + 6;
/// `NP_CloseOutput` (`NP_Dummy + 7`): close `pr_COS` at exit. Default
/// TRUE.
const NP_CLOSEOUTPUT: u32 = NP_DUMMY + 7;
/// `NP_Error` (`NP_Dummy + 8`): `pr_CES`. Default `0`.
const NP_ERROR: u32 = NP_DUMMY + 8;
/// `NP_CloseError` (`NP_Dummy + 9`): close `pr_CES` at exit. Default
/// FALSE.
const NP_CLOSEERROR: u32 = NP_DUMMY + 9;
/// `NP_StackSize` (`NP_Dummy + 11`): stack size in bytes.
const NP_STACKSIZE: u32 = NP_DUMMY + 11;
/// `NP_Name` (`NP_Dummy + 12`): process name, a plain NUL-terminated C
/// string per the autodoc (not a `BSTR`).
const NP_NAME: u32 = NP_DUMMY + 12;
/// `NP_Priority` (`NP_Dummy + 13`).
const NP_PRIORITY: u32 = NP_DUMMY + 13;
/// `NP_ConsoleTask` (`NP_Dummy + 14`): `pr_ConsoleTask`. Default: the
/// parent's.
const NP_CONSOLETASK: u32 = NP_DUMMY + 14;
/// `NP_WindowPtr` (`NP_Dummy + 15`): `pr_WindowPtr`. Default: the
/// parent's.
const NP_WINDOWPTR: u32 = NP_DUMMY + 15;
/// `NP_ExitCode` (`NP_Dummy + 24`): code to call as the process ends.
pub(crate) const NP_EXITCODE: u32 = NP_DUMMY + 24;
/// `NP_ExitData` (`NP_Dummy + 25`): its argument, passed in `D1`.
pub(crate) const NP_EXITDATA: u32 = NP_DUMMY + 25;
/// `NP_Arguments` (`NP_Dummy + 21`): a C string, copied, that the entry
/// point gets in `A0` with its length in `D0`, and `pr_Arguments`.
const NP_ARGUMENTS: u32 = NP_DUMMY + 21;

/// Real `CreateNewProc`'s documented default `NP_StackSize`: 4000
/// bytes.
const DEFAULT_NP_STACKSIZE: u32 = 4000;
/// Real `CreateNewProc`'s documented default `NP_Name`.
const DEFAULT_NP_NAME: &str = "New Process";

/// Builds a minimal `struct Process`/`struct Task` for a
/// `dos.library/CreateNewProc` child task (issue #121) -- the same
/// struct-`Process` layout [`create_current_task`] builds for the main
/// task, but *not* its CLI-process fields: `pr_CLI` stays `NULL`.
/// [`create_new_proc_handler`] fills in the standard handles,
/// `pr_ConsoleTask` and `pr_WindowPtr` from its tags.
///
/// Returns `None` (rather than panicking, unlike [`create_current_task`]
/// -- which only ever runs once, at [`crate::dispatch::Runtime::new`]
/// time, when the heap is essentially empty) if the guest heap has no
/// room: `CreateNewProc` can be called at any point in a long-running
/// guest program, so running out of heap here is a real, expected
/// failure mode its caller needs to turn into a `NULL` result, not a
/// host panic.
fn create_task<M: AddressSpace>(
    mem: &mut M,
    heap: &mut GuestHeap,
    name: &str,
    priority: i8,
    sp_lower: u32,
    sp_upper: u32,
) -> Option<u32> {
    let task = heap.alloc(mem, PROCESS_STRUCT_SIZE).ok()?;
    for i in 0..PROCESS_STRUCT_SIZE {
        mem.write_u8(task.wrapping_add(i), 0);
    }
    mem.write_u8(task + LN_TYPE, NT_PROCESS);
    mem.write_u8(task + crate::execlist::LN_PRI, priority as u8);

    let name_bytes = name.as_bytes();
    let name_addr = heap.alloc(mem, name_bytes.len() as u32 + 1).ok()?;
    write_c_string(mem, name_addr, name_bytes);
    mem.write_u32(task + LN_NAME, name_addr);

    mem.write_u32(task + TC_SIGALLOC, SIG_SYSTEM_RESERVED);
    mem.write_u32(task + TC_SPLOWER, sp_lower);
    mem.write_u32(task + TC_SPUPPER, sp_upper);
    init_msg_port_fields(mem, task + PR_MSGPORT_OFFSET, task);

    Some(task)
}

/// The `ti_Data` of tag `id` in the tag list at `tags`, if it is there.
pub(crate) fn tag_data<M: AddressSpace>(mem: &M, tags: u32, id: u32) -> Option<u32> {
    let item = crate::utility::find_tag_item_impl(mem, id, tags);
    (item != 0).then(|| mem.read_u32(item + 4))
}

/// A new `NIL:` handle's `BPTR` for a child's standard handle, or `0`.
fn open_nil<C: Cpu>(ctx: &mut HandlerContext<'_, C>, mode: i32) -> u32 {
    ctx.dos.open(ctx.heap, ctx.mem, "NIL:", mode).unwrap_or(0)
}

/// `dos.library`'s `CreateNewProc` (LVO -498: `D1` = tag list). `D0` =
/// the new `struct Process*`, or `NULL` on failure -- see issue #121
/// and `crate::taskswitch`'s module doc for the scheduling this plugs
/// a new task into.
///
/// Scope: `NP_Entry` (required), `NP_StackSize`, `NP_Name`,
/// `NP_Priority`, the standard handles (`NP_Input`/`NP_Output`/
/// `NP_Error` and their `NP_Close*` flags, which [`task_exit_stub_handler`]
/// acts on), `NP_ConsoleTask`, `NP_WindowPtr`, `NP_Arguments` and
/// `NP_ExitCode`/`NP_ExitData` (see [`task_exit_stub_handler`]), with
/// the defaults `<dos/dostags.h>` documents. The handles are stored as
/// given, unchecked, as real DOS does: libpthread passes a thread's
/// slot number in `NP_Input` and reads it back from `pr_CIS`.
/// `NP_CurrentDir`/`NP_HomeDir` are not read, since this runtime has
/// one current directory for the whole run, not one per process.
/// `NP_Seglist` -- the other, mutually
/// exclusive way real `CreateNewProc` accepts what to run (a seglist,
/// as from `LoadSeg`, rather than a bare function pointer) -- isn't
/// supported; every known caller that actually needs this runtime's
/// `CreateNewProc` support (both the libpthread and native toolchain
/// thread models, per issue #121's thread) always supplies `NP_Entry`.
/// The remaining tags (`NP_Cli`/`NP_Path`/`NP_CopyVars`/...) are
/// ignored.
///
/// The child's initial registers: `A7` = the top of its own freshly
/// allocated stack (with [`crate::dispatch::TASK_EXIT_STUB_ADDR`]
/// pre-pushed as the return address for its outermost `rts`, exactly
/// the way [`crate::dispatch::Runtime::new`] pre-pushes `EXIT_STUB_ADDR`
/// for the main task's own outermost `rts`), `PC` = `NP_Entry`, `A0`/`D0`
/// = the `NP_Arguments` copy and its length (`0`/`0` without one), every
/// other register zeroed.
fn create_new_proc_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let tags = ctx.cpu.data_register(DataRegister(1));

    let entry_item = crate::utility::find_tag_item_impl(ctx.mem, NP_ENTRY, tags);
    if entry_item == 0 {
        return Err(DispatchError::HandlerFailed {
            library: "dos.library".to_string(),
            lvo: -498,
            handler_name: "CreateNewProc".to_string(),
            message: "NP_Entry is required -- NP_Seglist (the other, mutually exclusive way \
                      to specify what to run) is not supported"
                .to_string(),
        });
    }
    let entry = ctx.mem.read_u32(entry_item + 4);

    let stack_size = tag_data(ctx.mem, tags, NP_STACKSIZE).unwrap_or(DEFAULT_NP_STACKSIZE);

    let name = match tag_data(ctx.mem, tags, NP_NAME) {
        Some(ptr) => {
            String::from_utf8_lossy(&crate::guestmem::read_c_string(ctx.mem, ptr)).into_owned()
        }
        None => DEFAULT_NP_NAME.to_string(),
    };

    let priority = match tag_data(ctx.mem, tags, NP_PRIORITY) {
        Some(pri) => pri as i8,
        None => ctx.mem.read_u8(ctx.current_task + crate::execlist::LN_PRI) as i8,
    };

    // NP_Arguments: copied, as real DOS does, and then the caller's own
    // string may go away.
    let args = match tag_data(ctx.mem, tags, NP_ARGUMENTS) {
        Some(ptr) if ptr != 0 => Some(crate::guestmem::read_c_string(ctx.mem, ptr)),
        _ => None,
    };

    let mut flags = 0;
    if tag_data(ctx.mem, tags, NP_CLOSEINPUT).is_none_or(|v| v != 0) {
        flags |= PRF_CLOSEINPUT;
    }
    if tag_data(ctx.mem, tags, NP_CLOSEOUTPUT).is_none_or(|v| v != 0) {
        flags |= PRF_CLOSEOUTPUT;
    }
    if tag_data(ctx.mem, tags, NP_CLOSEERROR).is_some_and(|v| v != 0) {
        flags |= PRF_CLOSEERROR;
    }

    let spec = SpawnSpec {
        entry,
        stack_size,
        name,
        priority,
        args,
        input: tag_data(ctx.mem, tags, NP_INPUT),
        output: tag_data(ctx.mem, tags, NP_OUTPUT),
        error: tag_data(ctx.mem, tags, NP_ERROR).unwrap_or(0),
        flags,
        console: tag_data(ctx.mem, tags, NP_CONSOLETASK),
        window: tag_data(ctx.mem, tags, NP_WINDOWPTR),
        exit_code: tag_data(ctx.mem, tags, NP_EXITCODE).unwrap_or(0),
        exit_data: tag_data(ctx.mem, tags, NP_EXITDATA).unwrap_or(0),
        cli_command_name: None,
        seglist: 0,
    };
    let task = spawn_process(ctx, spec).unwrap_or(0);
    ctx.cpu.set_data_register(DataRegister(0), task);
    Ok(())
}

/// What [`spawn_process`] needs to start a process: everything
/// `CreateNewProc`'s tags can say, resolved to values, plus what only
/// `SystemTagList` needs.
pub(crate) struct SpawnSpec {
    pub entry: u32,
    pub stack_size: u32,
    pub name: String,
    pub priority: i8,
    /// `NP_Arguments`: the bytes DOS copies and hands the entry in
    /// `A0`/`D0`. `None` means none (`A0`/`D0` = 0).
    pub args: Option<Vec<u8>>,
    /// The standard handles, as BPTRs; `None` for input/output means a
    /// `NIL:` of the child's own.
    pub input: Option<u32>,
    pub output: Option<u32>,
    pub error: u32,
    /// `pr_Flags`' `PRF_CLOSE*` bits.
    pub flags: u32,
    /// `None` inherits the parent's.
    pub console: Option<u32>,
    pub window: Option<u32>,
    /// `NP_ExitCode`/`NP_ExitData`; `0` for no exit routine.
    pub exit_code: u32,
    pub exit_data: u32,
    /// `Some(name)` gives the process a real `CommandLineInterface`
    /// whose `cli_CommandName` is `name`, as a process started by a shell
    /// has -- startup code treats a process without one as launched from
    /// Workbench and waits for a `WBStartup` message that never comes.
    pub cli_command_name: Option<String>,
    /// A seglist (from `LoadSeg`) holding the code at `entry`, to
    /// `UnLoadSeg` when the process ends; `0` for none.
    pub seglist: u32,
}

/// Creates a process from `spec` and makes it ready to run (it doesn't
/// run until some later switch point picks it, as `CreateNewProc` never
/// yields). Returns its `struct Process*`, or `None` if the guest heap
/// has no room.
///
/// The child's initial registers: `A7` = the top of its own freshly
/// allocated stack, below the `pr_ReturnAddr` entry frame (with
/// [`crate::dispatch::TASK_EXIT_STUB_ADDR`] as the return address for
/// its outermost `rts`, exactly the way
/// [`crate::dispatch::Runtime::new`] builds the same frame with
/// `EXIT_STUB_ADDR` for the main task), `PC` = `entry`, `A0`/`D0`
/// = the argument copy and its length (`0`/`0` without one), every
/// other register zeroed.
pub(crate) fn spawn_process<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
    spec: SpawnSpec,
) -> Option<u32> {
    let stack_size = spec.stack_size.max(crate::guestmem::MIN_STACK_SIZE);

    let (args, args_len) = match &spec.args {
        Some(bytes) => {
            let copy = ctx.heap.alloc(ctx.mem, bytes.len() as u32 + 1).ok()?;
            write_c_string(ctx.mem, copy, bytes);
            (copy, bytes.len() as u32)
        }
        None => (0, 0),
    };

    let stack_block = ctx.heap.alloc(ctx.mem, stack_size).ok()?;
    let sp_upper = stack_block.wrapping_add(stack_size) & !3;
    // The child's entry frame, in the same `RunCommand`/`CallEntry`
    // layout `Runtime::new` builds for the main task (see
    // [`PR_RETURNADDR_OFFSET`]'s doc): the caller's-stack-pointer slot
    // (`0` -- the child's outermost `rts` lands on the task exit stub,
    // there is no guest-side caller), then the reserved stack size
    // with `pr_ReturnAddr` pointing at it, then the return address at
    // the child's initial `sp`, one longword below. Startup code that
    // keys off `pr_ReturnAddr` -- vbcc's `minres.o`, ixemul's stack
    // extension -- needs this on a `CreateNewProc` child exactly as
    // much as on the main task (issue #279).
    ctx.mem.write_u32(sp_upper.wrapping_sub(4), 0);
    let return_addr_footer = sp_upper.wrapping_sub(8);
    ctx.mem
        .write_u32(return_addr_footer, return_addr_footer - stack_block);
    let sp = return_addr_footer.wrapping_sub(4);
    ctx.mem.write_u32(sp, crate::dispatch::TASK_EXIT_STUB_ADDR);

    let task = create_task(
        ctx.mem,
        ctx.heap,
        &spec.name,
        spec.priority,
        stack_block,
        sp_upper,
    )?;

    let input = match spec.input {
        Some(bptr) => bptr,
        None => open_nil(ctx, crate::dosfile::MODE_OLDFILE),
    };
    let output = match spec.output {
        Some(bptr) => bptr,
        None => open_nil(ctx, crate::dosfile::MODE_NEWFILE),
    };
    let parent = ctx.current_task;
    let console = spec
        .console
        .unwrap_or_else(|| ctx.mem.read_u32(parent + PR_CONSOLE_TASK_OFFSET));
    let window = spec
        .window
        .unwrap_or_else(|| ctx.mem.read_u32(parent + PR_WINDOW_PTR_OFFSET));
    ctx.mem.write_u32(task + PR_CIS_OFFSET, input);
    ctx.mem.write_u32(task + PR_COS_OFFSET, output);
    ctx.mem.write_u32(task + PR_CES_OFFSET, spec.error);
    ctx.mem.write_u32(task + PR_FLAGS_OFFSET, spec.flags);
    ctx.mem.write_u32(task + PR_CONSOLE_TASK_OFFSET, console);
    ctx.mem.write_u32(task + PR_WINDOW_PTR_OFFSET, window);
    ctx.mem.write_u32(task + PR_ARGUMENTS_OFFSET, args);
    ctx.mem
        .write_u32(task + PR_RETURNADDR_OFFSET, return_addr_footer);
    ctx.mem.write_u32(task + PR_EXITCODE_OFFSET, spec.exit_code);
    ctx.mem.write_u32(task + PR_EXITDATA_OFFSET, spec.exit_data);

    if let Some(command_name) = &spec.cli_command_name {
        let cli = ctx.heap.alloc(ctx.mem, CLI_STRUCT_SIZE).ok()?;
        for i in 0..CLI_STRUCT_SIZE {
            ctx.mem.write_u8(cli.wrapping_add(i), 0);
        }
        let name_bytes = command_name.as_bytes();
        let name_bstr = ctx
            .heap
            .alloc(ctx.mem, 1 + name_bytes.len().min(255) as u32)
            .ok()?;
        write_bstr(ctx.mem, name_bstr, name_bytes);
        ctx.mem
            .write_u32(cli + CLI_COMMAND_NAME_OFFSET, bptr_from_addr(name_bstr));
        ctx.mem.write_u32(cli + CLI_STANDARD_INPUT_OFFSET, input);
        ctx.mem
            .write_u32(cli + CLI_DEFAULT_STACK_OFFSET, stack_size / 4);
        ctx.mem.write_u32(task + PR_CLI_OFFSET, bptr_from_addr(cli));
    }
    if spec.seglist != 0 {
        ctx.dos.process_seglists.insert(task, spec.seglist);
    }

    let mut a = [0u32; 8];
    a[0] = args;
    a[7] = sp;
    let mut d = [0u32; 8];
    d[0] = args_len;
    let snapshot = crate::cpu::RegisterSnapshot {
        d,
        a,
        pc: spec.entry,
        // The parent's own current SR, not a hardcoded value: this
        // runtime never models the user/supervisor distinction
        // itself (every task just inherits whatever `CpuCore`'s own
        // reset default is), and the `m68k` crate's `set_sr` banks
        // the stack pointer on an S-bit change (see
        // `crate::cpu::restore`'s doc) -- a child whose SR differs
        // from everything else this runtime assumes would be a
        // needless, confusing special case for no benefit.
        sr: ctx.cpu.sr(),
    };
    ctx.tasks.spawn_ready(task, snapshot, stack_block, sp_upper);
    Some(task)
}

/// The stub handler bound to [`crate::dispatch::TASK_EXIT_STUB_ADDR`]
/// (issue #121): a `CreateNewProc` child task fell off the end of its
/// `NP_Entry` without calling anything terminal itself. The current
/// task (whichever one just `rts`'d into this stub) is gone -- see
/// [`crate::taskswitch::TaskScheduler::terminate_current`]'s doc for
/// what that does and doesn't clean up.
///
/// If no other task is ready, a pending timer request may still wake
/// one, so it is fired early, as [`wait_handler`] does when every task
/// is blocked: a task that ends while the only other one waits on a
/// timer (a joined thread whose joiner then does a timed condition
/// wait) is not a deadlock.
///
/// First, as real DOS does, the standard handles that the process's
/// `pr_Flags` say to close (`NP_CloseInput`/`NP_CloseOutput`/
/// `NP_CloseError`) are closed: whatever `pr_CIS`/`pr_COS`/`pr_CES`
/// hold by now, which is why libpthread clears `pr_CIS` once it has
/// read its slot number from it.
///
/// Issue #254: real `dos.library` frees a process's stack and `struct
/// Process` when it ends -- this didn't, leaving roughly one stack's
/// worth of guest heap permanently allocated per `CreateNewProc`'d
/// task, noticeable after many short-lived ones (codewiz's repro: 50
/// processes with 64 KB stacks each). Every address needed is
/// recoverable from the exiting task's own struct, read out *before*
/// any of it is freed (freeing may poison the block's bytes, e.g.
/// under `--sanitize`'s quarantine): the stack block (`tc_SPLower`,
/// the same address [`crate::exectask::create_new_proc_handler`]
/// originally allocated it at and handed to
/// [`crate::taskswitch::TaskScheduler::spawn_ready`] -- not whatever
/// `A7`/`tc_SPLower` happen to read if the task `StackSwap`ped onto a
/// *different* stack and never swapped back, which stays leaked same
/// as before), the process-name string (`ln_Name`), and the
/// `NP_Arguments` copy (`pr_Arguments`, if any -- most callers pass
/// none). The `struct Process` itself is freed last, once nothing
/// above still needs to read it. Each address was allocated exactly
/// once, by this same task's own `CreateNewProc` call, and never
/// freed anywhere else, so a failed free here would mean a real
/// double-free/corruption bug in this runtime, not a guest one --
/// worth failing loudly over rather than silently leaking again.
pub(crate) fn task_exit_stub_handler<C: Cpu + 'static>(
    ctx: &mut HandlerContext<'_, C>,
) -> Result<(), DispatchError> {
    let task = ctx.current_task;
    // The program's own `rts` left its return code in D0. A process
    // that has a CLI reports it there, which is where a command's
    // return code lives (`cli_ReturnCode`).
    let rc = ctx.cpu.data_register(DataRegister(0));
    let cli = crate::guestmem::addr_from_bptr(ctx.mem.read_u32(task + PR_CLI_OFFSET));
    if cli != 0 {
        ctx.mem.write_u32(cli + CLI_RETURN_CODE_OFFSET, rc);
    }

    // NP_ExitCode: DOS calls it as the process ends, in the process's
    // own context, `D0` = return code, `D1` = NP_ExitData -- libnix's
    // popen() signals the parent that way. It is guest code, so it runs
    // through the trampoline and the rest of the exit follows in the
    // continuation.
    let hook = ctx.mem.read_u32(task + PR_EXITCODE_OFFSET);
    if hook != 0 {
        let data = ctx.mem.read_u32(task + PR_EXITDATA_OFFSET);
        ctx.mem.write_u32(task + PR_EXITCODE_OFFSET, 0);
        ctx.cpu.set_data_register(DataRegister(0), rc);
        ctx.cpu.set_data_register(DataRegister(1), data);
        ctx.continuations
            .trampoline(ctx.cpu, ctx.mem, hook, finish_task_exit::<C>);
        return Ok(());
    }
    finish_task_exit(ctx)
}

/// The rest of [`task_exit_stub_handler`], once any `NP_ExitCode`
/// routine has returned: close the handles, free what the process owned
/// and switch to the next task.
fn finish_task_exit<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let task = ctx.current_task;
    let flags = ctx.mem.read_u32(task + PR_FLAGS_OFFSET);
    for (flag, offset) in [
        (PRF_CLOSEINPUT, PR_CIS_OFFSET),
        (PRF_CLOSEOUTPUT, PR_COS_OFFSET),
        (PRF_CLOSEERROR, PR_CES_OFFSET),
    ] {
        let bptr = ctx.mem.read_u32(task + offset);
        if flags & flag != 0 && bptr != 0 {
            crate::dospipe::close(ctx, crate::guestmem::addr_from_bptr(bptr));
        }
    }
    // Abandon this task's own still-pending TR_ADDREQUESTs rather than
    // letting them fire later against an ioreq address this task's own
    // exit may leave free for the heap to hand out again -- see
    // PendingTimer::owner_task's doc.
    ctx.timers.remove_owned_by(task);

    let stack_block = ctx.mem.read_u32(task + TC_SPLOWER);
    let name_addr = ctx.mem.read_u32(task + LN_NAME);
    let args_addr = ctx.mem.read_u32(task + PR_ARGUMENTS_OFFSET);
    // Only if `tc_SPLower` still names a live allocation: a task that
    // `StackSwap`ped onto some other stack and never swapped back
    // before exiting leaves `tc_SPLower` naming that *other* stack
    // instead (StackSwap's whole contract is overwriting it) -- not
    // this runtime's own heap allocation to free, and possibly not a
    // heap allocation at all. Leaked in that case, same as every
    // CreateNewProc child's stack was before this fix.
    if ctx.heap.size_of_live_alloc(stack_block).is_some() {
        ctx.heap
            .free(stack_block)
            .unwrap_or_else(|e| panic!("exiting task's own stack block: {e}"));
    }
    ctx.heap
        .free(name_addr)
        .unwrap_or_else(|e| panic!("exiting task's own process-name string: {e}"));
    if args_addr != 0 {
        ctx.heap
            .free(args_addr)
            .unwrap_or_else(|e| panic!("exiting task's own NP_Arguments copy: {e}"));
    }
    // A process started with its own CLI (`SystemTagList`) owns that
    // struct and its command-name BSTR; one loaded from a seglist owns
    // that too.
    let cli = crate::guestmem::addr_from_bptr(ctx.mem.read_u32(task + PR_CLI_OFFSET));
    if cli != 0 {
        let name_bstr =
            crate::guestmem::addr_from_bptr(ctx.mem.read_u32(cli + CLI_COMMAND_NAME_OFFSET));
        ctx.heap
            .free(name_bstr)
            .unwrap_or_else(|e| panic!("exiting task's own cli_CommandName: {e}"));
        ctx.heap
            .free(cli)
            .unwrap_or_else(|e| panic!("exiting task's own CommandLineInterface: {e}"));
    }
    if let Some(seglist) = ctx.dos.process_seglists.remove(&task) {
        ctx.dos
            .unload_seg(ctx.heap, seglist)
            .unwrap_or_else(|e| panic!("exiting task's own seglist: {e}"));
    }
    ctx.heap
        .free(task)
        .unwrap_or_else(|e| panic!("exiting task's own struct Process: {e}"));

    loop {
        if ctx.tasks.terminate_current(ctx.mem).is_ok() {
            return Ok(());
        }
        if !ctx.timers.fire_earliest(ctx.mem, ctx.tasks) {
            return Err(DispatchError::TaskDeadlock);
        }
    }
}

/// `FindTask` (LVO -294): `A1` = task name, or `NULL` for the current
/// task. See the module docs' "`FindTask`" section.
fn find_task_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let name_ptr = ctx.cpu.address_register(AddressRegister(1));
    let result = if name_ptr == 0 { ctx.current_task } else { 0 };
    ctx.cpu.set_data_register(DataRegister(0), result);
    Ok(())
}

/// `SetTaskPri` (LVO -300): `A1` = `struct Task*`, `D0` = new priority.
/// `D0` = the old priority, sign-extended. clib2's exit code calls it
/// to restore the priority its startup saved.
///
/// Issue #254: an earlier version of this handler treated setting a
/// task's own priority to the value it already had as a yield point,
/// based on a mistaken reading of the idiom -- real exec's scheduler
/// only reschedules out of `SetTaskPri` when the calling task's new
/// priority actually drops *below* some ready task's, never on a tie
/// (confirmed against real exec, reported by codewiz: same-priority
/// `SetTaskPri` keeps the CPU, and it's a lower-then-restore pair --
/// `SetTaskPri(self, pri - 1); SetTaskPri(self, pri);`, the pattern
/// AmigaPorts/gcc's `__gthread_yield()` uses -- that hands it to an
/// equal, during the lowering call only). [`TaskScheduler::
/// reschedule_if_outranked`] implements that real rule; only called
/// for the *current* task setting its own priority -- one task
/// changing another's priority (real, if less common, usage) isn't a
/// reschedule point for either of them.
fn set_task_pri_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let task = ctx.cpu.address_register(AddressRegister(1));
    let new_pri = ctx.cpu.data_register(DataRegister(0)) as u8;
    let old_pri = ctx.mem.read_u8(task + crate::execlist::LN_PRI) as i8;
    ctx.mem.write_u8(task + crate::execlist::LN_PRI, new_pri);
    ctx.cpu
        .set_data_register(DataRegister(0), old_pri as i32 as u32);
    if task == ctx.current_task {
        ctx.tasks.request_reschedule_if_outranked(new_pri as i8);
    }
    Ok(())
}

/// `AT_DeadEnd` (bit 31, `0x80000000`), per `<exec/alerts.h>` -- set in
/// `alertNum` when the condition is fatal (system integrity can no
/// longer be guaranteed; a real kernel typically reboots or hangs
/// rather than returning to the caller). Clear means "recoverable":
/// real `Alert()` flashes the power light / shows a requester and
/// returns to the caller so execution continues.
const AT_DEAD_END: u32 = 0x8000_0000;

/// `alertNum`'s `SubSysId` field (bits 24-30, 7 bits), per
/// `<exec/alerts.h>`'s documented layout: `D | SubSysId(7) |
/// GeneralError(8) | SubSystemSpecificError(16)`.
const ALERT_SUBSYS_ID_SHIFT: u32 = 24;
const ALERT_SUBSYS_ID_MASK: u32 = 0x7F;
/// `alertNum`'s `General Error` field (bits 16-23, 8 bits).
const ALERT_GENERAL_SHIFT: u32 = 16;
const ALERT_GENERAL_MASK: u32 = 0xFF;
/// `alertNum`'s `SubSystem Specific Error` field (bits 0-15, 16 bits).
const ALERT_SPECIFIC_MASK: u32 = 0xFFFF;

/// `Alert` (LVO -108): `D7` = `alertNum` (per [`crate::lvos::exec::EXEC_LVOS`],
/// already verified against a primary source). This runtime has no
/// Guru Meditation display to show, so both cases are handled as
/// honestly as a headless runtime can: a recoverable alert
/// ([`AT_DEAD_END`] clear) is impossible to usefully act on either, so
/// it's simply logged (see below) and returned from, matching real
/// `Alert()`'s documented "flashes and returns" behavior for this
/// case. A dead-end alert ([`AT_DEAD_END`] set) means the guest itself
/// has declared system integrity can no longer be guaranteed, so this
/// fails loudly instead of pretending to continue past it -- the real
/// machine wouldn't either.
///
/// Unlike every other handler in this runtime, `Alert` always prints
/// its own diagnostic straight to stderr, unconditionally (not gated
/// behind `--verbose`/`--snoop`): on real hardware a Guru Meditation
/// is never silent, and a *recoverable* alert in particular would
/// otherwise vanish without a trace the moment the guest continues
/// past it -- there's no later error for anything to surface it
/// through. `dos/dispatch.rs`'s module docs on `CallInfo::detail`
/// explain why ad hoc `eprintln!`s in handlers are otherwise
/// discouraged; this is the deliberate, documented exception, for the
/// same reason a dead-end `HandlerFailed`'s message is never
/// optional. Found needed while running the real `AmiSnap` binary
/// (`~/src/amisnap`): `PC - EXEC_LIBRARY_BASE == -108` exactly matched
/// this LVO in the `UnknownCall` diagnostic's candidate list, i.e. the
/// guest really was calling `Alert()`, not some math/AmiSSL library (a
/// wrong initial guess -- `amisslmaster.library` is opened
/// conditionally and this runtime never even attempts it).
fn alert_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let alert_num = ctx.cpu.data_register(DataRegister(7));
    let dead_end = alert_num & AT_DEAD_END != 0;
    let subsys_id = (alert_num >> ALERT_SUBSYS_ID_SHIFT) & ALERT_SUBSYS_ID_MASK;
    let general = (alert_num >> ALERT_GENERAL_SHIFT) & ALERT_GENERAL_MASK;
    let specific = alert_num & ALERT_SPECIFIC_MASK;
    eprintln!(
        "volamos: Alert({alert_num:#010x}): {} -- SubSysId={subsys_id:#04x} \
         GeneralError={general:#04x} SpecificError={specific:#06x}",
        if dead_end {
            "AT_DeadEnd"
        } else {
            "AT_Recovery"
        }
    );
    if dead_end {
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
///
/// `signalNum` is a real `BYTE` parameter (`clib/exec_protos.h`): only
/// `D0`'s low byte is meaningful, the upper three bytes are whatever a
/// real C compiler's calling convention happened to leave there (never
/// guaranteed sign-extended). Reading the full 32-bit `D0` as `requested`
/// treated `AllocSignal(-1)` as `255` instead of `-1` whenever a real
/// compiler didn't happen to zero those upper bytes -- caught via a real
/// conformance suite (`bsdsocktest`'s own `alloc_signal()`/`AllocSignal
/// (-1)` wrapper, real SAS/C-compiled) failing every single signal
/// allocation, even the very first one on a freshly booted task.
fn alloc_signal_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let requested = (ctx.cpu.data_register(DataRegister(0)) as u8) as i8 as i32;
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
/// rather than an error, since there's no bit to clear. Same real `BYTE`
/// parameter caveat as [`alloc_signal_handler`] -- only `D0`'s low byte
/// is meaningful.
fn free_signal_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let signal_num = (ctx.cpu.data_register(DataRegister(0)) as u8) as i8 as i32;
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
/// docs' "`Wait`: trapping unrunnable blocking waits" section, and
/// [`crate::taskswitch`]'s module doc for the issue #121 blocking path
/// below, which replaces what used to be an unconditional failure.
fn wait_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let signal_set = ctx.cpu.data_register(DataRegister(0));
    let task = ctx.current_task;
    fold_pending_host_break(ctx.mem, task);
    ctx.timers.fire_due(ctx.mem, ctx.tasks);

    loop {
        let recvd = ctx.mem.read_u32(task + TC_SIGRECVD);
        let satisfied = recvd & signal_set;
        if satisfied != 0 {
            ctx.mem.write_u32(task + TC_SIGRECVD, recvd & !satisfied);
            ctx.mem.write_u32(task + TC_SIGWAIT, 0);
            ctx.cpu.set_data_register(DataRegister(0), satisfied);
            return Ok(());
        }

        // Nothing satisfied: record what we were waiting for
        // (best-effort fidelity -- a real kernel leaves tc_SigWait set
        // exactly like this while the task is actually suspended),
        // then try to block and switch to another runnable task
        // (issue #121) rather than failing outright.
        // `block_current_on_signal` takes `ctx.trap_pc`, not
        // `ctx.cpu.pc()`, as the resume address -- see that method's
        // and `crate::taskswitch`'s doc for why: resuming this task
        // later means re-trapping into this exact `Wait` call from
        // scratch, which is also why there's nothing here that
        // "remembers" to deliver `satisfied` on wake -- the re-entered
        // call above does that fresh.
        ctx.mem.write_u32(task + TC_SIGWAIT, signal_set);
        if ctx
            .tasks
            .block_current_on_signal(ctx.cpu, ctx.mem, ctx.trap_pc, signal_set)
            .is_ok()
        {
            return Ok(());
        }

        // No other task is runnable. If a timer request is still
        // pending (issue #237, sub-problem #2: e.g. a condvar-with-
        // timeout `Wait` racing a `timerequest`'s signal against
        // another task's), it is the only event left that could ever
        // make progress in this single-process cooperative model --
        // refusing to fire it early doesn't make the wait any more
        // "real", it just turns an elapsed timer into a permanent
        // deadlock. Fire the earliest one and retry; this loop ends
        // either once something's satisfied or once there are no more
        // pending timers to fall back on.
        if !ctx.timers.fire_earliest(ctx.mem, ctx.tasks) {
            // No other task could ever satisfy this, and no pending
            // timer either -- a real deadlock, not just "no
            // scheduler": fail loudly instead of hanging the host
            // process forever, the same posture every single-task
            // stand-in in this module already used before issue #121's
            // scheduler existed.
            return Err(DispatchError::HandlerFailed {
                library: "exec.library".to_string(),
                lvo: -318,
                handler_name: "Wait".to_string(),
                message: format!(
                    "Wait({signal_set:#010x}) would deadlock: no other task is runnable, no \
                     timer request is pending, and none of the requested signals are \
                     currently pending in tc_SigRecvd"
                ),
            });
        }
    }
}

/// `Signal` (LVO -324): `A1` = target task, `D0` = signal bits to set.
/// `target` must be the current task or a task [`crate::taskswitch::
/// TaskScheduler`] actually knows about (issue #121: created via
/// `CreateNewProc` and not yet exited) -- anything else fails loudly,
/// since it can't plausibly refer to a real task (almost certainly a
/// guest bug, e.g. a stale or uninitialized task pointer).
///
/// `Signal` itself never switches tasks, matching real hardware: it
/// only marks a blocked target `Ready` if the newly-set bits intersect
/// what it's waiting for. The target only actually runs once some
/// later switch point (on either task) picks it -- see
/// `crate::taskswitch`'s module doc.
fn signal_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let target = ctx.cpu.address_register(AddressRegister(1));
    let signals = ctx.cpu.data_register(DataRegister(0));

    if target != ctx.current_task && !ctx.tasks.is_known(target) {
        return Err(DispatchError::HandlerFailed {
            library: "exec.library".to_string(),
            lvo: -324,
            handler_name: "Signal".to_string(),
            message: format!(
                "Signal({target:#010x}, {signals:#010x}): unknown task -- not the current \
                 task ({:#010x}) and not a task this runtime created",
                ctx.current_task
            ),
        });
    }

    let recvd = ctx.mem.read_u32(target + TC_SIGRECVD);
    let recvd = recvd | signals;
    ctx.mem.write_u32(target + TC_SIGRECVD, recvd);
    if target != ctx.current_task {
        ctx.tasks.wake_if_signal_matches(target, recvd);
    }
    Ok(())
}

/// `dos.library`'s `CheckSignal` (LVO -792): `D1` = mask of signals to
/// check. Returns (and clears) the intersection of `tc_SigRecvd` and the
/// mask, folding in any pending host break first -- see the module
/// docs -- and any due timer request (issue #237, sub-problem #2): a
/// polling loop that uses `CheckSignal` instead of blocking in `Wait`
/// still needs a `TR_ADDREQUEST`'s signal to actually arrive.
fn check_signal_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let mask = ctx.cpu.data_register(DataRegister(1));
    let task = ctx.current_task;
    fold_pending_host_break(ctx.mem, task);
    ctx.timers.fire_due(ctx.mem, ctx.tasks);

    let recvd = ctx.mem.read_u32(task + TC_SIGRECVD);
    let hit = recvd & mask;
    ctx.mem.write_u32(task + TC_SIGRECVD, recvd & !hit);
    ctx.cpu.set_data_register(DataRegister(0), hit);
    Ok(())
}

/// `exec.library`'s `AddTask` (LVO -282): `A1` = `struct Task*`, `A2` =
/// initial PC, `A3` = final PC. Pretends to succeed: `D0` = the task
/// pointer (the V36+ success return), but the task is **never
/// scheduled** -- this runtime is single-tasking by design (see the
/// module docs), so the new task's code never runs. That's the honest
/// least-wrong answer for the observed real-world use (the `DiskSpeed`
/// benchmark's CPU-availability counter task: a background task that
/// increments a shared counter while the main task does disk I/O -- an
/// unscheduled task simply reads as 0% CPU available, a cosmetic
/// wrong-number, not a hang or crash). A guest that genuinely *blocks*
/// on its new task doing something will hit `Wait`'s existing
/// fail-loudly path rather than deadlock silently.
fn add_task_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let task = ctx.cpu.address_register(AddressRegister(1));
    ctx.cpu.set_data_register(DataRegister(0), task);
    Ok(())
}

/// `exec.library`'s `RemTask` (LVO -288): `A1` = `struct Task*` (or
/// `NULL` for self). Removing a task [`add_task_handler`] never
/// scheduled is a no-op. `NULL`/self-removal would mean the *main*
/// program removing itself -- a real exit path this runtime doesn't
/// model through `RemTask`; fail loudly rather than pretend the
/// program is gone while continuing to run it.
fn rem_task_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let task = ctx.cpu.address_register(AddressRegister(1));
    if task == 0 || task == ctx.current_task {
        return Err(DispatchError::HandlerFailed {
            library: "exec.library".to_string(),
            lvo: -288,
            handler_name: "RemTask".to_string(),
            message: "RemTask(NULL/self): the running program asked to remove itself -- this \
                      single-tasking runtime has no scheduler to return to; a real machine \
                      would end the task here"
                .to_string(),
        });
    }
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

    // A corrupted or never-initialized StackSwapStruct (a guest bug,
    // not this runtime's) would otherwise run on silently -- A7 lands
    // on garbage, the generic post-dispatch `rts` above pops whatever
    // happens to be there, and the failure that eventually surfaces
    // (an UnknownCall at a nonsense pc, or EmptyContinuationStack) is
    // far removed from this call site and much more confusing to
    // debug than the actual cause. Fail loudly here instead, the same
    // posture every other malformed-input check in this module takes.
    if new_lower > new_upper {
        return Err(DispatchError::HandlerFailed {
            library: "exec.library".to_string(),
            lvo: -732,
            handler_name: "StackSwap".to_string(),
            message: format!(
                "StackSwap({struct_ptr:#010x}): stk_Lower {new_lower:#010x} > stk_Upper \
                 {new_upper:#010x} -- an invalid or uninitialized StackSwapStruct"
            ),
        });
    }
    if new_pointer < new_lower || new_pointer > new_upper {
        return Err(DispatchError::HandlerFailed {
            library: "exec.library".to_string(),
            lvo: -732,
            handler_name: "StackSwap".to_string(),
            message: format!(
                "StackSwap({struct_ptr:#010x}): stk_Pointer {new_pointer:#010x} is outside \
                 [stk_Lower {new_lower:#010x}, stk_Upper {new_upper:#010x}] -- an invalid or \
                 uninitialized StackSwapStruct"
            ),
        });
    }
    if new_pointer - new_lower < 4 {
        return Err(DispatchError::HandlerFailed {
            library: "exec.library".to_string(),
            lvo: -732,
            handler_name: "StackSwap".to_string(),
            message: format!(
                "StackSwap({struct_ptr:#010x}): stk_Pointer {new_pointer:#010x} leaves no room \
                 above stk_Lower {new_lower:#010x} for the return address this call needs to \
                 push -- the new stack is already full"
            ),
        });
    }

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

    // Tell the sanitizer's shadow map about the switch, if one is
    // active (`--sanitize`). This has to be `new_sp` -- the *post-push*
    // A7, not `new_pointer` (the "nothing pending" value read from the
    // struct) -- because `new_sp` is where the live stack pointer
    // actually ends up and what every subsequent instruction will be
    // checked against; passing `new_pointer` instead would poison
    // `[new_lower, new_pointer)` as BelowStackPointer, which includes
    // `new_sp` itself -- the very slot this handler just wrote the real,
    // live return address into. The generic post-dispatch `rts` (see
    // this module's `StackSwap` doc) reads that slot before this
    // function's caller ever gets a chance to run another
    // update_stack_pointer call, so poisoning it here would report a
    // false "invalid read (below stack pointer)" on volamos's own
    // bookkeeping, for every single StackSwap call. Using
    // `reset_stack_tracking` (rather than plumbing this through the
    // generic per-instruction `update_stack_pointer` path) matters too:
    // see that method's doc for why a StackSwap needs eager,
    // handler-driven cleanup instead of waiting for the run loop to
    // eventually notice the SP left the old region.
    if let Some(shadow) = ctx.mem.shadow_mut() {
        shadow.reset_stack_tracking(new_lower, new_upper, new_sp);
    }

    Ok(())
}

/// `ExecBase.SysFlags`'s `SFB_SAR` bit (bit 15, `0x8000`) -- "Stop And
/// Reschedule". Real `sched_yield()` on Amiga (`Forbid(); SysBase->
/// SysFlags |= SFB_SAR; Permit();`) sets this directly in guest memory
/// (not through any library call) between a no-op `Forbid` and the
/// `Permit` that follows; [`permit_handler`] is what actually notices
/// it (issue #121).
const SFB_SAR: u16 = 1 << 15;

/// `exec.library`'s `Forbid` (LVO -132, no args, no return value): real
/// `Forbid` disables task switching until a matching `Permit`,
/// protecting a critical section (e.g. walking a shared list) from
/// being preempted mid-scan. Found missing running the real Workbench
/// 3.1.4 `C:/Avail` binary, which calls `Forbid` before walking exec's
/// memory-pool list and `Permit` after.
///
/// This runtime has no preemption (switches only ever happen at a
/// handler's own request -- see `crate::taskswitch`'s module doc), so
/// nothing can interrupt a critical section regardless. Issue #262 found
/// `Forbid` still matters for the *voluntary* switch points, though: it
/// now keeps a real per-task nest count
/// ([`crate::taskswitch::TaskScheduler::enter_forbid`]) so that a
/// `SetTaskPri`-triggered reschedule, and `Permit`'s own `SFB_SAR`
/// yield, wait for the outermost `Permit` the way real exec's do.
///
/// The count is deliberately *not* mirrored into the guest-visible
/// `ExecBase.TDNestCnt`/`Task.tc_TDNestCnt` bytes (nor is `Disable`'s
/// `IDNestCnt`, see [`disable_handler`]): a guest that reads those
/// directly rather than calling `Forbid`/`Permit` is relying on more
/// than this API-level runtime promises.
fn forbid_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    ctx.tasks.enter_forbid();
    Ok(())
}

/// `exec.library`'s `Permit` (LVO -138, no args, no return value). See
/// [`forbid_handler`] for the nest count this unwinds.
///
/// Only the *outermost* `Permit` acts on anything, as in real exec.
/// Issue #121's one use of it is real `sched_yield()`'s `SFB_SAR` trick
/// (see [`SFB_SAR`]'s doc): if the guest has set that bit in
/// `ExecBase->SysFlags`, this clears it (real hardware consumes the
/// request, it doesn't stay set) and requests a yield -- see
/// `crate::taskswitch::TaskScheduler::request_yield`'s doc for why this
/// only *requests* one rather than switching immediately. Inside a
/// nested `Forbid` the bit is left set for the outermost `Permit` to
/// find. A deferred `SetTaskPri` reschedule (issue #262) needs no
/// action here: it is picked up by the generic post-dispatch epilogue
/// right after this handler, now that the count is back to zero.
fn permit_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    if !ctx.tasks.leave_forbid() {
        return Ok(());
    }
    let sys_flags_addr =
        crate::dispatch::EXEC_LIBRARY_BASE + crate::dispatch::EXEC_BASE_SYSFLAGS_OFFSET;
    let sys_flags = ctx.mem.read_u16(sys_flags_addr);
    if sys_flags & SFB_SAR != 0 {
        ctx.mem.write_u16(sys_flags_addr, sys_flags & !SFB_SAR);
        ctx.tasks.request_yield();
    }
    Ok(())
}

/// `exec.library`'s `Disable`/`Enable` (LVO -120/-126, no args, no
/// return value): real `Disable` masks all hardware interrupts (down to
/// but not including NMI) until a matching `Enable`, protecting a
/// critical section from being interrupted at the hardware level rather
/// than just from being task-switched away from (that's `Forbid`/
/// `Permit`, just above). This runtime has no interrupt sources at all
/// -- there's no timer tick, no hardware, nothing async that could ever
/// fire during a "disabled" window -- so exactly the same reasoning that
/// makes `Forbid`/`Permit` no-ops applies here: both are true no-ops.
/// Found missing running the real 1998 `LawBreaker` binary (37.72),
/// which calls `Disable` immediately after `Forbid`.
///
/// Deliberately *not* maintaining `ExecBase.IDNestCnt` (real NDK
/// `exec/execbase.h` offset 0x126, a signed byte, `-1` meaning "not
/// nested", incremented per `Disable` and decremented per `Enable`)
/// despite that field existing at a real, reserved offset in this
/// runtime's `ExecBase` layout (see
/// [`crate::dispatch::EXEC_BASE_ATTNFLAGS_OFFSET`]'s doc, which derives
/// `IDNestCnt`/`TDNestCnt`'s offsets as part of the same field-by-field
/// walk). The tempting argument for maintaining it is that a guest
/// reading `SysBase->IDNestCnt` directly (rather than calling `Disable`/
/// `Enable`) would see something coherent instead of a stuck `0`.
/// Rejected: `Forbid`/`Permit` don't mirror their own nest count into
/// the exactly parallel `TDNestCnt` either (see [`forbid_handler`]), and
/// giving `Disable`/`Enable` a guest-visible counter while their sibling
/// pair keeps its count host-side would be an inconsistency with no
/// real payoff. Unlike `Forbid`'s count, nothing here branches on this
/// one at all (there's no interrupt delivery to gate), so a maintained
/// counter would only be a guest-visible number, not an actual
/// guarantee. A guest that inspects the field directly instead of
/// trusting `Disable`/`Enable`'s return contract is already relying on
/// behavior this API-level runtime doesn't promise.
fn disable_handler<C: Cpu>(_ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    Ok(())
}

/// See [`disable_handler`].
fn enable_handler<C: Cpu>(_ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
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
/// `IOERR_ABORTED` (`exec/errors.h`): "request terminated early" --
/// what a real `AbortIO`'d request's `io_Error` ends up holding.
const IOERR_ABORTED: i8 = -2;

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
/// Tracks `TR_ADDREQUEST` requests [`send_io_handler`] handed back
/// immediately but which haven't reached their deadline yet (issue
/// #237, sub-problem #2). `DoIO` suspends only its caller until its
/// deadline, as `Delay` does (see [`do_io_handler`]'s doc), through a
/// `Delay` entry here rather than one of these -- but `SendIO` must
/// return immediately, matching its documented
/// asynchronous contract, which real pthread/newlib condvar-with-
/// timeout code genuinely relies on: it `SendIO`s a `timerequest`, then
/// `Wait()`s on a mask combining the condvar's signal with the timer's
/// own reply port's signal bit, expecting *either* to wake it. A
/// timer that only ever completes via a blocking `std::thread::sleep`
/// inside the trap handler that sent it defeats that entirely -- this
/// single-threaded runtime can't run anything else (e.g. whatever
/// would eventually set the condvar) while that sleep blocks, so the
/// combined `Wait()` would still only ever see the timer's own bit,
/// defeating the whole point of racing the two.
///
/// Since this runtime has no real wall-clock-driven background thread
/// (it only ever makes progress when the guest CPU traps into a
/// library call), completion is checked lazily, at the same two
/// seams [`fold_pending_host_break`] already uses to fold in another
/// asynchronous-ish event (`Wait`/`CheckSignal`): [`Self::fire_due`]
/// fires anything whose deadline has genuinely passed. The remaining
/// case -- every task blocked, nothing left that could ever make
/// progress except a still-pending timer -- is exactly what
/// [`Self::fire_earliest`] is for; see [`wait_handler`]'s retry loop.
pub struct PendingTimers {
    entries: Vec<PendingTimer>,
}

/// What firing a [`PendingTimer`] actually does -- a real guest
/// `TR_ADDREQUEST` (completed via `ReplyMsg`, [`complete_timer_request`]),
/// or (issue #254) a `Delay`-blocked task's own deadline, which has no
/// guest-visible I/O struct to complete -- just a task to wake
/// (`TaskScheduler::wake_task`). Sharing one `Vec` sorted by deadline
/// means `fire_due`/`fire_earliest` naturally interleave both kinds
/// correctly relative to each other, which is the whole point: a real
/// pending timer request must still complete on time even while some
/// *other* task is blocked in `Delay`, and a `Delay`'s own deadline
/// must still be considered by another task's `Wait`-driven
/// `fire_due`/`fire_earliest` call.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TimerCompletion {
    Request(u32),
    Delay(u32),
}

struct PendingTimer {
    completion: TimerCompletion,
    deadline: std::time::Instant,
    /// Only ever set for a [`TimerCompletion::Delay`] entry: its
    /// deadline has passed and the delaying task has already been
    /// woken, but the entry itself is deliberately kept until that
    /// task's own `delay_handler` re-trap retires it with
    /// [`PendingTimers::remove_delay`].
    ///
    /// A `Delay` entry does two jobs at once -- it's the wake event
    /// *and* the deadline memo [`PendingTimers::delay_deadline`] hands
    /// back on every re-trap (see its doc for why the re-trap model
    /// has nothing else durable to recognize a re-entry by). Removing
    /// it at the moment it fires, the way a completed
    /// [`TimerCompletion::Request`] is removed, destroys the memo too:
    /// the woken task re-enters `delay_handler`, finds no entry, and
    /// registers `now + the full original duration` all over again --
    /// so its `Delay` never returns as long as anything keeps waking
    /// it (issue #262: a child's `Delay(10)` restarted forever while
    /// the main task polled with `Delay(1)`, because each of the main
    /// task's own blocking waits fired the child's elapsed entry).
    ///
    /// Keeping the entry fixes that, but it must then be invisible to
    /// every later scan -- [`PendingTimers::fire_due`] would otherwise
    /// re-wake the same task on every call, and
    /// [`PendingTimers::fire_earliest_impl`] would pick this
    /// already-elapsed entry as "the earliest thing that could still
    /// make progress" forever, reporting progress to its callers'
    /// retry loops without any being made.
    elapsed: bool,
    /// The task that owns this entry -- `SendIO`'s caller for a
    /// `Request`, or the delaying task itself for a `Delay` -- used to
    /// drop it without completing it if that task exits (falls off its
    /// `NP_Entry`) while it's still pending, matching real
    /// `timer.device` abandoning in-flight requests when their owning
    /// process dies. Without this, a since-removed, possibly-reused
    /// `ioreq` address would otherwise still fire and write through it
    /// (see [`complete_timer_request`]'s callers).
    owner_task: u32,
}

impl Default for PendingTimers {
    fn default() -> Self {
        Self::new()
    }
}

impl PendingTimers {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Registers `ioreq` (a `TR_ADDREQUEST` request already past
    /// `SendIO`'s normal validation) as due at `deadline`, owned by
    /// `owner_task`. Replaces (not duplicates) any still-pending entry
    /// for the same `ioreq`: a guest re-`SendIO`ing a request it never
    /// `WaitIO`/`CheckIO`/`AbortIO`'d is a guest bug, but shouldn't
    /// leave two entries that would otherwise both complete and
    /// double-link the same message `Node` onto its reply port
    /// (`put_msg_impl`'s `AddTail`, called twice on one node, corrupts
    /// that list).
    fn push(&mut self, ioreq: u32, deadline: std::time::Instant, owner_task: u32) {
        self.remove(ioreq);
        self.entries.push(PendingTimer {
            completion: TimerCompletion::Request(ioreq),
            deadline,
            owner_task,
            elapsed: false,
        });
    }

    /// Returns `task`'s already-registered `Delay` deadline, or
    /// registers a fresh one (`now + duration`) and returns that.
    /// [`delay_handler`]'s first entry for a given `Delay` call must
    /// compute the deadline once and every subsequent re-trap (after
    /// blocking) must reuse that exact same deadline rather than
    /// restarting the clock -- the re-trap model re-enters
    /// `delay_handler` from scratch each time, with `D1` still holding
    /// the original tick count, so there is nothing else durable to
    /// recognize "this is a re-entry, not a fresh call" by.
    pub(crate) fn delay_deadline(
        &mut self,
        task: u32,
        duration: std::time::Duration,
    ) -> std::time::Instant {
        if let Some(entry) = self
            .entries
            .iter()
            .find(|e| e.completion == TimerCompletion::Delay(task))
        {
            return entry.deadline;
        }
        let deadline = std::time::Instant::now() + duration;
        self.entries.push(PendingTimer {
            completion: TimerCompletion::Delay(task),
            deadline,
            owner_task: task,
            elapsed: false,
        });
        deadline
    }

    /// Whether `task` has a `Delay` entry: whether its current
    /// `Delay`, or synchronous `DoIO(TR_ADDREQUEST)`, is a re-trap
    /// after blocking rather than a fresh call.
    pub(crate) fn has_delay(&self, task: u32) -> bool {
        self.entries
            .iter()
            .any(|e| e.completion == TimerCompletion::Delay(task))
    }

    /// Removes `task`'s own `Delay` entry, if any, once its `Delay`
    /// call has genuinely finished (so a later, unrelated `Delay` call
    /// from the same task starts with a clean slate).
    pub(crate) fn remove_delay(&mut self, task: u32) {
        self.entries
            .retain(|e| e.completion != TimerCompletion::Delay(task));
    }

    /// Removes (without completing) every entry owned by `task` --
    /// real `timer.device` abandons a process's in-flight requests
    /// when it dies rather than delivering them into memory it no
    /// longer owns; a `Delay` entry is dropped the same way (nothing
    /// left to wake). Called from [`task_exit_stub_handler`].
    fn remove_owned_by(&mut self, task: u32) {
        self.entries.retain(|e| e.owner_task != task);
    }

    /// Whether `ioreq` is still pending (not yet completed) -- used by
    /// [`check_io_handler`]'s "still queued" answer.
    fn contains(&self, ioreq: u32) -> bool {
        self.entries
            .iter()
            .any(|e| e.completion == TimerCompletion::Request(ioreq))
    }

    /// Removes `ioreq` without completing it (real `AbortIO`'s
    /// contract: cancel, don't deliver). Returns whether it was
    /// actually still pending. `pub(crate)` rather than private: also
    /// called from `execmem::free_mem_handler` to drop a still-pending
    /// request whose backing memory is being freed out from under it
    /// (see that call site's doc).
    pub(crate) fn remove(&mut self, ioreq: u32) -> bool {
        let before = self.entries.len();
        self.entries
            .retain(|e| e.completion != TimerCompletion::Request(ioreq));
        self.entries.len() != before
    }

    /// Completes and removes `ioreq` right now, regardless of its
    /// deadline -- real `WaitIO`'s contract is to block until the
    /// request is actually done, and there is nothing else host-side
    /// that blocking could usefully wait for in the meantime. A no-op
    /// if `ioreq` isn't (or is no longer) pending.
    fn complete_now<M: AddressSpace>(
        &mut self,
        mem: &mut M,
        tasks: &mut TaskScheduler,
        ioreq: u32,
    ) {
        if self.remove(ioreq) {
            complete_timer_request(mem, tasks, ioreq);
        }
    }

    /// Completes and removes every entry whose deadline has already
    /// passed. Called at the same two points [`fold_pending_host_break`]
    /// already is (`Wait`, `CheckSignal`), and from [`delay_handler`]'s
    /// own loop -- the ordinary path for a timer (or another task's
    /// `Delay`) that genuinely elapsed while other guest code ran.
    pub(crate) fn fire_due<M: AddressSpace>(&mut self, mem: &mut M, tasks: &mut TaskScheduler) {
        let now = std::time::Instant::now();
        let mut due = Vec::new();
        for entry in &mut self.entries {
            if !entry.elapsed && entry.deadline <= now {
                entry.elapsed = true;
                due.push(entry.completion);
            }
        }
        // A fired `Delay` entry stays, flagged -- see [`PendingTimer::
        // elapsed`]'s doc. A completed `Request` goes, as always.
        self.entries
            .retain(|e| matches!(e.completion, TimerCompletion::Delay(_)) || !e.elapsed);
        for completion in due {
            Self::complete(mem, tasks, completion);
        }
    }

    /// Fires the single earliest-due pending entry -- [`wait_handler`]'s
    /// last resort when every task is blocked and a pending timer is
    /// the only event left that could ever unblock anything: in a
    /// single-process cooperative model, refusing to fire it eventually
    /// doesn't make the wait any more "real", it only turns an elapsed
    /// timer into a permanent deadlock. Returns whether anything was
    /// fired.
    ///
    /// Sleeps for real, synchronously, until the deadline first if it
    /// hasn't actually passed yet -- found missing by codewiz while
    /// testing #237's fixes: firing instantly regardless of the real
    /// deadline made a 0.3s timed wait measure 0.00s elapsed whenever
    /// nothing else was ever going to be `Ready` (the common case for
    /// a single pthread using a timed wait as a plain delay). This is
    /// safe here specifically *because* this is the last-resort path:
    /// by construction, nothing else could possibly make progress
    /// while this sleeps anyway (every other task is blocked, or there
    /// never was another task), which is also the only time `Delay`
    /// and a synchronous `DoIO` sleep the host for real
    /// ([`crate::dosdate::block_current_for`]).
    pub(crate) fn fire_earliest<M: AddressSpace>(
        &mut self,
        mem: &mut M,
        tasks: &mut TaskScheduler,
    ) -> bool {
        self.fire_earliest_impl(mem, tasks, None)
    }

    /// As [`Self::fire_earliest`], but only fires if the earliest
    /// pending entry's deadline is at or before `bound` -- [`delay_handler`]'s
    /// own last-resort case (no other task runnable, but some *other*
    /// entry -- another task's real timer request, or another task's
    /// own `Delay` -- might legitimately be due before this task's own
    /// deadline, and firing it could make a task `Ready` to switch to
    /// instead of just sleeping blindly for the rest of this `Delay`).
    /// Returns `false` without sleeping or touching anything if the
    /// earliest entry is later than `bound` (including when there are
    /// no entries at all) -- `delay_handler` handles its own deadline
    /// directly rather than through this path.
    pub(crate) fn fire_earliest_bounded<M: AddressSpace>(
        &mut self,
        mem: &mut M,
        tasks: &mut TaskScheduler,
        bound: std::time::Instant,
    ) -> bool {
        self.fire_earliest_impl(mem, tasks, Some(bound))
    }

    fn fire_earliest_impl<M: AddressSpace>(
        &mut self,
        mem: &mut M,
        tasks: &mut TaskScheduler,
        bound: Option<std::time::Instant>,
    ) -> bool {
        let Some((idx, deadline)) = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.elapsed)
            .map(|(i, e)| (i, e.deadline))
            .min_by_key(|&(_, deadline)| deadline)
        else {
            return false;
        };
        if let Some(bound) = bound
            && deadline > bound
        {
            return false;
        }
        let now = std::time::Instant::now();
        if deadline > now {
            std::thread::sleep(deadline - now);
        }
        // A fired `Delay` entry stays, flagged -- see [`PendingTimer::
        // elapsed`]'s doc. A completed `Request` goes, as always.
        let completion = match self.entries[idx].completion {
            completion @ TimerCompletion::Delay(_) => {
                self.entries[idx].elapsed = true;
                completion
            }
            completion @ TimerCompletion::Request(_) => {
                self.entries.remove(idx);
                completion
            }
        };
        Self::complete(mem, tasks, completion);
        true
    }

    fn complete<M: AddressSpace>(
        mem: &mut M,
        tasks: &mut TaskScheduler,
        completion: TimerCompletion,
    ) {
        match completion {
            TimerCompletion::Request(ioreq) => complete_timer_request(mem, tasks, ioreq),
            TimerCompletion::Delay(task) => tasks.wake_task(task),
        }
    }
}

/// Finishes a `TR_ADDREQUEST` request exactly the way real
/// `timer.device` does: `io_Error` = 0, the request replied onto its
/// own `mn_ReplyPort` (via [`crate::execlist::put_msg_impl`], same as
/// a real `ReplyMsg`), and -- since this runtime's message ports never
/// carry a real `AllocSignal`'d bit by default (see
/// [`crate::execlist`]'s module doc), but a hand-built `timerequest`'s
/// reply port, as real pthread/newlib code constructs one, genuinely
/// does -- the port's own `mp_SigBit` OR'd into `mp_SigTask`'s
/// `tc_SigRecvd`, with [`TaskScheduler::wake_if_signal_matches`] run
/// exactly like [`signal_handler`] already does for a real `Signal()`.
/// From the woken task's side, a completed timer and a real `Signal()`
/// are indistinguishable, which is the whole point.
fn complete_timer_request<M: AddressSpace>(mem: &mut M, tasks: &mut TaskScheduler, ioreq: u32) {
    mem.write_u8(ioreq + IO_ERROR_OFFSET, 0);

    let reply_port = mem.read_u32(ioreq + crate::execlist::MN_REPLYPORT);
    if reply_port == 0 {
        return;
    }
    crate::execlist::put_msg_impl(mem, reply_port, ioreq, crate::execlist::NT_REPLYMSG);

    let sig_task = mem.read_u32(reply_port + crate::execlist::MP_SIGTASK);
    if sig_task == 0 {
        return;
    }
    let sig_bit = mem.read_u8(reply_port + crate::execlist::MP_SIGBIT);
    let bit = 1u32 << sig_bit;
    let recvd = mem.read_u32(sig_task + TC_SIGRECVD) | bit;
    mem.write_u32(sig_task + TC_SIGRECVD, recvd);
    if sig_task != tasks.current() {
        tasks.wake_if_signal_matches(sig_task, recvd);
    }
}

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
        // TR_ADDREQUEST never gets here: do_io_handler and
        // send_io_handler wait for it, or queue it, before they call
        // this, and a host sleep here would stall every task.
        _ => IOERR_NOCMD,
    }
}

/// `exec.library`'s `DoIO` (LVO -456: `A1` = `struct IORequest*`).
/// `D0` = `io_Error` (also written into the request itself, matching
/// real `DoIO`). Found missing while running the real `PhxAss`
/// assembler.
///
/// A `timer.device` `TR_ADDREQUEST` waits for its request's reply, as
/// real `DoIO` does: it suspends only the calling task, the way `Delay`
/// does ([`crate::dosdate::block_current_for`]), so the other tasks run
/// until its time is up -- to the microsecond, so a `sleep_for(100us)`
/// loop gives them the CPU too. A request already due when it starts
/// (`{0, 0}`, which code uses as a yield) still gives the other ready
/// tasks a turn, as the timer's reply does on real hardware. Like
/// `Delay`, this relies on those tasks reaching a library call
/// eventually: this runtime doesn't preempt, so a task that busy-waits
/// on memory without one keeps the CPU, and the `DoIO` caller only runs
/// again once it traps. The other timer commands complete at once, in
/// [`run_io_request`].
fn do_io_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let ioreq = ctx.cpu.address_register(AddressRegister(1));
    let device = ctx.mem.read_u32(ioreq + IO_DEVICE_OFFSET);
    let command = ctx.mem.read_u16(ioreq + IO_COMMAND_OFFSET);
    if device == TIMER_DEVICE_BASE && command == TR_ADDREQUEST {
        let (secs, micro) = read_timeval(ctx.mem, ioreq + TR_TIME_SECS_OFFSET);
        let duration = timeval_duration(secs, micro);
        let retrap = ctx.timers.has_delay(ctx.current_task);
        if crate::dosdate::block_current_for(ctx, duration) {
            return Ok(());
        }
        ctx.mem.write_u8(ioreq + IO_ERROR_OFFSET, 0);
        ctx.cpu.set_data_register(DataRegister(0), 0);
        if !retrap {
            // Due at once, without blocking: still a turn for the others.
            ctx.tasks.request_yield();
        }
        return Ok(());
    }
    let error = run_io_request(ctx.mem, ioreq);
    ctx.mem.write_u8(ioreq + IO_ERROR_OFFSET, error as u8);
    ctx.cpu
        .set_data_register(DataRegister(0), error as i32 as u32);
    Ok(())
}

/// `exec.library`'s `SendIO` (LVO -462: `A1` = `struct IORequest*`).
/// No return value (real `SendIO` is asynchronous and doesn't report
/// success/failure directly -- the caller finds out via `WaitIO`/
/// `CheckIO`/the reply message). `TR_GETSYSTIME`/`TR_SETSYSTIME` still
/// complete immediately, same as `DoIO` -- there's no meaningful delay
/// to defer. `TR_ADDREQUEST` (issue #237, sub-problem #2) is the one
/// real asynchronous case: this registers it with [`PendingTimers`]
/// and returns with the request genuinely still pending, instead of
/// suspending its caller until the deadline the way [`do_io_handler`]'s
/// `DoIO` does -- see [`PendingTimers`]'s doc for why `SendIO` can't do
/// that.
fn send_io_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let ioreq = ctx.cpu.address_register(AddressRegister(1));
    let device = ctx.mem.read_u32(ioreq + IO_DEVICE_OFFSET);
    let command = ctx.mem.read_u16(ioreq + IO_COMMAND_OFFSET);
    if device == TIMER_DEVICE_BASE && command == TR_ADDREQUEST {
        let (secs, micro) = read_timeval(ctx.mem, ioreq + TR_TIME_SECS_OFFSET);
        ctx.timers.push(
            ioreq,
            std::time::Instant::now() + timeval_duration(secs, micro),
            ctx.current_task,
        );
        return Ok(());
    }
    let error = run_io_request(ctx.mem, ioreq);
    ctx.mem.write_u8(ioreq + IO_ERROR_OFFSET, error as u8);
    Ok(())
}

/// `exec.library`'s `WaitIO` (LVO -474: `A1` = `struct IORequest*`).
/// `D0` = `io_Error`. Every request this runtime hands out other than
/// a `SendIO`'d `TR_ADDREQUEST` is already complete by the time
/// `SendIO`/`DoIO` returns, so this just reads back `io_Error` for
/// those. A still-pending timer request genuinely blocks a real
/// `WaitIO` until it completes; since nothing else host-side could
/// usefully happen in the meantime either way, this just completes it
/// immediately (see [`PendingTimers::complete_now`]'s doc) rather than
/// tracking actual wall-clock elapse.
fn wait_io_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let ioreq = ctx.cpu.address_register(AddressRegister(1));
    ctx.timers.complete_now(ctx.mem, ctx.tasks, ioreq);
    let error = ctx.mem.read_u8(ioreq + IO_ERROR_OFFSET);
    ctx.cpu
        .set_data_register(DataRegister(0), error as i8 as i32 as u32);
    Ok(())
}

/// `exec.library`'s `CheckIO` (LVO -468: `A1` = `struct IORequest*`).
/// `D0` = the request pointer (matching real `CheckIO`'s "still
/// non-`NULL` `A1` on completion" contract) if complete, or `0` if
/// still pending in [`PendingTimers`] -- the only request this runtime
/// ever leaves genuinely incomplete (issue #237, sub-problem #2).
/// Fires anything actually due first, same as [`wait_handler`]/
/// [`check_signal_handler`].
fn check_io_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let ioreq = ctx.cpu.address_register(AddressRegister(1));
    ctx.timers.fire_due(ctx.mem, ctx.tasks);
    let result = if ctx.timers.contains(ioreq) { 0 } else { ioreq };
    ctx.cpu.set_data_register(DataRegister(0), result);
    Ok(())
}

/// `exec.library`'s `AbortIO` (LVO -480: `A1` = `struct IORequest*`).
/// No return value. Every request this runtime hands out other than a
/// still-pending `TR_ADDREQUEST` has already completed synchronously
/// by the time `AbortIO` could possibly be called on it, matching real
/// `AbortIO`'s documented "no effect on an already-completed request"
/// behavior. A still-pending timer is genuinely cancelled -- removed
/// from [`PendingTimers`] without ever completing -- and `io_Error`
/// set to `IOERR_ABORTED`, matching real `timer.device`.
fn abort_io_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let ioreq = ctx.cpu.address_register(AddressRegister(1));
    if ctx.timers.remove(ioreq) {
        ctx.mem
            .write_u8(ioreq + IO_ERROR_OFFSET, IOERR_ABORTED as u8);
    }
    Ok(())
}

/// The current wall-clock time as AmigaOS-epoch (1978-01-01) seconds
/// plus microseconds -- the value `GetSysTime`/`TR_GETSYSTIME` report,
/// per the RKRM Devices book's "Timer Device" chapter ("By convention,
/// it tells how many seconds have passed since midnight, January 1,
/// 1978"). Reads the same host clock as
/// [`crate::dosdate::now_as_datestamp`], but keeps the microseconds
/// instead of going through DateStamp's 1/50s ticks: `ReadEClock` is
/// built on this too, and programs time short intervals with it. Also
/// reused by `crate::intuition`'s `CurrentTime`, which is documented to
/// report exactly this same epoch/units.
pub(crate) fn host_time_secs_micro() -> (u32, u32) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64 - crate::dosdate::UNIX_TO_AMIGA_EPOCH_DAYS * 86_400;
    if secs < 0 {
        return (0, 0);
    }
    (secs as u32, now.subsec_micros())
}

/// Reads a `struct timeval` (`tv_secs`/`tv_micro`, both `ULONG`) at
/// `addr`.
fn read_timeval(mem: &dyn AddressSpace, addr: u32) -> (u32, u32) {
    (mem.read_u32(addr), mem.read_u32(addr.wrapping_add(4)))
}

/// The time a `TR_ADDREQUEST` asks for, to the microsecond: rounding to
/// whole milliseconds would make a 900us wait return at once and a
/// 1900us one last 1ms.
fn timeval_duration(secs: u32, micro: u32) -> std::time::Duration {
    std::time::Duration::from_secs(u64::from(secs))
        + std::time::Duration::from_micros(u64::from(micro))
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
    let secs_carry = (micro / 1_000_000) as u32;
    micro %= 1_000_000;
    let secs = dsecs.wrapping_add(ssecs).wrapping_add(secs_carry);
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
    let mut secs_diff = (dsecs as i64) - (ssecs as i64);
    let mut micro_diff = (dmicro as i64) - (smicro as i64);
    while micro_diff < 0 {
        secs_diff -= 1;
        micro_diff += 1_000_000;
    }
    write_timeval(ctx.mem, dest, secs_diff as u32, micro_diff as u32);
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
///
/// That's the default. With `--clock-mhz` (issue #102, see
/// [`crate::backend::M68kCpu::set_clock_mhz`]) the *tick count* instead
/// comes from `ctx.cpu`'s accumulated emulated cycle count
/// ([`Cpu::emulated_cycles`]) divided by the configured clock rate
/// ([`Cpu::clock_hz`]) -- real emulated seconds, converted to
/// E-Clock-rate ticks the same way the host-clock path converts real
/// wall-clock seconds. The point is reproducible, host-load-independent
/// A/B benchmarking: two `ReadEClock` calls bracketing some guest work
/// now measure *emulated* elapsed time (derived from how many m68k
/// cycles the guest actually executed) rather than however long the
/// host machine happened to take, which varies with scheduling noise,
/// thermal throttling, whatever else is running, and so on.
///
/// **The rate reported in `D0` is always [`ECLOCK_PAL_HZ`], regardless
/// of `--clock-mhz`.** This is deliberate, not an oversight: on real
/// hardware the E-Clock is derived from the video/CIA timing chain, not
/// the CPU's own clock -- a PAL Amiga's E-Clock ticks at the same
/// 709,379 Hz whether it has a stock 68000 or an aftermarket
/// accelerator running many times faster, because the accelerator's
/// higher clock never touches the chipset's E-Clock source. So
/// `--clock-mhz` (which models a faster/slower *CPU*) has no business
/// changing the *rate value* a real program would read out of `D0` --
/// only the numerator (how much emulated time actually elapsed) is
/// meant to move. Reporting anything else in `D0` here would be a
/// self-inconsistent EClockVal: a caller that (correctly, per the RKRM)
/// divides an interval's tick delta by the rate in `D0` needs that rate
/// to be the same one the ticks were generated at.
fn read_eclock_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let dest = ctx.cpu.address_register(AddressRegister(0));
    let ticks = match ctx.cpu.clock_hz() {
        Some(clock_hz) => {
            let emulated_secs = ctx.cpu.emulated_cycles() as f64 / clock_hz;
            (emulated_secs * f64::from(ECLOCK_PAL_HZ)) as u64
        }
        None => {
            let (secs, micro) = host_time_secs_micro();
            u64::from(secs) * u64::from(ECLOCK_PAL_HZ)
                + u64::from(micro) * u64::from(ECLOCK_PAL_HZ) / 1_000_000
        }
    };
    ctx.mem.write_u32(dest, (ticks >> 32) as u32);
    ctx.mem.write_u32(dest.wrapping_add(4), ticks as u32);
    ctx.cpu.set_data_register(DataRegister(0), ECLOCK_PAL_HZ);
    Ok(())
}

/// Registers every implemented task/signal handler: `exec.library`'s
/// `FindTask`/`SetSignal`/`SetExcept`/`Wait`/`Signal`/`AllocSignal`/
/// `FreeSignal`/`StackSwap`/`Forbid`/`Permit`/`Disable`/`Enable`/`OpenDevice`/
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
    reg_exec!("SetTaskPri", set_task_pri_handler::<C>);
    reg_exec!("Alert", alert_handler::<C>);
    reg_exec!("SetSignal", set_signal_handler::<C>);
    reg_exec!("SetExcept", set_except_handler::<C>);
    reg_exec!("Wait", wait_handler::<C>);
    reg_exec!("Signal", signal_handler::<C>);
    reg_exec!("AllocSignal", alloc_signal_handler::<C>);
    reg_exec!("FreeSignal", free_signal_handler::<C>);
    reg_exec!("StackSwap", stack_swap_handler::<C>);
    reg_exec!("AddTask", add_task_handler::<C>);
    reg_exec!("RemTask", rem_task_handler::<C>);
    reg_exec!("Forbid", forbid_handler::<C>);
    reg_exec!("Permit", permit_handler::<C>);
    reg_exec!("Disable", disable_handler::<C>);
    reg_exec!("Enable", enable_handler::<C>);
    reg_exec!("OpenDevice", open_device_handler::<C>);
    reg_exec!("CloseDevice", close_device_handler::<C>);
    reg_exec!("DoIO", do_io_handler::<C>);
    reg_exec!("SendIO", send_io_handler::<C>);
    reg_exec!("WaitIO", wait_io_handler::<C>);
    reg_exec!("CheckIO", check_io_handler::<C>);
    reg_exec!("AbortIO", abort_io_handler::<C>);

    macro_rules! reg_dos {
        ($name:literal, $handler:expr) => {
            table
                .register_by_name(
                    mem,
                    DOS_LIBRARY_BASE,
                    DOS_LVOS,
                    "dos.library",
                    $name,
                    $handler,
                )
                .unwrap_or_else(|e| panic!("{} should be in DOS_LVOS: {e}", $name));
        };
    }
    reg_dos!("CheckSignal", check_signal_handler::<C>);
    reg_dos!("CreateNewProc", create_new_proc_handler::<C>);

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
    use crate::dispatch::{EXIT_STUB_ADDR, Runtime, StartConfig, TraceEvent};
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

    // --- SetTaskPri ---

    #[test]
    fn set_task_pri_returns_the_old_priority_sign_extended() {
        let _guard = lock_host_break();
        let mut words = Vec::new();
        words.push(move_imm_to_a(1)); // A1 = 0 (NULL)
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-294)); // FindTask(NULL) -> D0
        words.push(0x2240); // movea.l d0,a1
        words.push(0x70FB); // moveq #-5,d0
        words.extend_from_slice(&jsr_disp16_a6(-300)); // SetTaskPri(task, -5)
        words.push(0x7003); // moveq #3,d0
        words.extend_from_slice(&jsr_disp16_a6(-300)); // SetTaskPri(task, 3) -> D0 = -5
        words.push(RTS);

        let mut rt = exec_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, -5, "the old priority, sign-extended");
        assert_eq!(
            rt.memory()
                .read_u8(rt.current_task() + crate::execlist::LN_PRI) as i8,
            3
        );
    }

    // --- SetTaskPri scheduling (issue #254) ---

    /// Builds a program that spawns a `CreateNewProc` child at the
    /// parent's own priority (the real `NP_Priority` default), has the
    /// parent run `pri_program` (free to call `SetTaskPri` on itself
    /// however it likes -- returning its own words plus the relative
    /// index of each `move_imm_to_a(1)`'s immediate operand that needs
    /// the real parent task address patched in), then exits reporting
    /// whatever marker byte the child wrote (`0x2A` if the child got to
    /// run before the parent's own exit, `0` if it never did) -- shared
    /// by the two regression tests below, which differ only in
    /// `pri_program`.
    fn run_set_task_pri_scheduling_test(
        pri_program: impl FnOnce() -> (Vec<u16>, Vec<usize>),
    ) -> i32 {
        let _guard = lock_host_break();
        let mut words = Vec::new();

        // --- parent: CreateNewProc(tags); pri_program; read marker ---
        words.extend_from_slice(&movea_dos_base_to_a6());
        let tags_imm_idx = words.len();
        words.push(move_imm_to_d(1)); // D1 = tags (patched below)
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-498)); // CreateNewProc

        words.extend_from_slice(&movea_exec_base_to_a6());
        let pri_program_base = words.len();
        let (pri_words, parent_task_patch_offsets) = pri_program();
        words.extend_from_slice(&pri_words);

        let marker_imm_idx_parent = words.len();
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
        words.push(move_imm_to_d(0)); // D0 = 0x2A
        words.push(0);
        words.push(0x2A);
        words.push(move_d0_to_a_indirect(0)); // *marker_addr = D0
        words.push(RTS); // -> TASK_EXIT_STUB_ADDR, pre-pushed by CreateNewProc

        let entry = TRAP_TABLE_END;
        let code_end = entry + (words.len() as u32) * 2;
        let marker_addr = code_end + 16;
        let tags_addr = marker_addr + 16;
        let load_end = tags_addr + 32;

        words[tags_imm_idx + 1] = (tags_addr >> 16) as u16;
        words[tags_imm_idx + 2] = tags_addr as u16;
        words[marker_imm_idx_parent + 1] = (marker_addr >> 16) as u16;
        words[marker_imm_idx_parent + 2] = marker_addr as u16;
        words[marker_imm_idx_child + 1] = (marker_addr >> 16) as u16;
        words[marker_imm_idx_child + 2] = marker_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        mem.write_u32(marker_addr, 0);
        // tags: [NP_ENTRY, child_entry], [TAG_DONE(0), 0] -- no
        // NP_Priority, so the child inherits the parent's own current
        // priority, per real CreateNewProc's documented default.
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
        for offset in parent_task_patch_offsets {
            let patch_addr = entry + ((pri_program_base + offset) as u32) * 2;
            rt.memory_mut().write_u32(patch_addr, parent_task);
        }

        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed")
    }

    #[test]
    fn set_task_pri_to_the_same_priority_does_not_run_a_ready_task_of_that_priority() {
        // codewiz's issue #254 "yield-test.c", the "same" case: real
        // exec never reschedules on a tie, so the child (spawned at the
        // parent's own priority, CreateNewProc's default) must still
        // not have run by the time the parent reads the marker.
        let code = run_set_task_pri_scheduling_test(|| {
            let mut words = vec![
                move_imm_to_a(1), // A1 = parent task (patched below)
                0,
                0,
                move_imm_to_d(0), // D0 = 0 (same as the parent's own default priority)
                0,
                0,
            ];
            words.extend_from_slice(&jsr_disp16_a6(-300)); // SetTaskPri(self, same)
            (words, vec![1])
        });
        assert_eq!(
            code, 0,
            "same-priority SetTaskPri must not hand the CPU to an equal-priority ready task"
        );
    }

    #[test]
    fn set_task_pri_dip_then_restore_runs_a_ready_task_of_that_priority() {
        // codewiz's issue #254 "yield-test.c", the "dip and back" case
        // (`__gthread_yield()`'s real pattern): dropping below the
        // child's priority, even briefly, must hand it the CPU; the
        // child runs to completion (writing the marker) before the
        // parent's restoring second call ever gets to execute, let
        // alone its own exit.
        let code = run_set_task_pri_scheduling_test(|| {
            let mut words = vec![
                move_imm_to_a(1), // A1 = parent task (patched below)
                0,
                0,
                move_imm_to_d(0), // D0 = -1 (dip below the child's priority)
                0xFFFF,
                0xFFFF,
            ];
            words.extend_from_slice(&jsr_disp16_a6(-300)); // SetTaskPri(self, -1)
            let restore_patch_offset = words.len() + 1;
            words.push(move_imm_to_a(1)); // A1 = parent task again (patched below)
            words.push(0);
            words.push(0);
            words.push(move_imm_to_d(0)); // D0 = 0 (restore)
            words.push(0);
            words.push(0);
            words.extend_from_slice(&jsr_disp16_a6(-300)); // SetTaskPri(self, 0)
            (words, vec![1, restore_patch_offset])
        });
        assert_eq!(
            code, 0x2A,
            "dipping below a ready task's priority and restoring it must let that task run \
             during the dip"
        );
    }

    #[test]
    fn set_task_pri_dip_and_restore_inside_a_forbid_does_not_run_an_equal_priority_task() {
        // Issue #262: a reschedule that comes due inside `Forbid()` is
        // decided at the outermost `Permit()`, not at the
        // `SetTaskPri()`. Same dip-and-restore as above, but the whole
        // thing sits in a Forbid: by the Permit the parent is back at
        // the child's priority, and exec never preempts on a tie. (The
        // un-Forbidden version above hands the child the CPU during the
        // dip; deciding at the dip here would too.)
        let code = run_set_task_pri_scheduling_test(|| {
            let mut words = Vec::new();
            words.extend_from_slice(&jsr_disp16_a6(-132)); // Forbid
            let dip_patch_offset = words.len() + 1;
            words.push(move_imm_to_a(1)); // A1 = parent task (patched below)
            words.push(0);
            words.push(0);
            words.push(move_imm_to_d(0)); // D0 = -1
            words.push(0xFFFF);
            words.push(0xFFFF);
            words.extend_from_slice(&jsr_disp16_a6(-300)); // SetTaskPri(self, -1)
            let restore_patch_offset = words.len() + 1;
            words.push(move_imm_to_a(1)); // A1 = parent task again (patched below)
            words.push(0);
            words.push(0);
            words.push(move_imm_to_d(0)); // D0 = 0
            words.push(0);
            words.push(0);
            words.extend_from_slice(&jsr_disp16_a6(-300)); // SetTaskPri(self, 0)
            words.extend_from_slice(&jsr_disp16_a6(-138)); // Permit
            (words, vec![dip_patch_offset, restore_patch_offset])
        });
        assert_eq!(
            code, 0,
            "the parent was back at the child's priority by the Permit, so the child must \
             not have run"
        );
    }

    #[test]
    fn a_priority_drop_inside_a_forbid_takes_effect_at_the_outermost_permit() {
        // The other half of issue #262: deferring must not mean
        // dropping. The parent lowers itself below the ready child
        // inside *nested* Forbids and stays lowered, so the child must
        // run by the outermost Permit. This pins down the dispatch
        // wiring -- the request survives `SetTaskPri`'s epilogue and is
        // acted on at `Permit`'s; that nothing runs *before* the
        // outermost Permit is pinned down by the scheduler-level tests
        // in `crate::taskswitch`.
        let code = run_set_task_pri_scheduling_test(|| {
            let mut words = Vec::new();
            words.extend_from_slice(&jsr_disp16_a6(-132)); // Forbid
            words.extend_from_slice(&jsr_disp16_a6(-132)); // Forbid (nested)
            let dip_patch_offset = words.len() + 1;
            words.push(move_imm_to_a(1)); // A1 = parent task (patched below)
            words.push(0);
            words.push(0);
            words.push(move_imm_to_d(0)); // D0 = -1
            words.push(0xFFFF);
            words.push(0xFFFF);
            words.extend_from_slice(&jsr_disp16_a6(-300)); // SetTaskPri(self, -1)
            words.extend_from_slice(&jsr_disp16_a6(-138)); // Permit (inner: nothing yet)
            words.extend_from_slice(&jsr_disp16_a6(-138)); // Permit (outermost: child runs)
            (words, vec![dip_patch_offset])
        });
        assert_eq!(
            code, 0x2A,
            "the deferred reschedule must fire at the outermost Permit, with the parent still \
             below the child"
        );
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

        assert_eq!(rt.memory().read_u8(task_addr + LN_TYPE), NT_PROCESS);
        let name_ptr = rt.memory().read_u32(task_addr + LN_NAME);
        assert_ne!(name_ptr, 0);
        assert_eq!(
            crate::guestmem::read_c_string(rt.memory(), name_ptr),
            PROCESS_NAME
        );
    }

    #[test]
    fn pr_return_addr_and_cli_default_stack_match_the_runcommand_convention() {
        // Issues #178 and #279: ixemul.library's startup code computes
        // its stack bounds from pr_ReturnAddr and decides whether to
        // extend from cli_DefaultStack; vbcc's minres.o startup reads
        // its own saved data block at pr_ReturnAddr - 8, i.e. relative
        // to an initial sp of pr_ReturnAddr - 4.
        // FlatMemory::new(0x2_0000) with the default 64 KiB stack
        // (DEFAULT_STACK_SIZE) gives top = 0x20000, stack_base =
        // 0x10000, pr_ReturnAddr = 0x1FFF8, sp = 0x1FFF4.
        let rt = runtime_with_program(&[RTS]);
        let mem = rt.memory();
        let task = rt.current_task();

        let pr_return_addr = mem.read_u32(task + PR_RETURNADDR_OFFSET);
        assert_eq!(
            pr_return_addr, 0x1FFF8,
            "pr_ReturnAddr should point 4 bytes *above* the initial sp (0x1FFF4)"
        );
        // [pr_ReturnAddr]: the reserved stack size, in bytes -- the
        // distance from this longword down to the stack's lower bound,
        // as on real AmigaOS, so `lower = pr_ReturnAddr - size` is
        // exact (libnix's __init_stk does precisely that).
        let size = mem.read_u32(pr_return_addr);
        assert_eq!(size, 0x1FFF8 - 0x10000);
        assert_eq!(pr_return_addr - size, 0x10000);
        // [pr_ReturnAddr - 4]: the return address the outermost rts
        // lands on, which is also the initial sp.
        assert_eq!(
            mem.read_u32(pr_return_addr - 4),
            crate::dispatch::EXIT_STUB_ADDR,
            "pr_ReturnAddr - 4 is the exit-stub return address at the initial sp"
        );
        // upper = pr_ReturnAddr + 8: the stack region's top, the other
        // derivation ixemul uses (lower from it is 8 bytes
        // conservative, exactly as on real AmigaOS).
        assert_eq!(pr_return_addr + 8, 0x20000);

        let cli_bptr = mem.read_u32(task + PR_CLI_OFFSET);
        let cli_addr = crate::guestmem::addr_from_bptr(cli_bptr);
        assert_eq!(
            mem.read_u32(cli_addr + CLI_DEFAULT_STACK_OFFSET),
            0x10000 / 4,
            "cli_DefaultStack is the reserved stack size in longwords"
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

    /// Regression test for a real bug: a `BYTE` parameter like
    /// `AllocSignal`'s `signalNum` only guarantees `D0`'s low byte is
    /// meaningful -- a real compiler is not obligated to sign-extend the
    /// upper three bytes when passing `-1`. This reproduces exactly that
    /// real-world shape (`D0 = 0x000000FF`, not `0xFFFFFFFF`) -- what a
    /// real SAS/C-compiled `AllocSignal(-1)` call left in `D0`, caught
    /// via `bsdsocktest`'s own `alloc_signal()` wrapper failing every
    /// single allocation, even the very first one on a freshly booted
    /// task.
    #[test]
    fn alloc_signal_minus_one_as_a_plain_byte_still_means_any() {
        let _guard = lock_host_break();
        let mut words = Vec::new();
        words.push(move_imm_to_d(0)); // D0 = 0x000000FF -- byte -1, NOT sign-extended
        words.push(0x0000);
        words.push(0x00FF);
        words.extend_from_slice(&jsr_disp16_a6(-330)); // AllocSignal(-1)
        words.push(RTS);

        let mut rt = exec_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, 31,
            "a byte-only -1 (0x000000FF) must still mean \"any free bit\", not \"bit 255\""
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

    /// `MOVE.L D0,(A0)` (destination `(An)` indirect, source `Dn`
    /// direct). Generalizes `move_d0_to_d`'s base opcode the same way
    /// `movea_dn`-style helpers elsewhere in this crate generalize
    /// `movea_imm`'s: `0x2000` (size long, dest reg 0, dest mode `000`
    /// Dn, src mode `000` src reg `000`=D0) with the destination
    /// register substituted in at bits 11-9 and its mode changed to
    /// `010` ((An) indirect, bits 8-6).
    fn move_d0_to_a_indirect(an: u16) -> u16 {
        0x2080 | (an << 9)
    }

    /// `MOVE.L (A0),D0` (destination `Dn` direct, source `(An)`
    /// indirect) -- the load counterpart of
    /// [`move_d0_to_a_indirect`].
    fn move_a_indirect_to_d0(an: u16) -> u16 {
        0x2010 | an
    }

    /// The vbcc `minres.o` idiom, as a word sequence: stash `marker`
    /// at entry `sp - 4` (the startup's own data block), then recover
    /// it the way `_exit`/`_geta4` do -- through
    /// `ExecBase->ThisTask->pr_ReturnAddr - 8` -- leaving it in `D0`
    /// and the stack pointer where it started. See
    /// [`PR_RETURNADDR_OFFSET`]'s doc (issue #279).
    fn minres_stash_and_recover(marker: u16) -> Vec<u16> {
        vec![
            0x2F3C, 0, marker, // move.l #marker,-(sp)
            0x2C78, 0x0004, // movea.l (4).w,a6         ; ExecBase
            0x206E, 0x0114, // movea.l 276(a6),a0       ; ThisTask
            0x2068, 0x00B0, // movea.l 176(a0),a0       ; pr_ReturnAddr
            0x2028, 0xFFF8, // move.l -8(a0),d0         ; the stashed block
            0x588F, // addq.l #4,sp             ; drop the stash
        ]
    }

    #[test]
    fn minres_style_startup_recovers_its_block_through_pr_return_addr() {
        // Issue #279: vbcc's resident startup (`minres.o`) saves the
        // block it AllocMem'd at entry `sp - 4` and reads it back at
        // `pr_ReturnAddr - 8` to FreeMem it, which only works when the
        // entry frame has the stack size *above* the return address
        // (`sp == pr_ReturnAddr - 4`). volamos built that frame the
        // other way round, so an empty `vc +aos68kr` program died at
        // exit with a bogus FreeMem. The exit code here is the
        // recovered marker: a frame off by so much as a longword
        // reports something else.
        let _guard = lock_host_break();
        let mut words = minres_stash_and_recover(0x2A2A);
        words.push(RTS); // -> EXIT_STUB_ADDR, exit code = the marker

        let entry = TRAP_TABLE_END;
        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: entry + (words.len() as u32) * 2 + 16,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, 0x2A2A,
            "a minres.o-style startup must read back its own block at \
             pr_ReturnAddr - 8"
        );
    }

    #[test]
    fn create_new_proc_child_gets_its_own_pr_return_addr_frame() {
        // Issue #279, the CreateNewProc side: a child's stack gets the
        // same entry frame as the main task's, with pr_ReturnAddr
        // pointing into the child's own stack -- before this it was
        // left at 0 (and the child would have read the *parent's*
        // frame had it inherited one). The child runs the minres.o
        // idiom and leaves what it recovered in the marker the parent
        // reports, so a missing or misplaced frame reports something
        // other than the marker it stashed.
        let _guard = lock_host_break();
        let mut words = Vec::new();

        // --- parent: CreateNewProc(tags); Wait(join signal); read marker ---
        words.extend_from_slice(&movea_dos_base_to_a6());
        let tags_imm_idx = words.len();
        words.push(move_imm_to_d(1)); // D1 = tags (patched below)
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-498)); // CreateNewProc

        words.extend_from_slice(&movea_exec_base_to_a6());
        words.push(move_imm_to_d(0)); // D0 = 0x00010000 (the join signal)
        words.push(0x0001);
        words.push(0x0000);
        words.extend_from_slice(&jsr_disp16_a6(-318)); // Wait

        let marker_imm_idx_parent = words.len();
        words.push(move_imm_to_a(0)); // A0 = marker_addr (patched below)
        words.push(0);
        words.push(0);
        words.push(move_a_indirect_to_d0(0)); // D0 = *marker_addr
        words.push(RTS); // -> EXIT_STUB_ADDR, exit code = the marker

        // --- child: the minres.o idiom on its own stack; store what it
        // recovered in the marker; Signal(parent); exit ---
        let child_entry = TRAP_TABLE_END + (words.len() as u32) * 2;
        words.extend_from_slice(&minres_stash_and_recover(0x3B3B));
        let marker_imm_idx_child = words.len();
        words.push(move_imm_to_a(1)); // A1 = marker_addr (patched below)
        words.push(0);
        words.push(0);
        words.push(move_d0_to_a_indirect(1)); // *marker_addr = D0

        words.extend_from_slice(&movea_exec_base_to_a6());
        let parent_task_imm_idx = words.len();
        words.push(move_imm_to_a(1)); // A1 = parent task (patched below)
        words.push(0);
        words.push(0);
        words.push(move_imm_to_d(0)); // D0 = 0x00010000
        words.push(0x0001);
        words.push(0x0000);
        words.extend_from_slice(&jsr_disp16_a6(-324)); // Signal
        words.push(RTS); // -> TASK_EXIT_STUB_ADDR

        let entry = TRAP_TABLE_END;
        let code_end = entry + (words.len() as u32) * 2;
        let marker_addr = code_end + 16;
        let tags_addr = marker_addr + 16;
        let load_end = tags_addr + 32;

        words[tags_imm_idx + 1] = (tags_addr >> 16) as u16;
        words[tags_imm_idx + 2] = tags_addr as u16;
        words[marker_imm_idx_parent + 1] = (marker_addr >> 16) as u16;
        words[marker_imm_idx_parent + 2] = marker_addr as u16;
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
        let parent_task = rt.current_task();
        let parent_task_patch_addr = entry + (parent_task_imm_idx as u32) * 2 + 2;
        rt.memory_mut()
            .write_u32(parent_task_patch_addr, parent_task);

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, 0x3B3B,
            "the child should recover its own stashed block through its \
             own pr_ReturnAddr frame"
        );
    }

    #[test]
    fn create_new_proc_runs_a_second_task_that_signals_the_parent() {
        // End-to-end exercise of issue #121's whole mechanism: CreateNewProc
        // spawns a real second task; the parent blocks in Wait (switching to
        // the child, the only Ready task); the child runs its own code (on
        // its own stack, with its own registers -- proven by having it
        // write a marker byte only it knows), Signals the parent (marking
        // it Ready, without switching itself), then falls off the end into
        // the task-exit stub (switching back to the parent); the parent's
        // Wait call re-traps, now finds the signal satisfied, and resumes
        // for real. The marker the child wrote is what the parent's exit
        // code reports, so a wrong value anywhere in this chain -- the
        // child never running, running on the wrong stack, or the parent
        // resuming before the child actually ran -- fails the assertion.
        let _guard = lock_host_break();
        let mut words = Vec::new();

        // --- parent: CreateNewProc(tags); Wait(SIGNAL); read marker ---
        words.push(move_imm_to_a(6)); // A6 = DOS_LIBRARY_BASE
        words.push((DOS_LIBRARY_BASE >> 16) as u16);
        words.push(DOS_LIBRARY_BASE as u16);

        let tags_imm_idx = words.len();
        words.push(move_imm_to_d(1)); // D1 = tags (patched below)
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-498)); // CreateNewProc

        words.push(move_imm_to_a(6)); // A6 = EXEC_LIBRARY_BASE
        words.push((EXEC_LIBRARY_BASE >> 16) as u16);
        words.push(EXEC_LIBRARY_BASE as u16);
        words.push(move_imm_to_d(0)); // D0 = 0x00010000 (the join signal)
        words.push(0x0001);
        words.push(0x0000);
        words.extend_from_slice(&jsr_disp16_a6(-318)); // Wait

        let marker_imm_idx_parent = words.len();
        words.push(move_imm_to_a(0)); // A0 = marker_addr (patched below)
        words.push(0);
        words.push(0);
        words.push(move_a_indirect_to_d0(0)); // D0 = *marker_addr
        words.push(RTS); // -> EXIT_STUB_ADDR, exit code = the marker

        // --- child: write the marker; Signal(parent, join signal); fall
        // off the end into the task-exit stub ---
        let child_entry = TRAP_TABLE_END + (words.len() as u32) * 2;

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

        let parent_task_imm_idx = words.len();
        words.push(move_imm_to_a(1)); // A1 = parent task (patched below,
        words.push(0); // once the parent's task address is known --
        words.push(0); // see this test's note on why it's patched
        // post-construction rather than predicted here.
        words.push(move_imm_to_d(0)); // D0 = 0x00010000
        words.push(0x0001);
        words.push(0x0000);
        words.extend_from_slice(&jsr_disp16_a6(-324)); // Signal
        words.push(RTS); // -> TASK_EXIT_STUB_ADDR, pre-pushed on the
        // child's own stack by CreateNewProc.

        let entry = TRAP_TABLE_END;
        let code_end = entry + (words.len() as u32) * 2;
        let marker_addr = code_end + 16;
        let tags_addr = marker_addr + 16;
        let load_end = tags_addr + 32;

        words[tags_imm_idx + 1] = (tags_addr >> 16) as u16;
        words[tags_imm_idx + 2] = tags_addr as u16;
        words[marker_imm_idx_parent + 1] = (marker_addr >> 16) as u16;
        words[marker_imm_idx_parent + 2] = marker_addr as u16;
        words[marker_imm_idx_child + 1] = (marker_addr >> 16) as u16;
        words[marker_imm_idx_child + 2] = marker_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        mem.write_u32(marker_addr, 0);
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
        // The parent's task address depends on exactly how many heap
        // allocations Runtime::new made before create_current_task
        // (currently none, but that's an implementation detail this test
        // shouldn't have to predict) -- read it back from the real
        // Runtime instead of hardcoding it, and patch the child's
        // immediate operand with the real value.
        let parent_task = rt.current_task();
        let parent_task_patch_addr = entry + (parent_task_imm_idx as u32) * 2 + 2;
        rt.memory_mut()
            .write_u32(parent_task_patch_addr, parent_task);

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, 0x2A,
            "the parent should read the marker only the child task's own \
             code could have written, proving the child actually ran"
        );
    }

    #[test]
    fn create_new_proc_frees_the_childs_stack_and_process_struct_on_exit() {
        // codewiz's issue #254 "proc-mem.c" repro, scaled down from 50
        // processes to 3 (the mechanism is the same regardless of
        // count -- this just needs to prove a child's heap allocations
        // come back, not survive a long-running fuzz test): three
        // CreateNewProc children, one after another, each immediately
        // Signal-ing the parent and exiting. If their stack/`struct
        // Process`/name string are freed on exit (this fix), the
        // host-measured free heap space is identical before the first
        // spawn and after the last child's exit; before this fix, each
        // iteration leaked a stack's worth permanently.
        let _guard = lock_host_break();
        let mut words = Vec::new();

        // --- parent: [CreateNewProc(tags); Wait(SIGNAL)] x3; exit ---
        let mut tags_imm_idxs = Vec::new();
        for _ in 0..3 {
            words.push(move_imm_to_a(6)); // A6 = DOS_LIBRARY_BASE
            words.push((DOS_LIBRARY_BASE >> 16) as u16);
            words.push(DOS_LIBRARY_BASE as u16);
            tags_imm_idxs.push(words.len());
            words.push(move_imm_to_d(1)); // D1 = tags (patched below, same
            words.push(0); // tags block reused for every iteration --
            words.push(0); // nothing here mutates it)
            words.extend_from_slice(&jsr_disp16_a6(-498)); // CreateNewProc

            words.push(move_imm_to_a(6)); // A6 = EXEC_LIBRARY_BASE
            words.push((EXEC_LIBRARY_BASE >> 16) as u16);
            words.push(EXEC_LIBRARY_BASE as u16);
            words.push(move_imm_to_d(0)); // D0 = 0x00010000 (the join signal)
            words.push(0x0001);
            words.push(0x0000);
            words.extend_from_slice(&jsr_disp16_a6(-318)); // Wait
        }
        words.push(RTS); // -> EXIT_STUB_ADDR

        // --- child (one shared entry point, reused by all three spawns):
        // Signal(parent, join signal); fall off the end into the
        // task-exit stub ---
        let child_entry = TRAP_TABLE_END + (words.len() as u32) * 2;

        words.push(move_imm_to_a(6)); // A6 = EXEC_LIBRARY_BASE
        words.push((EXEC_LIBRARY_BASE >> 16) as u16);
        words.push(EXEC_LIBRARY_BASE as u16);
        let parent_task_imm_idx = words.len();
        words.push(move_imm_to_a(1)); // A1 = parent task (patched below)
        words.push(0);
        words.push(0);
        words.push(move_imm_to_d(0)); // D0 = 0x00010000
        words.push(0x0001);
        words.push(0x0000);
        words.extend_from_slice(&jsr_disp16_a6(-324)); // Signal
        words.push(RTS); // -> TASK_EXIT_STUB_ADDR, pre-pushed on each
        // child's own stack by CreateNewProc.

        let entry = TRAP_TABLE_END;
        let code_end = entry + (words.len() as u32) * 2;
        let tags_addr = code_end + 16;
        let load_end = tags_addr + 32;

        // One tags block, reused by all three (structurally identical)
        // CreateNewProc call sites.
        for idx in tags_imm_idxs {
            words[idx + 1] = (tags_addr >> 16) as u16;
            words[idx + 2] = tags_addr as u16;
        }

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
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
        let parent_task_patch_addr = entry + (parent_task_imm_idx as u32) * 2 + 2;
        rt.memory_mut()
            .write_u32(parent_task_patch_addr, parent_task);

        let free_before = rt.heap_mut().free_bytes();
        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed");
        let free_after = rt.heap_mut().free_bytes();
        assert_eq!(
            free_before, free_after,
            "each CreateNewProc child's stack, struct Process, and name string must be \
             fully freed by the time it exits -- any difference here is exactly the leak \
             issue #254 reported (64 KB per process, in the real repro)"
        );
    }

    #[test]
    fn create_new_proc_child_input_is_np_input_as_given() {
        // As the test above, but the child stores what Input() returns:
        // NP_Input's value, unchecked, as real DOS stores it in pr_CIS
        // (libpthread passes a thread's slot number there). The child
        // exits with the default NP_CloseInput, so the exit path also
        // closes that non-handle, harmlessly.
        let _guard = lock_host_break();
        let mut words = Vec::new();

        // --- parent: CreateNewProc(tags); Wait(SIGNAL); read marker ---
        words.push(move_imm_to_a(6)); // A6 = DOS_LIBRARY_BASE
        words.push((DOS_LIBRARY_BASE >> 16) as u16);
        words.push(DOS_LIBRARY_BASE as u16);
        let tags_imm_idx = words.len();
        words.push(move_imm_to_d(1)); // D1 = tags (patched below)
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-498)); // CreateNewProc
        words.push(move_imm_to_a(6)); // A6 = EXEC_LIBRARY_BASE
        words.push((EXEC_LIBRARY_BASE >> 16) as u16);
        words.push(EXEC_LIBRARY_BASE as u16);
        words.push(move_imm_to_d(0)); // D0 = 0x00010000 (the join signal)
        words.push(0x0001);
        words.push(0x0000);
        words.extend_from_slice(&jsr_disp16_a6(-318)); // Wait
        let marker_imm_idx_parent = words.len();
        words.push(move_imm_to_a(0)); // A0 = marker_addr (patched below)
        words.push(0);
        words.push(0);
        words.push(move_a_indirect_to_d0(0)); // D0 = *marker_addr
        words.push(RTS);

        // --- child: *marker = Input(); Signal(parent, join signal) ---
        let child_entry = TRAP_TABLE_END + (words.len() as u32) * 2;
        words.push(move_imm_to_a(6)); // A6 = DOS_LIBRARY_BASE
        words.push((DOS_LIBRARY_BASE >> 16) as u16);
        words.push(DOS_LIBRARY_BASE as u16);
        words.extend_from_slice(&jsr_disp16_a6(-54)); // Input
        let marker_imm_idx_child = words.len();
        words.push(move_imm_to_a(0)); // A0 = marker_addr (patched below)
        words.push(0);
        words.push(0);
        words.push(move_d0_to_a_indirect(0)); // *marker_addr = D0
        words.push(move_imm_to_a(6)); // A6 = EXEC_LIBRARY_BASE
        words.push((EXEC_LIBRARY_BASE >> 16) as u16);
        words.push(EXEC_LIBRARY_BASE as u16);
        let parent_task_imm_idx = words.len();
        words.push(move_imm_to_a(1)); // A1 = parent task (patched below)
        words.push(0);
        words.push(0);
        words.push(move_imm_to_d(0)); // D0 = 0x00010000
        words.push(0x0001);
        words.push(0x0000);
        words.extend_from_slice(&jsr_disp16_a6(-324)); // Signal
        words.push(RTS); // -> TASK_EXIT_STUB_ADDR

        let entry = TRAP_TABLE_END;
        let code_end = entry + (words.len() as u32) * 2;
        let marker_addr = code_end + 16;
        let tags_addr = marker_addr + 16;
        let load_end = tags_addr + 32;

        words[tags_imm_idx + 1] = (tags_addr >> 16) as u16;
        words[tags_imm_idx + 2] = tags_addr as u16;
        words[marker_imm_idx_parent + 1] = (marker_addr >> 16) as u16;
        words[marker_imm_idx_parent + 2] = marker_addr as u16;
        words[marker_imm_idx_child + 1] = (marker_addr >> 16) as u16;
        words[marker_imm_idx_child + 2] = marker_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        mem.write_u32(marker_addr, 0);
        // tags: [NP_ENTRY, child_entry], [NP_INPUT, 0x1234], [TAG_DONE, 0]
        mem.write_u32(tags_addr, NP_ENTRY);
        mem.write_u32(tags_addr + 4, child_entry);
        mem.write_u32(tags_addr + 8, NP_INPUT);
        mem.write_u32(tags_addr + 12, 0x1234);
        mem.write_u32(tags_addr + 16, 0);
        mem.write_u32(tags_addr + 20, 0);

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
        let parent_task_patch_addr = entry + (parent_task_imm_idx as u32) * 2 + 2;
        rt.memory_mut()
            .write_u32(parent_task_patch_addr, parent_task);

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 0x1234, "the child's Input() should be NP_Input");
    }

    #[test]
    fn task_switch_runs_tc_switch_and_tc_launch_on_the_outgoing_and_incoming_task() {
        // Issue #237: when the parent loses the CPU (blocking in Wait,
        // switching to the child) its tc_Switch must run, and when it
        // gets the CPU back (the child Signals it and exits) its
        // tc_Launch must run -- both real guest subroutines, not just
        // flags this runtime ignores. Exercised on the parent for both
        // halves (rather than needing the child's own, dynamically
        // allocated struct patched from guest code): the parent's own
        // task address is known before `rt.run()`, so TF_SWITCH/
        // TF_LAUNCH and tc_Switch/tc_Launch can be poked in directly,
        // same as the existing test patches the parent's task address
        // into the child's Signal call.
        let _guard = lock_host_break();
        let mut words = Vec::new();

        // --- parent: CreateNewProc(tags); Wait(SIGNAL); read marker ---
        words.push(move_imm_to_a(6)); // A6 = DOS_LIBRARY_BASE
        words.push((DOS_LIBRARY_BASE >> 16) as u16);
        words.push(DOS_LIBRARY_BASE as u16);

        let tags_imm_idx = words.len();
        words.push(move_imm_to_d(1)); // D1 = tags (patched below)
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-498)); // CreateNewProc

        words.push(move_imm_to_a(6)); // A6 = EXEC_LIBRARY_BASE
        words.push((EXEC_LIBRARY_BASE >> 16) as u16);
        words.push(EXEC_LIBRARY_BASE as u16);
        words.push(move_imm_to_d(0)); // D0 = 0x00010000 (the join signal)
        words.push(0x0001);
        words.push(0x0000);
        words.extend_from_slice(&jsr_disp16_a6(-318)); // Wait

        let marker_imm_idx_parent = words.len();
        words.push(move_imm_to_a(0)); // A0 = marker_addr (patched below)
        words.push(0);
        words.push(0);
        words.push(move_a_indirect_to_d0(0)); // D0 = *marker_addr
        words.push(RTS); // -> EXIT_STUB_ADDR, exit code = the marker

        // --- child: write the marker; Signal(parent, join signal); fall
        // off the end into the task-exit stub ---
        let child_entry = TRAP_TABLE_END + (words.len() as u32) * 2;

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

        let parent_task_imm_idx = words.len();
        words.push(move_imm_to_a(1)); // A1 = parent task (patched below)
        words.push(0);
        words.push(0);
        words.push(move_imm_to_d(0)); // D0 = 0x00010000
        words.push(0x0001);
        words.push(0x0000);
        words.extend_from_slice(&jsr_disp16_a6(-324)); // Signal
        words.push(RTS); // -> TASK_EXIT_STUB_ADDR

        // --- tc_Switch: *switch_marker_addr = 0x53 ('S'); rts (into
        // the trampoline's continuation stub, not a real caller) ---
        let switch_entry = TRAP_TABLE_END + (words.len() as u32) * 2;
        let switch_marker_imm_idx = words.len();
        words.push(move_imm_to_a(0)); // A0 = switch_marker_addr (patched below)
        words.push(0);
        words.push(0);
        words.push(move_imm_to_d(0)); // D0 = 0x53
        words.push(0);
        words.push(0x53);
        words.push(move_d0_to_a_indirect(0));
        words.push(RTS);

        // --- tc_Launch: *launch_marker_addr = 0x4C ('L'); rts ---
        let launch_entry = TRAP_TABLE_END + (words.len() as u32) * 2;
        let launch_marker_imm_idx = words.len();
        words.push(move_imm_to_a(0)); // A0 = launch_marker_addr (patched below)
        words.push(0);
        words.push(0);
        words.push(move_imm_to_d(0)); // D0 = 0x4C
        words.push(0);
        words.push(0x4C);
        words.push(move_d0_to_a_indirect(0));
        words.push(RTS);

        let entry = TRAP_TABLE_END;
        let code_end = entry + (words.len() as u32) * 2;
        let marker_addr = code_end + 16;
        let switch_marker_addr = marker_addr + 16;
        let launch_marker_addr = switch_marker_addr + 16;
        let tags_addr = launch_marker_addr + 16;
        let load_end = tags_addr + 32;

        words[tags_imm_idx + 1] = (tags_addr >> 16) as u16;
        words[tags_imm_idx + 2] = tags_addr as u16;
        words[marker_imm_idx_parent + 1] = (marker_addr >> 16) as u16;
        words[marker_imm_idx_parent + 2] = marker_addr as u16;
        words[marker_imm_idx_child + 1] = (marker_addr >> 16) as u16;
        words[marker_imm_idx_child + 2] = marker_addr as u16;
        words[switch_marker_imm_idx + 1] = (switch_marker_addr >> 16) as u16;
        words[switch_marker_imm_idx + 2] = switch_marker_addr as u16;
        words[launch_marker_imm_idx + 1] = (launch_marker_addr >> 16) as u16;
        words[launch_marker_imm_idx + 2] = launch_marker_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        mem.write_u32(marker_addr, 0);
        mem.write_u32(switch_marker_addr, 0);
        mem.write_u32(launch_marker_addr, 0);
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
        let parent_task_patch_addr = entry + (parent_task_imm_idx as u32) * 2 + 2;
        rt.memory_mut()
            .write_u32(parent_task_patch_addr, parent_task);

        // Give the parent TF_SWITCH/TF_LAUNCH and real tc_Switch/
        // tc_Launch pointers -- the parent's own task struct, known and
        // writable before the run starts, unlike the child's.
        rt.memory_mut()
            .write_u8(parent_task + TC_FLAGS, TF_SWITCH | TF_LAUNCH);
        rt.memory_mut()
            .write_u32(parent_task + TC_SWITCH_OFFSET, switch_entry);
        rt.memory_mut()
            .write_u32(parent_task + TC_LAUNCH_OFFSET, launch_entry);

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, 0x2A,
            "the end-to-end CreateNewProc/Wait/Signal flow must still work \
             with tc_Switch/tc_Launch wired in"
        );
        assert_eq!(
            rt.memory().read_u32(switch_marker_addr),
            0x53,
            "tc_Switch must run when the parent loses the CPU (blocking in Wait)"
        );
        assert_eq!(
            rt.memory().read_u32(launch_marker_addr),
            0x4C,
            "tc_Launch must run when the parent gets the CPU back (the child \
             Signals and exits)"
        );
    }

    #[test]
    fn wait_on_nothing_pending_with_no_other_task_deadlocks_loudly() {
        // Issue #121: Wait now tries to block-and-switch before
        // failing, but with no other task ever created there is
        // nothing to switch to, so this must still fail loudly rather
        // than hang.
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
            msg.contains("would deadlock"),
            "expected a deadlock diagnostic, got: {msg}"
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
        // Opt in to the fold this test actually exercises (issue #210):
        // without this, `rt.run`'s dispatch loop no longer touches
        // PENDING_HOST_BREAK at all, and every other test module's own
        // Runtime::run calls can't clobber this one's flag either.
        rt.enable_host_break_folding();

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
        // See the opt-in comment in
        // pending_host_break_folds_into_check_signal above.
        rt.enable_host_break_folding();

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

    /// Builds a program that calls `StackSwap` once on a `StackSwapStruct`
    /// at a fixed address, with `init_struct` free to fill in a
    /// deliberately invalid `stk_Lower`/`stk_Upper`/`stk_Pointer` before
    /// the run -- for the sanity-check regression tests below. Feedback
    /// from a separate tool built on this runtime: a corrupted or
    /// never-initialized `StackSwapStruct` otherwise runs on silently
    /// until the failure surfaces as something far removed from this call
    /// site (most often `EmptyContinuationStack`), rather than failing
    /// loudly right here.
    fn run_stack_swap_with_struct(
        init_struct: impl FnOnce(&mut FlatMemory, u32),
    ) -> crate::dispatch::RuntimeError {
        let _guard = lock_host_break();
        let entry = TRAP_TABLE_END;

        let mut words = Vec::new();
        words.extend_from_slice(&movea_exec_base_to_a6());
        words.push(move_imm_to_a(0)); // A0 = struct_addr (patched below)
        let a0_patch = words.len();
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-732)); // StackSwap
        words.push(RTS);

        let code_len = words.len() as u32 * 2;
        let struct_addr = entry + code_len;
        let load_end = struct_addr + 32;

        words[a0_patch] = (struct_addr >> 16) as u16;
        words[a0_patch + 1] = struct_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        init_struct(&mut mem, struct_addr);

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
        rt.run(&mut out, None)
            .expect_err("StackSwap with an invalid StackSwapStruct should fail loudly")
    }

    fn assert_stack_swap_failed(err: crate::dispatch::RuntimeError) {
        match err {
            crate::dispatch::RuntimeError::Dispatch(DispatchError::HandlerFailed {
                library,
                lvo,
                handler_name,
                ..
            }) => {
                assert_eq!(library, "exec.library");
                assert_eq!(lvo, -732);
                assert_eq!(handler_name, "StackSwap");
            }
            other => panic!("expected HandlerFailed, got {other:?}"),
        }
    }

    #[test]
    fn stack_swap_rejects_inverted_bounds() {
        let err = run_stack_swap_with_struct(|mem, struct_addr| {
            // stk_Lower > stk_Upper: never produced by a correctly
            // initialized struct.
            mem.write_u32(struct_addr, 0x5000); // stk_Lower
            mem.write_u32(struct_addr + 4, 0x4000); // stk_Upper
            mem.write_u32(struct_addr + 8, 0x4800); // stk_Pointer
        });
        assert_stack_swap_failed(err);
    }

    #[test]
    fn stack_swap_rejects_pointer_outside_its_own_bounds() {
        let err = run_stack_swap_with_struct(|mem, struct_addr| {
            mem.write_u32(struct_addr, 0x4000); // stk_Lower
            mem.write_u32(struct_addr + 4, 0x5000); // stk_Upper
            mem.write_u32(struct_addr + 8, 0x6000); // stk_Pointer, past stk_Upper
        });
        assert_stack_swap_failed(err);
    }

    #[test]
    fn stack_swap_rejects_a_pointer_with_no_room_for_the_return_address() {
        let err = run_stack_swap_with_struct(|mem, struct_addr| {
            mem.write_u32(struct_addr, 0x4000); // stk_Lower
            mem.write_u32(struct_addr + 4, 0x5000); // stk_Upper
            mem.write_u32(struct_addr + 8, 0x4000); // stk_Pointer == stk_Lower: no room to push
        });
        assert_stack_swap_failed(err);
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

    // --- Disable/Enable ---

    #[test]
    fn disable_then_enable_is_a_harmless_no_op() {
        let _guard = lock_host_break();
        let mut words = Vec::new();
        words.extend_from_slice(&jsr_disp16_a6(-120)); // Disable
        words.extend_from_slice(&jsr_disp16_a6(-126)); // Enable
        words.push(0x7000 | 0x2A); // moveq #42,d0
        words.push(RTS);

        let mut rt = exec_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, 42,
            "Disable/Enable should leave D0 untouched by anyone else"
        );
    }

    #[test]
    fn nested_disable_enable_pairs_do_not_fail() {
        let _guard = lock_host_break();
        let mut words = Vec::new();
        // Mimic a guest nesting Disable/Forbid critical sections, e.g.
        // Disable(); Forbid(); ...; Permit(); Enable();
        words.extend_from_slice(&jsr_disp16_a6(-120)); // Disable
        words.extend_from_slice(&jsr_disp16_a6(-120)); // Disable (nested)
        words.extend_from_slice(&jsr_disp16_a6(-132)); // Forbid
        words.extend_from_slice(&jsr_disp16_a6(-138)); // Permit
        words.extend_from_slice(&jsr_disp16_a6(-126)); // Enable
        words.extend_from_slice(&jsr_disp16_a6(-126)); // Enable (matching)
        words.push(0x7000 | 0x2A); // moveq #42,d0
        words.push(RTS);

        let mut rt = exec_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 42, "nested Disable/Enable pairs should not fail");
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
    fn task_exit_wakes_a_task_blocked_only_on_a_pending_timer() {
        // Issue #237, sub-problem #2 follow-up, found by codewiz: a
        // CreateNewProc child falling off the end (task_exit_stub_
        // handler -> terminate_current) used to fail outright if the
        // only other task was blocked in Wait purely on a still-
        // pending TR_ADDREQUEST's signal -- `pick_ready` only ever
        // looks for `Ready` tasks, and a timer's wake is never that
        // until it actually fires. The parent genuinely isn't
        // deadlocked here (the timer would wake it on its own even if
        // the child never exited), so the child falling off first
        // must not report one either -- same "fire the only future
        // event left" fix as wait_handler's own retry loop, just on
        // the other side of the exact same race. (The fix itself
        // landed in #241; this is the dedicated regression test for
        // it #241 didn't add.)
        let _guard = lock_host_break();
        let entry = TRAP_TABLE_END;
        let name = b"timer.device\0";

        let mut words = movea_dos_base_to_a6().to_vec();
        let tags_idx = words.len();
        words.push(move_imm_to_d(1)); // D1 = tags (patched below)
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-498)); // CreateNewProc

        words.extend_from_slice(&movea_exec_base_to_a6());
        let name_idx = words.len();
        words.push(move_imm_to_a(0)); // A0 = name (patched below)
        words.push(0);
        words.push(0);
        words.push(0x7000); // moveq #0,d0 (unit)
        let ioreq_idx1 = words.len();
        words.push(move_imm_to_a(1)); // A1 = ioreq (patched below)
        words.push(0);
        words.push(0);
        words.push(0x7200); // moveq #0,d1 (flags)
        words.extend_from_slice(&jsr_disp16_a6(-444)); // OpenDevice

        let ioreq_idx2 = words.len();
        words.push(move_imm_to_a(1)); // A1 = ioreq (patched below)
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-462)); // SendIO

        words.push(move_imm_to_d(0)); // D0 = the reply port's signal bit
        words.push(0);
        words.push(0x0010); // bit 4 -> 0x10
        words.extend_from_slice(&jsr_disp16_a6(-318)); // Wait

        let marker_idx = words.len();
        words.push(move_imm_to_a(0)); // A0 = marker (patched below)
        words.push(0);
        words.push(0);
        words.push(move_d0_to_a_indirect(0)); // *marker = D0 (Wait's result)
        words.push(RTS);

        // --- child: immediately falls off into the task-exit stub,
        // without ever signalling the parent -- only the timer is left.
        let child_entry = entry + (words.len() as u32) * 2;
        words.push(RTS);

        let code_end = entry + (words.len() as u32) * 2;
        let name_addr = code_end + 16;
        let ioreq_addr = (name_addr + name.len() as u32 + 3) & !3;
        let reply_port_addr = ioreq_addr + 64;
        let marker_addr = reply_port_addr + 64;
        let tags_addr = marker_addr + 16;
        let load_end = tags_addr + 32;

        words[tags_idx + 1] = (tags_addr >> 16) as u16;
        words[tags_idx + 2] = tags_addr as u16;
        words[name_idx + 1] = (name_addr >> 16) as u16;
        words[name_idx + 2] = name_addr as u16;
        words[ioreq_idx1 + 1] = (ioreq_addr >> 16) as u16;
        words[ioreq_idx1 + 2] = ioreq_addr as u16;
        words[ioreq_idx2 + 1] = (ioreq_addr >> 16) as u16;
        words[ioreq_idx2 + 2] = ioreq_addr as u16;
        words[marker_idx + 1] = (marker_addr >> 16) as u16;
        words[marker_idx + 2] = marker_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        crate::guestmem::write_c_string(&mut mem, name_addr, name);
        for i in 0..64 {
            mem.write_u8(ioreq_addr + i, 0);
            mem.write_u8(reply_port_addr + i, 0);
        }
        mem.write_u32(marker_addr, 0);
        mem.write_u32(tags_addr, NP_ENTRY);
        mem.write_u32(tags_addr + 4, child_entry);
        mem.write_u32(tags_addr + 8, 0);
        mem.write_u32(tags_addr + 12, 0);

        crate::execlist::init_list_header(&mut mem, reply_port_addr + crate::execlist::MP_MSGLIST);
        mem.write_u8(reply_port_addr + crate::execlist::MP_SIGBIT, 4);
        mem.write_u16(ioreq_addr + IO_COMMAND_OFFSET, TR_ADDREQUEST);
        mem.write_u32(ioreq_addr + crate::execlist::MN_REPLYPORT, reply_port_addr);
        // Short but real -- PendingTimers::fire_earliest genuinely
        // sleeps out the remaining duration now (see the previous
        // test's doc), so this doesn't need to be long to prove
        // anything; it just needs to actually complete.
        mem.write_u32(ioreq_addr + TR_TIME_SECS_OFFSET, 0);
        mem.write_u32(ioreq_addr + TR_TIME_MICRO_OFFSET, 50_000);

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
        rt.memory_mut()
            .write_u32(reply_port_addr + crate::execlist::MP_SIGTASK, parent_task);

        let started = std::time::Instant::now();
        let mut out = Vec::new();
        let code = rt
            .run(&mut out, None)
            .expect("the parent must resume once the child exits, not deadlock");
        let elapsed = started.elapsed();

        assert_eq!(
            code, 0x10,
            "the parent's Wait should resume with the timer's own bit satisfied"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "should resolve well within a couple of seconds for a 50ms request -- \
             took {elapsed:?}"
        );
    }

    #[test]
    fn a_pending_timer_request_completes_on_time_during_another_tasks_delay() {
        // codewiz's issue #254 "delay-time.c" repro: a real pending
        // timer request in one task must still complete on its own
        // schedule even while a *different* task is blocked in
        // `Delay` for much longer -- before this fix, `Delay` was a
        // flat host sleep that gave nothing else (including
        // `PendingTimers`' own bookkeeping) a chance to run, so the
        // parent's 50ms timer request would only have completed once
        // the child's much longer Delay finished.
        //
        // Parent: OpenDevice+SendIO a short (50ms) real timer request,
        // spawn a child that immediately Delays for far longer
        // (500ms), then Wait on the request's own reply-port signal.
        // With the fix, the parent's Wait resolves around 50ms (the
        // timer firing wakes it while the child is still deep in its
        // own Delay); measuring total elapsed time well under the
        // child's 500ms distinguishes this from the pre-fix behavior.
        let _guard = lock_host_break();
        let entry = TRAP_TABLE_END;
        let name = b"timer.device\0";

        let mut words = movea_exec_base_to_a6().to_vec();
        let name_idx = words.len();
        words.push(move_imm_to_a(0)); // A0 = name (patched below)
        words.push(0);
        words.push(0);
        words.push(0x7000); // moveq #0,d0 (unit)
        let ioreq_idx1 = words.len();
        words.push(move_imm_to_a(1)); // A1 = ioreq (patched below)
        words.push(0);
        words.push(0);
        words.push(0x7200); // moveq #0,d1 (flags)
        words.extend_from_slice(&jsr_disp16_a6(-444)); // OpenDevice

        let ioreq_idx2 = words.len();
        words.push(move_imm_to_a(1)); // A1 = ioreq (patched below)
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-462)); // SendIO

        words.extend_from_slice(&movea_dos_base_to_a6());
        let tags_idx = words.len();
        words.push(move_imm_to_d(1)); // D1 = tags (patched below)
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-498)); // CreateNewProc

        words.extend_from_slice(&movea_exec_base_to_a6());
        words.push(move_imm_to_d(0)); // D0 = the reply port's signal bit
        words.push(0);
        words.push(0x0010); // bit 4 -> 0x10
        words.extend_from_slice(&jsr_disp16_a6(-318)); // Wait

        let marker_idx = words.len();
        words.push(move_imm_to_a(0)); // A0 = marker (patched below)
        words.push(0);
        words.push(0);
        words.push(move_d0_to_a_indirect(0)); // *marker = D0 (Wait's result)
        words.push(RTS);

        // --- child: Delay(25) [500ms], far longer than the parent's
        // own 50ms timer request, then fall off the end ---
        let child_entry = entry + (words.len() as u32) * 2;
        words.extend_from_slice(&movea_dos_base_to_a6());
        words.push(move_imm_to_d(1)); // D1 = 25 ticks (500ms)
        words.push(0);
        words.push(25);
        words.extend_from_slice(&jsr_disp16_a6(-198)); // Delay
        words.push(RTS);

        let code_end = entry + (words.len() as u32) * 2;
        let name_addr = code_end + 16;
        let ioreq_addr = (name_addr + name.len() as u32 + 3) & !3;
        let reply_port_addr = ioreq_addr + 64;
        let marker_addr = reply_port_addr + 64;
        let tags_addr = marker_addr + 16;
        let load_end = tags_addr + 32;

        words[name_idx + 1] = (name_addr >> 16) as u16;
        words[name_idx + 2] = name_addr as u16;
        words[ioreq_idx1 + 1] = (ioreq_addr >> 16) as u16;
        words[ioreq_idx1 + 2] = ioreq_addr as u16;
        words[ioreq_idx2 + 1] = (ioreq_addr >> 16) as u16;
        words[ioreq_idx2 + 2] = ioreq_addr as u16;
        words[tags_idx + 1] = (tags_addr >> 16) as u16;
        words[tags_idx + 2] = tags_addr as u16;
        words[marker_idx + 1] = (marker_addr >> 16) as u16;
        words[marker_idx + 2] = marker_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        crate::guestmem::write_c_string(&mut mem, name_addr, name);
        for i in 0..64 {
            mem.write_u8(ioreq_addr + i, 0);
            mem.write_u8(reply_port_addr + i, 0);
        }
        mem.write_u32(marker_addr, 0);
        mem.write_u32(tags_addr, NP_ENTRY);
        mem.write_u32(tags_addr + 4, child_entry);
        mem.write_u32(tags_addr + 8, 0);
        mem.write_u32(tags_addr + 12, 0);

        crate::execlist::init_list_header(&mut mem, reply_port_addr + crate::execlist::MP_MSGLIST);
        mem.write_u8(reply_port_addr + crate::execlist::MP_SIGBIT, 4);
        mem.write_u16(ioreq_addr + IO_COMMAND_OFFSET, TR_ADDREQUEST);
        mem.write_u32(ioreq_addr + crate::execlist::MN_REPLYPORT, reply_port_addr);
        mem.write_u32(ioreq_addr + TR_TIME_SECS_OFFSET, 0);
        mem.write_u32(ioreq_addr + TR_TIME_MICRO_OFFSET, 50_000); // 50ms

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
        rt.memory_mut()
            .write_u32(reply_port_addr + crate::execlist::MP_SIGTASK, parent_task);

        let started = std::time::Instant::now();
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        let elapsed = started.elapsed();

        assert_eq!(
            code, 0x10,
            "the parent's Wait should resume with the timer's own bit satisfied"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(300),
            "the 50ms timer request must complete on its own schedule, not wait for the \
             child's unrelated 500ms Delay to finish first -- took {elapsed:?}"
        );
    }

    #[test]
    fn pending_timers_push_replaces_a_duplicate_ioreq_without_double_linking() {
        // Code review finding: SendIO on an ioreq that's still pending
        // (never WaitIO/CheckIO/AbortIO'd) used to add a second
        // PendingTimer entry for the same address. Both would
        // eventually fire and double-link the same message Node onto
        // its reply port. push must replace, not duplicate.
        let mut timers = PendingTimers::new();
        let deadline1 = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let deadline2 = std::time::Instant::now() + std::time::Duration::from_secs(20);
        timers.push(0x1500, deadline1, 0x1000);
        timers.push(0x1500, deadline2, 0x1000);
        assert_eq!(
            timers.entries.len(),
            1,
            "push must replace, not duplicate, a still-pending ioreq"
        );
    }

    #[test]
    fn pending_timers_remove_owned_by_drops_only_the_exiting_tasks_entries() {
        // Code review finding: a task falling off its NP_Entry while
        // it still had a pending SendIO left that entry dangling,
        // tracked only by the raw ioreq address -- liable to fire
        // later against whatever a future allocation reuses that
        // address for. task_exit_stub_handler now drops its own
        // task's entries without completing them; a different task's
        // entries must be left alone.
        let mut timers = PendingTimers::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        timers.push(0x1500, deadline, 0x1000);
        timers.push(0x1600, deadline, 0x2000);
        timers.remove_owned_by(0x1000);
        assert!(
            !timers.contains(0x1500),
            "the exiting task's own entry must be dropped"
        );
        assert!(
            timers.contains(0x1600),
            "a different task's entry must be untouched"
        );
    }

    #[test]
    fn pending_timers_remove_drops_a_still_pending_entry_by_exact_address() {
        // The same primitive FreeMem now calls (execmem::
        // free_mem_handler) so a still-pending SendIO's backing memory
        // being freed doesn't leave a dangling PendingTimers entry.
        let mut timers = PendingTimers::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        timers.push(0x1500, deadline, 0x1000);
        assert!(timers.remove(0x1500));
        assert!(!timers.contains(0x1500));
    }

    #[test]
    fn send_io_add_request_completes_via_wait_without_blocking_the_host() {
        // Issue #237, sub-problem #2: SendIO(TR_ADDREQUEST) must return
        // immediately (not block this whole single-threaded runtime in
        // a host sleep -- that would defeat the entire point of racing
        // a timer against some other signal in a later Wait). The
        // later Wait() on the reply port's own signal bit must then
        // actually see it complete, which (nothing else is runnable
        // here) goes through PendingTimers::fire_earliest's own real
        // sleep until the deadline -- found missing by codewiz: firing
        // instantly regardless of the real deadline made a timed wait
        // measure ~0ms elapsed whenever nothing else was ever going to
        // be Ready (the common case for a single pthread using a timed
        // wait as a plain delay). So this asserts two different things
        // about elapsed time, via the trace callback's per-dispatch
        // timestamps: SendIO's own dispatch returns near-instantly
        // (proving it isn't the one blocking), while Wait's dispatch
        // -- whose entire sleep happens before it returns, hence before
        // its own trace event fires -- takes roughly the full requested
        // duration (proving the sleep happened there, for real, and
        // wasn't skipped).
        let _guard = lock_host_break();
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

        words.push(move_imm_to_a(1)); // A1 = ioreq, for SendIO
        let a1_sendio_idx = words.len();
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-462)); // SendIO

        words.push(move_imm_to_d(0)); // D0 = the reply port's signal bit
        words.push(0);
        words.push(0x0010); // bit 4 -> 0x10
        words.extend_from_slice(&jsr_disp16_a6(-318)); // Wait

        let marker_idx = words.len();
        words.push(move_imm_to_a(0)); // A0 = marker (patched below)
        words.push(0);
        words.push(0);
        words.push(move_d0_to_a_indirect(0)); // *marker = D0 (Wait's result)
        words.push(RTS);

        let name_addr = entry + (movea_exec_base_to_a6().len() + words.len()) as u32 * 2;
        let ioreq_addr = (name_addr + name.len() as u32 + 3) & !3;
        let reply_port_addr = ioreq_addr + 64;
        let marker_addr = reply_port_addr + 64;
        let load_end = marker_addr + 16;

        words[name_idx] = (name_addr >> 16) as u16;
        words[name_idx + 1] = name_addr as u16;
        words[ioreq_idx] = (ioreq_addr >> 16) as u16;
        words[ioreq_idx + 1] = ioreq_addr as u16;
        words[a1_sendio_idx] = (ioreq_addr >> 16) as u16;
        words[a1_sendio_idx + 1] = ioreq_addr as u16;
        words[marker_idx + 1] = (marker_addr >> 16) as u16;
        words[marker_idx + 2] = marker_addr as u16;

        let mut full = movea_exec_base_to_a6().to_vec();
        full.extend_from_slice(&words);
        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &full);
        crate::guestmem::write_c_string(&mut mem, name_addr, name);
        for i in 0..64 {
            mem.write_u8(ioreq_addr + i, 0);
            mem.write_u8(reply_port_addr + i, 0);
        }
        mem.write_u32(marker_addr, 0);

        // The timerequest's mn_ReplyPort, built by hand exactly like
        // real pthread/newlib code would (a real AllocSignal'd bit, a
        // real owning task) rather than via CreateMsgPort (whose
        // placeholder mp_SigBit, per crate::execlist's module doc,
        // isn't meant to be a real one).
        crate::execlist::init_list_header(&mut mem, reply_port_addr + crate::execlist::MP_MSGLIST);
        mem.write_u8(reply_port_addr + crate::execlist::MP_SIGBIT, 4);

        let io_command_addr = ioreq_addr + IO_COMMAND_OFFSET;
        mem.write_u16(io_command_addr, TR_ADDREQUEST);
        mem.write_u32(ioreq_addr + crate::execlist::MN_REPLYPORT, reply_port_addr);
        // A short-but-measurable real duration: long enough that an
        // instantly-fired timer (0ms elapsed) and a genuinely-slept
        // one (~this long) are unambiguously distinguishable, short
        // enough not to slow the test suite down noticeably.
        const REQUESTED_MS: u64 = 200;
        mem.write_u32(ioreq_addr + TR_TIME_SECS_OFFSET, 0);
        mem.write_u32(
            ioreq_addr + TR_TIME_MICRO_OFFSET,
            (REQUESTED_MS * 1000) as u32,
        );

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
        rt.memory_mut()
            .write_u32(reply_port_addr + crate::execlist::MP_SIGTASK, parent_task);

        let started = std::time::Instant::now();
        let mut send_io_elapsed = None;
        let mut wait_elapsed = None;
        let mut out = Vec::new();
        let code = rt
            .run(
                &mut out,
                Some(&mut |ev: &TraceEvent| match ev.handler_name.as_str() {
                    "SendIO" => send_io_elapsed = Some(started.elapsed()),
                    "Wait" => wait_elapsed = Some(started.elapsed()),
                    _ => {}
                }),
            )
            .expect("run should succeed");

        let send_io_elapsed = send_io_elapsed.expect("SendIO should have been dispatched");
        let wait_elapsed = wait_elapsed.expect("Wait should have been dispatched");

        assert_eq!(
            code, 0x10,
            "exit code is whatever D0 held at RTS -- Wait's own return value, since \
             nothing overwrites D0 between Wait and the marker write"
        );
        assert_eq!(
            rt.memory().read_u32(marker_addr),
            0x10,
            "Wait(0x10) should return with the timer's own signal bit satisfied"
        );
        assert!(
            send_io_elapsed < std::time::Duration::from_millis(50),
            "SendIO(TR_ADDREQUEST) must return near-instantly, not block the host for \
             anywhere close to the requested {REQUESTED_MS}ms -- took {send_io_elapsed:?}"
        );
        assert!(
            wait_elapsed >= std::time::Duration::from_millis(REQUESTED_MS)
                && wait_elapsed < std::time::Duration::from_secs(2),
            "Wait's pending-timer fallback must actually sleep for the real remaining \
             duration once nothing else is runnable, not fire the timer instantly -- \
             took {wait_elapsed:?} for a {REQUESTED_MS}ms request"
        );
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
    fn do_io_timer_request_lets_another_ready_task_run() {
        // DoIO(TR_ADDREQUEST) waits for its request the way Delay
        // does (dosdate's delay_lets_another_ready_task_run): a
        // CreateNewProc child spawned right before the parent's 100ms
        // DoIO must get the CPU during it and write its marker. As a
        // flat host sleep it starved every other task: libstdc++'s
        // std::this_thread::sleep_for sleeps through DoIO, so a thread
        // waiting that way for another one hung.
        do_io_timer_request_runs_the_child(0, 100_000);
    }

    #[test]
    fn a_sub_millisecond_do_io_timer_request_lets_another_task_run() {
        // sleep_for(100us): it used to round to a zero wait that never
        // switched. Either the wait or the yield of a request due at
        // once now gives the child its turn; timeval_duration's test
        // pins the resolution.
        do_io_timer_request_runs_the_child(0, 100);
    }

    #[test]
    fn timeval_duration_keeps_the_microseconds() {
        use std::time::Duration;
        for (secs, micro, expected) in [
            (0, 0, Duration::ZERO),
            (0, 100, Duration::from_micros(100)),
            (0, 1_900, Duration::from_micros(1_900)),
            (1, 500_000, Duration::from_millis(1_500)),
        ] {
            assert_eq!(
                timeval_duration(secs, micro),
                expected,
                "{{{secs}, {micro}}}"
            );
        }
    }

    #[test]
    fn a_zero_do_io_timer_request_yields_to_another_ready_task() {
        // {0, 0}, used as a yield: due at once, it still gives the
        // ready child a turn before the parent goes on.
        do_io_timer_request_runs_the_child(0, 0);
    }

    /// The parent creates a child that writes a marker, then calls
    /// `DoIO(TR_ADDREQUEST)` for `secs`/`micro` and returns the marker:
    /// the child must have run by the time `DoIO` returns.
    fn do_io_timer_request_runs_the_child(secs: u32, micro: u32) {
        const NP_ENTRY: u32 = 0x8000_0000 + 1000 + 3; // TAG_USER + NP_Dummy + NP_Entry

        fn patch(words: &mut [u16], idx: usize, value: u32) {
            words[idx] = (value >> 16) as u16;
            words[idx + 1] = value as u16;
        }

        let entry = TRAP_TABLE_END;
        let mut words = Vec::new();

        // --- parent: CreateNewProc(tags); DoIO(ioreq); return marker ---
        words.extend_from_slice(&movea_dos_base_to_a6());
        words.push(0x223C); // move.l #imm32,D1 = tags (patched below)
        let tags_idx = words.len();
        words.extend_from_slice(&[0, 0]);
        words.extend_from_slice(&jsr_disp16_a6(-498)); // CreateNewProc
        words.extend_from_slice(&movea_exec_base_to_a6());
        words.push(move_imm_to_a(1)); // A1 = ioreq (patched below)
        let ioreq_idx = words.len();
        words.extend_from_slice(&[0, 0]);
        words.extend_from_slice(&jsr_disp16_a6(-456)); // DoIO
        words.push(move_imm_to_a(0)); // A0 = marker (patched below)
        let marker_idx = words.len();
        words.extend_from_slice(&[0, 0]);
        words.push(move_a_indirect_to_d0(0)); // D0 = *marker
        words.push(RTS); // -> EXIT_STUB_ADDR, exit code = the marker

        // --- child: write the marker and exit ---
        let child_entry = entry + (words.len() as u32) * 2;
        words.push(move_imm_to_a(0)); // A0 = marker (patched below)
        let marker_idx_child = words.len();
        words.extend_from_slice(&[0, 0]);
        words.push(0x7000 | 0x2A); // moveq #0x2A,d0
        words.push(move_d0_to_a_indirect(0)); // *marker = D0
        words.push(RTS); // -> TASK_EXIT_STUB_ADDR, pre-pushed by CreateNewProc

        let code_end = entry + (words.len() as u32) * 2;
        let marker_addr = code_end + 16;
        let tags_addr = marker_addr + 16;
        let ioreq_addr = tags_addr + 32;
        let load_end = ioreq_addr + 64;
        patch(&mut words, tags_idx, tags_addr);
        patch(&mut words, ioreq_idx, ioreq_addr);
        patch(&mut words, marker_idx, marker_addr);
        patch(&mut words, marker_idx_child, marker_addr);

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        mem.write_u32(marker_addr, 0);
        mem.write_u32(tags_addr, NP_ENTRY);
        mem.write_u32(tags_addr + 4, child_entry);
        mem.write_u32(tags_addr + 8, 0);
        mem.write_u32(tags_addr + 12, 0);
        for i in 0..40 {
            mem.write_u8(ioreq_addr + i, 0);
        }
        mem.write_u32(ioreq_addr + IO_DEVICE_OFFSET, TIMER_DEVICE_BASE);
        mem.write_u16(ioreq_addr + IO_COMMAND_OFFSET, TR_ADDREQUEST);
        mem.write_u32(ioreq_addr + TR_TIME_SECS_OFFSET, secs);
        mem.write_u32(ioreq_addr + TR_TIME_MICRO_OFFSET, micro);

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
            "the child must have run and written the marker during the parent's \
             DoIO({secs}s, {micro}us), not been starved of the CPU"
        );
        assert_eq!(rt.memory().read_u8(ioreq_addr + IO_ERROR_OFFSET), 0);
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
    fn dl_timereq_idiom_fetches_io_device_and_calls_read_eclock() {
        // libnix's clock() (clock.o in libnix20.a) bypasses
        // OpenDevice/DoIO entirely: it reads
        // ((struct DosLibrary*)DOSBase)->dl_TimeReq->io_Device directly
        // and calls through it at LVO -60 (ReadEClock). Before
        // dl_TimeReq was populated this read back the sentinel prefill
        // left at DOS_LIBRARY_BASE's unmodeled positive offsets, so the
        // call jumped through a bogus address and crashed -- see
        // dispatch::DOS_DL_TIMEREQ_OFFSET's doc.
        let _guard = lock_host_break();
        let entry = TRAP_TABLE_END;

        let mut words = movea_dos_base_to_a6().to_vec(); // A6 = DOS_LIBRARY_BASE
        words.push(0x206E); // movea.l (58,a6),a0 -- A0 = dl_TimeReq
        words.push(0x003A);
        words.push(0x2C68); // movea.l (0x14,a0),a6 -- A6 = io_Device
        words.push(0x0014);
        words.push(move_imm_to_a(0)); // A0 = dest eclock (patched)
        let dest_idx = words.len();
        words.push(0);
        words.push(0);
        words.push(0x4EAE); // jsr -60(a6) -- ReadEClock
        words.push(0xFFC4);
        words.push(RTS);

        let dest_addr = entry + (words.len() as u32) * 2 + 16;
        words[dest_idx] = (dest_addr >> 16) as u16;
        words[dest_idx + 1] = dest_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: dest_addr + 16,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, ECLOCK_PAL_HZ as i32,
            "ReadEClock's D0 return (eclock ticks/sec)"
        );
        assert_ne!(
            rt.memory().read_u32(dest_addr),
            0,
            "or the eclock high word"
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
    fn add_time_normal_microseconds_carry() {
        // (0s, 800_000µs) + (0s, 700_000µs) = (1s, 500_000µs). Normal carry
        // path with valid microseconds (< 1_000_000 each).
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
        words.push(RTS);

        let dest_addr = entry + (words.len() as u32) * 2 + 4;
        let src_addr = dest_addr + 8;
        words[dest_idx] = (dest_addr >> 16) as u16;
        words[dest_idx + 1] = dest_addr as u16;
        words[src_idx] = (src_addr >> 16) as u16;
        words[src_idx + 1] = src_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        mem.write_u32(dest_addr, 0);
        mem.write_u32(dest_addr + 4, 800_000);
        mem.write_u32(src_addr, 0);
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
        rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(rt.memory().read_u32(dest_addr), 1, "AddTime tv_secs");
        assert_eq!(
            rt.memory().read_u32(dest_addr + 4),
            500_000,
            "AddTime tv_micro"
        );
    }

    #[test]
    fn sub_time_borrow_wraps_seconds() {
        // (0s, 0µs) - (1s, 0µs) = (-1s, 0µs) = (u32::MAX, 0). Tests that
        // the borrow loop handles secs_diff = -1 correctly and wrapping.
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
        words.extend_from_slice(&jsr_disp16_a6(-48)); // SubTime
        words.push(RTS);

        let dest_addr = entry + (words.len() as u32) * 2 + 4;
        let src_addr = dest_addr + 8;
        words[dest_idx] = (dest_addr >> 16) as u16;
        words[dest_idx + 1] = dest_addr as u16;
        words[src_idx] = (src_addr >> 16) as u16;
        words[src_idx + 1] = src_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        mem.write_u32(dest_addr, 0);
        mem.write_u32(dest_addr + 4, 0);
        mem.write_u32(src_addr, 1);
        mem.write_u32(src_addr + 4, 0);

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
        rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            rt.memory().read_u32(dest_addr),
            u32::MAX,
            "SubTime tv_secs (wraps)"
        );
        assert_eq!(rt.memory().read_u32(dest_addr + 4), 0, "SubTime tv_micro");
    }

    #[test]
    fn sub_time_normal_borrow() {
        // (1s, 0µs) - (0s, 100_000µs) = (0s, 900_000µs). Normal single
        // borrow path — verifies the loop works for the common case.
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
        words.extend_from_slice(&jsr_disp16_a6(-48)); // SubTime
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
        mem.write_u32(dest_addr + 4, 0);
        mem.write_u32(src_addr, 0);
        mem.write_u32(src_addr + 4, 100_000);

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
        rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(rt.memory().read_u32(dest_addr), 0, "SubTime tv_secs");
        assert_eq!(
            rt.memory().read_u32(dest_addr + 4),
            900_000,
            "SubTime tv_micro"
        );
    }

    #[test]
    fn add_time_out_of_spec_micros() {
        // (0s, 2_000_000µs) + (3s, 0us) = (5s, 0us).
        // Micro > 1_000_000 is out of spec (must be normalized per
        // timer.device docs) but the fix handles it correctly.
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
        words.push(RTS);

        let dest_addr = entry + (words.len() as u32) * 2 + 4;
        let src_addr = dest_addr + 8;
        words[dest_idx] = (dest_addr >> 16) as u16;
        words[dest_idx + 1] = dest_addr as u16;
        words[src_idx] = (src_addr >> 16) as u16;
        words[src_idx + 1] = src_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        mem.write_u32(dest_addr, 0);
        mem.write_u32(dest_addr + 4, 2_000_000);
        mem.write_u32(src_addr, 3);
        mem.write_u32(src_addr + 4, 0);

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
        rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(rt.memory().read_u32(dest_addr), 5, "AddTime tv_secs");
        assert_eq!(rt.memory().read_u32(dest_addr + 4), 0, "AddTime tv_micro");
    }

    #[test]
    fn sub_time_out_of_spec_micros() {
        // (4s, 0µs) - (0s, 2_000_000µs) = (2s, 0µs).
        // Micro > 1_000_000 is out of spec but handled correctly.
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
        words.extend_from_slice(&jsr_disp16_a6(-48)); // SubTime
        words.push(RTS);

        let dest_addr = entry + (words.len() as u32) * 2 + 4;
        let src_addr = dest_addr + 8;
        words[dest_idx] = (dest_addr >> 16) as u16;
        words[dest_idx + 1] = dest_addr as u16;
        words[src_idx] = (src_addr >> 16) as u16;
        words[src_idx + 1] = src_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        mem.write_u32(dest_addr, 4);
        mem.write_u32(dest_addr + 4, 0);
        mem.write_u32(src_addr, 0);
        mem.write_u32(src_addr + 4, 2_000_000);

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
        rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(rt.memory().read_u32(dest_addr), 2, "SubTime tv_secs");
        assert_eq!(rt.memory().read_u32(dest_addr + 4), 0, "SubTime tv_micro");
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

    /// The host clock behind `GetSysTime` and `ReadEClock` must resolve
    /// microseconds, not DateStamp's 1/50s ticks: with the latter, every
    /// `tv_micro` was a multiple of 20000 and every interval timed with
    /// `ReadEClock` a multiple of 20 ms.
    #[test]
    fn host_time_has_microsecond_resolution() {
        let off_tick = (0..1000).any(|_| !host_time_secs_micro().1.is_multiple_of(20_000));
        assert!(off_tick, "every sample fell on a 1/50s boundary");
    }

    // --- ReadEClock under --clock-mhz (issue #102) ---

    #[test]
    fn read_eclock_reports_the_pal_rate_in_d0_even_under_clock_mhz_mode() {
        // The single most load-bearing detail of the whole --clock-mhz
        // feature: the *rate* ReadEClock reports in D0 must stay
        // ECLOCK_PAL_HZ regardless of the configured clock -- see
        // read_eclock_handler's doc comment for why (real hardware
        // derives the E-Clock from the chipset's video/CIA timing, not
        // the CPU's own clock, so a faster/slower emulated CPU has no
        // business changing the reported rate, only the numerator).
        let _guard = lock_host_break();
        let entry = TRAP_TABLE_END;

        let mut words = Vec::new();
        words.push(move_imm_to_a(6)); // A6 = TIMER_DEVICE_BASE
        words.push((TIMER_DEVICE_BASE >> 16) as u16);
        words.push(TIMER_DEVICE_BASE as u16);
        words.push(move_imm_to_a(0)); // A0 = EClockVal dest (patched)
        let ev_idx = words.len();
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-60)); // ReadEClock -> D0 = rate
        words.push(RTS);

        let ev_addr = entry + (words.len() as u32) * 2 + 4;
        words[ev_idx] = (ev_addr >> 16) as u16;
        words[ev_idx + 1] = ev_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);

        let mut cpu = M68kCpu::new();
        cpu.set_clock_mhz(Some(7.14)); // an arbitrary, non-round rate
        let mut rt = Runtime::new(
            cpu,
            mem,
            StartConfig {
                entry,
                load_end: ev_addr + 8,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code as u32, ECLOCK_PAL_HZ,
            "ReadEClock's D0 rate must stay ECLOCK_PAL_HZ regardless of --clock-mhz"
        );
    }

    #[test]
    fn read_eclock_under_clock_mhz_mode_derives_ticks_from_emulated_cycles_not_wall_clock() {
        // Two ReadEClock calls, bracketing a handful of ordinary guest
        // instructions, at a deliberately very slow configured clock
        // (100 Hz -- `set_clock_mhz`'s `mhz` argument is in MHz, so
        // 0.0001). At that rate even the tiny number of real 68000
        // cycles a few NOPs and their surrounding MOVEA/JSR setup cost
        // (tens of cycles) works out to hundreds of milliseconds of
        // *emulated* elapsed time -- hundreds of thousands of E-Clock
        // ticks. A host wall clock, by contrast, cannot produce
        // anywhere near that many ticks for a handful of synchronous
        // instructions inside a single host function call (a few
        // microseconds at most, worth low single-digit ticks at
        // ECLOCK_PAL_HZ's ~0.71 ticks/microsecond). So a large observed
        // delta here can only come from the cycle-derived formula, not
        // from `host_time_secs_micro`, which is exactly what this test
        // pins down: read_eclock_handler must actually be branching on
        // `Cpu::clock_hz` rather than falling through to the host-clock
        // path by accident.
        let _guard = lock_host_break();
        let entry = TRAP_TABLE_END;

        let mut words = Vec::new();
        words.push(move_imm_to_a(6)); // A6 = TIMER_DEVICE_BASE
        words.push((TIMER_DEVICE_BASE >> 16) as u16);
        words.push(TIMER_DEVICE_BASE as u16);
        words.push(move_imm_to_a(0)); // A0 = ev1 dest (patched)
        let ev1_idx = words.len();
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-60)); // ReadEClock #1
        words.push(0x4E71); // NOP
        words.push(0x4E71); // NOP
        words.push(0x4E71); // NOP -- "guest instructions between them"
        words.push(move_imm_to_a(0)); // A0 = ev2 dest (patched)
        let ev2_idx = words.len();
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-60)); // ReadEClock #2
        words.push(RTS);

        let ev1_addr = entry + (words.len() as u32) * 2 + 4;
        let ev2_addr = ev1_addr + 8;
        words[ev1_idx] = (ev1_addr >> 16) as u16;
        words[ev1_idx + 1] = ev1_addr as u16;
        words[ev2_idx] = (ev2_addr >> 16) as u16;
        words[ev2_idx + 1] = ev2_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);

        let mut cpu = M68kCpu::new();
        cpu.set_clock_mhz(Some(0.0001)); // 100 Hz
        let mut rt = Runtime::new(
            cpu,
            mem,
            StartConfig {
                entry,
                load_end: ev2_addr + 8,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code as u32, ECLOCK_PAL_HZ,
            "ReadEClock's D0 rate must stay ECLOCK_PAL_HZ under --clock-mhz too"
        );

        let ev1 = (u64::from(rt.memory().read_u32(ev1_addr)) << 32)
            | u64::from(rt.memory().read_u32(ev1_addr + 4));
        let ev2 = (u64::from(rt.memory().read_u32(ev2_addr)) << 32)
            | u64::from(rt.memory().read_u32(ev2_addr + 4));
        assert!(
            ev2 > ev1,
            "the second ReadEClock should report a later tick count than the first"
        );
        let delta = ev2 - ev1;
        assert!(
            delta > 50_000,
            "a handful of guest instructions at 100 Hz should be hundreds of thousands of \
             E-Clock ticks apart if the delta is cycle-derived, not wall-clock-derived; got \
             {delta}"
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
