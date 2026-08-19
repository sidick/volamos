//! `dos.library` `DoPkt`: synchronous direct packet communication.
//!
//! Found missing while running the real Workbench 3.1.4 `C:/Info`
//! binary: real `Info` bypasses the `Info()` library wrapper and sends
//! `ACTION_DISK_INFO`/`ACTION_INFO` packets straight to a volume's
//! `dol_Task`, per real AmigaDOS convention (the RKRM's own
//! `NameOfVolume` example code does the same thing for
//! `ACTION_CURRENT_VOLUME`).
//!
//! # Scope: three packet types, host-independent stats
//!
//! This runtime has no real handler processes (see `crate::dosdevproc`'s
//! module docs for the established "no packet round-trip" scope
//! boundary this continues), so `DoPkt` doesn't actually send anything
//! anywhere -- it recognizes `port` as one of `crate::dosdevlist`'s
//! per-volume synthetic task ids ([`crate::dosdevlist::volume_for_task_id`])
//! and answers three packet types directly for a known volume:
//! `ACTION_IS_FILESYSTEM` (always `DOSTRUE` -- every volume this runtime
//! knows about is backed by a real host directory), and
//! `ACTION_DISK_INFO`/`ACTION_INFO`, which fill a `struct InfoData` with
//! fixed, host-independent stats (a fixed large capacity, `0` used)
//! rather than querying the host filesystem for real free-space numbers
//! -- this runtime has no portable way to do that, and per this
//! project's established "don't reintroduce host-state non-determinism"
//! principle (see `crate::dosmeta`'s module docs), a fixed answer is
//! preferable to a host-specific one anyway. Any other packet type, or a
//! `port` this runtime doesn't recognize, fails cleanly with
//! `ERROR_ACTION_NOT_KNOWN` (the real, documented "handler doesn't
//! understand this packet" convention) rather than panicking or lying.
//!
//! # Known gap: `Info`'s "Mounted disks" table
//!
//! Real `Info`'s output has two sections: "Mounted disks" (keyed by
//! *device* unit, e.g. `DF0`) and "Volumes available" (keyed by
//! *volume* name). This runtime only models volumes, not the
//! underlying device units they'd be mounted on (see
//! `crate::dosdevlist`'s "Scope: volumes only" docs), so the real
//! `Info` binary's "Mounted disks" row for a volume this runtime backs
//! prints `Invalid/unknown` in its size/status columns -- it can't find
//! a device entry to report on -- while the "Volumes available" section
//! (which only needs what `DoPkt` answers here) prints correctly.
//! Confirmed harmless: `Info` still exits `0` and prints the volume as
//! `[Mounted]`. Modeling fake per-volume device units to fix the first
//! table's cosmetics wasn't judged worth the added complexity for a
//! purely cosmetic gap.
//!
//! # `struct InfoData` layout
//!
//! ```text
//! offset  0  id_NumSoftErrors  LONG
//! offset  4  id_UnitNumber     LONG
//! offset  8  id_DiskState      LONG  (ID_VALIDATED)
//! offset 12  id_NumBlocks      LONG
//! offset 16  id_NumBlocksUsed  LONG
//! offset 20  id_BytesPerBlock  LONG
//! offset 24  id_DiskType       LONG  (ID_DOS_DISK)
//! offset 28  id_VolumeNode     BPTR  (always 0 here -- see the module docs)
//! offset 32  id_InUse          LONG
//! -- total 36 bytes
//! ```

use crate::cpu::{Cpu, DataRegister};
use crate::dispatch::{DOS_LIBRARY_BASE, DispatchError, HandlerContext, LibraryTable};
use crate::dosfile::ERROR_ACTION_NOT_KNOWN;
use crate::guestmem::addr_from_bptr;
use crate::lvos::dos::DOS_LVOS;
use crate::memory::AddressSpace;

const ACTION_DISK_INFO: u32 = 25;
const ACTION_INFO: u32 = 26;
const ACTION_IS_FILESYSTEM: u32 = 1027;

const ID_VALIDATED: u32 = 82;
const ID_DOS_DISK: u32 = 0x444F_5300;

const ID_NUMSOFTERRORS_OFFSET: u32 = 0;
const ID_UNITNUMBER_OFFSET: u32 = 4;
const ID_DISKSTATE_OFFSET: u32 = 8;
const ID_NUMBLOCKS_OFFSET: u32 = 12;
const ID_NUMBLOCKSUSED_OFFSET: u32 = 16;
const ID_BYTESPERBLOCK_OFFSET: u32 = 20;
const ID_DISKTYPE_OFFSET: u32 = 24;
const ID_VOLUMENODE_OFFSET: u32 = 28;
const ID_INUSE_OFFSET: u32 = 32;

/// A plausible, fixed 512-byte-block capacity -- see the module docs
/// for why this isn't queried from the host.
const FIXED_NUM_BLOCKS: u32 = 100_000;
const BYTES_PER_BLOCK: u32 = 512;

const DOSTRUE: u32 = 0xFFFF_FFFF;
const DOSFALSE: u32 = 0;

fn fill_info_data(mem: &mut dyn AddressSpace, addr: u32) {
    mem.write_u32(addr + ID_NUMSOFTERRORS_OFFSET, 0);
    mem.write_u32(addr + ID_UNITNUMBER_OFFSET, 0);
    mem.write_u32(addr + ID_DISKSTATE_OFFSET, ID_VALIDATED);
    mem.write_u32(addr + ID_NUMBLOCKS_OFFSET, FIXED_NUM_BLOCKS);
    mem.write_u32(addr + ID_NUMBLOCKSUSED_OFFSET, 0);
    mem.write_u32(addr + ID_BYTESPERBLOCK_OFFSET, BYTES_PER_BLOCK);
    mem.write_u32(addr + ID_DISKTYPE_OFFSET, ID_DOS_DISK);
    mem.write_u32(addr + ID_VOLUMENODE_OFFSET, 0);
    mem.write_u32(addr + ID_INUSE_OFFSET, 0);
}

/// `DoPkt` (`D1` = port, `D2` = action, `D3`-`D7` = `arg1`-`arg5`).
/// `D0` = `dp_Res1`; `IoErr()` set to `dp_Res2`.
fn do_pkt_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let port = ctx.cpu.data_register(DataRegister(1));
    let action = ctx.cpu.data_register(DataRegister(2));
    let arg1 = ctx.cpu.data_register(DataRegister(3));
    let arg2 = ctx.cpu.data_register(DataRegister(4));

    let known_volume = crate::dosdevlist::volume_for_task_id(ctx.dos, port).is_some();
    let (res1, res2) = match action {
        ACTION_DISK_INFO if known_volume => {
            fill_info_data(ctx.mem, addr_from_bptr(arg1));
            (DOSTRUE, 0)
        }
        ACTION_INFO if known_volume => {
            fill_info_data(ctx.mem, addr_from_bptr(arg2));
            (DOSTRUE, 0)
        }
        // Every volume this runtime knows about is backed by a real
        // host directory, i.e. is always "a file system" -- see the
        // RKRM's own IsFileSystem() fallback note (crate::dosfs's
        // module docs) for why real dos.library ROM code sends this
        // packet directly rather than calling through a simpler
        // wrapper.
        ACTION_IS_FILESYSTEM if known_volume => (DOSTRUE, 0),
        _ => (DOSFALSE, ERROR_ACTION_NOT_KNOWN as u32),
    };

    ctx.dos.set_io_err(res2 as i32);
    ctx.cpu.set_data_register(DataRegister(0), res1);
    Ok(())
}

/// Registers `DoPkt` onto [`DOS_LIBRARY_BASE`], looked up by name
/// through [`DOS_LVOS`]. Called from [`crate::dispatch::Runtime::new`]
/// alongside the other `dos.library` registrations.
pub fn register_dospkt_handlers<C: Cpu + 'static>(
    table: &mut LibraryTable<C>,
    mem: &mut C::Memory,
) {
    table
        .register_by_name(
            mem,
            DOS_LIBRARY_BASE,
            DOS_LVOS,
            "dos.library",
            "DoPkt",
            do_pkt_handler::<C>,
        )
        .unwrap_or_else(|e| panic!("DoPkt should be in DOS_LVOS: {e}"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{Runtime, StartConfig};
    use crate::memory::FlatMemory;
    use crate::vfs::{Vfs, VfsConfig};
    use std::path::PathBuf;

    fn move_imm_to_d(n: u16) -> u16 {
        0x203C | (n << 9)
    }
    fn jsr_disp16(an: u16) -> u16 {
        0x4EA8 | an
    }
    const RTS: u16 = 0x4E75;
    const MOVE_L_D0_D1: u16 = 0x2200;

    fn push_move_imm_to_d(words: &mut Vec<u16>, dn: u16, imm: u32) {
        words.push(move_imm_to_d(dn));
        words.push((imm >> 16) as u16);
        words.push(imm as u16);
    }
    fn push_jsr(words: &mut Vec<u16>, an: u16, disp: i32) {
        words.push(jsr_disp16(an));
        words.push(disp as u16);
    }
    fn load_words(mem: &mut FlatMemory, addr: u32, words: &[u16]) {
        let mut offset = addr;
        for &w in words {
            mem.write_u16(offset, w);
            offset += 2;
        }
    }

    #[test]
    fn end_to_end_action_disk_info_via_a_real_volumes_task() {
        // LockDosList(LDF_VOLUMES) -> D0 = header
        // D1 = header; D2 = LDF_VOLUMES
        // NextDosEntry -> D0 = entry
        // D1 = dol_Task, read out of the found entry via MOVE.L
        // (8,A0),D1 (dol_Task is at struct offset 8 -- see
        // crate::dosdevlist's module docs).
        let mut words = Vec::new();
        push_move_imm_to_d(&mut words, 1, 0x8); // LDF_VOLUMES
        push_jsr(&mut words, 6, -654); // LockDosList(a6)
        words.push(MOVE_L_D0_D1); // D1 = header
        push_move_imm_to_d(&mut words, 2, 0x8); // LDF_VOLUMES
        push_jsr(&mut words, 6, -690); // NextDosEntry(a6) -> D0 = entry
        words.push(0x2040); // MOVEA.L D0,A0 (entry addr)
        words.push(0x2228); // MOVE.L (8,A0),D1 (dol_Task)
        words.push(8); // displacement

        push_move_imm_to_d(&mut words, 2, ACTION_DISK_INFO);
        let infodata_idx = words.len();
        push_move_imm_to_d(&mut words, 3, 0); // D3 = InfoData addr (patched)
        push_jsr(&mut words, 6, -240); // DoPkt(a6)
        words.push(RTS);

        // BPTRs can only address 4-byte-aligned locations (the low 2
        // bits are shifted away) -- round up.
        let infodata_addr = (TRAP_TABLE_END + (words.len() as u32) * 2 + 3) & !3;
        let infodata_bptr = crate::guestmem::bptr_from_addr(infodata_addr);
        words[infodata_idx + 1] = (infodata_bptr >> 16) as u16;
        words[infodata_idx + 2] = infodata_bptr as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: infodata_addr + 64,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let vfs = Vfs::new(VfsConfig {
            volumes: vec![("SYS".to_string(), PathBuf::from("/tmp"))],
            assigns: vec![],
            auto_assign_root: None,
            cwd: "SYS:".to_string(),
        })
        .expect("build vfs");
        rt.set_vfs(vfs);

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, DOSTRUE as i32, "ACTION_DISK_INFO should succeed");

        assert_eq!(
            rt.memory().read_u32(infodata_addr + ID_DISKSTATE_OFFSET),
            ID_VALIDATED
        );
        assert_eq!(
            rt.memory().read_u32(infodata_addr + ID_DISKTYPE_OFFSET),
            ID_DOS_DISK
        );
        assert_eq!(
            rt.memory().read_u32(infodata_addr + ID_NUMBLOCKS_OFFSET),
            FIXED_NUM_BLOCKS
        );
    }

    #[test]
    fn end_to_end_action_is_filesystem_succeeds_for_a_known_volume() {
        // LockDosList(LDF_VOLUMES) -> D0 = header; NextDosEntry -> D0 =
        // entry; D1 = dol_Task (offset 8); D2 = ACTION_IS_FILESYSTEM.
        let mut words = Vec::new();
        push_move_imm_to_d(&mut words, 1, 0x8); // LDF_VOLUMES
        push_jsr(&mut words, 6, -654); // LockDosList(a6)
        words.push(MOVE_L_D0_D1); // D1 = header
        push_move_imm_to_d(&mut words, 2, 0x8); // LDF_VOLUMES
        push_jsr(&mut words, 6, -690); // NextDosEntry(a6) -> D0 = entry
        words.push(0x2040); // MOVEA.L D0,A0
        words.push(0x2228); // MOVE.L (8,A0),D1 (dol_Task)
        words.push(8);
        push_move_imm_to_d(&mut words, 2, ACTION_IS_FILESYSTEM);
        push_jsr(&mut words, 6, -240); // DoPkt(a6)
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);
        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: TRAP_TABLE_END + (words.len() as u32) * 2 + 0x100,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let vfs = Vfs::new(VfsConfig {
            volumes: vec![("SYS".to_string(), PathBuf::from("/tmp"))],
            assigns: vec![],
            auto_assign_root: None,
            cwd: "SYS:".to_string(),
        })
        .expect("build vfs");
        rt.set_vfs(vfs);

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, DOSTRUE as i32);
    }

    #[test]
    fn end_to_end_unknown_port_fails_with_action_not_known() {
        let mut words = Vec::new();
        push_move_imm_to_d(&mut words, 1, 0x1234); // unrecognized port
        push_move_imm_to_d(&mut words, 2, ACTION_DISK_INFO);
        push_move_imm_to_d(&mut words, 3, 0);
        push_jsr(&mut words, 6, -240); // DoPkt(a6)
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);
        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: TRAP_TABLE_END + (words.len() as u32) * 2 + 0x100,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, DOSFALSE as i32);
    }
}
