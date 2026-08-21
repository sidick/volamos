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

// The file-I/O packet types [`handle_fs_packet`] answers -- the four
// that implement `Read`/`Write`/`Seek`/`Close` at the packet level, per
// the RKRM's `packet-documentation.md` (each cited at its match arm).
const ACTION_READ: u32 = 82;
const ACTION_WRITE: u32 = 87;
const ACTION_END: u32 = 1007;
const ACTION_SEEK: u32 = 1008;

// --- struct DosPacket field offsets (dos/dosextens.h) -- all LONGs. ---

/// `dp_Port`: the `MsgPort` the handler sends the packet back to once
/// done -- per the RKRM's `direct-packet-communication.md`, "the reply
/// port of the message in `mn_ReplyPort` is *not* used; instead, the
/// message carrying the packet is sent back to `dp_Port`".
const DP_PORT: u32 = 4;
const DP_TYPE: u32 = 8;
const DP_RES1: u32 = 12;
const DP_RES2: u32 = 16;
const DP_ARG1: u32 = 20;
const DP_ARG2: u32 = 24;
const DP_ARG3: u32 = 28;

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
/// The `-1` a packet's `dp_Res1` reports for a failed
/// `ACTION_READ`/`ACTION_WRITE`/`ACTION_SEEK` -- numerically the same
/// word as [`DOSTRUE`], named separately because the packet contract's
/// "-1 = error" and the boolean contract's "-1 = success" mean opposite
/// things.
const RESULT_ERROR: u32 = 0xFFFF_FFFF;

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

/// Services one guest-sent file-I/O `DosPacket` and replies to it --
/// the host-side stand-in for a real filesystem handler process's
/// packet loop, called from [`crate::execlist`]'s `PutMsg` handler
/// whenever the destination port is [`crate::dosfile::DosState`]'s
/// filesystem handler port (the port every non-`NIL:` `FileHandle`'s
/// `fh_Type` points at).
///
/// # Why `PutMsg`, not just `DoPkt`
///
/// Real programs bypass `Read()`/`Write()` (and `DoPkt`/`SendPkt`)
/// entirely and speak the packet protocol by hand: build a
/// `StandardPacket`, `PutMsg` its message to `fh_Type`, then
/// `WaitPort`/`GetMsg` the reply port. Found via the real SAS/C `sc`
/// compiler (issue #24): `sc1.library` does all of its source-file
/// reading and object-file writing this way -- with no host-side
/// handler behind `fh_Type`, its first read deadlocked waiting for a
/// reply that could never come. Servicing the packet *synchronously
/// inside `PutMsg`* -- reply already queued by the time `PutMsg`
/// returns -- makes the client's subsequent `GetMsg` succeed
/// immediately, so its `WaitPort` fallback path never needs to block
/// (`vamos` structures its own equivalent the same way, and the real
/// `sc` compiles successfully against it).
///
/// # Packet anatomy (RKRM `direct-packet-communication.md`)
///
/// "`mn_Node.ln_Name` of the exec message is (mis-)used to point to
/// the `DosPacket` and its `dp_Link` element points back to the
/// message"; the reply goes to `dp_Port`, and `mn_ReplyPort` is
/// explicitly not part of the protocol. The reply itself mirrors real
/// `ReplyPkt()`: fill `dp_Res1`/`dp_Res2`, send the message to
/// `dp_Port` (`ln_Type` set to `NT_REPLYMSG`, matching what an exec
/// message that has been replied looks like).
///
/// # Actions
///
/// `ACTION_READ`/`ACTION_WRITE`/`ACTION_SEEK`/`ACTION_END` -- the four
/// packet-level faces of `Read`/`Write`/`Seek`/`Close`, each routed to
/// the same [`crate::dosfile::DosState`] backing the wrapper LVOs use
/// (`dp_Arg1` echoes `fh_Arg1`, which this runtime sets to the handle
/// struct's own guest address -- see `dosfile.rs`'s `FH_ARG1_OFFSET`
/// doc -- so the lookup is direct). A write to the `Output()` default
/// handle goes through [`HandlerContext::out`], same special case as
/// the `Write` LVO handler. Anything else gets `dp_Res1 = -1` /
/// `dp_Res2 = ERROR_ACTION_NOT_KNOWN`, the real, documented "handler
/// doesn't implement this packet" convention (RKRM
/// `packet-documentation.md`'s own `ACTION_SEEK` note: "set `dp_Res1`
/// to -1 -- *and not 0* -- and `dp_Res2` to `ERROR_ACTION_NOT_KNOWN`").
pub fn handle_fs_packet<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
    msg: u32,
) -> Result<(), DispatchError> {
    let pkt = ctx.mem.read_u32(msg + crate::execlist::LN_NAME);
    let action = ctx.mem.read_u32(pkt + DP_TYPE);
    let arg1 = ctx.mem.read_u32(pkt + DP_ARG1);
    let arg2 = ctx.mem.read_u32(pkt + DP_ARG2);
    let arg3 = ctx.mem.read_u32(pkt + DP_ARG3);

    let (res1, res2) = match action {
        // RKRM packet-documentation.md, ACTION_READ (82): Arg1 =
        // fh_Arg1, Arg2 = buffer (APTR), Arg3 = length; Res1 = bytes
        // read (0 legal at EOF) or -1 + Res2 on error.
        ACTION_READ => match ctx.dos.read(arg1, arg3 as usize) {
            Ok(data) => {
                for (i, b) in data.iter().enumerate() {
                    ctx.mem.write_u8(arg2.wrapping_add(i as u32), *b);
                }
                (data.len() as u32, 0)
            }
            Err(code) => (RESULT_ERROR, code as u32),
        },
        // ACTION_WRITE (87): same shape; Res1 = bytes written or -1.
        // The Output() default handle write goes through ctx.out, same
        // as the Write LVO handler (see HostHandle::Stdout's doc).
        ACTION_WRITE => {
            let mut buf = vec![0u8; arg3 as usize];
            for (i, b) in buf.iter_mut().enumerate() {
                *b = ctx.mem.read_u8(arg2.wrapping_add(i as u32));
            }
            if ctx.dos.is_output_default(arg1) {
                match ctx.out.write_all(&buf) {
                    Ok(()) => (arg3, 0),
                    Err(e) => (RESULT_ERROR, crate::dosfile::map_io_error(&e) as u32),
                }
            } else {
                match ctx.dos.write(arg1, &buf) {
                    Ok(n) => (n as u32, 0),
                    Err(code) => (RESULT_ERROR, code as u32),
                }
            }
        }
        // ACTION_SEEK (1008): Arg2 = offset, Arg3 = mode; Res1 =
        // previous position or -1.
        ACTION_SEEK => match ctx.dos.seek(arg1, arg2 as i32, arg3 as i32) {
            Ok(old) => (old as u32, 0),
            Err(code) => (RESULT_ERROR, code as u32),
        },
        // ACTION_END (1007): Res1 = boolean. Close of the
        // Input()/Output() defaults is the same documented no-op
        // success as the Close LVO's.
        ACTION_END => {
            if ctx.dos.close(ctx.heap, arg1) {
                (DOSTRUE, 0)
            } else {
                (DOSFALSE, ERROR_ACTION_NOT_KNOWN as u32)
            }
        }
        _ => (RESULT_ERROR, ERROR_ACTION_NOT_KNOWN as u32),
    };

    ctx.mem.write_u32(pkt + DP_RES1, res1);
    ctx.mem.write_u32(pkt + DP_RES2, res2);
    *ctx.call_detail = Some(format!(
        "DosPacket action {action} (fh_Arg1 {arg1:#x}) -> Res1 {res1:#x}"
    ));

    let reply_port = ctx.mem.read_u32(pkt + DP_PORT);
    crate::execlist::put_msg_impl(ctx.mem, reply_port, msg, crate::execlist::NT_REPLYMSG);
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

    // --- Packet-level file I/O via raw PutMsg (handle_fs_packet) ---

    struct TempDir {
        path: std::path::PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let pid = std::process::id();
            let path = std::env::temp_dir().join(format!("volamos-dospkt-test-{tag}-{pid}-{n}"));
            std::fs::create_dir_all(&path).expect("create temp dir");
            TempDir { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// Builds and runs the raw packet-I/O guest program `sc1.library`'s
    /// own idiom distills to (issue #24): `Open` a file, read `fh_Type`
    /// and `fh_Arg1` straight off the returned `FileHandle`, finish a
    /// pre-built `DosPacket` with them, `PutMsg` its message to
    /// `fh_Type`, then `GetMsg` the packet's reply port. Returns
    /// `(exit_code, mem, pkt, msg, buf)` for the caller to assert on --
    /// the packet's static fields (`dp_Type`/`dp_Arg2`/`dp_Arg3`) come
    /// from `action`/`arg3`, so each test drives a different action
    /// through the identical guest code path.
    fn run_packet_program(
        dir: &TempDir,
        action: u32,
        arg3: u32,
    ) -> (i32, Runtime<M68kCpu>, u32, u32, u32) {
        let mut words = Vec::new();
        let name_idx = words.len();
        push_move_imm_to_d(&mut words, 1, 0); // D1 = name (patched)
        push_move_imm_to_d(&mut words, 2, 1005); // D2 = MODE_OLDFILE
        push_jsr(&mut words, 6, -30); // Open(a6) -> D0 = BPTR
        words.push(0xE580); // ASL.L #2,D0 (BPTR -> address)
        words.push(0x2640); // MOVEA.L D0,A3 (handle addr)
        let arg1_idx = words.len();
        words.push(0x23EB); // MOVE.L (36,A3),(xxx).L -- dp_Arg1 = fh_Arg1
        words.push(36);
        words.push(0); // dest hi (patched)
        words.push(0); // dest lo (patched)
        words.push(0x206B); // MOVEA.L (8,A3),A0 -- A0 = fh_Type
        words.push(8);
        let msg_idx = words.len();
        words.push(0x227C); // MOVEA.L #MSG,A1 (patched)
        words.push(0);
        words.push(0);
        words.push(0x2C7C); // MOVEA.L #EXEC_LIBRARY_BASE,A6
        words.push((crate::dispatch::EXEC_LIBRARY_BASE >> 16) as u16);
        words.push(crate::dispatch::EXEC_LIBRARY_BASE as u16);
        push_jsr(&mut words, 6, -366); // PutMsg(a6)
        let rport_idx = words.len();
        words.push(0x207C); // MOVEA.L #RPORT,A0 (patched)
        words.push(0);
        words.push(0);
        push_jsr(&mut words, 6, -372); // GetMsg(a6) -> D0 = reply msg or 0
        words.push(RTS);

        let data = (TRAP_TABLE_END + (words.len() as u32) * 2 + 3) & !3;
        let (name, pkt, msg, rport, buf) = (data, data + 12, data + 44, data + 60, data + 96);
        words[name_idx + 1] = (name >> 16) as u16;
        words[name_idx + 2] = name as u16;
        words[arg1_idx + 2] = ((pkt + DP_ARG1) >> 16) as u16;
        words[arg1_idx + 3] = (pkt + DP_ARG1) as u16;
        words[msg_idx + 1] = (msg >> 16) as u16;
        words[msg_idx + 2] = msg as u16;
        words[rport_idx + 1] = (rport >> 16) as u16;
        words[rport_idx + 2] = rport as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);
        crate::guestmem::write_c_string(&mut mem, name, b"SYS:f.txt");
        // The pre-built StandardPacket, minus the run-time dp_Arg1 the
        // guest code fills in: message.ln_Name -> packet (the packet/
        // message linkage the RKRM documents), dp_Port -> the reply
        // port, whose mp_MsgList the client initializes itself.
        mem.write_u32(msg + crate::execlist::LN_NAME, pkt);
        mem.write_u32(pkt + DP_PORT, rport);
        mem.write_u32(pkt + DP_TYPE, action);
        mem.write_u32(pkt + DP_ARG2, buf);
        mem.write_u32(pkt + DP_ARG3, arg3);
        crate::execlist::init_list_header(&mut mem, rport + crate::execlist::MP_MSGLIST);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: data + 0x100,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let vfs = Vfs::new(VfsConfig {
            volumes: vec![("SYS".to_string(), dir.path.clone())],
            assigns: vec![],
            auto_assign_root: None,
            cwd: "SYS:".to_string(),
        })
        .expect("build vfs");
        rt.set_vfs(vfs);

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        (code, rt, pkt, msg, buf)
    }

    #[test]
    fn end_to_end_action_read_packet_via_raw_putmsg() {
        let dir = TempDir::new("pkt-read");
        std::fs::write(dir.path.join("f.txt"), b"packet payload").unwrap();
        let (code, rt, pkt, msg, buf) = run_packet_program(&dir, ACTION_READ, 64);
        let mem = rt.memory();

        assert_eq!(
            code as u32, msg,
            "GetMsg must find the reply already queued -- the whole point \
             of servicing the packet synchronously inside PutMsg"
        );
        assert_eq!(mem.read_u32(pkt + DP_RES1), 14, "dp_Res1 = bytes read");
        assert_eq!(mem.read_u32(pkt + DP_RES2), 0);
        assert_eq!(
            mem.read_u8(msg + crate::execlist::LN_TYPE),
            crate::execlist::NT_REPLYMSG,
            "the replied message must look replied"
        );
        let got: Vec<u8> = (0..14).map(|i| mem.read_u8(buf + i)).collect();
        assert_eq!(got, b"packet payload");
    }

    #[test]
    fn end_to_end_unknown_action_packet_replies_action_not_known() {
        let dir = TempDir::new("pkt-unknown");
        std::fs::write(dir.path.join("f.txt"), b"x").unwrap();
        let (code, rt, pkt, msg, _) = run_packet_program(&dir, 4242, 0);
        let mem = rt.memory();

        assert_eq!(
            code as u32, msg,
            "even an unknown action must be replied, not swallowed -- a \
             client blocked on the reply port would otherwise hang"
        );
        assert_eq!(mem.read_u32(pkt + DP_RES1), RESULT_ERROR, "-1, not 0");
        assert_eq!(mem.read_u32(pkt + DP_RES2), ERROR_ACTION_NOT_KNOWN as u32);
    }
}
