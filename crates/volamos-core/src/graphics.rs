//! `graphics.library`: `OpenFont`/`CloseFont`/`SetFont` only, and only
//! for the one font this runtime can honestly claim to know everything
//! about -- the built-in ROM font `"topaz.font"` at 8pt ("topaz 8").
//! (`SetFont` itself is font-agnostic -- a pure RastPort field update,
//! see [`set_font_handler`] -- but the only font `OpenFont` will ever
//! hand a guest here is topaz 8.) Every other `graphics.library`
//! function (rendering, blitting, custom screens,
//! any other font/size) stays unregistered/unhandled: this runtime
//! never draws anything (see `crate::intuition`'s module docs for the
//! default-screen/window model this shares a `RastPort`/`BitMap` shape
//! with), and loading a real disk-based `.font` file would mean
//! actually parsing bitmap font data this runtime has no use for.
//!
//! # Why topaz 8 specifically
//!
//! Unlike a screen's pixel geometry (plausible-looking but unverifiable
//! without real hardware to render on), topaz 8's metrics are a fixed,
//! well-documented historical fact: it's the 8x8 fixed-pitch ROM font
//! every real Kickstart ships, used for the default Workbench/Shell
//! font since the earliest AmigaOS releases. A guest program that calls
//! `OpenFont({ta_Name: "topaz.font", ta_YSize: 8, ...})` -- extremely
//! common, since it's what `GfxBase->DefaultFont` already is on a real
//! machine -- gets back a real `struct TextFont` with genuinely correct
//! `tf_XSize`/`tf_Baseline`/`tf_BoldSmear`/`tf_LoChar`/`tf_HiChar`
//! metrics, useful for any layout math (`TextLength`-style character
//! counting a caller does itself) even though this runtime implements
//! neither `TextLength` nor real rendering. `tf_CharData` (the actual
//! glyph bitmap pointer) is left `NULL` -- there is no pixel data
//! behind it, matching the default screen's `BitMap.Planes[]` staying
//! all-`NULL` for the identical reason.
//!
//! Any other font name, or any `ta_YSize` other than `8`, fails with
//! `NULL` -- the same outcome a real machine gives when the requested
//! disk font genuinely isn't installed, not a fake success.
//!
//! Struct layouts (`TA_*`/`TF_*` byte-offset consts below) are
//! hand-computed from `<graphics/text.h>` (`struct TextAttr`, `struct
//! TextFont`) plus `<exec/ports.h>`/`<exec/nodes.h>` (`struct Message`/
//! `struct Node`, `tf_Message`'s embedded type) using m68k's word (not
//! long) struct alignment -- same provenance discipline as
//! `crate::intuition`'s `SCR_*`/`WIN_*` consts.

use crate::cpu::{AddressRegister, Cpu};
use crate::dispatch::{DispatchError, HandlerContext, LibraryTable};
use crate::execlist::{LN_NAME, LN_PRED, LN_PRI, LN_SUCC, LN_TYPE};
use crate::guestmem::{GuestHeap, read_c_string};
use crate::lvos::graphics::GRAPHICS_LVOS;
use crate::memory::AddressSpace;

// ---- struct TextAttr (graphics/text.h), OpenFont's input --------------

const TA_NAME: u32 = 0;
const TA_YSIZE: u32 = 4;
/// `ta_Style`/`ta_Flags` -- not read by [`open_font_handler`] (topaz 8
/// has exactly one style/flags combination this runtime honors
/// regardless of what the caller requests), only used by this module's
/// own tests to build a realistic `TextAttr`.
#[cfg(test)]
const TA_STYLE: u32 = 6;
#[cfg(test)]
const TA_FLAGS: u32 = 7;

// ---- struct TextFont (graphics/text.h), OpenFont's output -------------

/// `sizeof(struct TextFont)`.
const TEXTFONT_SIZE: u32 = 52;
/// Byte offset of the embedded `struct Message` (`tf_Message`) within
/// `struct TextFont`.
const TF_MESSAGE: u32 = 0;
const TF_YSIZE: u32 = 20;
const TF_STYLE: u32 = 22;
const TF_FLAGS: u32 = 23;
const TF_XSIZE: u32 = 24;
const TF_BASELINE: u32 = 26;
const TF_BOLDSMEAR: u32 = 28;
const TF_ACCESSORS: u32 = 30;
const TF_LOCHAR: u32 = 32;
const TF_HICHAR: u32 = 33;
const TF_CHARDATA: u32 = 34;
const TF_MODULO: u32 = 38;
const TF_CHARLOC: u32 = 40;
const TF_CHARSPACE: u32 = 44;
const TF_CHARKERN: u32 = 48;

// ---- struct RastPort (graphics/rastport.h), SetFont's target ----------
//
// Only the text-attribute fields SetFont touches -- the RastPort
// itself is allocated elsewhere (the guest's own, or
// `crate::intuition::open_window_handler`'s).

const RP_FGPEN: u32 = 25;
const RP_BGPEN: u32 = 26;
const RP_DRAWMODE: u32 = 28;
const RP_CP_X: u32 = 36;
const RP_CP_Y: u32 = 38;
const RP_FONT: u32 = 52;
const RP_ALGOSTYLE: u32 = 56;
const RP_TXFLAGS: u32 = 57;
const RP_TXHEIGHT: u32 = 58;
const RP_TXWIDTH: u32 = 60;
const RP_TXBASELINE: u32 = 62;
const RP_TXSPACING: u32 = 64;

// ---- struct Message (exec/ports.h), embedded as tf_Message ------------

const MN_NODE: u32 = 0;
const MN_REPLYPORT: u32 = 14;
const MN_LENGTH: u32 = 18;

/// `exec/nodes.h`'s `NT_FONT` (12) -- a font's `tf_Message.mn_Node.
/// ln_Type` on the real graphics.library font list. This runtime never
/// actually links the font onto any list (there is no font list here),
/// but a caller reading the node type back sees the real value.
const NT_FONT: u8 = 12;

/// `FS_NORMAL` (0), per `<graphics/text.h>` -- topaz 8's intrinsic
/// style.
const FS_NORMAL: u8 = 0;
/// `FPF_ROMFONT | FPF_DESIGNED` (`0x01 | 0x40`), per `<graphics/
/// text.h>` -- topaz 8 is a built-in ROM font with an explicitly
/// designed (not constructed) size.
const TOPAZ_FLAGS: u8 = 0x41;

/// `"topaz.font"`, the only font name [`open_font_handler`] honors.
const TOPAZ_NAME: &[u8] = b"topaz.font";
/// The only `ta_YSize` [`open_font_handler`] honors -- see the module
/// docs for why this runtime can't honestly claim any other size.
const TOPAZ_YSIZE: u16 = 8;
const TOPAZ_XSIZE: u16 = 8;
const TOPAZ_BASELINE: u16 = 6;
const TOPAZ_BOLDSMEAR: u16 = 1;
const TOPAZ_LOCHAR: u8 = 0;
const TOPAZ_HICHAR: u8 = 255;

/// `OpenFont` (`A0` = `const struct TextAttr*`). Succeeds only for
/// `ta_Name == "topaz.font"` and `ta_YSize == 8` -- see the module
/// docs. `D0` = a real `struct TextFont*` with genuine topaz-8 metrics
/// (no glyph bitmap data behind `tf_CharData`), or `0` for any other
/// request, matching real `OpenFont`'s own "font not found" return.
fn open_font_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let text_attr = ctx.cpu.address_register(AddressRegister(0));
    let name_ptr = ctx.mem.read_u32(text_attr + TA_NAME);
    let y_size = ctx.mem.read_u16(text_attr + TA_YSIZE);
    let name = read_c_string(ctx.mem, name_ptr);

    if name != TOPAZ_NAME || y_size != TOPAZ_YSIZE {
        ctx.cpu.set_data_register(crate::cpu::DataRegister(0), 0);
        return Ok(());
    }

    let Ok(font) = ctx.heap.alloc(TEXTFONT_SIZE) else {
        ctx.cpu.set_data_register(crate::cpu::DataRegister(0), 0);
        return Ok(());
    };
    let name_addr = match alloc_font_name(ctx.heap, ctx.mem) {
        Some(addr) => addr,
        None => {
            ctx.heap
                .free(font)
                .expect("just-allocated TextFont is live");
            ctx.cpu.set_data_register(crate::cpu::DataRegister(0), 0);
            return Ok(());
        }
    };

    let msg = font + TF_MESSAGE;
    ctx.mem.write_u32(msg + MN_NODE + LN_SUCC, 0);
    ctx.mem.write_u32(msg + MN_NODE + LN_PRED, 0);
    ctx.mem.write_u8(msg + MN_NODE + LN_TYPE, NT_FONT);
    ctx.mem.write_u8(msg + MN_NODE + LN_PRI, 0);
    ctx.mem.write_u32(msg + MN_NODE + LN_NAME, name_addr);
    ctx.mem.write_u32(msg + MN_REPLYPORT, 0);
    ctx.mem.write_u16(msg + MN_LENGTH, TEXTFONT_SIZE as u16);

    ctx.mem.write_u16(font + TF_YSIZE, TOPAZ_YSIZE);
    ctx.mem.write_u8(font + TF_STYLE, FS_NORMAL);
    ctx.mem.write_u8(font + TF_FLAGS, TOPAZ_FLAGS);
    ctx.mem.write_u16(font + TF_XSIZE, TOPAZ_XSIZE);
    ctx.mem.write_u16(font + TF_BASELINE, TOPAZ_BASELINE);
    ctx.mem.write_u16(font + TF_BOLDSMEAR, TOPAZ_BOLDSMEAR);
    ctx.mem.write_u16(font + TF_ACCESSORS, 0);
    ctx.mem.write_u8(font + TF_LOCHAR, TOPAZ_LOCHAR);
    ctx.mem.write_u8(font + TF_HICHAR, TOPAZ_HICHAR);
    ctx.mem.write_u32(font + TF_CHARDATA, 0);
    ctx.mem.write_u16(font + TF_MODULO, 0);
    ctx.mem.write_u32(font + TF_CHARLOC, 0);
    ctx.mem.write_u32(font + TF_CHARSPACE, 0);
    ctx.mem.write_u32(font + TF_CHARKERN, 0);

    ctx.cpu.set_data_register(crate::cpu::DataRegister(0), font);
    Ok(())
}

/// Allocates and writes the `"topaz.font"` name string used as both
/// `tf_Message.mn_Node.ln_Name` and (per real `OpenFont` convention)
/// the font's own identity -- a fresh copy per `OpenFont` call, freed
/// by [`close_font_handler`] alongside the rest.
fn alloc_font_name<M: AddressSpace>(heap: &mut GuestHeap, mem: &mut M) -> Option<u32> {
    let addr = heap.alloc(TOPAZ_NAME.len() as u32 + 1).ok()?;
    crate::guestmem::write_c_string(mem, addr, TOPAZ_NAME);
    Some(addr)
}

/// `CloseFont` (`A1` = `struct TextFont*`). Frees the `TextFont` and
/// its name string allocated by [`open_font_handler`]. `NULL` is a
/// legal no-op, matching this runtime's other `Close*` handlers'
/// convention.
fn close_font_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let font = ctx.cpu.address_register(AddressRegister(1));
    if font == 0 {
        return Ok(());
    }
    let name_addr = ctx.mem.read_u32(font + TF_MESSAGE + MN_NODE + LN_NAME);
    let fail =
        |what: &str, addr: u32, e: crate::guestmem::GuestHeapError| DispatchError::HandlerFailed {
            library: "graphics.library".to_string(),
            lvo: -78,
            handler_name: "CloseFont".to_string(),
            message: format!("CloseFont({font:#010x}): freeing {what} at {addr:#010x}: {e}"),
        };
    if name_addr != 0 {
        ctx.heap
            .free(name_addr)
            .map_err(|e| fail("font name", name_addr, e))?;
    }
    ctx.heap.free(font).map_err(|e| fail("TextFont", font, e))?;
    Ok(())
}

/// `SetFont` (`A1` = `struct RastPort*`, `A0` = `struct TextFont*`).
/// Stores the font in `rp_Font` and copies its metrics into the
/// RastPort's text attributes (`TxHeight`/`TxWidth`/`TxBaseline` from
/// `tf_YSize`/`tf_XSize`/`tf_Baseline`, `TxSpacing` = 0), clearing any
/// previous soft style (`AlgoStyle`/`TxFlags` = 0) -- exactly the
/// real function's documented effect ("sets the font in the RastPort
/// ... and updates the text attributes to reflect that change. This
/// function clears the effect of any previous soft styles", per the
/// Autodoc). No return value. A pure struct-field update -- nothing to
/// render, so nothing here is faked. A `NULL` font is a no-op (the
/// Autodoc explicitly discourages `SetFont(rp, 0)` and documents it as
/// broken on real releases; failing quietly beats emulating "spurious
/// low memory accesses").
fn set_font_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let rp = ctx.cpu.address_register(AddressRegister(1));
    let font = ctx.cpu.address_register(AddressRegister(0));
    if font == 0 {
        return Ok(());
    }
    let y_size = ctx.mem.read_u16(font + TF_YSIZE);
    let x_size = ctx.mem.read_u16(font + TF_XSIZE);
    let baseline = ctx.mem.read_u16(font + TF_BASELINE);
    ctx.mem.write_u32(rp + RP_FONT, font);
    ctx.mem.write_u8(rp + RP_ALGOSTYLE, 0);
    ctx.mem.write_u8(rp + RP_TXFLAGS, 0);
    ctx.mem.write_u16(rp + RP_TXHEIGHT, y_size);
    ctx.mem.write_u16(rp + RP_TXWIDTH, x_size);
    ctx.mem.write_u16(rp + RP_TXBASELINE, baseline);
    ctx.mem.write_u16(rp + RP_TXSPACING, 0);
    Ok(())
}

// ---- Rendering state setters + pretend-succeed render calls -----------
//
// The state setters (`SetAPen`/`SetBPen`/`SetDrMd`/`Move`) store their
// values honestly into the caller's RastPort -- pure field updates a
// guest can read back consistently, exactly what the real functions do
// minus the hardware side effects. The render calls (`Draw`/`Text`/
// `RectFill`/`SetRast`) pretend to succeed without drawing anything --
// there are no pixels here to draw into (`BitMap.Planes[]` is
// all-`NULL`, see `crate::intuition`) -- but still perform their
// documented *state* side effects (`Draw`/`Text` advance the pen
// position) so a guest doing incremental layout math stays consistent.
// `Text`'s advance and `TextLength`'s return are genuinely correct, not
// pretend, for this runtime's fixed-width topaz 8: width is exactly
// `count * (TxWidth + TxSpacing)`. `WaitTOF`/`WaitBlit` return
// immediately -- with no display beam or blitter, "done" is the honest
// answer.

/// `SetAPen` (`A1` = rp, `D0` = pen). Stores `FgPen`.
fn set_a_pen_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let rp = ctx.cpu.address_register(AddressRegister(1));
    let pen = ctx.cpu.data_register(crate::cpu::DataRegister(0));
    ctx.mem.write_u8(rp + RP_FGPEN, pen as u8);
    Ok(())
}

/// `SetBPen` (`A1` = rp, `D0` = pen). Stores `BgPen`.
fn set_b_pen_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let rp = ctx.cpu.address_register(AddressRegister(1));
    let pen = ctx.cpu.data_register(crate::cpu::DataRegister(0));
    ctx.mem.write_u8(rp + RP_BGPEN, pen as u8);
    Ok(())
}

/// `SetDrMd` (`A1` = rp, `D0` = drawing mode). Stores `DrawMode`.
fn set_dr_md_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let rp = ctx.cpu.address_register(AddressRegister(1));
    let mode = ctx.cpu.data_register(crate::cpu::DataRegister(0));
    ctx.mem.write_u8(rp + RP_DRAWMODE, mode as u8);
    Ok(())
}

/// `Move` (`A1` = rp, `D0` = x, `D1` = y). Stores the pen position
/// (`cp_x`/`cp_y`) -- the real function's entire effect.
fn move_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let rp = ctx.cpu.address_register(AddressRegister(1));
    let x = ctx.cpu.data_register(crate::cpu::DataRegister(0));
    let y = ctx.cpu.data_register(crate::cpu::DataRegister(1));
    ctx.mem.write_u16(rp + RP_CP_X, x as u16);
    ctx.mem.write_u16(rp + RP_CP_Y, y as u16);
    Ok(())
}

/// `Draw` (`A1` = rp, `D0` = x, `D1` = y). No line is drawn (nothing
/// to draw into); the pen position still moves to the endpoint, real
/// `Draw`'s documented state side effect.
fn draw_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    move_handler(ctx)
}

/// `Text` (`A1` = rp, `A0` = string, `D0` = count). Nothing is
/// rendered; the pen position still advances by the text's width --
/// genuinely correct for fixed-width topaz 8: `count * (TxWidth +
/// TxSpacing)`. `D0` = `0` (the classic return).
fn text_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let rp = ctx.cpu.address_register(AddressRegister(1));
    let count = ctx.cpu.data_register(crate::cpu::DataRegister(0)) & 0xFFFF;
    let advance = text_pixel_width(ctx.mem, rp, count);
    let cp_x = ctx.mem.read_u16(rp + RP_CP_X);
    ctx.mem
        .write_u16(rp + RP_CP_X, cp_x.wrapping_add(advance as u16));
    ctx.cpu.set_data_register(crate::cpu::DataRegister(0), 0);
    Ok(())
}

/// `TextLength` (`A1` = rp, `A0` = string, `D0` = count). `D0` = the
/// text's pixel width -- genuinely correct for a fixed-width font, see
/// [`text_handler`].
fn text_length_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let rp = ctx.cpu.address_register(AddressRegister(1));
    let count = ctx.cpu.data_register(crate::cpu::DataRegister(0)) & 0xFFFF;
    let width = text_pixel_width(ctx.mem, rp, count);
    ctx.cpu
        .set_data_register(crate::cpu::DataRegister(0), width);
    Ok(())
}

/// `count * (TxWidth + TxSpacing)` off the RastPort's current text
/// attributes -- exact for any fixed-width font (the only kind
/// [`open_font_handler`] hands out).
fn text_pixel_width<M: AddressSpace>(mem: &M, rp: u32, count: u32) -> u32 {
    let tx_width = mem.read_u16(rp + RP_TXWIDTH) as u32;
    let tx_spacing = mem.read_u16(rp + RP_TXSPACING) as i16 as i32;
    count.wrapping_mul((tx_width as i32 + tx_spacing).max(0) as u32)
}

/// `RectFill` (`A1` = rp, `D0`-`D3` = corners). Pretends to succeed;
/// nothing to fill. No state side effects (real `RectFill` doesn't
/// move the pen).
fn rect_fill_handler<C: Cpu>(_ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    Ok(())
}

/// `SetRast` (`A1` = rp, `D0` = pen). Pretends to succeed; nothing to
/// clear.
fn set_rast_handler<C: Cpu>(_ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    Ok(())
}

/// `WaitTOF` (no args). Returns immediately -- no display beam to wait
/// on.
fn wait_tof_handler<C: Cpu>(_ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    Ok(())
}

/// `WaitBlit` (no args). Returns immediately -- no blitter.
fn wait_blit_handler<C: Cpu>(_ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    Ok(())
}

/// Registers this module's `graphics.library` handlers, looked up by
/// name through [`GRAPHICS_LVOS`], following
/// [`crate::intuition::register_intuition_handlers`]'s registration
/// pattern. Called unconditionally from [`crate::dispatch::Runtime::
/// new`] -- `graphics.library` is a
/// [`crate::dispatch::STANDARD_WORKBENCH_LIBRARIES`] member (always
/// present, matching real KS/WB 3.1 ROM-resident behavior).
pub fn register_graphics_handlers<C: Cpu + 'static>(
    table: &mut LibraryTable<C>,
    mem: &mut C::Memory,
) {
    macro_rules! reg {
        ($name:literal, $handler:expr) => {
            table
                .register_by_name(
                    mem,
                    crate::dispatch::GRAPHICS_LIBRARY_BASE,
                    GRAPHICS_LVOS,
                    "graphics.library",
                    $name,
                    $handler,
                )
                .unwrap_or_else(|e| panic!("{} should be in GRAPHICS_LVOS: {e}", $name));
        };
    }
    reg!("OpenFont", open_font_handler::<C>);
    reg!("CloseFont", close_font_handler::<C>);
    reg!("SetFont", set_font_handler::<C>);
    reg!("SetAPen", set_a_pen_handler::<C>);
    reg!("SetBPen", set_b_pen_handler::<C>);
    reg!("SetDrMd", set_dr_md_handler::<C>);
    reg!("Move", move_handler::<C>);
    reg!("Draw", draw_handler::<C>);
    reg!("Text", text_handler::<C>);
    reg!("TextLength", text_length_handler::<C>);
    reg!("RectFill", rect_fill_handler::<C>);
    reg!("SetRast", set_rast_handler::<C>);
    reg!("WaitTOF", wait_tof_handler::<C>);
    reg!("WaitBlit", wait_blit_handler::<C>);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{GRAPHICS_LIBRARY_BASE, Runtime, StartConfig};
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
    fn jsr_disp16_a6(disp: i32) -> [u16; 2] {
        [0x4EAE, disp as u16]
    }
    const RTS: u16 = 0x4E75;

    fn movea_graphics_base_to_a6() -> [u16; 3] {
        [
            move_imm_to_a(6),
            (GRAPHICS_LIBRARY_BASE >> 16) as u16,
            GRAPHICS_LIBRARY_BASE as u16,
        ]
    }

    fn runtime_with_program_and_text_attr(
        name: &[u8],
        y_size: u16,
        words: &[u16],
    ) -> Runtime<M68kCpu> {
        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        let mut full = movea_graphics_base_to_a6().to_vec();
        full.extend_from_slice(words);
        load_words(&mut mem, entry, &full);

        let name_addr: u32 = 0x1_8000;
        let ta_addr: u32 = 0x1_8100;
        crate::guestmem::write_c_string(&mut mem, name_addr, name);
        mem.write_u32(ta_addr + TA_NAME, name_addr);
        mem.write_u16(ta_addr + TA_YSIZE, y_size);
        mem.write_u8(ta_addr + TA_STYLE, 0);
        mem.write_u8(ta_addr + TA_FLAGS, 0);

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

    #[test]
    fn end_to_end_open_font_topaz_8_returns_real_metrics() {
        let ta_addr: u32 = 0x1_8100;
        let mut words = vec![
            move_imm_to_a(0), // A0 = &TextAttr
            (ta_addr >> 16) as u16,
            ta_addr as u16,
        ];
        words.extend_from_slice(&jsr_disp16_a6(-72)); // OpenFont
        words.push(RTS);
        let mut rt = runtime_with_program_and_text_attr(b"topaz.font", 8, &words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        let font = code as u32;
        assert_ne!(font, 0, "OpenFont(topaz.font, 8) should succeed");
        assert_eq!(rt.memory().read_u16(font + TF_YSIZE), 8);
        assert_eq!(rt.memory().read_u16(font + TF_XSIZE), 8);
        assert_eq!(rt.memory().read_u16(font + TF_BASELINE), 6);
        assert_eq!(rt.memory().read_u16(font + TF_BOLDSMEAR), 1);
        assert_eq!(rt.memory().read_u8(font + TF_LOCHAR), 0);
        assert_eq!(rt.memory().read_u8(font + TF_HICHAR), 255);
    }

    #[test]
    fn end_to_end_open_font_wrong_name_fails() {
        let ta_addr: u32 = 0x1_8100;
        let mut words = vec![move_imm_to_a(0), (ta_addr >> 16) as u16, ta_addr as u16];
        words.extend_from_slice(&jsr_disp16_a6(-72)); // OpenFont
        words.push(RTS);
        let mut rt = runtime_with_program_and_text_attr(b"garnet.font", 8, &words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 0);
    }

    #[test]
    fn end_to_end_open_font_wrong_size_fails() {
        let ta_addr: u32 = 0x1_8100;
        let mut words = vec![move_imm_to_a(0), (ta_addr >> 16) as u16, ta_addr as u16];
        words.extend_from_slice(&jsr_disp16_a6(-72)); // OpenFont
        words.push(RTS);
        let mut rt = runtime_with_program_and_text_attr(b"topaz.font", 9, &words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 0);
    }

    #[test]
    fn end_to_end_set_font_copies_metrics_into_rastport() {
        let ta_addr: u32 = 0x1_8100;
        let rp_addr: u32 = 0x1_8200;
        let mut words = vec![move_imm_to_a(0), (ta_addr >> 16) as u16, ta_addr as u16];
        words.extend_from_slice(&jsr_disp16_a6(-72)); // OpenFont -> D0
        words.push(0x2040); // MOVEA.L D0,A0 (the font)
        words.extend_from_slice(&[
            move_imm_to_a(1), // A1 = &RastPort
            (rp_addr >> 16) as u16,
            rp_addr as u16,
        ]);
        words.extend_from_slice(&jsr_disp16_a6(-66)); // SetFont
        words.push(RTS);
        let mut rt = runtime_with_program_and_text_attr(b"topaz.font", 8, &words);
        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed");
        let font = rt.memory().read_u32(rp_addr + RP_FONT);
        assert_ne!(font, 0, "rp_Font should hold the opened TextFont");
        assert_eq!(rt.memory().read_u16(rp_addr + RP_TXHEIGHT), 8);
        assert_eq!(rt.memory().read_u16(rp_addr + RP_TXWIDTH), 8);
        assert_eq!(rt.memory().read_u16(rp_addr + RP_TXBASELINE), 6);
        assert_eq!(rt.memory().read_u16(rp_addr + RP_TXSPACING), 0);
        assert_eq!(rt.memory().read_u16(font + TF_YSIZE), 8);
    }

    #[test]
    fn end_to_end_set_font_null_font_is_a_no_op() {
        let ta_addr: u32 = 0x1_8100;
        let rp_addr: u32 = 0x1_8200;
        let mut words = vec![
            move_imm_to_a(0), // A0 = NULL font
            0,
            0,
            move_imm_to_a(1), // A1 = &RastPort
            (rp_addr >> 16) as u16,
            rp_addr as u16,
        ];
        words.extend_from_slice(&jsr_disp16_a6(-66)); // SetFont
        words.push(RTS);
        let mut rt = runtime_with_program_and_text_attr(b"topaz.font", 8, &words);
        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            rt.memory().read_u32(rp_addr + RP_FONT),
            0,
            "NULL SetFont should leave the RastPort untouched"
        );
    }

    fn move_imm_to_d(n: u16) -> u16 {
        0x203C | (n << 9)
    }

    #[test]
    fn end_to_end_pen_mode_and_position_setters_store_into_rastport() {
        let ta_addr: u32 = 0x1_8100;
        let rp_addr: u32 = 0x1_8200;
        let mut words = vec![
            move_imm_to_a(1), // A1 = &RastPort (kept across all calls)
            (rp_addr >> 16) as u16,
            rp_addr as u16,
            move_imm_to_d(0), // D0 = pen 3
            0,
            3,
        ];
        words.extend_from_slice(&jsr_disp16_a6(-342)); // SetAPen
        words.extend_from_slice(&[move_imm_to_d(0), 0, 1]);
        words.extend_from_slice(&jsr_disp16_a6(-348)); // SetBPen
        words.extend_from_slice(&[move_imm_to_d(0), 0, 2]);
        words.extend_from_slice(&jsr_disp16_a6(-354)); // SetDrMd (COMPLEMENT)
        words.extend_from_slice(&[
            move_imm_to_d(0), // D0 = x 100
            0,
            100,
            move_imm_to_d(1), // D1 = y 50
            0,
            50,
        ]);
        words.extend_from_slice(&jsr_disp16_a6(-240)); // Move
        words.extend_from_slice(&[
            move_imm_to_d(0), // D0 = x 200
            0,
            200,
            move_imm_to_d(1), // D1 = y 80
            0,
            80,
        ]);
        words.extend_from_slice(&jsr_disp16_a6(-246)); // Draw
        words.push(RTS);
        let mut rt = runtime_with_program_and_text_attr(b"topaz.font", 8, &words);
        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(rt.memory().read_u8(rp_addr + RP_FGPEN), 3);
        assert_eq!(rt.memory().read_u8(rp_addr + RP_BGPEN), 1);
        assert_eq!(rt.memory().read_u8(rp_addr + RP_DRAWMODE), 2);
        // Draw moved the pen to its endpoint, past Move's position.
        assert_eq!(rt.memory().read_u16(rp_addr + RP_CP_X), 200);
        assert_eq!(rt.memory().read_u16(rp_addr + RP_CP_Y), 80);
    }

    #[test]
    fn end_to_end_text_length_and_text_advance_use_real_topaz_width() {
        let ta_addr: u32 = 0x1_8100;
        let rp_addr: u32 = 0x1_8200;
        let mut words = vec![move_imm_to_a(0), (ta_addr >> 16) as u16, ta_addr as u16];
        words.extend_from_slice(&jsr_disp16_a6(-72)); // OpenFont -> D0
        words.push(0x2040); // MOVEA.L D0,A0 (the font)
        words.extend_from_slice(&[
            move_imm_to_a(1), // A1 = &RastPort
            (rp_addr >> 16) as u16,
            rp_addr as u16,
        ]);
        words.extend_from_slice(&jsr_disp16_a6(-66)); // SetFont
        // Text(rp, str, 5): string pointer doesn't matter for width
        // math (fixed-width font), reuse the TextAttr address.
        words.extend_from_slice(&[
            move_imm_to_d(0), // D0 = count 5
            0,
            5,
        ]);
        words.extend_from_slice(&jsr_disp16_a6(-60)); // Text
        words.extend_from_slice(&[move_imm_to_d(0), 0, 5]);
        words.extend_from_slice(&jsr_disp16_a6(-54)); // TextLength -> D0
        words.push(RTS);
        let mut rt = runtime_with_program_and_text_attr(b"topaz.font", 8, &words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 40, "TextLength(5 chars) should be 5 * 8 pixels");
        assert_eq!(
            rt.memory().read_u16(rp_addr + RP_CP_X),
            40,
            "Text should advance cp_x by the rendered width"
        );
    }

    #[test]
    fn end_to_end_open_close_font_round_trip() {
        let ta_addr: u32 = 0x1_8100;
        let mut words = vec![move_imm_to_a(0), (ta_addr >> 16) as u16, ta_addr as u16];
        words.extend_from_slice(&jsr_disp16_a6(-72)); // OpenFont -> D0
        words.push(0x2240); // MOVEA.L D0,A1
        words.extend_from_slice(&jsr_disp16_a6(-78)); // CloseFont
        words.push(RTS);
        let mut rt = runtime_with_program_and_text_attr(b"topaz.font", 8, &words);
        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed");
    }
}
