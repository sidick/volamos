//! `expansion.library`: the Zorro autoconfig bookkeeping library -- and,
//! on this runtime, the truthful answer that nothing is plugged in.
//!
//! # Why a library for a machine with no slots
//!
//! `expansion.library` has been in Kickstart since 1.2 and is present on
//! every real Amiga, including a bare A500 with an empty expansion
//! connector. Before this module `OpenLibrary("expansion.library")`
//! returned `NULL` here -- it isn't one of
//! [`crate::dispatch::STANDARD_WORKBENCH_LIBRARIES`] and the disk-library
//! path can't find it in `LIBS:` -- which left a guest unable to
//! distinguish "this machine has no expansion bus" from "this Kickstart
//! is broken". They are very different facts, and real programs act on
//! the difference: the real Workbench 3.1.4 `BindDrivers` exits `20`
//! (`RETURN_FAIL`) on the `NULL`, where on a machine with no unbound
//! boards it should print nothing and exit `0`.
//!
//! That made it an inconsistency in this runtime's own model rather than
//! a missing feature: volamos already decided a ROM library must always
//! open (see [`crate::dispatch::STANDARD_WORKBENCH_LIBRARIES`]'s doc),
//! and `expansion.library` is more unconditionally present than any
//! library on that list.
//!
//! # Honest emptiness, not a stub
//!
//! The distinction this module holds to: an empty board list and a
//! `FindConfigDev` that finds nothing are *correct answers* for a
//! runtime with no Zorro bus, in the way that a `graphics.library` which
//! accepted drawing calls and rendered nothing would not be. A guest
//! asks what is plugged in and is told, accurately, nothing.
//!
//! So the enumeration and bookkeeping calls are implemented for real:
//!
//! - `FindConfigDev` walks a real, empty `ConfigDev` list -- and finds a
//!   board a guest added itself with `AddConfigDev`, the same consistency
//!   `AddResource`/`OpenResource` keep in [`crate::execlist`].
//! - `AddConfigDev`/`RemConfigDev` link and unlink on that list through
//!   `crate::execlist`'s own `AddTail`/`Remove` primitives.
//! - `ObtainConfigBinding`/`ReleaseConfigBinding` are no-ops. The autodoc
//!   describes them as "built directly on Exec SignalSemaphore code", and
//!   they exist so two driver binders don't claim the same board; with no
//!   boards to claim there is nothing to serialize. `BindDrivers` calls
//!   them, so they have to answer rather than fail.
//! - `GetCurrentBinding`/`SetCurrentBinding` copy the `CurrentBinding`
//!   struct in and out of `ExpansionBase`'s own private field, which is
//!   exactly what they do on hardware ("out of a private place... really
//!   a kludge, but it is the only way to pass extra arguments to a newly
//!   configured device").
//! - `AllocExpansionMem`/`AllocBoardMem` fail (`0`). There is no
//!   autoconfig address space to carve a board window out of, and
//!   "cannot" is the truthful answer; `FreeExpansionMem`/`FreeBoardMem`
//!   are no-ops for symmetry.
//! - `AllocConfigDev`/`FreeConfigDev` really allocate and free a zeroed
//!   `ConfigDev` on the guest heap -- it is just memory, and a guest that
//!   wants to describe a board of its own should be able to.
//!
//! # What is deliberately left unimplemented
//!
//! Everything that *acts* on hardware or on the filesystem namespace,
//! because a silent success there is the plausible-wrong-answer trap this
//! runtime's whole posture is against -- a guest would believe it had
//! configured a board or mounted a volume:
//!
//! - `ConfigBoard`/`ConfigChain` -- configure a physical board into the
//!   address map.
//! - `ReadExpansionRom`/`ReadExpansionByte`/`WriteExpansionByte` --
//!   address a board's autoconfig space directly. With no boards there is
//!   no address to pass, so reaching these means a guest assumed hardware
//!   this runtime hasn't got.
//! - `MakeDosNode`/`AddDosNode`/`AddBootNode` -- mount a filesystem.
//!   Volumes come from `-V`/`-a` here (see [`crate::vfs`]), and a guest
//!   that thinks it has mounted one would be wrong in a way it could not
//!   detect.
//!
//! Each of those reaches this runtime's "unknown call" diagnostic, which
//! names the library and the LVO so the autodoc identifies it
//! immediately.
//!
//! Layout and LVOs are from the NDK 3.2 includes:
//! `libraries/expansionbase.i` (`struct ExpansionBase`),
//! `libraries/configvars.i` (`ConfigDev`, `CurrentBinding`),
//! `libraries/configregs.i` (`ExpansionRom`) and `lvo/expansion_lib.i`.

use crate::cpu::{AddressRegister, Cpu, DataRegister};
use crate::dispatch::{DispatchError, EXPANSION_LIBRARY_BASE, HandlerContext, LibraryTable};
use crate::memory::AddressSpace;

/// `eb_Flags`, right after the `struct Library` header (`LIB_SIZE`, 34).
/// `0`: none of the `EBF_*` conditions (clogged, short memory, bad
/// memory) can arise with no bus to configure.
const EB_FLAGS: u32 = 34;

/// `eb_Private04`, the `CurrentBinding` struct: `eb_Flags` (1) +
/// `eb_Private01` (1) + `eb_Private02` (4) + `eb_Private03` (4) past
/// `LIB_SIZE`, so 34 + 10 = 44.
const EB_CURRENT_BINDING: u32 = 44;

/// `struct CurrentBinding` is four pointers (`cb_ConfigDev`,
/// `cb_FileName`, `cb_ProductString`, `cb_ToolTypes`).
const CURRENT_BINDING_SIZE: u32 = 16;

/// `eb_Private05`, the board list: `EB_CURRENT_BINDING` + 16. The NDK
/// header calls it private and says "Direct access is not permitted. Use
/// FindConfigDev() to scan the board list" -- so it is a real, walkable
/// `struct List` here, but only this module's own handlers read it.
const EB_BOARD_LIST: u32 = EB_CURRENT_BINDING + CURRENT_BINDING_SIZE;

/// `eb_MountList`, holding `struct BootNode` entries:
/// `EB_BOARD_LIST` + `LH_SIZE` (14). Empty, and kept so a guest walking
/// it finds a well-formed empty list rather than the trap-table prefill.
const EB_MOUNT_LIST: u32 = EB_BOARD_LIST + crate::execlist::LIST_SIZE;

/// Bytes of `struct ExpansionBase` this module maintains, through
/// `eb_MountList`'s end.
pub(crate) const EXPANSION_BASE_SIZE: u32 = EB_MOUNT_LIST + crate::execlist::LIST_SIZE;

/// The struct has to fit between its base and the two end-anchored
/// A-line stubs at the top of the reserved region -- overwriting the
/// first of those would stop the guest's outermost `rts` trapping, so
/// growing this struct (or moving the base) past them is a compile
/// error rather than a mystery at runtime. Same guard, and the same
/// reason, as `crate::battclock`'s own.
const _: () = assert!(
    EXPANSION_LIBRARY_BASE + EXPANSION_BASE_SIZE < crate::dispatch::EXIT_STUB_ADDR,
    "struct ExpansionBase must fit below EXIT_STUB_ADDR"
);

/// `cd_Rom` within `struct ConfigDev`: `cd_Node` (`LN_SIZE`, 14) +
/// `cd_Flags` (1) + `cd_Pad` (1).
const CD_ROM: u32 = 16;

/// `er_Product` within `struct ExpansionRom`: right after `er_Type`.
const ER_PRODUCT: u32 = CD_ROM + 1;

/// `er_Manufacturer` (a `UWORD`): `er_Type`/`er_Product`/`er_Flags`/
/// `er_Reserved03` are one byte each.
const ER_MANUFACTURER: u32 = CD_ROM + 4;

/// `sizeof(struct ConfigDev)`, summed field by field from
/// `libraries/configvars.i`: `cd_Node` (14), `cd_Flags` and `cd_Pad`
/// (1 each), `cd_Rom` (`ExpansionRom_SIZEOF`, 16), `cd_BoardAddr` (4),
/// `cd_BoardSize` (4), `cd_SlotAddr` and `cd_SlotSize` (2 each),
/// `cd_Driver` (4), `cd_NextCD` (4), `cd_Unused[4]` (16) -- 68 bytes.
const CONFIG_DEV_SIZE: u32 = 68;

const LVO_ADD_CONFIG_DEV: i32 = -30;
const LVO_ALLOC_BOARD_MEM: i32 = -42;
const LVO_ALLOC_CONFIG_DEV: i32 = -48;
const LVO_ALLOC_EXPANSION_MEM: i32 = -54;
const LVO_FIND_CONFIG_DEV: i32 = -72;
const LVO_FREE_BOARD_MEM: i32 = -78;
const LVO_FREE_CONFIG_DEV: i32 = -84;
const LVO_FREE_EXPANSION_MEM: i32 = -90;
const LVO_REM_CONFIG_DEV: i32 = -108;
const LVO_OBTAIN_CONFIG_BINDING: i32 = -120;
const LVO_RELEASE_CONFIG_BINDING: i32 = -126;
const LVO_SET_CURRENT_BINDING: i32 = -132;
const LVO_GET_CURRENT_BINDING: i32 = -138;

/// Guest address of `ExpansionBase`'s board list.
const BOARD_LIST_ADDR: u32 = EXPANSION_LIBRARY_BASE + EB_BOARD_LIST;

/// Builds `struct ExpansionBase` at [`EXPANSION_LIBRARY_BASE`]: a real
/// `struct Library` header, zeroed private fields, and two well-formed
/// empty lists (boards and mounts). Called once from
/// [`crate::dispatch::Runtime::new`].
pub fn install_expansion_base<M: AddressSpace>(mem: &mut M) {
    crate::dispatch::write_library_node(mem, EXPANSION_LIBRARY_BASE);
    // Everything past the struct Library header, zeroed before the two
    // lists are built over the top of it -- so eb_Flags and the private
    // fields read as a clean 0 rather than the trap table's own
    // "unknown call" prefill.
    for offset in EB_FLAGS..EXPANSION_BASE_SIZE {
        mem.write_u8(EXPANSION_LIBRARY_BASE + offset, 0);
    }
    crate::execlist::init_list_header(mem, EXPANSION_LIBRARY_BASE + EB_BOARD_LIST);
    crate::execlist::init_list_header(mem, EXPANSION_LIBRARY_BASE + EB_MOUNT_LIST);
}

/// `FindConfigDev` (LVO -72): `A0` = the `ConfigDev` to resume after (`0`
/// to start at the head), `D0` = manufacturer, `D1` = product, each `-1`
/// as a wildcard. `D0` = the matching `ConfigDev*`, or `0`.
///
/// Walks the real list, so it finds whatever a guest added with
/// `AddConfigDev` and nothing otherwise -- which on this runtime is the
/// answer every time, there being no bus to enumerate.
fn find_config_dev_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let old = ctx.cpu.address_register(AddressRegister(0));
    let manufacturer = ctx.cpu.data_register(DataRegister(0));
    let product = ctx.cpu.data_register(DataRegister(1));

    // Resume after `old`, or start at the head. `ln_Succ` of the last
    // real node is the list's own tail sentinel, whose `ln_Succ` is 0.
    let mut node = if old == 0 {
        ctx.mem.read_u32(BOARD_LIST_ADDR + crate::execlist::LH_HEAD)
    } else {
        ctx.mem.read_u32(old + crate::execlist::LN_SUCC)
    };
    let found = loop {
        if node == 0 || ctx.mem.read_u32(node + crate::execlist::LN_SUCC) == 0 {
            // The tail sentinel: nothing (further) on the list.
            break 0;
        }
        let board_manufacturer = u32::from(ctx.mem.read_u16(node + ER_MANUFACTURER));
        let board_product = u32::from(ctx.mem.read_u8(node + ER_PRODUCT));
        let manufacturer_matches = manufacturer == u32::MAX || manufacturer == board_manufacturer;
        let product_matches = product == u32::MAX || product == board_product;
        if manufacturer_matches && product_matches {
            break node;
        }
        node = ctx.mem.read_u32(node + crate::execlist::LN_SUCC);
    };
    ctx.cpu.set_data_register(DataRegister(0), found);
    Ok(())
}

/// `AddConfigDev` (LVO -30): `A0` = the `ConfigDev` to add. No result.
fn add_config_dev_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let config_dev = ctx.cpu.address_register(AddressRegister(0));
    crate::execlist::add_tail_impl(ctx.mem, BOARD_LIST_ADDR, config_dev);
    Ok(())
}

/// `RemConfigDev` (LVO -108): `A0` = the `ConfigDev` to unlink. No
/// result -- real `RemConfigDev` is `Remove` plus bookkeeping this
/// runtime has none of.
fn rem_config_dev_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let config_dev = ctx.cpu.address_register(AddressRegister(0));
    crate::execlist::remove_impl(ctx.mem, config_dev);
    Ok(())
}

/// `ObtainConfigBinding` (LVO -120) / `ReleaseConfigBinding` (-126): no
/// arguments, no result, and nothing to do -- see the module docs.
fn config_binding_noop_handler<C: Cpu>(
    _ctx: &mut HandlerContext<'_, C>,
) -> Result<(), DispatchError> {
    Ok(())
}

/// `GetCurrentBinding` (LVO -138): `A0` = a `CurrentBinding` to fill,
/// `D0` (word) = its size. `D0` = the number of bytes actually copied.
fn get_current_binding_handler<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
) -> Result<(), DispatchError> {
    let dest = ctx.cpu.address_register(AddressRegister(0));
    // A UWORD size, per the autodoc's `D0:16`.
    let size = ctx.cpu.data_register(DataRegister(0)) & 0xFFFF;
    let copied = size.min(CURRENT_BINDING_SIZE);
    for i in 0..copied {
        let byte = ctx
            .mem
            .read_u8(EXPANSION_LIBRARY_BASE + EB_CURRENT_BINDING + i);
        ctx.mem.write_u8(dest.wrapping_add(i), byte);
    }
    ctx.cpu.set_data_register(DataRegister(0), copied);
    Ok(())
}

/// `SetCurrentBinding` (LVO -132): `A0` = a `CurrentBinding` to store,
/// `D0` (word) = its size. No result.
fn set_current_binding_handler<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
) -> Result<(), DispatchError> {
    let src = ctx.cpu.address_register(AddressRegister(0));
    let size = ctx.cpu.data_register(DataRegister(0)) & 0xFFFF;
    let copied = size.min(CURRENT_BINDING_SIZE);
    for i in 0..copied {
        let byte = ctx.mem.read_u8(src.wrapping_add(i));
        ctx.mem
            .write_u8(EXPANSION_LIBRARY_BASE + EB_CURRENT_BINDING + i, byte);
    }
    Ok(())
}

/// `AllocBoardMem` (LVO -42) / `AllocExpansionMem` (-54): `D0` = `0`,
/// the documented failure. There is no autoconfig address space to carve
/// a board window out of here -- see the module docs.
fn alloc_expansion_mem_handler<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
) -> Result<(), DispatchError> {
    ctx.cpu.set_data_register(DataRegister(0), 0);
    Ok(())
}

/// `FreeBoardMem` (LVO -78) / `FreeExpansionMem` (-90): no-ops, since
/// nothing was ever allocated to free.
fn free_expansion_mem_handler<C: Cpu>(
    _ctx: &mut HandlerContext<'_, C>,
) -> Result<(), DispatchError> {
    Ok(())
}

/// `AllocConfigDev` (LVO -48): `D0` = a zeroed `ConfigDev*` on the guest
/// heap, or `0` if the heap is exhausted.
fn alloc_config_dev_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let addr = match ctx.heap.alloc(ctx.mem, CONFIG_DEV_SIZE) {
        Ok(addr) => {
            for i in 0..CONFIG_DEV_SIZE {
                ctx.mem.write_u8(addr + i, 0);
            }
            addr
        }
        Err(_) => 0,
    };
    ctx.cpu.set_data_register(DataRegister(0), addr);
    Ok(())
}

/// `FreeConfigDev` (LVO -84): `A0` = the `ConfigDev` to free. No result;
/// a pointer this runtime never handed out is ignored, the same latitude
/// `crate::execmem`'s `FreeMem` takes for a stale block.
fn free_config_dev_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let addr = ctx.cpu.address_register(AddressRegister(0));
    if addr != 0 {
        let _ = ctx.heap.free(addr);
    }
    Ok(())
}

/// Registers the implemented `expansion.library` vectors on
/// [`EXPANSION_LIBRARY_BASE`]. Registered by explicit offset rather than
/// through a generated LVO table, as `crate::battclock` and
/// `timer.device` are: the offsets come from the NDK's own
/// `lvo/expansion_lib.i`, and only a subset is implemented on purpose
/// (see the module docs). Called unconditionally from
/// [`crate::dispatch::Runtime::new`].
pub fn register_expansion_handlers<C: Cpu + 'static>(
    table: &mut LibraryTable<C>,
    mem: &mut C::Memory,
) {
    macro_rules! reg {
        ($lvo:expr, $name:literal, $handler:expr) => {
            table.register(
                mem,
                EXPANSION_LIBRARY_BASE,
                $lvo,
                "expansion.library",
                $name,
                $handler,
            );
        };
    }
    reg!(
        LVO_FIND_CONFIG_DEV,
        "FindConfigDev",
        find_config_dev_handler::<C>
    );
    reg!(
        LVO_ADD_CONFIG_DEV,
        "AddConfigDev",
        add_config_dev_handler::<C>
    );
    reg!(
        LVO_REM_CONFIG_DEV,
        "RemConfigDev",
        rem_config_dev_handler::<C>
    );
    reg!(
        LVO_OBTAIN_CONFIG_BINDING,
        "ObtainConfigBinding",
        config_binding_noop_handler::<C>
    );
    reg!(
        LVO_RELEASE_CONFIG_BINDING,
        "ReleaseConfigBinding",
        config_binding_noop_handler::<C>
    );
    reg!(
        LVO_GET_CURRENT_BINDING,
        "GetCurrentBinding",
        get_current_binding_handler::<C>
    );
    reg!(
        LVO_SET_CURRENT_BINDING,
        "SetCurrentBinding",
        set_current_binding_handler::<C>
    );
    reg!(
        LVO_ALLOC_BOARD_MEM,
        "AllocBoardMem",
        alloc_expansion_mem_handler::<C>
    );
    reg!(
        LVO_ALLOC_EXPANSION_MEM,
        "AllocExpansionMem",
        alloc_expansion_mem_handler::<C>
    );
    reg!(
        LVO_FREE_BOARD_MEM,
        "FreeBoardMem",
        free_expansion_mem_handler::<C>
    );
    reg!(
        LVO_FREE_EXPANSION_MEM,
        "FreeExpansionMem",
        free_expansion_mem_handler::<C>
    );
    reg!(
        LVO_ALLOC_CONFIG_DEV,
        "AllocConfigDev",
        alloc_config_dev_handler::<C>
    );
    reg!(
        LVO_FREE_CONFIG_DEV,
        "FreeConfigDev",
        free_config_dev_handler::<C>
    );
}

#[cfg(test)]
#[allow(clippy::vec_init_then_push)]
mod tests {
    use super::*;
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{EXEC_LIBRARY_BASE, Runtime, StartConfig};
    use crate::memory::FlatMemory;

    fn load_words(mem: &mut FlatMemory, addr: u32, words: &[u16]) {
        let mut offset = addr;
        for &w in words {
            mem.write_u16(offset, w);
            offset += 2;
        }
    }

    /// `move.l #imm32,An` / `move.l #imm32,Dn`.
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

    fn push_imm_a(words: &mut Vec<u16>, n: u16, imm: u32) {
        words.push(move_imm_to_a(n));
        words.push((imm >> 16) as u16);
        words.push(imm as u16);
    }
    fn push_imm_d(words: &mut Vec<u16>, n: u16, imm: u32) {
        words.push(move_imm_to_d(n));
        words.push((imm >> 16) as u16);
        words.push(imm as u16);
    }

    /// Runs `words` with `A6` = [`EXPANSION_LIBRARY_BASE`] and returns
    /// the exit code (`D0` at the final `rts`), plus the runtime so a
    /// test can read memory back.
    fn run(words: &[u16]) -> (i32, Runtime<M68kCpu>) {
        let entry = TRAP_TABLE_END;
        let mut full = Vec::new();
        push_imm_a(&mut full, 6, EXPANSION_LIBRARY_BASE);
        full.extend_from_slice(words);
        let mut mem = FlatMemory::new(0x4_0000);
        load_words(&mut mem, entry, &full);
        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: entry + (full.len() as u32) * 2 + 0x400,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        (code, rt)
    }

    #[test]
    fn expansion_library_opens_like_the_rom_library_it_is() {
        // The bug this module exists for: expansion.library has been in
        // Kickstart since 1.2, so returning NULL let a guest read "no
        // expansion bus" as "broken Kickstart". The real BindDrivers
        // picked the second reading and exited 20.
        let mut words = Vec::new();
        push_imm_a(&mut words, 6, EXEC_LIBRARY_BASE);
        let name_idx = words.len();
        push_imm_a(&mut words, 1, 0); // A1 = "expansion.library" (patched)
        push_imm_d(&mut words, 0, 0); // D0 = any version
        words.extend_from_slice(&jsr_disp16_a6(-552)); // OpenLibrary
        words.push(RTS);

        let entry = TRAP_TABLE_END;
        let name_addr = entry + (words.len() as u32) * 2 + 8;
        words[name_idx + 1] = (name_addr >> 16) as u16;
        words[name_idx + 2] = name_addr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, entry, &words);
        crate::guestmem::write_c_string(&mut mem, name_addr, b"expansion.library");
        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: name_addr + 64,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let base = rt.run(&mut out, None).expect("run should succeed") as u32;
        assert_eq!(base, EXPANSION_LIBRARY_BASE, "a real base, not NULL");
    }

    #[test]
    fn find_config_dev_finds_nothing_because_nothing_is_plugged_in() {
        // The honest answer, and the one amitorture's probe wants:
        // FindConfigDev(NULL, -1, -1) walks a real, empty list.
        let mut words = Vec::new();
        push_imm_a(&mut words, 0, 0); // A0 = NULL (start at the head)
        push_imm_d(&mut words, 0, u32::MAX); // D0 = -1 (any manufacturer)
        push_imm_d(&mut words, 1, u32::MAX); // D1 = -1 (any product)
        words.extend_from_slice(&jsr_disp16_a6(LVO_FIND_CONFIG_DEV));
        words.push(RTS);
        assert_eq!(run(&words).0, 0);
    }

    #[test]
    fn a_board_a_guest_adds_itself_is_then_found() {
        // Same consistency AddResource/OpenResource keep: if the list is
        // real, what a guest puts on it has to come back out.
        const BOARD: u32 = 0x3_0000;
        const MANUFACTURER: u32 = 0x0204;
        const PRODUCT: u32 = 0x42;

        let mut words = Vec::new();
        push_imm_a(&mut words, 0, BOARD);
        words.extend_from_slice(&jsr_disp16_a6(LVO_ADD_CONFIG_DEV));
        // FindConfigDev(NULL, MANUFACTURER, PRODUCT)
        push_imm_a(&mut words, 0, 0);
        push_imm_d(&mut words, 0, MANUFACTURER);
        push_imm_d(&mut words, 1, PRODUCT);
        words.extend_from_slice(&jsr_disp16_a6(LVO_FIND_CONFIG_DEV));
        words.push(RTS);

        let entry = TRAP_TABLE_END;
        let mut full = Vec::new();
        push_imm_a(&mut full, 6, EXPANSION_LIBRARY_BASE);
        full.extend_from_slice(&words);
        let mut mem = FlatMemory::new(0x4_0000);
        load_words(&mut mem, entry, &full);
        // The board's own ConfigDev: only the ROM id fields matter here.
        for i in 0..CONFIG_DEV_SIZE {
            mem.write_u8(BOARD + i, 0);
        }
        mem.write_u16(BOARD + ER_MANUFACTURER, MANUFACTURER as u16);
        mem.write_u8(BOARD + ER_PRODUCT, PRODUCT as u8);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: entry + (full.len() as u32) * 2 + 0x400,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let found = rt.run(&mut out, None).expect("run should succeed") as u32;
        assert_eq!(found, BOARD, "the board the guest added");
    }

    #[test]
    fn find_config_dev_honors_the_manufacturer_and_product_filters() {
        const BOARD: u32 = 0x3_0000;
        let case = |manufacturer: u32, product: u32| {
            let mut words = Vec::new();
            push_imm_a(&mut words, 0, BOARD);
            words.extend_from_slice(&jsr_disp16_a6(LVO_ADD_CONFIG_DEV));
            push_imm_a(&mut words, 0, 0);
            push_imm_d(&mut words, 0, manufacturer);
            push_imm_d(&mut words, 1, product);
            words.extend_from_slice(&jsr_disp16_a6(LVO_FIND_CONFIG_DEV));
            words.push(RTS);

            let entry = TRAP_TABLE_END;
            let mut full = Vec::new();
            push_imm_a(&mut full, 6, EXPANSION_LIBRARY_BASE);
            full.extend_from_slice(&words);
            let mut mem = FlatMemory::new(0x4_0000);
            load_words(&mut mem, entry, &full);
            for i in 0..CONFIG_DEV_SIZE {
                mem.write_u8(BOARD + i, 0);
            }
            mem.write_u16(BOARD + ER_MANUFACTURER, 0x0204);
            mem.write_u8(BOARD + ER_PRODUCT, 0x42);
            let mut rt = Runtime::new(
                M68kCpu::new(),
                mem,
                StartConfig {
                    entry,
                    load_end: entry + (full.len() as u32) * 2 + 0x400,
                    args: Vec::new(),
                    ..StartConfig::default()
                },
            );
            let mut out = Vec::new();
            rt.run(&mut out, None).expect("run should succeed") as u32
        };
        assert_eq!(case(0x0204, 0x42), BOARD, "exact match");
        assert_eq!(case(u32::MAX, 0x42), BOARD, "manufacturer wildcard");
        assert_eq!(case(0x0204, u32::MAX), BOARD, "product wildcard");
        assert_eq!(case(0x0204, 0x43), 0, "wrong product");
        assert_eq!(case(0x0205, 0x42), 0, "wrong manufacturer");
    }

    #[test]
    fn the_current_binding_round_trips_through_expansion_base() {
        // "writes the contents of the currentBinding structure out of a
        // private place. It may be set via SetCurrentBinding(). This is
        // really a kludge, but it is the only way to pass extra
        // arguments to a newly configured device." (NDK expansion.doc)
        const SRC: u32 = 0x3_0000;
        const DST: u32 = 0x3_0100;
        let mut words = Vec::new();
        push_imm_a(&mut words, 0, SRC);
        push_imm_d(&mut words, 0, CURRENT_BINDING_SIZE);
        words.extend_from_slice(&jsr_disp16_a6(LVO_SET_CURRENT_BINDING));
        push_imm_a(&mut words, 0, DST);
        push_imm_d(&mut words, 0, CURRENT_BINDING_SIZE);
        words.extend_from_slice(&jsr_disp16_a6(LVO_GET_CURRENT_BINDING));
        words.push(RTS);

        let entry = TRAP_TABLE_END;
        let mut full = Vec::new();
        push_imm_a(&mut full, 6, EXPANSION_LIBRARY_BASE);
        full.extend_from_slice(&words);
        let mut mem = FlatMemory::new(0x4_0000);
        load_words(&mut mem, entry, &full);
        for i in 0..CURRENT_BINDING_SIZE {
            mem.write_u8(SRC + i, 0xA0 + i as u8);
            mem.write_u8(DST + i, 0);
        }
        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end: entry + (full.len() as u32) * 2 + 0x400,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let actual = rt.run(&mut out, None).expect("run should succeed") as u32;
        assert_eq!(actual, CURRENT_BINDING_SIZE, "bytes copied");
        for i in 0..CURRENT_BINDING_SIZE {
            assert_eq!(
                rt.memory().read_u8(DST + i),
                0xA0 + i as u8,
                "byte {i} round-tripped"
            );
        }
    }

    #[test]
    fn get_current_binding_never_writes_past_the_callers_struct() {
        // A caller passing a smaller size than the struct gets that many
        // bytes and no more -- the size argument is a UWORD (`D0:16`).
        const DST: u32 = 0x3_0100;
        let mut words = Vec::new();
        push_imm_a(&mut words, 0, DST);
        push_imm_d(&mut words, 0, 4);
        words.extend_from_slice(&jsr_disp16_a6(LVO_GET_CURRENT_BINDING));
        words.push(RTS);
        let (copied, rt) = run(&words);
        assert_eq!(copied, 4);
        assert_eq!(rt.memory().read_u8(DST + 4), 0, "the 5th byte is untouched");
    }

    #[test]
    fn allocating_expansion_memory_fails_because_there_is_no_bus() {
        // "cannot" is the truthful answer: there is no autoconfig
        // address space to carve a board window out of.
        for lvo in [LVO_ALLOC_EXPANSION_MEM, LVO_ALLOC_BOARD_MEM] {
            let mut words = Vec::new();
            push_imm_d(&mut words, 0, 1); // slots / size
            push_imm_d(&mut words, 1, 0);
            words.extend_from_slice(&jsr_disp16_a6(lvo));
            words.push(RTS);
            assert_eq!(run(&words).0, 0, "LVO {lvo} must fail");
        }
    }

    #[test]
    fn alloc_config_dev_hands_back_a_zeroed_struct() {
        let mut words = Vec::new();
        words.extend_from_slice(&jsr_disp16_a6(LVO_ALLOC_CONFIG_DEV));
        words.push(RTS);
        let (addr, rt) = run(&words);
        let addr = addr as u32;
        assert_ne!(addr, 0, "the guest heap has room for a ConfigDev");
        for i in 0..CONFIG_DEV_SIZE {
            assert_eq!(rt.memory().read_u8(addr + i), 0, "byte {i} is zeroed");
        }
    }

    #[test]
    fn expansion_base_carries_two_well_formed_empty_lists() {
        // A guest walking eb_MountList (or the board list) must find an
        // empty list, not the trap table's own sentinel prefill.
        let (_code, rt) = run(&[RTS]);
        let mem = rt.memory();
        for (what, list) in [
            ("boards", EXPANSION_LIBRARY_BASE + EB_BOARD_LIST),
            ("mounts", EXPANSION_LIBRARY_BASE + EB_MOUNT_LIST),
        ] {
            let head = mem.read_u32(list + crate::execlist::LH_HEAD);
            assert_eq!(
                head,
                list + crate::execlist::LH_TAIL,
                "{what}: an empty list's head points at its own tail sentinel"
            );
            assert_eq!(
                mem.read_u32(head + crate::execlist::LN_SUCC),
                0,
                "{what}: the sentinel terminates the walk"
            );
        }
    }
}
