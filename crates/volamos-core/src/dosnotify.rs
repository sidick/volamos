//! `dos.library` `StartNotify`/`EndNotify`: accept-and-forget stubs.
//!
//! Real `StartNotify`/`EndNotify` register and deregister a
//! `struct NotifyRequest` watching a file or directory for changes,
//! optionally signalling the caller (`NRF_SEND_SIGNAL`) or sending a
//! message (`NRF_SEND_MESSAGE`) when it does. This runtime has no
//! filesystem-change notification machinery at all -- nothing here
//! ever mutates a watched path out from under the guest except the
//! guest's own calls, which it could just as easily poll for -- so
//! there is nothing to actually deliver.
//!
//! Unlike `exec.library`'s interrupt-registration stubs
//! ([`crate::execint`]), refusing outright is the wrong call here:
//! `StartNotify` has a real, documented failure return (`DOSFALSE`,
//! `IoErr()` set), so a caller that only wants a "please tell me if
//! this changes" best-effort hint -- `ixemul.library`'s `ix_init.c`
//! watches `ENV:` to reload settings on change -- is written to
//! tolerate it failing and just never gets notified, matching a real
//! `StartNotify` call for a device/path that doesn't support
//! notification. `EndNotify` always succeeds on real AmigaOS once its
//! request is already pending (nothing to signal, nothing to undo),
//! so it just returns `DOSTRUE` unconditionally. Found needed running
//! a real `ixemul.library` 80.1-linked program (issue #178).

use crate::cpu::{Cpu, DataRegister};
use crate::dispatch::{DOS_LIBRARY_BASE, DispatchError, HandlerContext, LibraryTable};
use crate::lvos::dos::DOS_LVOS;

const DOSTRUE: u32 = 0xFFFF_FFFF;
const DOSFALSE: u32 = 0;

/// `ERROR_ACTION_NOT_KNOWN` (`<dos/dos.h>`): the real code a device or
/// handler returns for a request it doesn't implement, which is
/// exactly this runtime's situation -- no notification machinery at
/// all, on any path.
const ERROR_ACTION_NOT_KNOWN: i32 = 203;

/// `StartNotify` (`D1` = `struct NotifyRequest *`). Always fails with
/// `ERROR_ACTION_NOT_KNOWN` -- see this module's docs for why that is
/// the honest answer rather than a crash or a silent "sure, but never
/// fires".
fn start_notify_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    ctx.dos.set_io_err(ERROR_ACTION_NOT_KNOWN);
    ctx.cpu.set_data_register(DataRegister(0), DOSFALSE);
    Ok(())
}

/// `EndNotify` (`D1` = `struct NotifyRequest *`). Always succeeds --
/// see this module's docs.
fn end_notify_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    ctx.cpu.set_data_register(DataRegister(0), DOSTRUE);
    Ok(())
}

/// Registers `StartNotify`/`EndNotify` onto [`DOS_LIBRARY_BASE`].
/// Called from [`crate::dispatch::Runtime::new`] alongside the other
/// `dos.library` registrations.
pub fn register_dosnotify_handlers<C: Cpu + 'static>(
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
    reg!("StartNotify", start_notify_handler::<C>);
    reg!("EndNotify", end_notify_handler::<C>);
}

#[cfg(test)]
mod tests {
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{Runtime, StartConfig};
    use crate::memory::{AddressSpace, FlatMemory};

    use super::*;

    fn move_imm_to_d(n: u16) -> u16 {
        0x203C | (n << 9)
    }
    fn jsr_disp16(an: u16) -> u16 {
        0x4EA8 | an
    }
    const RTS: u16 = 0x4E75;

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

    /// Runs `words` as the whole program (via real A-line trap
    /// dispatch) and returns the final `D0` -- the program's own
    /// "exit code".
    fn run_and_get_d0(words: Vec<u16>) -> u32 {
        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);
        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: TRAP_TABLE_END + (words.len() as u32) * 2,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed") as u32
    }

    #[test]
    fn start_notify_fails_cleanly_with_action_not_known() {
        let mut words = Vec::new();
        push_move_imm_to_d(&mut words, 1, 0); // D1 = NotifyRequest * (unused)
        push_jsr(&mut words, 6, -888); // StartNotify(a6)
        push_jsr(&mut words, 6, -132); // IoErr(a6): D0 = the code StartNotify set
        words.push(RTS);

        // The program's final D0 (its "exit code") is IoErr()'s
        // result, which is only non-zero if StartNotify actually set
        // it -- a stray DOSTRUE-but-no-IoErr bug would read back as 0
        // here, not ERROR_ACTION_NOT_KNOWN.
        assert_eq!(run_and_get_d0(words), ERROR_ACTION_NOT_KNOWN as u32);
    }

    #[test]
    fn end_notify_always_succeeds() {
        let mut words = Vec::new();
        push_move_imm_to_d(&mut words, 1, 0); // D1 = NotifyRequest * (unused)
        push_jsr(&mut words, 6, -894); // EndNotify(a6)
        words.push(RTS);

        assert_eq!(run_and_get_d0(words), DOSTRUE);
    }
}
