//! Shell startup: `dos.library`'s `CliInitRun` (issue #20).
//!
//! A real AmigaDOS Shell is a segment with BCPL linkage that `dos.library`
//! starts as a process with a startup `DosPacket`. The Shell checks the
//! packet, moves its own segment in `pr_SegList` from slot 3 to slot 4,
//! then calls `CliInitRun(packet)` to have `dos.library` initialise the
//! process's `struct CommandLineInterface` from the packet. This module is
//! that call; [`crate::dispatch::Runtime::start_as_shell`] builds the packet
//! and the `pr_SegList` array the Shell expects.
//!
//! The protocol is the AmigaDOS Manual's "Writing Custom Shells" chapter
//! (the RKRM-style `shell.md`, including its sample shell startup code):
//!
//! - `CliInitRun` returns a `LONG` of flags (not in any official header;
//!   the manual suggests the names below): `FN_VALID` (bit 31) says the
//!   rest are valid, `FN_ASYNC` (3) says reply the packet at once,
//!   `FN_SYSTEM` (2) that the Shell ends at EOF of `cli_CurrentInput`,
//!   `FN_USERINPUT` (1) that it must not close `cli_StandardInput` on
//!   exit, `FN_RUNOUTPUT` (0) that it closes `cli_StandardOutput`.
//! - Failure is `FN_VALID` clear with `IoErr()` equal to the caller's
//!   process pointer, after replying the packet; this runtime has no way
//!   to fail yet beyond a process with no CLI.
//!
//! `SetCurrentDirName`, `SetPrompt` and `GetPrompt` are here too: the
//! Shell's `CD` and `Prompt` commands keep the current directory's name and
//! the prompt in the CLI. (`GetCurrentDirName` is in [`crate::doslock`]: it
//! answers from the process's actual current directory, which is also right
//! for a process whose CLI never had a name set.)
//!
//! # What this makes of the CLI
//!
//! The packet's arguments are "intentionally not documented", and the only
//! startup this runtime makes is the one an interactive Shell on the host
//! console needs, so they are not read: the CLI is initialised as a
//! console shell whose input and output are the process's `Input()` and
//! `Output()` (the host's console): `cli_StandardInput`/`cli_CurrentInput`
//! and `cli_StandardOutput`/`cli_CurrentOutput`, `cli_Background` false,
//! `cli_FailLevel` 10 (`RETURN_ERROR`, the `AllocDosObject(DOS_CLI)`
//! default), an empty `cli_CommandName`/`cli_CommandFile`, a default prompt
//! `%N> ` and `cli_SetName` the current directory's name. The returned
//! flags are `FN_VALID | FN_USERINPUT`: the Shell replies the packet when
//! its last command returns, reads commands from `cli_StandardInput` when a
//! script runs out, and leaves the console's handles open.

use crate::cpu::{Cpu, DataRegister};
use crate::dispatch::{DOS_LIBRARY_BASE, DispatchError, HandlerContext, LibraryTable};
use crate::dosfile::{DOSFALSE, DOSTRUE};
use crate::exectask;
use crate::guestmem::{addr_from_bptr, bptr_from_addr, read_bstr, read_c_string, write_bstr};
use crate::lvos::dos::DOS_LVOS;
use crate::memory::AddressSpace;

/// `FN_VALID`: bit 31 of `CliInitRun`'s result.
const FN_VALID: u32 = 1 << 31;
/// `FN_USERINPUT`: bit 1 -- do not close `cli_StandardInput` on exit.
const FN_USERINPUT: u32 = 1 << 1;
/// `RETURN_ERROR`, a fresh CLI's `cli_FailLevel`.
const RETURN_ERROR: u32 = 10;
/// The default prompt: `%N` is the CLI number.
const DEFAULT_PROMPT: &[u8] = b"%N> ";
/// Size of each `BSTR` buffer the CLI points at (`AllocDosObject(DOS_CLI)`
/// uses 255 too), plus the length byte.
const BUFFER_LEN: u32 = 255;

/// `CliInitRun` (LVO -936: `A0` = the startup `DosPacket`). `D0` = flags;
/// see the module docs.
fn cli_init_run_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let task = ctx.current_task;
    let cli = addr_from_bptr(ctx.mem.read_u32(task + exectask::PR_CLI_OFFSET));
    if cli == 0 {
        // No CLI to initialise: report failure the documented way (flags
        // clear, `IoErr()` the process).
        ctx.dos.set_io_err(task as i32);
        ctx.cpu.set_data_register(DataRegister(0), 0);
        return Ok(());
    }
    let heap_full = |what: &str| DispatchError::HandlerFailed {
        library: "dos.library".to_string(),
        lvo: -936,
        handler_name: "CliInitRun".to_string(),
        message: format!("no guest heap for the CLI's {what}"),
    };

    // The BSTR buffers: the CLI's own if it has them (a second call, or a
    // CLI made with them), else a fresh one, as `DOS_CLI` makes them. A
    // buffer already there is cleared and reused, not replaced, so a
    // repeated call leaks nothing.
    let buffer = |ctx: &mut HandlerContext<'_, C>, offset: u32, what: &str| {
        let existing = addr_from_bptr(ctx.mem.read_u32(cli + offset));
        let addr = if existing != 0 {
            existing
        } else {
            ctx.heap
                .alloc(&mut *ctx.mem, BUFFER_LEN + 1)
                .map_err(|_| heap_full(what))?
        };
        for i in 0..=BUFFER_LEN {
            ctx.mem.write_u8(addr + i, 0);
        }
        Ok::<u32, DispatchError>(addr)
    };
    let set_name = buffer(ctx, exectask::CLI_SET_NAME_OFFSET, "cli_SetName")?;
    let command_file = buffer(ctx, exectask::CLI_COMMAND_FILE_OFFSET, "cli_CommandFile")?;
    let prompt = buffer(ctx, exectask::CLI_PROMPT_OFFSET, "cli_Prompt")?;
    write_bstr(ctx.mem, prompt, DEFAULT_PROMPT);
    // `cli_CommandName` already has a buffer from process creation (the
    // program's name); a Shell has none to report.
    let name = addr_from_bptr(ctx.mem.read_u32(cli + exectask::CLI_COMMAND_NAME_OFFSET));
    if name != 0 {
        ctx.mem.write_u8(name, 0);
    }

    // `cli_SetName`: where the CLI is now.
    let dir_lock = addr_from_bptr(ctx.mem.read_u32(task + exectask::PR_CURRENT_DIR_OFFSET));
    let dir_name = ctx.dos.name_from_lock(dir_lock).unwrap_or_default();
    write_bstr(
        ctx.mem,
        set_name,
        &dir_name.as_bytes()[..dir_name.len().min(BUFFER_LEN as usize)],
    );

    let input = ctx
        .dos
        .input_addr(ctx.heap, ctx.mem)
        .map_err(|_| heap_full("input handle"))?;
    let output = ctx
        .dos
        .output_addr(ctx.heap, ctx.mem)
        .map_err(|_| heap_full("output handle"))?;
    let (input, output) = (bptr_from_addr(input), bptr_from_addr(output));

    ctx.mem.write_u32(
        cli + exectask::CLI_SET_NAME_OFFSET,
        bptr_from_addr(set_name),
    );
    ctx.mem.write_u32(
        cli + exectask::CLI_COMMAND_FILE_OFFSET,
        bptr_from_addr(command_file),
    );
    ctx.mem
        .write_u32(cli + exectask::CLI_PROMPT_OFFSET, bptr_from_addr(prompt));
    ctx.mem
        .write_u32(cli + exectask::CLI_FAIL_LEVEL_OFFSET, RETURN_ERROR);
    ctx.mem.write_u32(cli + exectask::CLI_BACKGROUND_OFFSET, 0);
    ctx.mem
        .write_u32(cli + exectask::CLI_STANDARD_INPUT_OFFSET, input);
    ctx.mem
        .write_u32(cli + exectask::CLI_CURRENT_INPUT_OFFSET, input);
    ctx.mem
        .write_u32(cli + exectask::CLI_STANDARD_OUTPUT_OFFSET, output);
    ctx.mem
        .write_u32(cli + exectask::CLI_CURRENT_OUTPUT_OFFSET, output);

    ctx.cpu
        .set_data_register(DataRegister(0), FN_VALID | FN_USERINPUT);
    Ok(())
}

/// The calling process's `CommandLineInterface`, or `None` for a process
/// without one (a Workbench start): these calls then fail, as the real
/// ones do.
fn current_cli<C: Cpu>(ctx: &HandlerContext<'_, C>) -> Option<u32> {
    let cli = addr_from_bptr(ctx.mem.read_u32(ctx.current_task + exectask::PR_CLI_OFFSET));
    (cli != 0).then_some(cli)
}

/// Longest value the CLI's string fields hold: a `BSTR` is at most 255
/// bytes, and the buffers made for a CLI are that big.
const MAX_FIELD: usize = 255;

/// `SetCurrentDirName`/`SetPrompt`: stores the string `D1` points at in
/// the `BSTR` field of `pr_CLI` at `offset`. `DOSFALSE` if there is no CLI
/// or the string is longer than a field holds. A CLI whose field has no
/// buffer yet (one this runtime made for `SystemTagList`) gets one.
fn set_cli_string<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
    offset: u32,
) -> Result<(), DispatchError> {
    let name = read_c_string(ctx.mem, ctx.cpu.data_register(DataRegister(1)));
    let result = match current_cli(ctx) {
        Some(cli) if name.len() <= MAX_FIELD => {
            let mut buffer = addr_from_bptr(ctx.mem.read_u32(cli + offset));
            if buffer == 0 {
                match ctx.heap.alloc(&mut *ctx.mem, MAX_FIELD as u32 + 1) {
                    Ok(addr) => {
                        buffer = addr;
                        ctx.mem.write_u32(cli + offset, bptr_from_addr(addr));
                    }
                    Err(_) => {
                        ctx.cpu.set_data_register(DataRegister(0), DOSFALSE);
                        return Ok(());
                    }
                }
            }
            write_bstr(ctx.mem, buffer, &name);
            DOSTRUE
        }
        _ => DOSFALSE,
    };
    ctx.cpu.set_data_register(DataRegister(0), result);
    Ok(())
}

/// `GetPrompt`: copies the `BSTR` field of `pr_CLI` at `offset` into the buffer `D1` (`D2` bytes) as a C string. `DOSFALSE`
/// with `ERROR_LINE_TOO_LONG` if it does not fit (what fits is copied, and
/// terminated); `DOSFALSE` if there is no CLI.
fn get_cli_string<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
    offset: u32,
) -> Result<(), DispatchError> {
    let buffer = ctx.cpu.data_register(DataRegister(1));
    let size = ctx.cpu.data_register(DataRegister(2)) as usize;
    let Some(cli) = current_cli(ctx) else {
        ctx.cpu.set_data_register(DataRegister(0), DOSFALSE);
        return Ok(());
    };
    let field = addr_from_bptr(ctx.mem.read_u32(cli + offset));
    let value = if field == 0 {
        Vec::new()
    } else {
        read_bstr(ctx.mem, field)
    };
    let fits = value.len() < size;
    if size > 0 {
        let n = value.len().min(size - 1);
        for (i, byte) in value[..n].iter().enumerate() {
            ctx.mem.write_u8(buffer + i as u32, *byte);
        }
        ctx.mem.write_u8(buffer + n as u32, 0);
    }
    if !fits {
        ctx.dos.set_io_err(crate::dospattern::ERROR_LINE_TOO_LONG);
    }
    ctx.cpu
        .set_data_register(DataRegister(0), if fits { DOSTRUE } else { DOSFALSE });
    Ok(())
}

fn set_current_dir_name_handler<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
) -> Result<(), DispatchError> {
    set_cli_string(ctx, exectask::CLI_SET_NAME_OFFSET)
}

fn set_prompt_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    set_cli_string(ctx, exectask::CLI_PROMPT_OFFSET)
}

fn get_prompt_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    get_cli_string(ctx, exectask::CLI_PROMPT_OFFSET)
}

/// Registers `CliInitRun` onto [`DOS_LIBRARY_BASE`]. Called from
/// [`crate::dispatch::Runtime::new`] alongside the other `dos.library`
/// registrations.
pub fn register_cli_handlers<C: Cpu + 'static>(table: &mut LibraryTable<C>, mem: &mut C::Memory) {
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
    reg!("CliInitRun", cli_init_run_handler::<C>);
    reg!("SetCurrentDirName", set_current_dir_name_handler::<C>);
    reg!("SetPrompt", set_prompt_handler::<C>);
    reg!("GetPrompt", get_prompt_handler::<C>);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{Runtime, StartConfig};
    use crate::memory::FlatMemory;

    const RTS: u16 = 0x4E75;

    fn push_move_imm_to_d(words: &mut Vec<u16>, dn: u16, imm: u32) {
        words.push(0x203C | (dn << 9));
        words.push((imm >> 16) as u16);
        words.push(imm as u16);
    }

    /// `jsr disp(A6)` (A6 is `dos.library` at program start).
    fn push_jsr_dos(words: &mut Vec<u16>, disp: i32) {
        words.push(0x4EAE);
        words.push(disp as u16);
    }

    fn push_store_d0(words: &mut Vec<u16>, addr: u32) {
        words.push(0x23C0);
        words.push((addr >> 16) as u16);
        words.push(addr as u16);
    }

    fn run(
        init: impl Fn(&mut FlatMemory, u32),
        build: impl Fn(&mut Vec<u16>, u32),
    ) -> (Runtime<M68kCpu>, u32) {
        let scratch = TRAP_TABLE_END + 0x400;
        let mut words = Vec::new();
        build(&mut words, scratch);
        words.push(RTS);
        assert!(words.len() * 2 < 0x400);
        let mut mem = FlatMemory::new(0x2_0000);
        for (i, &w) in words.iter().enumerate() {
            mem.write_u16(TRAP_TABLE_END + 2 * i as u32, w);
        }
        init(&mut mem, scratch);
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
        rt.run(&mut out, None).expect("run should succeed");
        (rt, scratch)
    }

    fn cli_of(rt: &Runtime<M68kCpu>) -> u32 {
        addr_from_bptr(
            rt.memory()
                .read_u32(rt.current_task() + exectask::PR_CLI_OFFSET),
        )
    }

    #[test]
    fn cli_init_run_makes_the_process_a_console_shell_20() {
        let (rt, scratch) = run(
            |_, _| {},
            |w, s| {
                // CliInitRun(A0 = a packet it does not read)
                w.push(0x207C); // movea.l #imm,a0
                w.push((s >> 16) as u16);
                w.push(s as u16);
                push_jsr_dos(w, -936);
                push_store_d0(w, s + 16);
            },
        );
        let mem = rt.memory();
        assert_eq!(
            mem.read_u32(scratch + 16),
            FN_VALID | FN_USERINPUT,
            "FN_VALID | FN_USERINPUT: reply when the last command ends, keep stdin"
        );
        let cli = cli_of(&rt);
        let field = |offset| mem.read_u32(cli + offset);
        assert_eq!(
            read_bstr(mem, addr_from_bptr(field(exectask::CLI_PROMPT_OFFSET))),
            b"%N> "
        );
        assert_eq!(field(exectask::CLI_FAIL_LEVEL_OFFSET), RETURN_ERROR);
        assert_eq!(field(exectask::CLI_BACKGROUND_OFFSET), 0);
        let input = field(exectask::CLI_STANDARD_INPUT_OFFSET);
        assert_ne!(input, 0);
        assert_eq!(field(exectask::CLI_CURRENT_INPUT_OFFSET), input);
        let output = field(exectask::CLI_STANDARD_OUTPUT_OFFSET);
        assert_ne!(output, 0);
        assert_eq!(field(exectask::CLI_CURRENT_OUTPUT_OFFSET), output);
    }

    #[test]
    fn set_prompt_and_get_prompt_round_trip_and_get_refuses_a_buffer_too_small_20() {
        // scratch: +0 "hello> ", +16 results, +32 out buffer, +64 small out.
        let (rt, scratch) = run(
            |mem, s| {
                for (i, b) in b"hello> \0".iter().enumerate() {
                    mem.write_u8(s + i as u32, *b);
                }
            },
            |w, s| {
                push_move_imm_to_d(w, 1, s);
                push_jsr_dos(w, -582); // SetPrompt
                push_store_d0(w, s + 16);
                push_move_imm_to_d(w, 1, s + 32);
                push_move_imm_to_d(w, 2, 32);
                push_jsr_dos(w, -588); // GetPrompt
                push_store_d0(w, s + 20);
                // 4 bytes hold three characters and the NUL, not seven.
                push_move_imm_to_d(w, 1, s + 64);
                push_move_imm_to_d(w, 2, 4);
                push_jsr_dos(w, -588);
                push_store_d0(w, s + 24);
            },
        );
        let mem = rt.memory();
        assert_eq!(mem.read_u32(scratch + 16), DOSTRUE, "SetPrompt");
        assert_eq!(mem.read_u32(scratch + 20), DOSTRUE, "GetPrompt");
        assert_eq!(
            crate::guestmem::read_c_string(mem, scratch + 32),
            b"hello> "
        );
        assert_eq!(mem.read_u32(scratch + 24), DOSFALSE, "too small");
        assert_eq!(
            crate::guestmem::read_c_string(mem, scratch + 64),
            b"hel",
            "what fits is copied and terminated"
        );
    }

    #[test]
    fn set_current_dir_name_stores_the_name_the_prompt_shows_20() {
        let (rt, scratch) = run(
            |mem, s| {
                for (i, b) in b"RAM:work\0".iter().enumerate() {
                    mem.write_u8(s + i as u32, *b);
                }
            },
            |w, s| {
                push_move_imm_to_d(w, 1, s);
                push_jsr_dos(w, -558); // SetCurrentDirName
                push_store_d0(w, s + 16);
            },
        );
        let mem = rt.memory();
        assert_eq!(mem.read_u32(scratch + 16), DOSTRUE);
        let cli = cli_of(&rt);
        assert_eq!(
            read_bstr(
                mem,
                addr_from_bptr(mem.read_u32(cli + exectask::CLI_SET_NAME_OFFSET))
            ),
            b"RAM:work",
            "the name the Shell's prompt (%S) shows"
        );
    }
}
