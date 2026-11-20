//! `dos.library`'s resident segment list: `AddSegment`, `FindSegment`,
//! `RemSegment` (issue #20).
//!
//! The list is what the AmigaDOS Shell keeps its built-in commands on, and
//! where `Resident` puts pure commands. A real Shell scans it for every
//! command it is asked to run and registers its own internal commands in
//! it at startup, so a Shell cannot start without it. Before issue #20
//! `FindSegment` was a stub that never found anything.
//!
//! The semantics are the AmigaDOS Manual's "Resident Segments" chapter:
//!
//! - A `struct Segment` is `seg_Next` (`BPTR`), `seg_UC` (`LONG`),
//!   `seg_Seg` (`BPTR` to the segment list) and the name as a `BSTR` in
//!   `seg_Name`. `AddSegment` allocates it (a caller never does) and
//!   copies the name, which is why it can return the list entry for a
//!   command the caller later frees.
//! - `seg_UC` is a use counter (initially 1) for a disk-based resident
//!   command, or a type: `CMD_SYSTEM` (-1) for a system segment that is
//!   not a command, `CMD_INTERNAL` (-2) for a command whose code is part of
//!   the ROM (the Shell adds these itself), `CMD_DISABLED` (-999) or lower
//!   for one the `Resident` command switched off.
//! - `FindSegment(name, start, system)` compares names case-insensitively,
//!   from the head or after `start`; with `system` false it finds only
//!   entries with a positive `seg_UC`, with it true only negative ones,
//!   disabled ones included. It returns the `Segment` (an address, not a
//!   `BPTR`), or `NULL` with `IoErr()` `ERROR_OBJECT_NOT_FOUND`. It changes
//!   no use counter.
//! - `RemSegment(segment)` fails with `ERROR_OBJECT_IN_USE` for a system or
//!   internal segment and for a command whose use counter is above 1;
//!   otherwise it unlinks the entry, unloads its segment list and frees it.
//!
//! The head of the list is host state ([`crate::dosfile::DosState::
//! resident_head`]), as no `RootNode`/`DosInfo` exist in guest memory, so
//! a guest that walks `dl_Root->rn_Info->di_ResList` itself would not find
//! it; the Shell uses the calls.

use crate::cpu::{Cpu, DataRegister};
use crate::dispatch::{DOS_LIBRARY_BASE, DispatchError, HandlerContext, LibraryTable};
use crate::dosfile::{ERROR_NO_FREE_STORE, ERROR_OBJECT_IN_USE, ERROR_OBJECT_NOT_FOUND};
use crate::guestmem::{addr_from_bptr, bptr_from_addr, read_bstr, read_c_string, write_bstr};
use crate::lvos::dos::DOS_LVOS;
use crate::memory::AddressSpace;

const SEG_NEXT: u32 = 0;
const SEG_UC: u32 = 4;
const SEG_SEG: u32 = 8;
const SEG_NAME: u32 = 12;

/// Longest name kept (a `BSTR` holds at most 255 bytes).
const MAX_NAME: usize = 255;

/// `CMD_INTERNAL` (-2): a command whose code is part of the Shell.
const CMD_INTERNAL: i32 = -2;

/// The code address of the internal command whose segment is `seg`, if one
/// is registered: `seg` is the `BPTR` the Shell gave `AddSegment` as the
/// `seg_Seg` of an entry with `seg_UC` `CMD_INTERNAL`, and the code starts
/// right after its `next segment` longword, as in any segment list. The
/// Shell runs these with `RunCommand`, though they are not `LoadSeg`
/// segment lists ([`crate::dosseg`]'s `RunCommand` accepts these and
/// nothing else that `LoadSeg` did not make).
pub(crate) fn internal_command_entry(
    mem: &dyn AddressSpace,
    dos: &crate::dosfile::DosState,
    seg: u32,
) -> Option<u32> {
    let mut next = dos.resident_head;
    while next != 0 {
        let entry = addr_from_bptr(next);
        if mem.read_u32(entry + SEG_SEG) == seg
            && mem.read_u32(entry + SEG_UC) as i32 == CMD_INTERNAL
        {
            return Some(addr_from_bptr(seg).wrapping_add(crate::dosseg::NEXT_SEG_OFFSET));
        }
        next = mem.read_u32(entry + SEG_NEXT);
    }
    None
}

/// `AddSegment` (`D1` = name `STRPTR`, `D2` = segment list `BPTR`, `D3` =
/// `seg_UC`). `D0` = `DOSTRUE`, or `DOSFALSE` with `ERROR_NO_FREE_STORE`.
/// The new entry goes at the head of the list.
fn add_segment_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let name_ptr = ctx.cpu.data_register(DataRegister(1));
    let seglist = ctx.cpu.data_register(DataRegister(2));
    let uc = ctx.cpu.data_register(DataRegister(3));
    let mut name = read_c_string(ctx.mem, name_ptr);
    name.truncate(MAX_NAME);

    let size = SEG_NAME + 1 + name.len() as u32;
    let Ok(seg) = ctx.heap.alloc(&mut *ctx.mem, size) else {
        ctx.dos.set_io_err(ERROR_NO_FREE_STORE);
        ctx.cpu.set_data_register(DataRegister(0), 0);
        return Ok(());
    };
    ctx.mem.write_u32(seg + SEG_NEXT, ctx.dos.resident_head);
    ctx.mem.write_u32(seg + SEG_UC, uc);
    ctx.mem.write_u32(seg + SEG_SEG, seglist);
    write_bstr(ctx.mem, seg + SEG_NAME, &name);
    ctx.dos.resident_head = bptr_from_addr(seg);

    ctx.cpu
        .set_data_register(DataRegister(0), crate::dosfile::DOSTRUE);
    Ok(())
}

/// Whether `seg_UC` `uc` is one `FindSegment(.., system)` is asking for:
/// a positive use counter for `system` false, a negative type for true.
fn uc_matches(uc: i32, system: bool) -> bool {
    if system { uc < 0 } else { uc > 0 }
}

/// `FindSegment` (`D1` = name `CString*`, `D2` = `Segment*` to continue
/// after, or `NULL`, `D3` = `system` flag). `D0` = the `Segment`, or `0`.
fn find_segment_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let name_ptr = ctx.cpu.data_register(DataRegister(1));
    let start = ctx.cpu.data_register(DataRegister(2));
    let system = ctx.cpu.data_register(DataRegister(3)) != 0;
    let wanted = read_c_string(ctx.mem, name_ptr);

    // After `start`, or from the head.
    let mut next = if start == 0 {
        ctx.dos.resident_head
    } else {
        ctx.mem.read_u32(start + SEG_NEXT)
    };
    while next != 0 {
        let seg = addr_from_bptr(next);
        let uc = ctx.mem.read_u32(seg + SEG_UC) as i32;
        if uc_matches(uc, system)
            && read_bstr(ctx.mem, seg + SEG_NAME).eq_ignore_ascii_case(&wanted)
        {
            ctx.cpu.set_data_register(DataRegister(0), seg);
            return Ok(());
        }
        next = ctx.mem.read_u32(seg + SEG_NEXT);
    }
    ctx.dos.set_io_err(ERROR_OBJECT_NOT_FOUND);
    ctx.cpu.set_data_register(DataRegister(0), 0);
    Ok(())
}

/// `RemSegment` (`D1` = `Segment*`). `D0` = `DOSTRUE`, or `DOSFALSE` with
/// `ERROR_OBJECT_IN_USE`: a system or internal segment, or a command whose
/// use counter is above 1, cannot go. A `Segment` that is not on the list
/// is refused the same way, as it is not one this call may free.
fn rem_segment_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let seg = ctx.cpu.data_register(DataRegister(1));
    let refuse = |ctx: &mut HandlerContext<'_, C>| {
        ctx.dos.set_io_err(ERROR_OBJECT_IN_USE);
        ctx.cpu.set_data_register(DataRegister(0), 0);
    };
    if seg == 0 {
        refuse(ctx);
        return Ok(());
    }

    // Find the entry that links to `seg` (or the head).
    let target = bptr_from_addr(seg);
    let mut prev: Option<u32> = None;
    let mut cur = ctx.dos.resident_head;
    while cur != 0 && cur != target {
        let addr = addr_from_bptr(cur);
        prev = Some(addr);
        cur = ctx.mem.read_u32(addr + SEG_NEXT);
    }
    let uc = ctx.mem.read_u32(seg + SEG_UC) as i32;
    if cur == 0 || !(0..=1).contains(&uc) {
        refuse(ctx);
        return Ok(());
    }

    let next = ctx.mem.read_u32(seg + SEG_NEXT);
    match prev {
        Some(prev) => ctx.mem.write_u32(prev + SEG_NEXT, next),
        None => ctx.dos.resident_head = next,
    }
    let seglist = ctx.mem.read_u32(seg + SEG_SEG);
    // The segment list of a disk command, as `LoadSeg` made it; the
    // entry is gone either way, so a failure here has nothing to report.
    // One a running process owns (a `CreateNewProc` `NP_Seglist` with
    // `NP_FreeSeglist`) is that process's to unload at its end, not ours.
    if !ctx
        .dos
        .process_seglists
        .values()
        .any(|&owned| owned == seglist)
    {
        let _ = ctx.dos.unload_seg(ctx.heap, ctx.mem, seglist);
    }
    let _ = ctx.heap.free(seg);
    ctx.cpu
        .set_data_register(DataRegister(0), crate::dosfile::DOSTRUE);
    Ok(())
}

/// Registers the resident-list calls onto [`DOS_LIBRARY_BASE`]. Called
/// from [`crate::dispatch::Runtime::new`] with the other `dos.library`
/// registrations.
pub fn register_resident_handlers<C: Cpu + 'static>(
    table: &mut LibraryTable<C>,
    mem: &mut C::Memory,
) {
    macro_rules! reg {
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
    reg!("AddSegment", add_segment_handler::<C>);
    reg!("FindSegment", find_segment_handler::<C>);
    reg!("RemSegment", rem_segment_handler::<C>);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{Runtime, StartConfig};
    use crate::memory::FlatMemory;

    const RTS: u16 = 0x4E75;

    /// `move.l #imm32,Dn`.
    fn push_move_imm_to_d(words: &mut Vec<u16>, dn: u16, imm: u32) {
        words.push(0x203C | (dn << 9));
        words.push((imm >> 16) as u16);
        words.push(imm as u16);
    }

    /// `jsr disp(A6)`: A6 is `dos.library` at program start.
    fn push_jsr_dos(words: &mut Vec<u16>, disp: i32) {
        words.push(0x4EAE);
        words.push(disp as u16);
    }

    /// `move.l D0,abs.l`.
    fn push_store_d0(words: &mut Vec<u16>, addr: u32) {
        words.push(0x23C0);
        words.push((addr >> 16) as u16);
        words.push(addr as u16);
    }

    fn load_words(mem: &mut FlatMemory, addr: u32, words: &[u16]) {
        for (i, &w) in words.iter().enumerate() {
            mem.write_u16(addr + 2 * i as u32, w);
        }
    }

    /// Runs `words` (built by `build`, which is given the address of a
    /// scratch area for strings and results) and returns the runtime, the
    /// scratch address and the exit code.
    fn run(
        scratch_init: impl Fn(&mut FlatMemory, u32),
        build: impl Fn(&mut Vec<u16>, u32),
    ) -> (Runtime<M68kCpu>, u32, i32) {
        // Code first, then the scratch: its address is fixed by a code
        // length that does not depend on it.
        let scratch = TRAP_TABLE_END + 0x400;
        let mut words = Vec::new();
        build(&mut words, scratch);
        words.push(RTS);
        assert!(words.len() * 2 < 0x400, "program fits before the scratch");
        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);
        scratch_init(&mut mem, scratch);
        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: scratch + 0x400,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        (rt, scratch, code)
    }

    fn put_str(mem: &mut FlatMemory, addr: u32, text: &str) {
        for (i, b) in text.bytes().enumerate() {
            mem.write_u8(addr + i as u32, b);
        }
        mem.write_u8(addr + text.len() as u32, 0);
    }

    #[test]
    fn find_segment_filters_by_the_system_flag() {
        // A disk command (positive counter) is a regular entry; a system
        // or internal segment (negative type, disabled ones included) is
        // found only when system is asked for.
        assert!(uc_matches(1, false) && !uc_matches(1, true));
        assert!(uc_matches(3, false));
        assert!(uc_matches(-1, true) && !uc_matches(-1, false));
        assert!(uc_matches(-2, true) && uc_matches(-999, true));
        assert!(!uc_matches(0, false) && !uc_matches(0, true));
    }

    #[test]
    fn add_segment_then_find_segment_is_case_insensitive_and_honours_the_system_flag_20() {
        // scratch: +0 "Echo", +8 "echo", +16 results (system, regular).
        let (rt, scratch, _) = run(
            |mem, s| {
                put_str(mem, s, "Echo");
                put_str(mem, s + 8, "echo");
            },
            |w, s| {
                // AddSegment("Echo", 0x1234, CMD_INTERNAL)
                push_move_imm_to_d(w, 1, s);
                push_move_imm_to_d(w, 2, 0x1234);
                push_move_imm_to_d(w, 3, (-2i32) as u32);
                push_jsr_dos(w, -774);
                push_store_d0(w, s + 16);
                // FindSegment("echo", NULL, system = TRUE) and FALSE
                for (i, system) in [(0u32, 0xFFFF_FFFFu32), (1, 0)] {
                    push_move_imm_to_d(w, 1, s + 8);
                    push_move_imm_to_d(w, 2, 0);
                    push_move_imm_to_d(w, 3, system);
                    push_jsr_dos(w, -780);
                    push_store_d0(w, s + 20 + 4 * i);
                }
            },
        );
        let mem = rt.memory();
        assert_eq!(mem.read_u32(scratch + 16), 0xFFFF_FFFF, "AddSegment: TRUE");
        let found = mem.read_u32(scratch + 20);
        assert_ne!(found, 0, "the internal command is found with system TRUE");
        assert_eq!(mem.read_u32(found + SEG_UC) as i32, -2);
        assert_eq!(mem.read_u32(found + SEG_SEG), 0x1234);
        assert_eq!(
            read_bstr(mem, found + SEG_NAME),
            b"Echo",
            "name kept as given"
        );
        assert_eq!(
            mem.read_u32(scratch + 24),
            0,
            "an internal command is not a regular entry"
        );
    }

    #[test]
    fn rem_segment_refuses_system_and_internal_entries_and_removes_a_disk_command_20() {
        // scratch: +0 "cmd", +8 "sys", +16 results.
        let (rt, scratch, _) = run(
            |mem, s| {
                put_str(mem, s, "cmd");
                put_str(mem, s + 8, "sys");
            },
            |w, s| {
                // A disk command (use counter 1, no segment list to unload)
                // and a system segment.
                push_move_imm_to_d(w, 1, s);
                push_move_imm_to_d(w, 2, 0);
                push_move_imm_to_d(w, 3, 1);
                push_jsr_dos(w, -774);
                push_move_imm_to_d(w, 1, s + 8);
                push_move_imm_to_d(w, 2, 0);
                push_move_imm_to_d(w, 3, (-1i32) as u32);
                push_jsr_dos(w, -774);
                // RemSegment(FindSegment("sys", NULL, TRUE)): refused.
                push_move_imm_to_d(w, 1, s + 8);
                push_move_imm_to_d(w, 2, 0);
                push_move_imm_to_d(w, 3, 0xFFFF_FFFF);
                push_jsr_dos(w, -780);
                w.push(0x2200); // move.l d0,d1
                push_jsr_dos(w, -786); // RemSegment
                push_store_d0(w, s + 16);
                // RemSegment(FindSegment("cmd", NULL, FALSE)): removed.
                push_move_imm_to_d(w, 1, s);
                push_move_imm_to_d(w, 2, 0);
                push_move_imm_to_d(w, 3, 0);
                push_jsr_dos(w, -780);
                w.push(0x2200);
                push_jsr_dos(w, -786);
                push_store_d0(w, s + 20);
                // ... so it is no longer found.
                push_move_imm_to_d(w, 1, s);
                push_move_imm_to_d(w, 2, 0);
                push_move_imm_to_d(w, 3, 0);
                push_jsr_dos(w, -780);
                push_store_d0(w, s + 24);
            },
        );
        let mem = rt.memory();
        assert_eq!(mem.read_u32(scratch + 16), 0, "a system segment stays");
        assert_eq!(mem.read_u32(scratch + 20), 0xFFFF_FFFF, "a command goes");
        assert_eq!(mem.read_u32(scratch + 24), 0, "and is gone from the list");
    }

    #[test]
    fn run_command_runs_a_registered_internal_command_but_not_any_other_segment_20() {
        // The Shell runs its built-ins with RunCommand on the BPTR it gave
        // AddSegment. scratch: +0 name, +16 the command's "segment"
        // (next-segment longword, then `moveq #42,d0; rts`), +32 results.
        let seg_addr = |s: u32| s + 16;
        let (rt, scratch, _) = run(
            |mem, s| {
                put_str(mem, s, "Beep");
                mem.write_u32(seg_addr(s), 0);
                mem.write_u16(seg_addr(s) + 4, 0x702A); // moveq #42,d0
                mem.write_u16(seg_addr(s) + 6, RTS);
            },
            |w, s| {
                let bptr = seg_addr(s) >> 2;
                push_move_imm_to_d(w, 1, s);
                push_move_imm_to_d(w, 2, bptr);
                push_move_imm_to_d(w, 3, (-2i32) as u32);
                push_jsr_dos(w, -774); // AddSegment
                // RunCommand(seg, stack, args, argsize)
                push_move_imm_to_d(w, 1, bptr);
                push_move_imm_to_d(w, 2, 4096);
                push_move_imm_to_d(w, 3, s + 8);
                push_move_imm_to_d(w, 4, 0);
                push_jsr_dos(w, -504);
                push_store_d0(w, s + 32);
                // A BPTR nobody registered is refused, not run.
                push_move_imm_to_d(w, 1, bptr + 4);
                push_move_imm_to_d(w, 2, 4096);
                push_move_imm_to_d(w, 3, s + 8);
                push_move_imm_to_d(w, 4, 0);
                push_jsr_dos(w, -504);
                push_store_d0(w, s + 36);
            },
        );
        let mem = rt.memory();
        assert_eq!(mem.read_u32(scratch + 32), 42, "the built-in ran");
        assert_eq!(
            mem.read_u32(scratch + 36) as i32,
            -1,
            "a BPTR nobody registered is refused"
        );
    }

    #[test]
    fn run_command_result_of_an_internal_command_is_its_return_code_20() {
        let seg_addr = |s: u32| s + 16;
        let (rt, scratch, _) = run(
            |mem, s| {
                put_str(mem, s, "Beep");
                mem.write_u32(seg_addr(s), 0);
                mem.write_u16(seg_addr(s) + 4, 0x702A);
                mem.write_u16(seg_addr(s) + 6, RTS);
            },
            |w, s| {
                let bptr = seg_addr(s) >> 2;
                push_move_imm_to_d(w, 1, s);
                push_move_imm_to_d(w, 2, bptr);
                push_move_imm_to_d(w, 3, (-2i32) as u32);
                push_jsr_dos(w, -774);
                push_move_imm_to_d(w, 1, bptr);
                push_move_imm_to_d(w, 2, 4096);
                push_move_imm_to_d(w, 3, s + 8);
                push_move_imm_to_d(w, 4, 0);
                push_jsr_dos(w, -504);
                push_store_d0(w, s + 32);
            },
        );
        assert_eq!(rt.memory().read_u32(scratch + 32), 42);
    }
}
