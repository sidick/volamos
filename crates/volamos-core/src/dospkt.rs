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
use crate::dosfile::{
    ERROR_ACTION_NOT_KNOWN, ERROR_OBJECT_NOT_FOUND, MODE_NEWFILE, MODE_OLDFILE, MODE_READWRITE,
};
use crate::guestmem::{addr_from_bptr, read_bstr, read_c_string};
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

// Issue #186 (ixemul's AllocDosObject(DOS_FILEHANDLE) + raw packet
// I/O): the open/lock/examine/link packet types `handle_fs_packet` now
// also answers. `ACTION_FINDINPUT`/`ACTION_FINDOUTPUT`/
// `ACTION_FINDUPDATE`'s numeric values are literally `MODE_OLDFILE`/
// `MODE_NEWFILE`/`MODE_READWRITE` (RKRM `packet-documentation.md`:
// "`Open()`... copies the packet type from its second argument"), so
// they're aliased from `crate::dosfile` rather than re-declared, to
// keep the two definitions from ever drifting apart.
const ACTION_FINDINPUT: u32 = MODE_OLDFILE as u32;
const ACTION_FINDOUTPUT: u32 = MODE_NEWFILE as u32;
const ACTION_FINDUPDATE: u32 = MODE_READWRITE as u32;
const ACTION_LOCATE_OBJECT: u32 = 8;
const ACTION_EXAMINE_FH: u32 = 1034;
const ACTION_READ_LINK: u32 = 1024;

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
/// `dp_Arg4` -- only `ACTION_READ_LINK` (issue #186) uses this one.
const DP_ARG4: u32 = 32;

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
        // ACTION_LOCATE_OBJECT (8): Arg1 = parent FileLock BPTR (0 =
        // volume root), Arg2 = BPTR to BSTR of the object name, Arg3 =
        // lock mode (SHARED_LOCK/EXCLUSIVE_LOCK). Res1 = BPTR to the
        // new FileLock, or 0 + Res2 on error -- *not* the -1 failure
        // convention the I/O actions above use: a lock's BPTR can
        // legitimately be any value, so "no lock" is reported the same
        // way Lock() itself reports it in D0 (0/NULL).
        ACTION_LOCATE_OBJECT => {
            let lock_addr = addr_from_bptr(arg1);
            let name_addr = addr_from_bptr(arg2);
            let name = String::from_utf8_lossy(&read_bstr(ctx.mem, name_addr)).into_owned();
            match ctx
                .dos
                .locate_object(ctx.heap, ctx.mem, lock_addr, &name, arg3 as i32)
            {
                Ok(bptr) => (bptr, 0),
                Err(code) => (0, code as u32),
            }
        }
        // ACTION_FINDINPUT (1005, == MODE_OLDFILE) / ACTION_FINDOUTPUT
        // (1006, == MODE_NEWFILE) / ACTION_FINDUPDATE (1004, ==
        // MODE_READWRITE): Arg1 = BPTR to the caller's own FileHandle
        // (already allocated via AllocDosObject(DOS_FILEHANDLE)), Arg2
        // = BPTR to parent FileLock (0 = volume root), Arg3 = BPTR to
        // BSTR of the path. Res1 = boolean. The packet's own `action`
        // (== dp_Type) *is* the access mode to open with -- see the
        // consts' own docs for why that's not a coincidence.
        ACTION_FINDINPUT | ACTION_FINDOUTPUT | ACTION_FINDUPDATE => {
            let fh_addr = addr_from_bptr(arg1);
            let lock_addr = addr_from_bptr(arg2);
            let name_addr = addr_from_bptr(arg3);
            let name = String::from_utf8_lossy(&read_bstr(ctx.mem, name_addr)).into_owned();
            match ctx
                .dos
                .open_into_handle(fh_addr, lock_addr, &name, action as i32)
            {
                Ok(()) => (DOSTRUE, 0),
                Err(code) => (DOSFALSE, code as u32),
            }
        }
        // ACTION_EXAMINE_FH (1034): Arg1 = fh_Arg1 of the FileHandle
        // being examined (a raw value, not a BPTR -- same convention
        // as ACTION_READ/WRITE/SEEK's own Arg1 above), Arg2 = APTR
        // (not BPTR) to a FileInfoBlock to fill, same raw-pointer
        // convention the ExamineFH LVO handler's own D2 uses. Res1 =
        // boolean.
        ACTION_EXAMINE_FH => match ctx.dos.examine_fh(ctx.mem, arg1, arg2) {
            Ok(()) => (DOSTRUE, 0),
            Err(code) => (DOSFALSE, code as u32),
        },
        // ACTION_READ_LINK (1024): Arg1 = BPTR to a FileLock the path
        // in Arg2 is relative to, Arg2 = APTR (*not* a BSTR -- the
        // RKRM is explicit this one's path argument differs from
        // every other packet's) to a NUL-terminated C string path,
        // Arg3 = APTR to a target buffer, Arg4 = buffer size. Res1 =
        // length of the resolved path on success, -1 if no soft link
        // is found at that path (the RKRM's own cited FFS convention:
        // ERROR_OBJECT_NOT_FOUND is "the right choice" there, see
        // packet-documentation.md's ACTION_READ_LINK section), or -2 +
        // ERROR_LINE_TOO_LONG if the buffer is too small.
        //
        // Reuses crate::doslock::DosState::read_link -- the same real
        // *host* symlink primitive issue #160 added for the
        // MakeLink/ReadLink library calls -- rather than fabricating a
        // second, hand-rolled "no symlinks" answer: a packet-level
        // caller gets a real resolved target if a host symlink exists
        // at that spot, and the documented clean failure otherwise.
        // This doesn't implement the RKRM's full multi-directory,
        // cross-assign soft-link scan (resolving a link buried in the
        // *middle* of a longer path, absolute-vs-relative link-target
        // merging) -- `read_link` only ever checks the exact
        // `(lock, path)` pair it's given, one component at a time,
        // which is also exactly how a real `ixemul.library`
        // `__plock()` calls this packet (issue #186's own text). No
        // corpus binary has needed more than that yet.
        ACTION_READ_LINK => {
            let lock_addr = addr_from_bptr(arg1);
            let path = String::from_utf8_lossy(&read_c_string(ctx.mem, arg2)).into_owned();
            let buf = arg3;
            let size = ctx.mem.read_u32(pkt + DP_ARG4) as i32;
            match ctx.dos.read_link(lock_addr, &path) {
                Ok(target) => {
                    let capacity = (size - 1).max(0) as usize;
                    if target.len() > capacity {
                        (0xFFFF_FFFE, crate::dospattern::ERROR_LINE_TOO_LONG as u32)
                    } else {
                        for (i, b) in target.iter().enumerate() {
                            ctx.mem.write_u8(buf.wrapping_add(i as u32), *b);
                        }
                        ctx.mem.write_u8(buf.wrapping_add(target.len() as u32), 0);
                        (target.len() as u32, 0)
                    }
                }
                Err(_) => (RESULT_ERROR, ERROR_OBJECT_NOT_FOUND as u32),
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

    // --- issue #186's end-to-end test needs a handful of opcodes none
    // of the existing helpers cover (register-to-register moves other
    // than D0->D1, address-register loads, and absolute-address
    // load/store) -- each derived by hand from the 68000 PRM's MOVE/
    // shift encoding tables and cross-checked against an existing,
    // already-passing instruction in this same file (noted per
    // function) rather than trusted blind.

    /// `MOVE.L Dsrc,Ddst` (register direct to register direct). Cross-
    /// checked against the existing `MOVE_L_D0_D1` constant (`0x2200`):
    /// `move_reg_to_reg(1, 0) == 0x2200`.
    fn move_reg_to_reg(dst: u16, src: u16) -> u16 {
        0x2000 | (dst << 9) | src
    }
    /// `MOVEA.L Dsrc,Adst`. Cross-checked against
    /// `run_packet_program`'s own `0x2640` ("MOVEA.L D0,A3"):
    /// `movea_reg(3, 0) == 0x2640`.
    fn movea_reg(dst_a: u16, src_d: u16) -> u16 {
        0x2040 | (dst_a << 9) | src_d
    }
    /// `MOVEA.L #imm,Adst` (`imm` follows as two words, hi then lo).
    /// Cross-checked against `run_packet_program`'s own `0x227C`/
    /// `0x207C` ("MOVEA.L #MSG,A1"/"MOVEA.L #RPORT,A0"):
    /// `movea_imm(1) == 0x227C`, `movea_imm(0) == 0x207C`.
    fn movea_imm(dst_a: u16) -> u16 {
        0x207C | (dst_a << 9)
    }
    /// `MOVE.L (disp,Asrc),Ddst` (`disp` follows as one word). Cross-
    /// checked against the disk-info test's own `0x2228` ("MOVE.L
    /// (8,A0),D1"): `move_disp_to_d(1, 0) == 0x2228`.
    fn move_disp_to_d(dst_d: u16, src_a: u16) -> u16 {
        0x2028 | (dst_d << 9) | src_a
    }
    /// `MOVE.L Dsrc,(xxx).L` (the absolute address follows as two
    /// words, hi then lo). Cross-checked against
    /// `run_packet_program`'s own `0x23EB` ("MOVE.L (36,A3),(xxx).L" --
    /// same destination-EA bits, different source EA): both encode
    /// destination mode 7/register 1 (absolute long) in bits 11-6.
    fn move_d_to_abs(src_d: u16) -> u16 {
        0x23C0 | src_d
    }
    /// `MOVE.L (xxx).L,Ddst` (the absolute address follows as two
    /// words, hi then lo) -- the mirror image of [`move_d_to_abs`]:
    /// same mode-7/register-1 "absolute long" encoding, now in the
    /// *source* EA bits instead of the destination's.
    fn move_abs_to_d(dst_d: u16) -> u16 {
        0x2039 | (dst_d << 9)
    }
    /// `LSR.L #2,Ddst` (converts a guest heap address to the `BPTR`
    /// it's shifted by, the same relationship [`crate::guestmem::
    /// bptr_from_addr`] computes host-side). Cross-checked against the
    /// disk-info test's sibling instruction `0xE580` ("ASL.L #2,D0"):
    /// identical count/size/immediate bits, only the direction bit (set
    /// here for *right*, there for *left*) and the AS/LS type bits
    /// differ.
    fn lsr_l_2_d(dst_d: u16) -> u16 {
        0xE488 | dst_d
    }
    fn push_word_pair(words: &mut Vec<u16>, value: u32) {
        words.push((value >> 16) as u16);
        words.push(value as u16);
    }

    /// Sanity-checks the hand-derived opcode helpers above against
    /// instructions this file's own, already-passing tests use
    /// elsewhere -- catches an encoding mistake here before it can
    /// manifest as a confusing failure in the real end-to-end test
    /// below.
    #[test]
    fn opcode_helpers_match_known_good_instructions() {
        assert_eq!(move_reg_to_reg(1, 0), MOVE_L_D0_D1, "move.l d0,d1");
        assert_eq!(movea_reg(3, 0), 0x2640, "movea.l d0,a3");
        assert_eq!(movea_imm(1), 0x227C, "movea.l #imm,a1");
        assert_eq!(movea_imm(0), 0x207C, "movea.l #imm,a0");
        assert_eq!(move_disp_to_d(1, 0), 0x2228, "move.l (8,a0),d1");
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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

    // --- issue #186: AllocDosObject(DOS_FILEHANDLE) + packet-level
    // ACTION_LOCATE_OBJECT/ACTION_FINDOUTPUT, reproducing the real
    // ixemul.library call sequence as closely as this test harness can
    // manage at the m68k-instruction level. ---

    /// `AllocDosObject(DOS_FILEHANDLE, NULL)` then immediately
    /// `FreeDosObject(DOS_FILEHANDLE, obj)` -- the simplest possible
    /// proof that both new `type` cases reach real trap dispatch
    /// cleanly (no `DispatchError`, i.e. neither side hits the
    /// "unimplemented type" guard nor a bogus heap-free). The fuller
    /// open/lock/packet dance below is a separate test so a failure
    /// here points straight at `alloc_dos_object_handler`/
    /// `free_dos_object_handler` rather than anywhere else.
    #[test]
    fn end_to_end_alloc_and_free_dos_filehandle_roundtrip() {
        const DOS_FILEHANDLE: u32 = 0;
        let mut words = Vec::new();
        push_move_imm_to_d(&mut words, 1, DOS_FILEHANDLE);
        push_move_imm_to_d(&mut words, 2, 0); // tags = NULL
        push_jsr(&mut words, 6, -228); // AllocDosObject(a6) -> D0 = addr
        words.push(move_reg_to_reg(2, 0)); // D2 = addr (FreeDosObject's object arg)
        push_move_imm_to_d(&mut words, 1, DOS_FILEHANDLE);
        push_jsr(&mut words, 6, -234); // FreeDosObject(a6)
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
        rt.run(&mut out, None)
            .expect("AllocDosObject(DOS_FILEHANDLE) then FreeDosObject should not error");
    }

    /// The full real-world sequence issue #186 is about:
    /// `AllocDosObject(DOS_FILEHANDLE)` to get a `FileHandle` of its
    /// own, `Lock("SYS:")` to get a directory lock, a raw
    /// `ACTION_LOCATE_OBJECT` packet sent to that lock's `fl_Task` to
    /// resolve a subdirectory by name (exactly `ixemul.library`'s
    /// `__plock()`), then a raw `ACTION_FINDOUTPUT` packet -- using the
    /// `AllocDosObject`'d `FileHandle` and the lock `ACTION_LOCATE_OBJECT`
    /// just returned -- to create a new file in that subdirectory
    /// (exactly `ixemul.library`'s `__open()`). Both packets are sent
    /// to the same port (the `FileHandle`'s own `fh_Type`, read back out
    /// of guest memory -- it's the same synthetic port a lock's
    /// `fl_Task` carries, see `doslock.rs`'s module docs). Routing,
    /// `ACTION_WRITE`/`ACTION_READ`/`ACTION_END`, and the matching
    /// `FreeDosObject` close-out are already covered by
    /// `end_to_end_action_read_packet_via_raw_putmsg` (above, via the
    /// ordinary `Open()`-sourced handle) and
    /// `crate::dosfile`'s/`crate::doslock`'s own direct `DosState` unit
    /// tests -- this test's job is specifically the new `AllocDosObject(
    /// DOS_FILEHANDLE)` + `ACTION_LOCATE_OBJECT` + `ACTION_FINDOUTPUT`
    /// wiring, proven through real trap dispatch rather than by calling
    /// `DosState` methods directly.
    #[test]
    fn end_to_end_allocdosobject_locate_object_and_findoutput_via_raw_putmsg() {
        let dir = TempDir::new("pkt-filehandle");
        std::fs::create_dir(dir.path.join("sub")).unwrap();

        let mut words = Vec::new();

        // AllocDosObject(DOS_FILEHANDLE, NULL) -> D0 = fh addr.
        push_move_imm_to_d(&mut words, 1, 0); // DOS_FILEHANDLE
        push_move_imm_to_d(&mut words, 2, 0); // tags = NULL
        push_jsr(&mut words, 6, -228); // AllocDosObject(a6)
        words.push(move_reg_to_reg(3, 0)); // D3 = fh addr (kept for later)
        words.push(lsr_l_2_d(0)); // D0 = fh BPTR
        let pkt2_arg1_patch = words.len();
        words.push(move_d_to_abs(0)); // MOVE.L D0,(pkt2+DP_ARG1).L
        push_word_pair(&mut words, 0); // patched below

        // Read fh_Type (offset 8) off the FileHandle we just got, so
        // both packets below know which port to PutMsg to.
        words.push(movea_reg(3, 3)); // A3 = fh addr
        words.push(move_disp_to_d(4, 3)); // D4 = (FH_TYPE_OFFSET,A3)
        words.push(crate::dosfile::FH_TYPE_OFFSET as u16);

        // Lock("SYS:", SHARED_LOCK) -> D0 = lock1 BPTR.
        let sysname_idx = words.len();
        push_move_imm_to_d(&mut words, 1, 0); // D1 = &"SYS:" (patched)
        push_move_imm_to_d(&mut words, 2, crate::doslock::SHARED_LOCK as u32);
        push_jsr(&mut words, 6, -84); // Lock(a6)
        let pkt1_arg1_patch = words.len();
        words.push(move_d_to_abs(0)); // MOVE.L D0,(pkt1+DP_ARG1).L
        push_word_pair(&mut words, 0); // patched below

        // Switch to exec.library for PutMsg/GetMsg.
        words.push(movea_imm(6)); // A6 = #EXEC_LIBRARY_BASE (patched)
        push_word_pair(&mut words, crate::dispatch::EXEC_LIBRARY_BASE);

        // PutMsg(pkt1's message) to fh_Type, then GetMsg the reply.
        words.push(movea_reg(0, 4)); // A0 = D4 (port)
        let msg1_idx = words.len();
        words.push(movea_imm(1)); // A1 = &msg1 (patched)
        push_word_pair(&mut words, 0);
        push_jsr(&mut words, 6, -366); // PutMsg(a6)
        let rport1_idx = words.len();
        words.push(movea_imm(0)); // A0 = &rport (patched)
        push_word_pair(&mut words, 0);
        push_jsr(&mut words, 6, -372); // GetMsg(a6)

        // Read pkt1's dp_Res1 (the ACTION_LOCATE_OBJECT result lock) and
        // store it into pkt2's dp_Arg2 (FINDOUTPUT's parent lock).
        let pkt1_res1_idx = words.len();
        words.push(move_abs_to_d(0)); // D0 = (pkt1+DP_RES1)
        push_word_pair(&mut words, 0);
        let pkt2_arg2_patch = words.len();
        words.push(move_d_to_abs(0)); // (pkt2+DP_ARG2) = D0
        push_word_pair(&mut words, 0);

        // PutMsg(pkt2's message) to fh_Type, then GetMsg the reply.
        words.push(movea_reg(0, 4)); // A0 = D4 (port)
        let msg2_idx = words.len();
        words.push(movea_imm(1)); // A1 = &msg2 (patched)
        push_word_pair(&mut words, 0);
        push_jsr(&mut words, 6, -366); // PutMsg(a6)
        let rport2_idx = words.len();
        words.push(movea_imm(0)); // A0 = &rport (patched)
        push_word_pair(&mut words, 0);
        push_jsr(&mut words, 6, -372); // GetMsg(a6)
        words.push(RTS);

        // --- Layout the data region right after the code, 4-byte
        // aligned throughout (every BSTR/packet/message/port address
        // below is passed as a BPTR somewhere, which requires it). ---
        let base = (TRAP_TABLE_END + (words.len() as u32) * 2 + 3) & !3;
        let sysname_addr = base;
        let subname_addr = sysname_addr + 8; // C string "SYS:\0", padded
        let pkt1 = subname_addr + 8; // BSTR len=3 "sub", padded
        let msg1 = pkt1 + 36;
        let pkt2 = msg1 + 16;
        let msg2 = pkt2 + 36;
        let rport = msg2 + 16;
        let filename_addr = rport + 36; // BSTR len=7 "new.txt"

        words[sysname_idx + 1] = (sysname_addr >> 16) as u16;
        words[sysname_idx + 2] = sysname_addr as u16;
        words[pkt2_arg1_patch + 1] = ((pkt2 + DP_ARG1) >> 16) as u16;
        words[pkt2_arg1_patch + 2] = (pkt2 + DP_ARG1) as u16;
        words[pkt1_arg1_patch + 1] = ((pkt1 + DP_ARG1) >> 16) as u16;
        words[pkt1_arg1_patch + 2] = (pkt1 + DP_ARG1) as u16;
        words[msg1_idx + 1] = (msg1 >> 16) as u16;
        words[msg1_idx + 2] = msg1 as u16;
        words[rport1_idx + 1] = (rport >> 16) as u16;
        words[rport1_idx + 2] = rport as u16;
        words[pkt1_res1_idx + 1] = ((pkt1 + DP_RES1) >> 16) as u16;
        words[pkt1_res1_idx + 2] = (pkt1 + DP_RES1) as u16;
        words[pkt2_arg2_patch + 1] = ((pkt2 + DP_ARG2) >> 16) as u16;
        words[pkt2_arg2_patch + 2] = (pkt2 + DP_ARG2) as u16;
        words[msg2_idx + 1] = (msg2 >> 16) as u16;
        words[msg2_idx + 2] = msg2 as u16;
        words[rport2_idx + 1] = (rport >> 16) as u16;
        words[rport2_idx + 2] = rport as u16;

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);
        crate::guestmem::write_c_string(&mut mem, sysname_addr, b"SYS:");
        crate::guestmem::write_bstr(&mut mem, subname_addr, b"sub");
        crate::guestmem::write_bstr(&mut mem, filename_addr, b"new.txt");

        mem.write_u32(pkt1 + DP_PORT, rport);
        mem.write_u32(pkt1 + DP_TYPE, ACTION_LOCATE_OBJECT);
        mem.write_u32(
            pkt1 + DP_ARG2,
            crate::guestmem::bptr_from_addr(subname_addr),
        );
        mem.write_u32(pkt1 + DP_ARG3, crate::doslock::SHARED_LOCK as u32);
        mem.write_u32(msg1 + crate::execlist::LN_NAME, pkt1);

        mem.write_u32(pkt2 + DP_PORT, rport);
        mem.write_u32(pkt2 + DP_TYPE, ACTION_FINDOUTPUT);
        mem.write_u32(
            pkt2 + DP_ARG3,
            crate::guestmem::bptr_from_addr(filename_addr),
        );
        mem.write_u32(msg2 + crate::execlist::LN_NAME, pkt2);

        crate::execlist::init_list_header(&mut mem, rport + crate::execlist::MP_MSGLIST);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: filename_addr + 16,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let vfs = Vfs::new(VfsConfig {
            volumes: vec![("SYS".to_string(), dir.path.clone())],
            assigns: vec![],
            auto_assign_root: None,
            cwd: "SYS:".to_string(),
            ..Default::default()
        })
        .expect("build vfs");
        rt.set_vfs(vfs);

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        let mem = rt.memory();

        assert_eq!(
            code as u32, msg2,
            "GetMsg must find packet 2's reply already queued"
        );
        assert_ne!(
            mem.read_u32(pkt1 + DP_RES1),
            0,
            "ACTION_LOCATE_OBJECT should produce a real lock on SYS:sub"
        );
        assert_eq!(mem.read_u32(pkt1 + DP_RES2), 0);
        assert_eq!(
            mem.read_u32(pkt2 + DP_RES1),
            DOSTRUE,
            "ACTION_FINDOUTPUT should succeed"
        );
        assert_eq!(mem.read_u32(pkt2 + DP_RES2), 0);
        assert!(
            dir.path.join("sub").join("new.txt").is_file(),
            "ACTION_FINDOUTPUT should have created SYS:sub/new.txt on the host"
        );
    }
}
