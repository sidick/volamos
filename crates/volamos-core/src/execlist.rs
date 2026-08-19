//! `exec.library` list/node primitives and single-threaded message ports
//! (Phase 3 stage 4).
//!
//! Per `docs/plan.md`'s Phase 3 scope, this module implements list/node
//! and message-port primitives "to the degree single-threaded CLI tools
//! touch them" -- i.e. faithfully enough that a guest program which
//! itself walks or builds these structures directly (rather than going
//! exclusively through the LVOs below) sees exactly the same bytes a real
//! AmigaOS kernel would have left there, but with no actual task
//! scheduling, signalling, or cross-process delivery behind it (there is
//! only ever one guest "task" running).
//!
//! # `struct Node` / `struct List` layout
//!
//! ```text
//! struct Node {              struct List {
//!     APTR  ln_Succ;   +0        struct Node *lh_Head;      +0
//!     APTR  ln_Pred;   +4        struct Node *lh_Tail;      +4
//!     UBYTE ln_Type;   +8        struct Node *lh_TailPred;  +8
//!     BYTE  ln_Pri;    +9        UBYTE        lh_Type;      +12
//!     char *ln_Name;   +10       UBYTE        lh_pad;       +13
//! };                  (14)   };                            (14)
//! ```
//!
//! Both structs are deliberately the same size (14 bytes) with the same
//! first two 4-byte fields -- this is not a coincidence, it's the classic
//! Exec "sentinel" trick every real list-manipulation routine below
//! depends on: a `struct List` can be treated as *two overlapping fake
//! `Node`s* sharing memory with it (one representing "the node before the
//! head", one "the node after the tail"), which is what lets `AddHead`/
//! `AddTail`/`Remove`/`RemHead`/`RemTail`/`Insert` avoid ever
//! special-casing an empty list. [`init_list_header`] sets this up exactly
//! as the real `NewList()` macro does:
//!
//! ```text
//! lh_Head     = &lh_Tail        (a fake Node whose ln_Succ is 0)
//! lh_Tail     = NULL
//! lh_TailPred = &lh_Head        (a fake Node whose ln_Pred is 0)
//! ```
//!
//! With that in place, walking `node = node->ln_Succ` from `lh_Head`
//! naturally terminates when `node` becomes the address `&lh_Tail`
//! itself, whose own `ln_Succ` field *is* `lh_Tail`, which is `0` --  no
//! `if (node == NULL)` check ever needed, exactly matching real Exec
//! semantics (and real Exec's actual C source, ported here as literally
//! as the register-based calling convention allows). Every function below
//! is transcribed from the well-known `exec/lists.c` algorithms (these
//! are, like the LVO offset facts documented in `docs/plan.md`'s fd/SFD
//! section, bare non-copyrightable algorithmic facts -- there is exactly
//! one way to implement a doubly-linked list with O(1) head/tail
//! operations and no NULL-check branches), not reverse-engineered or
//! guessed at.
//!
//! [`init_list_header`] is `pub` specifically so future modules (task
//! lists, library lists, ...) can reuse it -- there is no `NewList` LVO
//! (it's an `amiga.lib` macro, not a real library call), so this is the
//! only place that logic needs to live.
//!
//! # Message ports: single-threaded simplifications
//!
//! ```text
//! struct MsgPort {                    struct Message {
//!     struct Node mp_Node;    +0..14      struct Node mn_Node;   +0..14
//!     UBYTE   mp_Flags;       +14         struct MsgPort         +14
//!     UBYTE   mp_SigBit;      +15            *mn_ReplyPort;
//!     void   *mp_SigTask;     +16         UWORD mn_Length;       +18
//!     struct List mp_MsgList; +20..34  };                        (20)
//! };                                  (34)
//! ```
//!
//! [`create_msg_port_handler`] allocates a 34-byte `MsgPort` off the guest
//! heap and initializes it close to real `CreateMsgPort`, with one
//! deliberate simplification: real `CreateMsgPort` calls `AllocSignal` to
//! grab a real signal bit and stores the *current task*'s pointer in
//! `mp_SigTask`, so `Wait()`/`Signal()` can actually wake the owning task
//! up. This runtime doesn't call `AllocSignal` on the guest's behalf here
//! (no signal bit is actually reserved for the port), and is
//! single-threaded regardless (there's only ever the one fake current
//! task -- see `crate::exectask`'s module docs, added in Phase 3 stage
//! 5), so:
//!
//! - `mp_Flags` = [`PA_SIGNAL`] (`0`), matching real `CreateMsgPort`'s
//!   default.
//! - `mp_SigBit` = [`MSGPORT_SIGBIT_PLACEHOLDER`] (`0`), a fixed
//!   placeholder rather than an actually-allocated signal bit -- nothing
//!   in this runtime ever tests it, since [`GetMsg`] never blocks (see
//!   below) and there's no `Wait`/`Signal` implementation to consult it.
//! - `mp_SigTask` = [`HandlerContext::current_task`], the fake current
//!   task's guest address (Phase 3 stage 5 added `FindTask`/`Wait`/
//!   `Signal` -- see `crate::exectask`'s module docs). This is now the
//!   real value a guest program would see from real `CreateMsgPort`;
//!   it's still not meaningfully *actionable* here, since nothing in
//!   this runtime ever calls `Wait`/`Signal` on a guest's behalf itself
//!   -- a guest that reads `mp_SigTask` back just sees a consistent,
//!   correct pointer.
//!
//! [`GetMsg`] (LVO -372) never blocks: it's a plain `RemHead` of
//! `mp_MsgList`, returning `NULL` on an empty port instead of suspending
//! the (nonexistent) calling task -- exactly matching real `GetMsg`'s own
//! contract ("does NOT wait ... returns NULL if the port was empty"; the
//! *blocking* wait is a separate call, `WaitPort`, not implemented here).
//! [`PutMsg`] never signals anything (no task to signal). [`ReplyMsg`]
//! follows the real contract precisely: if the message's `mn_ReplyPort`
//! is non-`NULL`, it's queued there (as [`PutMsg`] would) with
//! `ln_Type` set to [`NT_REPLYMSG`]; if `NULL`, real `ReplyMsg` still
//! marks the message replied via `ln_Type` but has nowhere to deliver it
//! (see the Autodoc's own "if message has no reply port... [it is] not
//! sent anywhere" language) -- reproduced here identically, no delivery,
//! type set only.
//!
//! # `AddPort`/`RemPort`/`FindPort`: no public port registry
//!
//! Real `AddPort`/`RemPort` maintain Exec's own private, priority-ordered
//! public port list (keyed by `mp_Node.ln_Name`), which `FindPort` then
//! searches by name. This module takes the explicitly-sanctioned minimal
//! fallback (see `docs/plan.md`'s task description for this stage): no
//! host- or guest-side public port list is maintained at all.
//! [`add_port_handler`] only sets `ln_Type` = [`NT_MSGPORT`] on the port
//! (matching what real `AddPort` does to the node before enqueueing it) so
//! a guest that inspects its own port's type sees the right value;
//! [`rem_port_handler`] is a pure no-op; [`find_port_handler`] always
//! returns `NULL`, as if the name were never found -- documented,
//! acceptable behavior per the task brief for single-threaded CLI tools
//! that don't rely on cross-process port discovery (explicitly out of
//! scope: `docs/plan.md`'s "Out of scope" section already rules out
//! "cross-process IPC/message-port bridging").

use crate::cpu::{AddressRegister, Cpu, DataRegister};
use crate::dispatch::{DispatchError, EXEC_LIBRARY_BASE, HandlerContext, LibraryTable};
use crate::guestmem::{GuestHeap, read_c_string};
use crate::lvos::exec::EXEC_LVOS;
use crate::memory::AddressSpace;

// --- struct Node field offsets (bytes from the node's own address) ---

/// `ln_Succ`: `APTR`, offset 0.
pub const LN_SUCC: u32 = 0;
/// `ln_Pred`: `APTR`, offset 4.
pub const LN_PRED: u32 = 4;
/// `ln_Type`: `UBYTE`, offset 8.
pub const LN_TYPE: u32 = 8;
/// `ln_Pri`: `BYTE` (signed), offset 9.
pub const LN_PRI: u32 = 9;
/// `ln_Name`: `char *`, offset 10.
pub const LN_NAME: u32 = 10;
/// `sizeof(struct Node)`.
pub const NODE_SIZE: u32 = 14;

// --- struct List field offsets (bytes from the list header's own
// address) ---

/// `lh_Head`: `struct Node *`, offset 0.
pub const LH_HEAD: u32 = 0;
/// `lh_Tail`: `struct Node *`, offset 4.
pub const LH_TAIL: u32 = 4;
/// `lh_TailPred`: `struct Node *`, offset 8.
pub const LH_TAILPRED: u32 = 8;
/// `lh_Type`: `UBYTE`, offset 12.
pub const LH_TYPE: u32 = 12;
/// `lh_pad`: `UBYTE`, offset 13.
pub const LH_PAD: u32 = 13;
/// `sizeof(struct List)` (and `sizeof(struct MinList)` -- identical
/// layout for the fields this module touches).
pub const LIST_SIZE: u32 = 14;

// --- struct MsgPort field offsets ---

/// `mp_Node`: `struct Node`, offset 0 (so a `MsgPort` pointer and its
/// embedded node's address are the same value).
pub const MP_NODE: u32 = 0;
/// `mp_Flags`: `UBYTE`, offset 14.
pub const MP_FLAGS: u32 = 14;
/// `mp_SigBit`: `UBYTE`, offset 15.
pub const MP_SIGBIT: u32 = 15;
/// `mp_SigTask`: `void *`, offset 16.
pub const MP_SIGTASK: u32 = 16;
/// `mp_MsgList`: `struct List`, offset 20.
pub const MP_MSGLIST: u32 = 20;
/// `sizeof(struct MsgPort)`.
pub const MSGPORT_SIZE: u32 = MP_MSGLIST + LIST_SIZE;

// --- struct Message field offsets ---

/// `mn_Node`: `struct Node`, offset 0.
pub const MN_NODE: u32 = 0;
/// `mn_ReplyPort`: `struct MsgPort *`, offset 14.
pub const MN_REPLYPORT: u32 = 14;
/// `mn_Length`: `UWORD`, offset 18.
pub const MN_LENGTH: u32 = 18;
/// `sizeof(struct Message)` (the fixed header; real messages typically
/// embed extra payload fields after this, which this module never
/// touches).
pub const MESSAGE_HEADER_SIZE: u32 = 20;

// --- exec node types this module writes (a subset of `<exec/nodes.h>`'s
// `NT_*` constants -- only the ones message ports actually use) ---

/// `NT_MSGPORT` (4).
pub const NT_MSGPORT: u8 = 4;
/// `NT_MESSAGE` (5).
pub const NT_MESSAGE: u8 = 5;
/// `NT_REPLYMSG` (7).
pub const NT_REPLYMSG: u8 = 7;

/// `mp_Flags` value for "reply by Signal" (the default/only mode this
/// runtime supports, since it never actually delivers a signal -- see the
/// module docs).
pub const PA_SIGNAL: u8 = 0;

/// Fixed placeholder written to `mp_SigBit` by [`create_msg_port_handler`]
/// -- see the module docs' "Message ports: single-threaded
/// simplifications" section for why this isn't a real allocated signal
/// bit.
pub const MSGPORT_SIGBIT_PLACEHOLDER: u8 = 0;

/// Initializes a `struct List`/`struct MinList` header at `list` as an
/// empty list, exactly matching the real `NewList()` macro's sentinel
/// trick (see the module docs). `lh_Type`/`lh_pad` are zeroed; callers
/// that care about a specific `lh_Type` (real `CreateMsgPort` sets its
/// `mp_MsgList.lh_Type`, for instance) write it themselves afterward --
/// `NewList()` itself doesn't touch `lh_Type` either.
///
/// Exposed as `pub` (per this module's design) so any future module
/// needing a fresh guest list header doesn't have to duplicate the
/// sentinel setup: there is no `NewList` LVO to register a handler for
/// (it's an `amiga.lib` macro, not a real library call), so this is the
/// only place the logic lives.
pub fn init_list_header<M: AddressSpace + ?Sized>(mem: &mut M, list: u32) {
    mem.write_u32(list + LH_HEAD, list + LH_TAIL);
    mem.write_u32(list + LH_TAIL, 0);
    mem.write_u32(list + LH_TAILPRED, list + LH_HEAD);
    mem.write_u8(list + LH_TYPE, 0);
    mem.write_u8(list + LH_PAD, 0);
}

/// `AddHead` (LVO -240): links `node` in as the new first node of `list`.
fn add_head_impl<M: AddressSpace>(mem: &mut M, list: u32, node: u32) {
    let succ = mem.read_u32(list + LH_HEAD);
    mem.write_u32(node + LN_SUCC, succ);
    mem.write_u32(node + LN_PRED, list + LH_HEAD);
    mem.write_u32(succ + LN_PRED, node);
    mem.write_u32(list + LH_HEAD, node);
}

/// `AddTail` (LVO -246): links `node` in as the new last node of `list`.
/// `pub(crate)` so [`crate::dispatch::write_library_list_nodes`] can
/// build `ExecBase.LibList` out of the same primitive the real `AddTail`
/// handler uses, rather than duplicating the link-list splicing logic.
pub(crate) fn add_tail_impl<M: AddressSpace + ?Sized>(mem: &mut M, list: u32, node: u32) {
    let pred = mem.read_u32(list + LH_TAILPRED);
    mem.write_u32(node + LN_SUCC, list + LH_TAIL);
    mem.write_u32(node + LN_PRED, pred);
    mem.write_u32(pred + LN_SUCC, node);
    mem.write_u32(list + LH_TAILPRED, node);
}

/// `Remove` (LVO -252): unlinks `node` from whichever list it's currently
/// on (determined purely from its own `ln_Succ`/`ln_Pred`, per real
/// `Remove`'s contract -- it doesn't take a list argument at all).
fn remove_impl<M: AddressSpace>(mem: &mut M, node: u32) {
    let pred = mem.read_u32(node + LN_PRED);
    let succ = mem.read_u32(node + LN_SUCC);
    mem.write_u32(pred + LN_SUCC, succ);
    mem.write_u32(succ + LN_PRED, pred);
}

/// `RemHead` (LVO -258): unlinks and returns the first node of `list`, or
/// `0` if the list is empty.
fn rem_head_impl<M: AddressSpace>(mem: &mut M, list: u32) -> u32 {
    let node = mem.read_u32(list + LH_HEAD);
    if mem.read_u32(node + LN_SUCC) == 0 {
        return 0;
    }
    remove_impl(mem, node);
    node
}

/// `RemTail` (LVO -264): unlinks and returns the last node of `list`, or
/// `0` if the list is empty.
fn rem_tail_impl<M: AddressSpace>(mem: &mut M, list: u32) -> u32 {
    let node = mem.read_u32(list + LH_TAILPRED);
    if mem.read_u32(node + LN_PRED) == 0 {
        return 0;
    }
    remove_impl(mem, node);
    node
}

/// `Insert` (LVO -234): links `node` into `list` immediately after
/// `pred`, or -- per the Autodoc -- as the new head if `pred` is `NULL`.
fn insert_impl<M: AddressSpace>(mem: &mut M, list: u32, node: u32, pred: u32) {
    if pred == 0 {
        add_head_impl(mem, list, node);
        return;
    }
    let succ = mem.read_u32(pred + LN_SUCC);
    mem.write_u32(node + LN_SUCC, succ);
    mem.write_u32(node + LN_PRED, pred);
    mem.write_u32(pred + LN_SUCC, node);
    mem.write_u32(succ + LN_PRED, node);
}

/// `Enqueue` (LVO -270): inserts `node` into `list`, ordered by
/// `ln_Pri` (a signed byte, higher sorts earlier), after every existing
/// node with a priority `>= node`'s (i.e. FIFO order among nodes sharing
/// the same priority) and before the first node with a strictly lower
/// priority.
fn enqueue_impl<M: AddressSpace>(mem: &mut M, list: u32, node: u32) {
    let pri = mem.read_u8(node + LN_PRI) as i8;
    let mut next = mem.read_u32(list + LH_HEAD);
    loop {
        let succ = mem.read_u32(next + LN_SUCC);
        if succ == 0 {
            break;
        }
        let next_pri = mem.read_u8(next + LN_PRI) as i8;
        if pri > next_pri {
            break;
        }
        next = succ;
    }
    let pred = mem.read_u32(next + LN_PRED);
    insert_impl(mem, list, node, pred);
}

/// `FindName` (LVO -276): searches `list` (or, per the real API's loose
/// `struct List *` typing, a previously-found `struct Node *` to resume a
/// search from -- see the module docs) for the first node whose `ln_Name`
/// case-sensitively equals `name`. Returns that node's address, or `0` if
/// not found.
fn find_name_impl<M: AddressSpace>(mem: &M, list: u32, name: &[u8]) -> u32 {
    let mut node = mem.read_u32(list + LH_HEAD);
    loop {
        let succ = mem.read_u32(node + LN_SUCC);
        if succ == 0 {
            return 0;
        }
        let name_ptr = mem.read_u32(node + LN_NAME);
        if name_ptr != 0 && read_c_string(mem, name_ptr) == name {
            return node;
        }
        node = succ;
    }
}

fn add_head_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let list = ctx.cpu.address_register(AddressRegister(0));
    let node = ctx.cpu.address_register(AddressRegister(1));
    add_head_impl(ctx.mem, list, node);
    Ok(())
}

fn add_tail_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let list = ctx.cpu.address_register(AddressRegister(0));
    let node = ctx.cpu.address_register(AddressRegister(1));
    add_tail_impl(ctx.mem, list, node);
    Ok(())
}

fn remove_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let node = ctx.cpu.address_register(AddressRegister(1));
    remove_impl(ctx.mem, node);
    Ok(())
}

fn rem_head_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let list = ctx.cpu.address_register(AddressRegister(0));
    let node = rem_head_impl(ctx.mem, list);
    ctx.cpu.set_data_register(DataRegister(0), node);
    Ok(())
}

fn rem_tail_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let list = ctx.cpu.address_register(AddressRegister(0));
    let node = rem_tail_impl(ctx.mem, list);
    ctx.cpu.set_data_register(DataRegister(0), node);
    Ok(())
}

fn insert_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let list = ctx.cpu.address_register(AddressRegister(0));
    let node = ctx.cpu.address_register(AddressRegister(1));
    let pred = ctx.cpu.address_register(AddressRegister(2));
    insert_impl(ctx.mem, list, node, pred);
    Ok(())
}

fn enqueue_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let list = ctx.cpu.address_register(AddressRegister(0));
    let node = ctx.cpu.address_register(AddressRegister(1));
    enqueue_impl(ctx.mem, list, node);
    Ok(())
}

fn find_name_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let list = ctx.cpu.address_register(AddressRegister(0));
    let name_ptr = ctx.cpu.address_register(AddressRegister(1));
    let name = read_c_string(ctx.mem, name_ptr);
    let found = find_name_impl(ctx.mem, list, &name);
    ctx.cpu.set_data_register(DataRegister(0), found);
    Ok(())
}

/// `PutMsg` (LVO -366): queues `message` onto `port`'s `mp_MsgList` (an
/// ordinary `AddTail`) and sets its `ln_Type` to `ln_type`. Shared by
/// [`put_msg_handler`] (`ln_type` = [`NT_MESSAGE`]) and
/// [`reply_msg_handler`]'s non-`NULL`-reply-port path (`ln_type` =
/// [`NT_REPLYMSG`]).
fn put_msg_impl<M: AddressSpace>(mem: &mut M, port: u32, message: u32, ln_type: u8) {
    add_tail_impl(mem, port + MP_MSGLIST, message);
    mem.write_u8(message + LN_TYPE, ln_type);
}

fn put_msg_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let port = ctx.cpu.address_register(AddressRegister(0));
    let message = ctx.cpu.address_register(AddressRegister(1));
    put_msg_impl(ctx.mem, port, message, NT_MESSAGE);
    Ok(())
}

fn get_msg_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let port = ctx.cpu.address_register(AddressRegister(0));
    let message = rem_head_impl(ctx.mem, port + MP_MSGLIST);
    ctx.cpu.set_data_register(DataRegister(0), message);
    Ok(())
}

fn reply_msg_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let message = ctx.cpu.address_register(AddressRegister(1));
    let reply_port = ctx.mem.read_u32(message + MN_REPLYPORT);
    if reply_port != 0 {
        put_msg_impl(ctx.mem, reply_port, message, NT_REPLYMSG);
    } else {
        // No reply port: real ReplyMsg still marks the message as
        // replied but has nowhere to deliver it -- see the module docs.
        ctx.mem.write_u8(message + LN_TYPE, NT_REPLYMSG);
    }
    Ok(())
}

/// `CreateMsgPort` (LVO -666): allocates and initializes a `struct
/// MsgPort` on the guest heap. Returns its address in `D0`, or `0` if the
/// heap is exhausted. See the module docs for the `mp_Flags`/`mp_SigBit`/
/// `mp_SigTask` simplifications.
fn create_msg_port_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let port = match ctx.heap.alloc(MSGPORT_SIZE) {
        Ok(addr) => addr,
        Err(_) => {
            // Real CreateMsgPort returns NULL on failure (e.g.
            // AllocSignal exhaustion); the analogous failure here is
            // guest heap exhaustion, treated the same way.
            ctx.cpu.set_data_register(DataRegister(0), 0);
            return Ok(());
        }
    };

    // mp_Node: not yet linked onto any list (ln_Succ/ln_Pred left 0),
    // type NT_MSGPORT, default priority 0, no name.
    ctx.mem.write_u32(port + LN_SUCC, 0);
    ctx.mem.write_u32(port + LN_PRED, 0);
    ctx.mem.write_u8(port + LN_TYPE, NT_MSGPORT);
    ctx.mem.write_u8(port + LN_PRI, 0);
    ctx.mem.write_u32(port + LN_NAME, 0);

    ctx.mem.write_u8(port + MP_FLAGS, PA_SIGNAL);
    ctx.mem
        .write_u8(port + MP_SIGBIT, MSGPORT_SIGBIT_PLACEHOLDER);
    ctx.mem.write_u32(port + MP_SIGTASK, ctx.current_task);

    init_list_header(ctx.mem, port + MP_MSGLIST);

    ctx.cpu.set_data_register(DataRegister(0), port);
    Ok(())
}

/// `DeleteMsgPort` (LVO -672): frees a port allocated by
/// [`create_msg_port_handler`]. `NULL` is a legal no-op, matching real
/// `DeleteMsgPort`'s documented behavior.
fn delete_msg_port_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let port = ctx.cpu.address_register(AddressRegister(0));
    if port == 0 {
        return Ok(());
    }
    ctx.heap
        .free(port)
        .map_err(|e| DispatchError::HandlerFailed {
            library: "exec.library".to_string(),
            lvo: -672,
            handler_name: "DeleteMsgPort".to_string(),
            message: format!(
                "DeleteMsgPort called on {port:#010x}, which isn't a currently-live \
             CreateMsgPort allocation (never allocated, already deleted, or not a \
             MsgPort pointer at all): {e}"
            ),
        })
}

/// `CreateIORequest` (LVO -654: `A0` = reply `MsgPort*`, `D0` = size).
/// `D0` = a zeroed `size`-byte block, with `io_Message.mn_Node.ln_Type`
/// set to [`NT_MESSAGE`], `mn_Length` to `size` (so
/// [`delete_io_request_handler`] knows how much to free without a
/// separate size-tracking table), and `mn_ReplyPort` to `A0` -- or `0`
/// if the guest heap is exhausted. Found missing while running the
/// real `PhxAss` assembler, which builds a `timerequest` this way.
fn create_io_request_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let port = ctx.cpu.address_register(AddressRegister(0));
    let size = ctx.cpu.data_register(DataRegister(0));
    let addr = create_io_request(ctx.heap, ctx.mem, port, size).unwrap_or(0);
    ctx.cpu.set_data_register(DataRegister(0), addr);
    Ok(())
}

/// Core of `CreateIORequest`: allocates and initializes a zeroed
/// `size`-byte block. `None` if the guest heap is exhausted.
fn create_io_request(
    heap: &mut GuestHeap,
    mem: &mut dyn AddressSpace,
    port: u32,
    size: u32,
) -> Option<u32> {
    let addr = heap.alloc(size).ok()?;
    for i in 0..size {
        mem.write_u8(addr.wrapping_add(i), 0);
    }
    mem.write_u8(addr + LN_TYPE, NT_MESSAGE);
    mem.write_u16(addr + MN_LENGTH, size as u16);
    mem.write_u32(addr + MN_REPLYPORT, port);
    Some(addr)
}

/// `DeleteIORequest` (LVO -660: `A0` = `IORequest*` from
/// [`create_io_request_handler`]). No return value. `NULL` is a legal
/// no-op, matching real `DeleteIORequest`.
fn delete_io_request_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let addr = ctx.cpu.address_register(AddressRegister(0));
    if addr == 0 {
        return Ok(());
    }
    ctx.heap
        .free(addr)
        .map_err(|e| DispatchError::HandlerFailed {
            library: "exec.library".to_string(),
            lvo: -660,
            handler_name: "DeleteIORequest".to_string(),
            message: format!(
                "DeleteIORequest called on {addr:#010x}, which isn't a currently-live \
                 CreateIORequest allocation (never allocated, already deleted, or not an \
                 IORequest pointer at all): {e}"
            ),
        })
}

/// `AddPort` (LVO -354): see the module docs' "no public port registry"
/// section -- sets `ln_Type` to [`NT_MSGPORT`] (matching what real
/// `AddPort` does to the node before enqueueing it) but doesn't maintain
/// any actual searchable port list.
fn add_port_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let port = ctx.cpu.address_register(AddressRegister(1));
    ctx.mem.write_u8(port + LN_TYPE, NT_MSGPORT);
    Ok(())
}

/// `RemPort` (LVO -360): a no-op -- see the module docs.
fn rem_port_handler<C: Cpu>(_ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    Ok(())
}

/// `FindPort` (LVO -390): always returns `NULL` -- see the module docs.
fn find_port_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    ctx.cpu.set_data_register(DataRegister(0), 0);
    Ok(())
}

/// Registers every implemented list/node and message-port
/// `exec.library` handler onto [`EXEC_LIBRARY_BASE`], looked up by name
/// through [`EXEC_LVOS`], following [`crate::execmem::
/// register_execmem_handlers`]'s registration pattern. Called
/// unconditionally from [`crate::dispatch::Runtime::new`].
pub fn register_execlist_handlers<C: Cpu + 'static>(
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
    reg!("AddHead", add_head_handler::<C>);
    reg!("AddTail", add_tail_handler::<C>);
    reg!("Remove", remove_handler::<C>);
    reg!("RemHead", rem_head_handler::<C>);
    reg!("RemTail", rem_tail_handler::<C>);
    reg!("Insert", insert_handler::<C>);
    reg!("Enqueue", enqueue_handler::<C>);
    reg!("FindName", find_name_handler::<C>);
    reg!("PutMsg", put_msg_handler::<C>);
    reg!("GetMsg", get_msg_handler::<C>);
    reg!("ReplyMsg", reply_msg_handler::<C>);
    reg!("CreateMsgPort", create_msg_port_handler::<C>);
    reg!("DeleteMsgPort", delete_msg_port_handler::<C>);
    reg!("CreateIORequest", create_io_request_handler::<C>);
    reg!("DeleteIORequest", delete_io_request_handler::<C>);
    reg!("AddPort", add_port_handler::<C>);
    reg!("RemPort", rem_port_handler::<C>);
    reg!("FindPort", find_port_handler::<C>);
}

#[cfg(test)]
#[allow(clippy::vec_init_then_push)]
mod tests {
    use super::*;
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{Runtime, StartConfig};
    use crate::guestmem::write_c_string;
    use crate::memory::FlatMemory;

    fn load_words(mem: &mut FlatMemory, addr: u32, words: &[u16]) {
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

    /// `move.l D0,Dn`.
    fn move_d0_to_d(n: u16) -> u16 {
        0x2000 | (n << 9)
    }

    /// `move.l D0,An`.
    fn move_d0_to_a(n: u16) -> u16 {
        0x2040 | (n << 9)
    }

    /// `movea.l #EXEC_LIBRARY_BASE,a6` -- `Runtime::new` seeds A6 with
    /// `DOS_LIBRARY_BASE` (a Phase 1 compatibility shim), but these tests
    /// need A6 = `EXEC_LIBRARY_BASE` to call exec.library LVOs.
    fn movea_exec_base_to_a6() -> [u16; 3] {
        [
            move_imm_to_a(6),
            (EXEC_LIBRARY_BASE >> 16) as u16,
            EXEC_LIBRARY_BASE as u16,
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

    /// Prepends the A6-fixup to `words` before building the runtime.
    fn program(words: &[u16]) -> Runtime<M68kCpu> {
        let mut full = movea_exec_base_to_a6().to_vec();
        full.extend_from_slice(words);
        runtime_with_program(&full)
    }

    // --- host-side list-op tests (direct memory manipulation, no CPU
    // program needed -- mirrors utility.rs's tag-list traversal tests) ---

    #[test]
    fn add_head_and_add_tail_build_expected_order() {
        let mut mem = FlatMemory::new(0x1000);
        let list = 0x100u32;
        let a = 0x200u32;
        let b = 0x220u32;
        let c = 0x240u32;
        init_list_header(&mut mem, list);

        add_tail_impl(&mut mem, list, a); // [a]
        add_tail_impl(&mut mem, list, b); // [a, b]
        add_head_impl(&mut mem, list, c); // [c, a, b]

        assert_eq!(mem.read_u32(list + LH_HEAD), c);
        assert_eq!(mem.read_u32(c + LN_SUCC), a);
        assert_eq!(mem.read_u32(a + LN_PRED), c);
        assert_eq!(mem.read_u32(a + LN_SUCC), b);
        assert_eq!(mem.read_u32(b + LN_PRED), a);
        assert_eq!(mem.read_u32(list + LH_TAILPRED), b);
        // Tail sentinel: b's ln_Succ is &lh_Tail, whose own value is 0.
        let b_succ = mem.read_u32(b + LN_SUCC);
        assert_eq!(mem.read_u32(b_succ), 0);
    }

    #[test]
    fn rem_head_and_rem_tail_empty_list_return_null() {
        let mut mem = FlatMemory::new(0x1000);
        let list = 0x100u32;
        init_list_header(&mut mem, list);
        assert_eq!(rem_head_impl(&mut mem, list), 0);
        assert_eq!(rem_tail_impl(&mut mem, list), 0);
    }

    #[test]
    fn rem_head_rem_tail_round_trip_through_a_three_node_list() {
        let mut mem = FlatMemory::new(0x1000);
        let list = 0x100u32;
        let a = 0x200u32;
        let b = 0x220u32;
        let c = 0x240u32;
        init_list_header(&mut mem, list);
        add_tail_impl(&mut mem, list, a);
        add_tail_impl(&mut mem, list, b);
        add_tail_impl(&mut mem, list, c);

        assert_eq!(rem_head_impl(&mut mem, list), a);
        assert_eq!(rem_tail_impl(&mut mem, list), c);
        assert_eq!(rem_head_impl(&mut mem, list), b);
        assert_eq!(rem_head_impl(&mut mem, list), 0, "list should now be empty");
        assert_eq!(rem_tail_impl(&mut mem, list), 0, "list should now be empty");
    }

    #[test]
    fn remove_from_the_middle_relinks_neighbors() {
        let mut mem = FlatMemory::new(0x1000);
        let list = 0x100u32;
        let a = 0x200u32;
        let b = 0x220u32;
        let c = 0x240u32;
        init_list_header(&mut mem, list);
        add_tail_impl(&mut mem, list, a);
        add_tail_impl(&mut mem, list, b);
        add_tail_impl(&mut mem, list, c);

        remove_impl(&mut mem, b);

        assert_eq!(mem.read_u32(list + LH_HEAD), a);
        assert_eq!(mem.read_u32(a + LN_SUCC), c);
        assert_eq!(mem.read_u32(c + LN_PRED), a);
        assert_eq!(mem.read_u32(list + LH_TAILPRED), c);
    }

    #[test]
    fn insert_with_null_pred_behaves_like_add_head() {
        let mut mem = FlatMemory::new(0x1000);
        let list = 0x100u32;
        let a = 0x200u32;
        let b = 0x220u32;
        init_list_header(&mut mem, list);
        add_tail_impl(&mut mem, list, a);
        insert_impl(&mut mem, list, b, 0);
        assert_eq!(mem.read_u32(list + LH_HEAD), b);
        assert_eq!(mem.read_u32(b + LN_SUCC), a);
    }

    #[test]
    fn insert_after_a_given_pred() {
        let mut mem = FlatMemory::new(0x1000);
        let list = 0x100u32;
        let a = 0x200u32;
        let b = 0x220u32;
        let c = 0x240u32;
        init_list_header(&mut mem, list);
        add_tail_impl(&mut mem, list, a);
        add_tail_impl(&mut mem, list, c);
        insert_impl(&mut mem, list, b, a); // [a, b, c]

        assert_eq!(mem.read_u32(a + LN_SUCC), b);
        assert_eq!(mem.read_u32(b + LN_PRED), a);
        assert_eq!(mem.read_u32(b + LN_SUCC), c);
        assert_eq!(mem.read_u32(c + LN_PRED), b);
    }

    #[test]
    fn enqueue_orders_by_priority_fifo_within_priority() {
        let mut mem = FlatMemory::new(0x1000);
        let list = 0x100u32;
        init_list_header(&mut mem, list);

        // Nodes at addresses 0x200, 0x220, 0x240, 0x260 with priorities
        // 5, 10, 5, 0 respectively, enqueued in that order. Expected
        // final order (descending priority, FIFO within a tie):
        // 0x220 (10), 0x200 (5), 0x240 (5), 0x260 (0).
        let n1 = 0x200u32; // pri 5
        let n2 = 0x220u32; // pri 10
        let n3 = 0x240u32; // pri 5
        let n4 = 0x260u32; // pri 0
        mem.write_u8(n1 + LN_PRI, 5);
        mem.write_u8(n2 + LN_PRI, 10);
        mem.write_u8(n3 + LN_PRI, 5);
        mem.write_u8(n4 + LN_PRI, 0);

        enqueue_impl(&mut mem, list, n1);
        enqueue_impl(&mut mem, list, n2);
        enqueue_impl(&mut mem, list, n3);
        enqueue_impl(&mut mem, list, n4);

        let mut order = Vec::new();
        let mut cur = mem.read_u32(list + LH_HEAD);
        loop {
            let succ = mem.read_u32(cur + LN_SUCC);
            if succ == 0 {
                break;
            }
            order.push(cur);
            cur = succ;
        }
        assert_eq!(order, vec![n2, n1, n3, n4]);
    }

    #[test]
    fn enqueue_with_negative_priority_sorts_after_positive() {
        let mut mem = FlatMemory::new(0x1000);
        let list = 0x100u32;
        init_list_header(&mut mem, list);
        let pos = 0x200u32;
        let neg = 0x220u32;
        mem.write_u8(pos + LN_PRI, 1);
        mem.write_u8(neg + LN_PRI, (-1i8) as u8);

        enqueue_impl(&mut mem, list, neg);
        enqueue_impl(&mut mem, list, pos);

        assert_eq!(mem.read_u32(list + LH_HEAD), pos);
        assert_eq!(mem.read_u32(pos + LN_SUCC), neg);
    }

    #[test]
    fn find_name_hit_and_miss_case_sensitive() {
        let mut mem = FlatMemory::new(0x1000);
        let list = 0x100u32;
        let a = 0x200u32;
        let b = 0x220u32;
        init_list_header(&mut mem, list);

        let name_a = 0x300u32;
        let name_b = 0x310u32;
        write_c_string(&mut mem, name_a, b"Foo");
        write_c_string(&mut mem, name_b, b"bar");
        mem.write_u32(a + LN_NAME, name_a);
        mem.write_u32(b + LN_NAME, name_b);
        add_tail_impl(&mut mem, list, a);
        add_tail_impl(&mut mem, list, b);

        assert_eq!(find_name_impl(&mem, list, b"bar"), b);
        // Case-sensitive: "BAR" must NOT match "bar".
        assert_eq!(find_name_impl(&mem, list, b"BAR"), 0);
        // Not present at all.
        assert_eq!(find_name_impl(&mem, list, b"nope"), 0);
    }

    // --- full guest-program tests via jump-table dispatch ---

    #[test]
    fn add_head_add_tail_remove_via_dispatch() {
        // Build list header + 2 nodes right after the code. AddTail(a),
        // AddTail(b), Remove(a); exit code = list->lh_Head (should be b
        // after a is removed).
        let entry = TRAP_TABLE_END;
        let mut words = Vec::new();
        // A0 = list (patched), A1 = a (patched)
        words.push(move_imm_to_a(0));
        words.push(0);
        words.push(0);
        words.push(move_imm_to_a(1));
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-246)); // AddTail(list, a)

        // A1 = b (patched); A0 still = list
        words.push(move_imm_to_a(1));
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-246)); // AddTail(list, b)

        // A1 = a again, Remove(a)
        words.push(move_imm_to_a(1));
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-252)); // Remove(a)

        // D0 = list->lh_Head (word offset patch not needed -- read via
        // move.l (a0),d0 since A0 still holds list, LH_HEAD offset 0)
        words.push(0x2010); // move.l (a0),d0
        words.push(RTS);

        // Layout right after the code: list header (14 bytes), then node
        // a (14 bytes), then node b (14 bytes).
        let code_len = movea_exec_base_to_a6().len() + words.len();
        let list_addr = entry + (code_len as u32) * 2;
        let a_addr = list_addr + LIST_SIZE;
        let b_addr = a_addr + NODE_SIZE;

        // Patch the immediates: indices are within `words` (offset by 0
        // since movea_exec_base_to_a6 is prepended separately by
        // `program`, but we compute addresses using its length above, so
        // patch indices here are relative to `words` itself, matching
        // where they were pushed).
        words[1] = (list_addr >> 16) as u16;
        words[2] = list_addr as u16;
        words[4] = (a_addr >> 16) as u16;
        words[5] = a_addr as u16;
        words[9] = (b_addr >> 16) as u16;
        words[10] = b_addr as u16;
        words[14] = (a_addr >> 16) as u16;
        words[15] = a_addr as u16;

        let mut full = movea_exec_base_to_a6().to_vec();
        full.extend_from_slice(&words);

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &full);
        init_list_header(&mut mem, list_addr);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: b_addr + NODE_SIZE,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code as u32, b_addr, "b should be the sole remaining head");
    }

    #[test]
    fn create_msg_port_put_get_round_trip_via_dispatch() {
        // CreateMsgPort -> A2 (kept). Build a Message right after the
        // code. PutMsg(port, message); GetMsg(port) -> D0; exit code =
        // D0 (should equal the message's address, and its ln_Type should
        // have become NT_MESSAGE).
        let entry = TRAP_TABLE_END;
        let mut words = Vec::new();
        words.extend_from_slice(&jsr_disp16_a6(-666)); // idx0,1: CreateMsgPort -> D0
        words.push(move_d0_to_a(2)); // idx2: A2 = port
        words.push(move_d0_to_a(0)); // idx3: A0 = port
        words.push(move_imm_to_a(1)); // idx4: A1 = message, patched below
        words.push(0); // idx5: high word
        words.push(0); // idx6: low word
        words.extend_from_slice(&jsr_disp16_a6(-366)); // idx7,8: PutMsg(port, message)
        words.push(0x204A); // idx9: movea.l a2,a0 (a0 = port again)
        words.extend_from_slice(&jsr_disp16_a6(-372)); // idx10,11: GetMsg(port) -> D0
        words.push(RTS); // idx12

        let code_len = movea_exec_base_to_a6().len() + words.len();
        let message_addr = entry + (code_len as u32) * 2;
        words[5] = (message_addr >> 16) as u16;
        words[6] = message_addr as u16;

        let mut full = movea_exec_base_to_a6().to_vec();
        full.extend_from_slice(&words);

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &full);
        // Zero the message header (mn_ReplyPort = 0, mn_Length = 0) --
        // FlatMemory starts zeroed anyway, but be explicit.
        mem.write_u32(message_addr + MN_REPLYPORT, 0);
        mem.write_u16(message_addr + MN_LENGTH, 0);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: message_addr + MESSAGE_HEADER_SIZE,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code as u32, message_addr,
            "GetMsg should return the same message PutMsg queued"
        );
        assert_eq!(
            rt.memory().read_u8(message_addr + LN_TYPE),
            NT_MESSAGE,
            "PutMsg should have set ln_Type to NT_MESSAGE"
        );
    }

    #[test]
    fn create_then_delete_io_request_via_dispatch() {
        // CreateMsgPort -> A2 (kept as the reply port). CreateIORequest
        // (A2, size=24) -> D0 = ioreq; check its header, then
        // DeleteIORequest(ioreq) and DeleteMsgPort(port) should both
        // succeed without error.
        let entry = TRAP_TABLE_END;
        let mut words = Vec::new();
        words.extend_from_slice(&jsr_disp16_a6(-666)); // CreateMsgPort -> D0
        words.push(move_d0_to_a(2)); // A2 = port
        words.push(move_d0_to_a(0)); // A0 = port
        words.push(0x7018); // moveq #24,d0 (size)
        words.extend_from_slice(&jsr_disp16_a6(-654)); // CreateIORequest -> D0
        words.push(0x2600); // move.l d0,d3 (d3 = ioreq, kept across the next calls)
        words.push(0x2043); // movea.l d3,a0 (A0 = ioreq)
        words.extend_from_slice(&jsr_disp16_a6(-660)); // DeleteIORequest(ioreq)
        words.push(0x204A); // movea.l a2,a0 (A0 = port)
        words.extend_from_slice(&jsr_disp16_a6(-672)); // DeleteMsgPort(port)
        words.push(0x2003); // move.l d3,d0 (exit code = ioreq addr)
        words.push(RTS);

        let mut full = movea_exec_base_to_a6().to_vec();
        full.extend_from_slice(&words);

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &full);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: entry + (full.len() as u32) * 2 + 0x100,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_ne!(code, 0, "CreateIORequest should have succeeded");
    }

    #[test]
    fn create_io_request_fills_header_and_delete_frees_it() {
        let mut heap = GuestHeap::new(0x1000, 0x2000);
        let mut mem = FlatMemory::new(0x2000);
        let port_addr = 0x1000u32;
        let before_free = heap.total_free();

        let addr = create_io_request(&mut heap, &mut mem, port_addr, 24).unwrap();
        assert_eq!(mem.read_u8(addr + LN_TYPE), NT_MESSAGE);
        assert_eq!(mem.read_u16(addr + MN_LENGTH), 24);
        assert_eq!(mem.read_u32(addr + MN_REPLYPORT), port_addr);
        assert!(heap.total_free() < before_free);

        heap.free(addr).unwrap();
        assert_eq!(heap.total_free(), before_free);
    }

    #[test]
    fn reply_msg_with_reply_port_queues_it_there() {
        let mut mem = FlatMemory::new(0x1000);
        let reply_port = 0x100u32;
        let message = 0x200u32;
        init_list_header(&mut mem, reply_port + MP_MSGLIST);
        mem.write_u32(message + MN_REPLYPORT, reply_port);

        // Direct impl call (mirrors reply_msg_handler's body).
        let rp = mem.read_u32(message + MN_REPLYPORT);
        assert_ne!(rp, 0);
        put_msg_impl(&mut mem, rp, message, NT_REPLYMSG);

        assert_eq!(
            mem.read_u8(message + LN_TYPE),
            NT_REPLYMSG,
            "ReplyMsg should set ln_Type to NT_REPLYMSG"
        );
        assert_eq!(
            rem_head_impl(&mut mem, reply_port + MP_MSGLIST),
            message,
            "the message should now be queued on the reply port"
        );
    }

    #[test]
    fn reply_msg_without_reply_port_only_sets_type() {
        let mut mem = FlatMemory::new(0x1000);
        let message = 0x200u32;
        mem.write_u32(message + MN_REPLYPORT, 0);
        mem.write_u8(message + LN_TYPE, NT_MESSAGE);

        let rp = mem.read_u32(message + MN_REPLYPORT);
        assert_eq!(rp, 0);
        mem.write_u8(message + LN_TYPE, NT_REPLYMSG);

        assert_eq!(mem.read_u8(message + LN_TYPE), NT_REPLYMSG);
    }

    #[test]
    fn get_msg_on_empty_port_returns_null_never_blocks() {
        let mut words = Vec::new();
        words.extend_from_slice(&jsr_disp16_a6(-666)); // CreateMsgPort -> D0
        words.push(move_d0_to_a(0)); // A0 = port
        words.extend_from_slice(&jsr_disp16_a6(-372)); // GetMsg(port) -> D0
        words.push(RTS);

        let mut rt = program(&words);
        let mut out = Vec::new();
        let code = rt
            .run(&mut out, None)
            .expect("GetMsg on an empty port must return, not block");
        assert_eq!(code, 0, "GetMsg on an empty port should return NULL");
    }

    #[test]
    fn delete_msg_port_null_is_a_no_op() {
        let mut words = Vec::new();
        words.push(move_imm_to_a(0)); // A0 = 0
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-672)); // DeleteMsgPort(NULL)
        words.push(0x7000); // moveq #0,d0
        words.push(RTS);

        let mut rt = program(&words);
        let mut out = Vec::new();
        let code = rt
            .run(&mut out, None)
            .expect("DeleteMsgPort(NULL) should be a no-op, not an error");
        assert_eq!(code, 0);
    }

    #[test]
    fn create_then_delete_msg_port_round_trip() {
        let mut words = Vec::new();
        words.extend_from_slice(&jsr_disp16_a6(-666)); // CreateMsgPort -> D0
        words.push(move_d0_to_d(2)); // D2 = port (kept for the exit code)
        words.push(move_d0_to_a(0)); // A0 = port
        words.extend_from_slice(&jsr_disp16_a6(-672)); // DeleteMsgPort
        words.push(0x2002); // move.l D2,D0
        words.push(RTS);

        let mut rt = program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_ne!(code, 0, "CreateMsgPort should not return NULL");
    }

    #[test]
    fn find_port_always_returns_null() {
        let mut words = Vec::new();
        words.push(move_imm_to_a(1)); // A1 = some name pointer (content irrelevant)
        words.push((TRAP_TABLE_END >> 16) as u16);
        words.push(TRAP_TABLE_END as u16);
        words.extend_from_slice(&jsr_disp16_a6(-390)); // FindPort
        words.push(RTS);

        let mut rt = program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 0);
    }

    #[test]
    fn add_port_sets_ln_type_and_rem_port_is_a_no_op() {
        let mut mem = FlatMemory::new(0x1000);
        let port = 0x100u32;
        mem.write_u8(port + LN_TYPE, 0);
        mem.write_u8(port + LN_TYPE, NT_MSGPORT); // mirrors add_port_handler's body
        assert_eq!(mem.read_u8(port + LN_TYPE), NT_MSGPORT);
        // RemPort has nothing to assert beyond "doesn't panic" -- covered
        // by rem_port_handler's triviality; exercised via dispatch below.

        let mut words = Vec::new();
        words.push(move_imm_to_a(1)); // A1 = port, patched
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-360)); // RemPort
        words.push(0x7000); // moveq #0,d0
        words.push(RTS);
        let entry = TRAP_TABLE_END;
        let code_len = movea_exec_base_to_a6().len() + words.len();
        let port_addr = entry + (code_len as u32) * 2;
        words[1] = (port_addr >> 16) as u16;
        words[2] = port_addr as u16;

        let mut full = movea_exec_base_to_a6().to_vec();
        full.extend_from_slice(&words);
        let mut gm = FlatMemory::new(0x2_0000);
        load_words(&mut gm, entry, &full);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            gm,
            StartConfig {
                entry,
                load_end: port_addr + NODE_SIZE,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 0);
    }
}
