//! `intuition.library`: a thin stub -- `DisplayAlert`, `AutoRequest`,
//! `EasyRequestArgs`, `CurrentTime`, matching vamos's own scope for this
//! library ("just enough that a console tool calling a stray Intuition
//! function doesn't crash. No real windowing/GUI"), added closing a gap
//! found comparing volamos's coverage against vamos's own
//! (`docs/plan.md`'s dated entry). This runtime has no display and never
//! renders anything -- every function here does the least a headless
//! runtime honestly can, not a real GUI implementation.
//!
//! # The default screen: real struct, no pixels
//!
//! Unlike the rest of this module, there *is* now a `Screen`/`Window`
//! model, deliberately minimal: a single, always-available default
//! public screen (`"Workbench"`, 640x200x1 -- a plain NTSC lores
//! non-interlace mode, the same one a stock KS/WB 3.1 boot without a
//! Workbench disk comes up in), lazily allocated on first use as a real
//! `struct Screen` in guest memory ([`ensure_default_screen`]) with
//! honest `Width`/`Height`/`Flags`/embedded `ViewPort`/`RastPort`/
//! `BitMap` fields -- [`lock_pub_screen_handler`]/
//! [`get_screen_data_handler`] read real data off it, not canned
//! answers. There is deliberately no support for opening *additional*
//! or *custom* screens (`OpenScreen` stays unregistered/unhandled) --
//! see `docs/plan.md`'s dated entry for the reasoning: almost every
//! guest program that touches Intuition just wants to query the one
//! screen that's always there, not create its own.
//!
//! `OpenWindow`/`CloseWindow` extend the same idea to windows: a guest
//! that opens a window gets back a real `struct Window` -- correctly
//! wired `WScreen`/`RPort`/`UserPort` pointers, so any code that merely
//! *holds onto* the handle (passes it to another library call, reads
//! its geometry, polls its `UserPort` for messages) sees consistent,
//! honest data -- but nothing is ever drawn into its `RastPort`, and
//! its `UserPort` never receives an `IntuiMessage` (there's no user
//! input to report -- same limitation as `AutoRequest`/
//! `EasyRequestArgs` below). A program that actually expects to *see*
//! its window, or that blocks forever waiting for an `IDCMP` message
//! that will never arrive, won't work here -- an inherent limit of a
//! headless runtime, not something worth working around with fake
//! input.
//!
//! Struct layouts (`SCR_*`/`WIN_*`/`VP_*`/`RP_*`/`BM_*` byte-offset
//! consts below) are hand-computed from the NDK 3.2 headers
//! (`intuition/screens.h`, `intuition/intuition.h`, `graphics/view.h`,
//! `graphics/rastport.h`, `graphics/gfx.h`) using m68k's natural
//! (word, not long) struct alignment -- same provenance discipline as
//! `doslock.rs`'s `FL_*`/`FIB_*` consts. Two deliberate approximations:
//! `struct Layer_Info` (embedded in `Screen`) is reserved as an opaque
//! zeroed blob sized generously rather than field-accurate, since
//! nothing in this runtime (there's no `layers.library`) ever reads its
//! fields; and `struct Window`'s private tail past `MoreFlags` (per the
//! header's own "Intuition Private, DO NOT USE" comment) isn't
//! reproduced at all.
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
//! # `GetScreenData`/`LockPubScreen`/`UnlockPubScreen`: the default
//! screen, honestly
//!
//! `GetScreenData` now copies real `struct Screen` fields (from the
//! default screen -- see "The default screen" above) into the caller's
//! buffer, `min(size, sizeof(struct Screen))` bytes of it, and returns
//! `TRUE`. `LockPubScreen` returns the default screen's address for a
//! `NULL` name or a name matching it (case-sensitively -- see
//! [`lock_pub_screen_handler`]); any other name honestly fails with
//! `NULL`, since no other public screen actually exists here.
//! `UnlockPubScreen` is a no-op (this runtime doesn't track lock
//! counts; there's only ever the one screen and it never actually
//! closes).
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
use crate::execlist::{MSGPORT_SIZE, init_msg_port_fields};
use crate::exectask::host_time_secs_micro;
use crate::guestmem::{GuestHeap, read_c_string, write_c_string};
use crate::lvos::intuition::INTUITION_LVOS;
use crate::memory::AddressSpace;

// ---- struct Screen (intuition/screens.h) -----------------------------

/// `sizeof(struct Screen)` for this runtime's purposes -- see the module
/// docs' "Struct layouts" paragraph for the `Layer_Info` approximation.
const SCREEN_SIZE: u32 = 332;
const SCR_NEXTSCREEN: u32 = 0;
const SCR_FIRSTWINDOW: u32 = 4;
const SCR_LEFTEDGE: u32 = 8;
const SCR_TOPEDGE: u32 = 10;
const SCR_WIDTH: u32 = 12;
const SCR_HEIGHT: u32 = 14;
const SCR_MOUSEY: u32 = 16;
const SCR_MOUSEX: u32 = 18;
const SCR_FLAGS: u32 = 20;
const SCR_TITLE: u32 = 22;
const SCR_DEFAULTTITLE: u32 = 26;
const SCR_WBORTOP: u32 = 35;
const SCR_WBORLEFT: u32 = 36;
const SCR_WBORRIGHT: u32 = 37;
const SCR_WBORBOTTOM: u32 = 38;
const SCR_FONT: u32 = 40;
/// Byte offset of the embedded `struct ViewPort` within `struct Screen`.
const SCR_VIEWPORT: u32 = 44;
/// Byte offset of the embedded `struct RastPort` within `struct Screen`.
const SCR_RASTPORT: u32 = 84;
/// Byte offset of the embedded `struct BitMap` within `struct Screen`.
const SCR_BITMAP: u32 = 184;
/// Byte offset of the (opaque, zeroed) `struct Layer_Info` within
/// `struct Screen` -- reserved space only, see the module docs.
const SCR_LAYERINFO: u32 = 224;
const SCR_LAYERINFO_SIZE: u32 = 88;
const SCR_FIRSTGADGET: u32 = 312;
const SCR_DETAILPEN: u32 = 316;
const SCR_BLOCKPEN: u32 = 317;
const SCR_SAVECOLOR0: u32 = 318;
const SCR_BARLAYER: u32 = 320;
const SCR_EXTDATA: u32 = 324;
const SCR_USERDATA: u32 = 328;

/// `WBENCHSCREEN` (`0x0001`), per `intuition/screens.h` -- this
/// runtime's one default screen is always this type.
const WBENCHSCREEN: u16 = 0x0001;

// ---- struct ViewPort (graphics/view.h), embedded in Screen -----------

const VP_NEXT: u32 = 0;
const VP_COLORMAP: u32 = 4;
const VP_DWIDTH: u32 = 24;
const VP_DHEIGHT: u32 = 26;
const VP_MODES: u32 = 32;
const VP_RASINFO: u32 = 36;

// ---- struct RastPort (graphics/rastport.h), embedded in Screen -------

/// `sizeof(struct RastPort)`.
const RASTPORT_SIZE: u32 = 100;
const RP_LAYER: u32 = 0;
const RP_BITMAP: u32 = 4;

// ---- struct BitMap (graphics/gfx.h), embedded in Screen ---------------

const BM_BYTESPERROW: u32 = 0;
const BM_ROWS: u32 = 2;
const BM_FLAGS: u32 = 4;
const BM_DEPTH: u32 = 5;

// ---- struct Window (intuition/intuition.h) -----------------------------

/// `sizeof(struct Window)` up to (and including) `MoreFlags` -- the
/// header's own documented public boundary, see the module docs.
const WINDOW_SIZE: u32 = 136;
const WIN_NEXTWINDOW: u32 = 0;
const WIN_LEFTEDGE: u32 = 4;
const WIN_TOPEDGE: u32 = 6;
const WIN_WIDTH: u32 = 8;
const WIN_HEIGHT: u32 = 10;
const WIN_MOUSEY: u32 = 12;
const WIN_MOUSEX: u32 = 14;
const WIN_MINWIDTH: u32 = 16;
const WIN_MINHEIGHT: u32 = 18;
const WIN_MAXWIDTH: u32 = 20;
const WIN_MAXHEIGHT: u32 = 22;
const WIN_FLAGS: u32 = 24;
const WIN_MENUSTRIP: u32 = 28;
const WIN_TITLE: u32 = 32;
const WIN_FIRSTREQUEST: u32 = 36;
const WIN_DMREQUEST: u32 = 40;
const WIN_REQCOUNT: u32 = 44;
const WIN_WSCREEN: u32 = 46;
const WIN_RPORT: u32 = 50;
const WIN_BORDERLEFT: u32 = 54;
const WIN_BORDERTOP: u32 = 55;
const WIN_BORDERRIGHT: u32 = 56;
const WIN_BORDERBOTTOM: u32 = 57;
const WIN_BORDERRPORT: u32 = 58;
const WIN_FIRSTGADGET: u32 = 62;
const WIN_PARENT: u32 = 66;
const WIN_DESCENDANT: u32 = 70;
const WIN_POINTER: u32 = 74;
const WIN_PTRHEIGHT: u32 = 78;
const WIN_PTRWIDTH: u32 = 79;
const WIN_XOFFSET: u32 = 80;
const WIN_YOFFSET: u32 = 81;
const WIN_IDCMPFLAGS: u32 = 82;
const WIN_USERPORT: u32 = 86;
const WIN_WINDOWPORT: u32 = 90;
const WIN_MESSAGEKEY: u32 = 94;
const WIN_DETAILPEN: u32 = 98;
const WIN_BLOCKPEN: u32 = 99;
const WIN_CHECKMARK: u32 = 100;
const WIN_SCREENTITLE: u32 = 104;
const WIN_GZZMOUSEX: u32 = 108;
const WIN_GZZMOUSEY: u32 = 110;
const WIN_GZZWIDTH: u32 = 112;
const WIN_GZZHEIGHT: u32 = 114;
const WIN_EXTDATA: u32 = 116;
const WIN_USERDATA: u32 = 120;
const WIN_WLAYER: u32 = 124;
const WIN_IFONT: u32 = 128;
const WIN_MOREFLAGS: u32 = 132;

// ---- struct NewWindow (intuition/intuition.h), OpenWindow's input -----

const NW_LEFTEDGE: u32 = 0;
const NW_TOPEDGE: u32 = 2;
const NW_WIDTH: u32 = 4;
const NW_HEIGHT: u32 = 6;
const NW_DETAILPEN: u32 = 8;
const NW_BLOCKPEN: u32 = 9;
const NW_IDCMPFLAGS: u32 = 10;
const NW_FLAGS: u32 = 14;
const NW_FIRSTGADGET: u32 = 18;
const NW_CHECKMARK: u32 = 22;
const NW_TITLE: u32 = 26;
const NW_SCREEN: u32 = 30;

/// `BOOL` `TRUE`, per `<exec/types.h>`.
const DTRUE: u32 = 1;

/// Host-side `intuition.library` state: just the default screen's guest
/// address, lazily filled in by [`ensure_default_screen`] on first use
/// (mirroring how `OpenLibrary`'s fake libraries are created lazily on
/// first `OpenLibrary`, not eagerly at boot). See the module docs.
pub struct IntuitionState {
    default_screen: Option<u32>,
}

impl IntuitionState {
    pub fn new() -> Self {
        Self {
            default_screen: None,
        }
    }
}

impl Default for IntuitionState {
    fn default() -> Self {
        Self::new()
    }
}

/// Returns the guest address of the always-available default public
/// screen (`"Workbench"`, 640x200x1), allocating and initializing it on
/// first call. See the module docs for what is and isn't honestly
/// modeled.
fn ensure_default_screen<M: AddressSpace>(
    state: &mut IntuitionState,
    heap: &mut GuestHeap,
    mem: &mut M,
) -> u32 {
    if let Some(addr) = state.default_screen {
        return addr;
    }
    let screen = heap
        .alloc(SCREEN_SIZE)
        .expect("guest heap has room for the one default Screen");

    let title_bytes = b"Workbench Screen";
    let title_addr = heap
        .alloc(title_bytes.len() as u32 + 1)
        .expect("guest heap has room for the screen title string");
    write_c_string(mem, title_addr, title_bytes);

    mem.write_u32(screen + SCR_NEXTSCREEN, 0);
    mem.write_u32(screen + SCR_FIRSTWINDOW, 0);
    mem.write_u16(screen + SCR_LEFTEDGE, 0);
    mem.write_u16(screen + SCR_TOPEDGE, 0);
    mem.write_u16(screen + SCR_WIDTH, 640);
    mem.write_u16(screen + SCR_HEIGHT, 200);
    mem.write_u16(screen + SCR_MOUSEY, 0);
    mem.write_u16(screen + SCR_MOUSEX, 0);
    mem.write_u16(screen + SCR_FLAGS, WBENCHSCREEN);
    mem.write_u32(screen + SCR_TITLE, title_addr);
    mem.write_u32(screen + SCR_DEFAULTTITLE, title_addr);
    mem.write_u8(screen + SCR_WBORTOP, 0);
    mem.write_u8(screen + SCR_WBORLEFT, 0);
    mem.write_u8(screen + SCR_WBORRIGHT, 0);
    mem.write_u8(screen + SCR_WBORBOTTOM, 0);
    mem.write_u32(screen + SCR_FONT, 0);

    // ViewPort: no real ColorMap/copper lists, but honest dimensions.
    let vp = screen + SCR_VIEWPORT;
    mem.write_u32(vp + VP_NEXT, 0);
    mem.write_u32(vp + VP_COLORMAP, 0);
    mem.write_u16(vp + VP_DWIDTH, 640);
    mem.write_u16(vp + VP_DHEIGHT, 200);
    mem.write_u16(vp + VP_MODES, 0);
    mem.write_u32(vp + VP_RASINFO, 0);

    // RastPort: no Layer (no layers.library here), BitMap points at
    // this same Screen's own embedded BitMap below.
    let rp = screen + SCR_RASTPORT;
    mem.write_u32(rp + RP_LAYER, 0);
    mem.write_u32(rp + RP_BITMAP, screen + SCR_BITMAP);

    // BitMap: one bitplane, 640/8 = 80 bytes/row, 200 rows -- honest
    // geometry, but Planes[] stays all-NULL (nothing to render into or
    // read back).
    let bm = screen + SCR_BITMAP;
    mem.write_u16(bm + BM_BYTESPERROW, 80);
    mem.write_u16(bm + BM_ROWS, 200);
    mem.write_u8(bm + BM_FLAGS, 0);
    mem.write_u8(bm + BM_DEPTH, 1);

    mem.write_u32(screen + SCR_LAYERINFO, 0);
    // Zero the rest of the reserved Layer_Info blob.
    for off in (4..SCR_LAYERINFO_SIZE).step_by(4) {
        mem.write_u32(screen + SCR_LAYERINFO + off, 0);
    }
    mem.write_u32(screen + SCR_FIRSTGADGET, 0);
    mem.write_u8(screen + SCR_DETAILPEN, 0);
    mem.write_u8(screen + SCR_BLOCKPEN, 1);
    mem.write_u16(screen + SCR_SAVECOLOR0, 0);
    mem.write_u32(screen + SCR_BARLAYER, 0);
    mem.write_u32(screen + SCR_EXTDATA, 0);
    mem.write_u32(screen + SCR_USERDATA, 0);

    state.default_screen = Some(screen);
    screen
}

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

/// `GetScreenData` (`A0` = buffer, `D0` = buffer size, `D1` = screen
/// type, `A1` = screen -- `D1`/`A1` ignored, this runtime has only the
/// one default screen regardless of requested type). Copies
/// `min(size, sizeof(struct Screen))` bytes of the real default screen
/// into the buffer; `D0` = `TRUE`. See the module docs.
fn get_screen_data_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let buffer = ctx.cpu.address_register(AddressRegister(0));
    let size = ctx.cpu.data_register(DataRegister(0));
    let screen = ensure_default_screen(ctx.intuition, ctx.heap, ctx.mem);
    let copy_len = size.min(SCREEN_SIZE);
    for off in 0..copy_len {
        let byte = ctx.mem.read_u8(screen + off);
        ctx.mem.write_u8(buffer + off, byte);
    }
    ctx.cpu.set_data_register(DataRegister(0), DTRUE);
    Ok(())
}

/// `LockPubScreen` (`A0` = `CONST_STRPTR` screen name, or `NULL` for
/// the default public screen). Returns the default screen's address
/// for `NULL` or a name matching it; any other name returns `NULL` --
/// see the module docs.
fn lock_pub_screen_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let name_ptr = ctx.cpu.address_register(AddressRegister(0));
    let matches_default = if name_ptr == 0 {
        true
    } else {
        let name = read_c_string(ctx.mem, name_ptr);
        name == b"Workbench"
    };
    let result = if matches_default {
        ensure_default_screen(ctx.intuition, ctx.heap, ctx.mem)
    } else {
        0
    };
    ctx.cpu.set_data_register(DataRegister(0), result);
    Ok(())
}

/// `UnlockPubScreen` (`A0` = name, ignored; `A1` = screen). No-op --
/// see the module docs.
fn unlock_pub_screen_handler<C: Cpu>(
    _ctx: &mut HandlerContext<'_, C>,
) -> Result<(), DispatchError> {
    Ok(())
}

/// `OpenWindow` (`A0` = `struct NewWindow*`). Allocates a real `struct
/// Window`, attached to `NewWindow.Screen` if given (must be the
/// default screen -- there is no other), otherwise to the default
/// screen itself. `RPort` is a fresh `struct RastPort` pointed at the
/// window's screen's `BitMap`; `UserPort`/`WindowPort` are a real
/// no-op `struct MsgPort` (see [`crate::execlist`]'s `GetMsg`/
/// `WaitPort` -- it will simply never receive an `IntuiMessage`, there
/// being no user input to report). `D0` = the new `Window*`, or `0` on
/// guest-heap exhaustion (real `OpenWindow`'s own documented failure
/// return). See the module docs.
fn open_window_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let new_window = ctx.cpu.address_register(AddressRegister(0));

    let left_edge = ctx.mem.read_u16(new_window + NW_LEFTEDGE);
    let top_edge = ctx.mem.read_u16(new_window + NW_TOPEDGE);
    let width = ctx.mem.read_u16(new_window + NW_WIDTH);
    let height = ctx.mem.read_u16(new_window + NW_HEIGHT);
    let detail_pen = ctx.mem.read_u8(new_window + NW_DETAILPEN);
    let block_pen = ctx.mem.read_u8(new_window + NW_BLOCKPEN);
    let idcmp_flags = ctx.mem.read_u32(new_window + NW_IDCMPFLAGS);
    let flags = ctx.mem.read_u32(new_window + NW_FLAGS);
    let first_gadget = ctx.mem.read_u32(new_window + NW_FIRSTGADGET);
    let check_mark = ctx.mem.read_u32(new_window + NW_CHECKMARK);
    let title = ctx.mem.read_u32(new_window + NW_TITLE);
    let requested_screen = ctx.mem.read_u32(new_window + NW_SCREEN);

    let default_screen = ensure_default_screen(ctx.intuition, ctx.heap, ctx.mem);
    let wscreen = if requested_screen == 0 {
        default_screen
    } else {
        requested_screen
    };

    let Ok(window) = ctx.heap.alloc(WINDOW_SIZE) else {
        ctx.cpu.set_data_register(DataRegister(0), 0);
        return Ok(());
    };
    let Ok(rport) = ctx.heap.alloc(RASTPORT_SIZE) else {
        ctx.heap
            .free(window)
            .expect("just-allocated Window is live");
        ctx.cpu.set_data_register(DataRegister(0), 0);
        return Ok(());
    };
    let Ok(user_port) = ctx.heap.alloc(MSGPORT_SIZE) else {
        ctx.heap
            .free(rport)
            .expect("just-allocated RastPort is live");
        ctx.heap
            .free(window)
            .expect("just-allocated Window is live");
        ctx.cpu.set_data_register(DataRegister(0), 0);
        return Ok(());
    };

    ctx.mem.write_u32(rport + RP_LAYER, 0);
    ctx.mem.write_u32(rport + RP_BITMAP, wscreen + SCR_BITMAP);
    init_msg_port_fields(ctx.mem, user_port, ctx.current_task);

    ctx.mem.write_u32(window + WIN_NEXTWINDOW, 0);
    ctx.mem.write_u16(window + WIN_LEFTEDGE, left_edge);
    ctx.mem.write_u16(window + WIN_TOPEDGE, top_edge);
    ctx.mem.write_u16(window + WIN_WIDTH, width);
    ctx.mem.write_u16(window + WIN_HEIGHT, height);
    ctx.mem.write_u16(window + WIN_MOUSEY, 0);
    ctx.mem.write_u16(window + WIN_MOUSEX, 0);
    ctx.mem.write_u16(window + WIN_MINWIDTH, width);
    ctx.mem.write_u16(window + WIN_MINHEIGHT, height);
    ctx.mem.write_u16(window + WIN_MAXWIDTH, width);
    ctx.mem.write_u16(window + WIN_MAXHEIGHT, height);
    ctx.mem.write_u32(window + WIN_FLAGS, flags);
    ctx.mem.write_u32(window + WIN_MENUSTRIP, 0);
    ctx.mem.write_u32(window + WIN_TITLE, title);
    ctx.mem.write_u32(window + WIN_FIRSTREQUEST, 0);
    ctx.mem.write_u32(window + WIN_DMREQUEST, 0);
    ctx.mem.write_u16(window + WIN_REQCOUNT, 0);
    ctx.mem.write_u32(window + WIN_WSCREEN, wscreen);
    ctx.mem.write_u32(window + WIN_RPORT, rport);
    ctx.mem.write_u8(window + WIN_BORDERLEFT, 0);
    ctx.mem.write_u8(window + WIN_BORDERTOP, 0);
    ctx.mem.write_u8(window + WIN_BORDERRIGHT, 0);
    ctx.mem.write_u8(window + WIN_BORDERBOTTOM, 0);
    ctx.mem.write_u32(window + WIN_BORDERRPORT, 0);
    ctx.mem.write_u32(window + WIN_FIRSTGADGET, first_gadget);
    ctx.mem.write_u32(window + WIN_PARENT, 0);
    ctx.mem.write_u32(window + WIN_DESCENDANT, 0);
    ctx.mem.write_u32(window + WIN_POINTER, 0);
    ctx.mem.write_u8(window + WIN_PTRHEIGHT, 0);
    ctx.mem.write_u8(window + WIN_PTRWIDTH, 0);
    ctx.mem.write_u8(window + WIN_XOFFSET, 0);
    ctx.mem.write_u8(window + WIN_YOFFSET, 0);
    ctx.mem.write_u32(window + WIN_IDCMPFLAGS, idcmp_flags);
    ctx.mem.write_u32(window + WIN_USERPORT, user_port);
    ctx.mem.write_u32(window + WIN_WINDOWPORT, user_port);
    ctx.mem.write_u32(window + WIN_MESSAGEKEY, 0);
    ctx.mem.write_u8(window + WIN_DETAILPEN, detail_pen);
    ctx.mem.write_u8(window + WIN_BLOCKPEN, block_pen);
    ctx.mem.write_u32(window + WIN_CHECKMARK, check_mark);
    ctx.mem.write_u32(window + WIN_SCREENTITLE, 0);
    ctx.mem.write_u16(window + WIN_GZZMOUSEX, 0);
    ctx.mem.write_u16(window + WIN_GZZMOUSEY, 0);
    ctx.mem.write_u16(window + WIN_GZZWIDTH, width);
    ctx.mem.write_u16(window + WIN_GZZHEIGHT, height);
    ctx.mem.write_u32(window + WIN_EXTDATA, 0);
    ctx.mem.write_u32(window + WIN_USERDATA, 0);
    ctx.mem.write_u32(window + WIN_WLAYER, 0);
    ctx.mem.write_u32(window + WIN_IFONT, 0);
    ctx.mem.write_u32(window + WIN_MOREFLAGS, 0);

    ctx.cpu.set_data_register(DataRegister(0), window);
    Ok(())
}

/// `CloseWindow` (`A0` = `struct Window*`). Frees the `Window`, its
/// `RastPort`, and its `UserPort` allocated by
/// [`open_window_handler`]. `NULL` is a legal no-op, matching this
/// runtime's other `Close*` handlers' convention.
fn close_window_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let window = ctx.cpu.address_register(AddressRegister(0));
    if window == 0 {
        return Ok(());
    }
    let rport = ctx.mem.read_u32(window + WIN_RPORT);
    let user_port = ctx.mem.read_u32(window + WIN_USERPORT);
    let fail =
        |what: &str, addr: u32, e: crate::guestmem::GuestHeapError| DispatchError::HandlerFailed {
            library: "intuition.library".to_string(),
            lvo: -72,
            handler_name: "CloseWindow".to_string(),
            message: format!("CloseWindow({window:#010x}): freeing {what} at {addr:#010x}: {e}"),
        };
    ctx.heap
        .free(user_port)
        .map_err(|e| fail("UserPort", user_port, e))?;
    ctx.heap
        .free(rport)
        .map_err(|e| fail("RastPort", rport, e))?;
    ctx.heap
        .free(window)
        .map_err(|e| fail("Window", window, e))?;
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
    reg!("GetScreenData", get_screen_data_handler::<C>);
    reg!("CurrentTime", current_time_handler::<C>);
    reg!("LockPubScreen", lock_pub_screen_handler::<C>);
    reg!("UnlockPubScreen", unlock_pub_screen_handler::<C>);
    reg!("OpenWindow", open_window_handler::<C>);
    reg!("CloseWindow", close_window_handler::<C>);
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
    fn end_to_end_get_screen_data_copies_real_default_screen() {
        let buf_addr: u32 = 0x1_8000;
        let mut words = vec![
            move_imm_to_a(0), // A0 = buffer
            (buf_addr >> 16) as u16,
            buf_addr as u16,
            move_imm_to_d(0), // D0 = size (whole struct)
            (SCREEN_SIZE >> 16) as u16,
            SCREEN_SIZE as u16,
        ];
        words.extend_from_slice(&jsr_disp16_a6(-426)); // GetScreenData
        words.push(RTS);
        let mut rt = intuition_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 1, "GetScreenData should return TRUE");
        let width = rt.memory().read_u16(buf_addr + SCR_WIDTH);
        let height = rt.memory().read_u16(buf_addr + SCR_HEIGHT);
        assert_eq!(width, 640);
        assert_eq!(height, 200);
    }

    #[test]
    fn end_to_end_lock_pub_screen_default_returns_real_screen() {
        let words = [
            move_imm_to_a(0), // A0 = NULL name -> default public screen
            0,
            0,
        ];
        let mut full = movea_intuition_base_to_a6().to_vec();
        full.extend_from_slice(&words);
        full.extend_from_slice(&jsr_disp16_a6(-510)); // LockPubScreen
        full.push(RTS);
        let mut rt = runtime_with_program(&full);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_ne!(code, 0, "LockPubScreen(NULL) should return a real Screen*");
        let width = rt.memory().read_u16(code as u32 + SCR_WIDTH);
        assert_eq!(width, 640);
    }

    #[test]
    fn end_to_end_lock_pub_screen_unknown_name_fails() {
        let name_addr: u32 = 0x1_8000;
        let words = [move_imm_to_a(0), (name_addr >> 16) as u16, name_addr as u16];
        let mut full = movea_intuition_base_to_a6().to_vec();
        full.extend_from_slice(&words);
        full.extend_from_slice(&jsr_disp16_a6(-510)); // LockPubScreen
        full.push(RTS);
        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &full);
        crate::guestmem::write_c_string(&mut mem, name_addr, b"NoSuchScreen");
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
        assert_eq!(code, 0);
    }

    #[test]
    fn end_to_end_open_close_window_round_trip() {
        let nw_addr: u32 = 0x1_8000;
        let words = vec![
            move_imm_to_a(0), // A0 = &NewWindow
            (nw_addr >> 16) as u16,
            nw_addr as u16,
        ];
        let mut full = movea_intuition_base_to_a6().to_vec();
        full.extend_from_slice(&words);
        full.extend_from_slice(&jsr_disp16_a6(-204)); // OpenWindow -> D0
        // A0 = D0 (the returned Window*), then CloseWindow(A0).
        full.push(0x2040); // MOVEA.L D0,A0
        full.extend_from_slice(&jsr_disp16_a6(-72)); // CloseWindow
        full.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &full);
        mem.write_u16(nw_addr + NW_WIDTH, 100);
        mem.write_u16(nw_addr + NW_HEIGHT, 50);
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
        // CloseWindow returns void (D0 is left holding OpenWindow's
        // stale Window* -- not asserted here); what this test actually
        // exercises is that freeing the Window/RastPort/UserPort
        // succeeds without CloseWindow's own double/invalid-free
        // HandlerFailed check tripping.
        rt.run(&mut out, None).expect("run should succeed");
    }

    #[test]
    fn end_to_end_open_window_wires_real_screen_and_rastport() {
        let nw_addr: u32 = 0x1_8000;
        let words = [
            move_imm_to_a(0), // A0 = &NewWindow
            (nw_addr >> 16) as u16,
            nw_addr as u16,
        ];
        let mut full = movea_intuition_base_to_a6().to_vec();
        full.extend_from_slice(&words);
        full.extend_from_slice(&jsr_disp16_a6(-204)); // OpenWindow -> D0
        full.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &full);
        mem.write_u16(nw_addr + NW_WIDTH, 100);
        mem.write_u16(nw_addr + NW_HEIGHT, 50);
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
        let window = code as u32;
        assert_ne!(window, 0, "OpenWindow should return a real Window*");
        let wscreen = rt.memory().read_u32(window + WIN_WSCREEN);
        assert_ne!(wscreen, 0);
        assert_eq!(rt.memory().read_u16(wscreen + SCR_WIDTH), 640);
        let rport = rt.memory().read_u32(window + WIN_RPORT);
        assert_ne!(rport, 0);
        assert_eq!(
            rt.memory().read_u32(rport + RP_BITMAP),
            wscreen + SCR_BITMAP
        );
        let user_port = rt.memory().read_u32(window + WIN_USERPORT);
        assert_ne!(user_port, 0);
        assert_eq!(rt.memory().read_u16(window + WIN_WIDTH), 100);
        assert_eq!(rt.memory().read_u16(window + WIN_HEIGHT), 50);
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
