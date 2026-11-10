//! `PIPE:` -- named in-memory channels between two `FileHandle`s, the
//! part of AmigaOS's queue-handler that libnix's `pipe()` and `popen()`
//! (and so musl's libc-test `functional/fscanf`) rely on (issue #264).
//!
//! `Open("PIPE:name", MODE_NEWFILE)` opens the write end of the channel
//! `name`, `Open("PIPE:name", MODE_OLDFILE)` the read end; whichever
//! comes first creates it. That is all libnix needs, so that is all
//! there is: no buffer-size or flag suffixes in the name, no opening it
//! from a shell, no `MODE_READWRITE`.
//!
//! - A read returns what is buffered (up to the length asked for) and
//!   only waits when nothing is. Once a writer has been opened and
//!   every writer has closed, an empty channel reads 0 (EOF). Before
//!   any writer has been opened, a reader just waits.
//! - A write waits while the buffer is full, and completes only when
//!   every byte is in. Writes land in the buffer as space opens up, so
//!   a write bigger than the whole buffer still goes through once a
//!   reader drains it. [`BUFFER_SIZE`] is queue-handler's own default.
//! - Writing to a channel whose readers have all closed fails
//!   ([`ERROR_WRITE_PROTECTED`]): nothing would ever drain it.
//! - Both ends work from different tasks, as with `popen()`'s child.
//!
//! # Blocking
//!
//! `Read`/`Write` (and their `ACTION_READ`/`ACTION_WRITE` packet forms)
//! block the calling task the way `Wait` and `ObtainSemaphore` do: the
//! handler parks it ([`TaskState::BlockedOnPipe`]) and the call is
//! re-entered from scratch when the pipe changes, so every step here
//! must be safe to repeat. A write that only partly fits records how
//! far it got in [`DosState::pipe_write_progress`] so that the
//! re-entered call carries on from there instead of writing the front
//! of its data twice.
//!
//! The buffered-stdio calls (`FGetC`, `FGets`, `FRead`, ...) are not
//! blocking-aware: on a pipe with nothing to read they fail with
//! `ERROR_OBJECT_IN_USE` rather than wait, since `FRead` and `FGets`
//! consume bytes before they could block. libnix's stdio goes through
//! `Read`/`Write`, which is what this exists for.
//!
//! [`TaskState::BlockedOnPipe`]: crate::taskswitch::TaskState::BlockedOnPipe

use crate::cpu::Cpu;
use crate::dispatch::{DispatchError, HandlerContext};
use crate::dosfile::{
    DosState, ERROR_ACTION_NOT_KNOWN, ERROR_OBJECT_IN_USE, HostHandle, MODE_NEWFILE, MODE_OLDFILE,
};
use crate::guestmem::{GuestHeap, bptr_from_addr};
use crate::memory::AddressSpace;
use std::collections::VecDeque;

/// `ERROR_WRITE_PROTECTED` (223), `<dos/dos.h>`.
pub const ERROR_WRITE_PROTECTED: i32 = 223;

/// How many bytes a channel buffers before a write has to wait for a
/// reader: queue-handler's default.
pub const BUFFER_SIZE: usize = 4096;

/// The volume prefix that selects the pipe handler.
const PREFIX: &str = "PIPE:";

/// Which end of a channel a handle is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipeEnd {
    Read,
    Write,
}

/// One named channel.
pub struct PipeChannel {
    /// What a blocked task waits on ([`crate::taskswitch::TaskState::
    /// BlockedOnPipe`]); unique for the life of the run, so a channel
    /// that is closed and re-created under the same name is a different
    /// wait target.
    id: u32,
    buf: VecDeque<u8>,
    readers: u32,
    writers: u32,
    /// Whether a writer was ever opened. A reader that gets there first
    /// must wait for one, not see EOF.
    writer_seen: bool,
    /// Whether a reader was ever opened -- the same for a writer that
    /// finds every reader gone, which must fail rather than wait.
    reader_seen: bool,
}

/// What a read or write did, or why it can't yet.
pub enum IoStep<T> {
    /// The operation finished, successfully or not.
    Done(Result<T, i32>),
    /// Nothing can progress until the pipe with this id changes.
    Block(u32),
}

/// `Some(key)` if `name` is a `PIPE:` path: the channel name,
/// case-folded (AmigaDOS names are case-insensitive).
pub fn pipe_key(name: &str) -> Option<String> {
    let prefix = name.get(..PREFIX.len())?;
    prefix
        .eq_ignore_ascii_case(PREFIX)
        .then(|| name[PREFIX.len()..].to_ascii_uppercase())
}

impl DosState {
    /// `Open` of a `PIPE:` name: the new handle's BPTR, or an `IoErr()`
    /// code. `key` is [`pipe_key`]'s result.
    pub(crate) fn open_pipe(
        &mut self,
        heap: &mut GuestHeap,
        mem: &mut dyn AddressSpace,
        key: String,
        access_mode: i32,
    ) -> Result<u32, i32> {
        let end = match access_mode {
            MODE_NEWFILE => PipeEnd::Write,
            MODE_OLDFILE => PipeEnd::Read,
            _ => return Err(ERROR_ACTION_NOT_KNOWN),
        };
        let addr = self.alloc_file_handle(heap, mem, false)?;
        let id = self.next_pipe_id;
        let channel = self
            .pipes
            .entry(key.clone())
            .or_insert_with(|| PipeChannel {
                id,
                buf: VecDeque::new(),
                readers: 0,
                writers: 0,
                writer_seen: false,
                reader_seen: false,
            });
        if channel.id == id {
            self.next_pipe_id += 1;
        }
        match end {
            PipeEnd::Read => {
                channel.readers += 1;
                channel.reader_seen = true;
            }
            PipeEnd::Write => {
                channel.writers += 1;
                channel.writer_seen = true;
            }
        }
        self.handles
            .insert(addr, HostHandle::Pipe { channel: key, end });
        Ok(bptr_from_addr(addr))
    }

    /// The id of the pipe `addr` is an end of, if it is one -- what the
    /// handlers wake blocked tasks with after they change it.
    pub fn pipe_id(&self, addr: u32) -> Option<u32> {
        match self.handles.get(&addr) {
            Some(HostHandle::Pipe { channel, .. }) => self.pipes.get(channel).map(|c| c.id),
            _ => None,
        }
    }

    /// Drops one end's claim on its channel, and the channel itself once
    /// nobody has it open. Returns the pipe's id so the caller can wake
    /// whoever waits on the other end (an EOF, or a write that can no
    /// longer succeed, is news to them).
    pub(crate) fn close_pipe(&mut self, channel: &str, end: PipeEnd) -> Option<u32> {
        let c = self.pipes.get_mut(channel)?;
        match end {
            PipeEnd::Read => c.readers = c.readers.saturating_sub(1),
            PipeEnd::Write => c.writers = c.writers.saturating_sub(1),
        }
        let id = c.id;
        if c.readers == 0 && c.writers == 0 {
            self.pipes.remove(channel);
        }
        Some(id)
    }

    /// `Read` on a pipe handle. Not for non-pipe handles.
    pub(crate) fn pipe_read(&mut self, addr: u32, len: usize) -> IoStep<Vec<u8>> {
        let Some(HostHandle::Pipe { channel, end }) = self.handles.get(&addr) else {
            return IoStep::Done(Err(ERROR_ACTION_NOT_KNOWN));
        };
        if *end != PipeEnd::Read {
            return IoStep::Done(Err(ERROR_ACTION_NOT_KNOWN));
        }
        let Some(c) = self.pipes.get_mut(channel) else {
            return IoStep::Done(Err(ERROR_ACTION_NOT_KNOWN));
        };
        if len == 0 {
            return IoStep::Done(Ok(Vec::new()));
        }
        if !c.buf.is_empty() {
            let n = len.min(c.buf.len());
            return IoStep::Done(Ok(c.buf.drain(..n).collect()));
        }
        if c.writers == 0 && c.writer_seen {
            return IoStep::Done(Ok(Vec::new())); // EOF
        }
        IoStep::Block(c.id)
    }

    /// `Write` on a pipe handle by `task`: put as much of `data` in the
    /// buffer as fits, picking up where an earlier, blocked attempt left
    /// off. Not for non-pipe handles.
    pub(crate) fn pipe_write(&mut self, task: u32, addr: u32, data: &[u8]) -> IoStep<usize> {
        let Some(HostHandle::Pipe { channel, end }) = self.handles.get(&addr) else {
            return IoStep::Done(Err(ERROR_ACTION_NOT_KNOWN));
        };
        if *end != PipeEnd::Write {
            return IoStep::Done(Err(ERROR_ACTION_NOT_KNOWN));
        }
        let Some(c) = self.pipes.get_mut(channel) else {
            return IoStep::Done(Err(ERROR_ACTION_NOT_KNOWN));
        };
        let key = (task, addr);
        let done = self
            .pipe_write_progress
            .get(&key)
            .copied()
            .unwrap_or(0)
            .min(data.len());

        if c.readers == 0 && c.reader_seen {
            self.pipe_write_progress.remove(&key);
            return IoStep::Done(Err(ERROR_WRITE_PROTECTED));
        }
        let n = (BUFFER_SIZE - c.buf.len()).min(data.len() - done);
        c.buf.extend(&data[done..done + n]);
        let done = done + n;
        if done == data.len() {
            self.pipe_write_progress.remove(&key);
            return IoStep::Done(Ok(data.len()));
        }
        self.pipe_write_progress.insert(key, done);
        IoStep::Block(c.id)
    }

    /// `Read` for callers that can't block (the buffered-stdio calls):
    /// a pipe with nothing to read fails instead of waiting.
    pub(crate) fn pipe_read_nonblocking(&mut self, addr: u32, len: usize) -> Result<Vec<u8>, i32> {
        match self.pipe_read(addr, len) {
            IoStep::Done(r) => r,
            IoStep::Block(_) => Err(ERROR_OBJECT_IN_USE),
        }
    }

    /// `Write` for callers that can't block: a write that doesn't fit
    /// entirely fails, leaving the buffer as it was.
    pub(crate) fn pipe_write_nonblocking(
        &mut self,
        task: u32,
        addr: u32,
        data: &[u8],
    ) -> Result<usize, i32> {
        let free = match self.handles.get(&addr) {
            Some(HostHandle::Pipe { channel, .. }) => self
                .pipes
                .get(channel)
                .map_or(0, |c| BUFFER_SIZE - c.buf.len()),
            _ => 0,
        };
        if data.len() > free {
            // Not even a try: a partial write couldn't be taken back.
            return match self.pipe_write(task, addr, &[]) {
                IoStep::Done(Err(code)) => Err(code), // wrong end / no reader
                _ => Err(ERROR_OBJECT_IN_USE),
            };
        }
        match self.pipe_write(task, addr, data) {
            IoStep::Done(r) => r,
            IoStep::Block(_) => unreachable!("it fits, so it cannot block"),
        }
    }
}

/// What parking a task on a pipe came to.
pub enum Parked {
    /// The task is blocked and another was switched to: the handler
    /// returns without finishing, and runs again on wake-up.
    Switched,
    /// Another task or a timer made progress instead; try the
    /// operation again.
    Retry,
}

/// Blocks the current task until pipe `pipe` changes, or reports that
/// it can't happen. `lvo` and `what` name the call for the deadlock
/// message.
pub(crate) fn park_on_pipe<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
    pipe: u32,
    lvo: i32,
    what: &str,
) -> Result<Parked, DispatchError> {
    if ctx
        .tasks
        .block_current_on_pipe(ctx.cpu, ctx.mem, ctx.trap_pc, pipe)
        .is_ok()
    {
        return Ok(Parked::Switched);
    }
    if ctx.timers.fire_earliest(ctx.mem, ctx.tasks) {
        return Ok(Parked::Retry);
    }
    Err(DispatchError::HandlerFailed {
        library: "dos.library".to_string(),
        lvo,
        handler_name: what.to_string(),
        message: format!(
            "{what} on a PIPE: would deadlock: the pipe has nothing to read / no room to \
             write, and no other task is runnable and no pending timer could ever change that"
        ),
    })
}

/// How a blocking-aware `Read`/`Write` ended.
pub(crate) enum Outcome<T> {
    /// It finished, successfully or not.
    Done(Result<T, i32>),
    /// The task was blocked and another switched to; the handler must
    /// return at once (the call runs again on wake-up).
    Switched,
}

/// `Read` for the `Read` LVO and `ACTION_READ`: like
/// [`DosState::read`], but a pipe with nothing to read blocks the task
/// instead of failing. `lvo`/`what` name the caller in a deadlock error.
pub(crate) fn read<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
    addr: u32,
    len: usize,
    lvo: i32,
    what: &str,
) -> Result<Outcome<Vec<u8>>, DispatchError> {
    let Some(pipe) = ctx.dos.pipe_id(addr) else {
        return Ok(Outcome::Done(ctx.dos.read(addr, len)));
    };
    loop {
        match ctx.dos.pipe_read(addr, len) {
            IoStep::Done(result) => {
                if matches!(&result, Ok(data) if !data.is_empty()) {
                    // Room opened up for a writer that was waiting on it.
                    ctx.tasks.wake_pipe(pipe);
                }
                return Ok(Outcome::Done(result));
            }
            IoStep::Block(id) => match park_on_pipe(ctx, id, lvo, what)? {
                Parked::Switched => return Ok(Outcome::Switched),
                Parked::Retry => {}
            },
        }
    }
}

/// `Write` for the `Write` LVO and `ACTION_WRITE`: like
/// [`DosState::write`], but a pipe with no room blocks the task until
/// all of `data` is in the buffer.
pub(crate) fn write<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
    addr: u32,
    data: &[u8],
    lvo: i32,
    what: &str,
) -> Result<Outcome<usize>, DispatchError> {
    let Some(pipe) = ctx.dos.pipe_id(addr) else {
        return Ok(Outcome::Done(ctx.dos.write(addr, data)));
    };
    let task = ctx.current_task;
    loop {
        let step = ctx.dos.pipe_write(task, addr, data);
        // Whatever went in is news to a reader waiting for data.
        ctx.tasks.wake_pipe(pipe);
        match step {
            IoStep::Done(result) => return Ok(Outcome::Done(result)),
            IoStep::Block(id) => match park_on_pipe(ctx, id, lvo, what)? {
                Parked::Switched => return Ok(Outcome::Switched),
                Parked::Retry => {}
            },
        }
    }
}

/// `Close`/`ACTION_END`: [`DosState::close`], then wake anyone waiting
/// on the other end of a pipe (it just got EOF, or lost its reader).
pub(crate) fn close<C: Cpu>(ctx: &mut HandlerContext<'_, C>, addr: u32) -> bool {
    let pipe = ctx.dos.pipe_id(addr);
    let closed = ctx.dos.close(ctx.heap, addr);
    if let Some(pipe) = pipe {
        ctx.tasks.wake_pipe(pipe);
    }
    closed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{DOS_LIBRARY_BASE, EXEC_LIBRARY_BASE, Runtime, StartConfig};
    use crate::memory::FlatMemory;

    /// A `DosState` and heap to open pipe handles on, with the two ends
    /// of channel `name` open: `(state, write handle, read handle)`.
    fn fixture(name: &str) -> (DosState, u32, u32) {
        let mut mem = FlatMemory::new(0x2_0000);
        let mut heap = GuestHeap::new(0x1_0000, 0x1_8000);
        let mut dos = DosState::new(None);
        let w = dos
            .open(&mut heap, &mut mem, name, MODE_NEWFILE)
            .expect("write end opens");
        let r = dos
            .open(&mut heap, &mut mem, name, MODE_OLDFILE)
            .expect("read end opens");
        (
            dos,
            crate::guestmem::addr_from_bptr(w),
            crate::guestmem::addr_from_bptr(r),
        )
    }

    fn done<T: std::fmt::Debug>(step: IoStep<T>) -> Result<T, i32> {
        match step {
            IoStep::Done(r) => r,
            IoStep::Block(id) => panic!("expected the operation to finish, but it blocks on {id}"),
        }
    }

    fn blocks<T>(step: IoStep<T>) -> bool {
        matches!(step, IoStep::Block(_))
    }

    #[test]
    fn pipe_key_recognises_the_prefix_and_folds_case() {
        assert_eq!(pipe_key("PIPE:foo").as_deref(), Some("FOO"));
        assert_eq!(pipe_key("pipe:Foo-1").as_deref(), Some("FOO-1"));
        assert_eq!(pipe_key("PIPE:libnix-1-2").as_deref(), Some("LIBNIX-1-2"));
        assert_eq!(pipe_key("PIPE").as_deref(), None);
        assert_eq!(pipe_key("PIPES:foo").as_deref(), None);
        assert_eq!(pipe_key("RAM:PIPE:foo").as_deref(), None);
        assert_eq!(pipe_key("NIL:").as_deref(), None);
    }

    #[test]
    fn what_is_written_is_read_back_in_order() {
        let (mut dos, w, r) = fixture("PIPE:rt");
        assert_eq!(done(dos.pipe_write(1, w, b"hello ")), Ok(6));
        assert_eq!(done(dos.pipe_write(1, w, b"world")), Ok(5));
        assert_eq!(done(dos.pipe_read(r, 4)), Ok(b"hell".to_vec()));
        assert_eq!(
            done(dos.pipe_read(r, 100)),
            Ok(b"o world".to_vec()),
            "a read returns what is buffered, not what it asked for"
        );
    }

    #[test]
    fn reading_an_empty_pipe_waits_until_the_writer_closes() {
        let (mut dos, w, r) = fixture("PIPE:eof");
        assert!(blocks(dos.pipe_read(r, 8)), "writer open, nothing buffered");
        done(dos.pipe_write(1, w, b"xy")).unwrap();
        let mut heap = GuestHeap::new(0x1_0000, 0x1_8000);
        assert_eq!(done(dos.pipe_read(r, 8)), Ok(b"xy".to_vec()));
        assert!(
            blocks(dos.pipe_read(r, 8)),
            "drained, but the writer is still open"
        );
        assert!(dos.close(&mut heap, w));
        assert_eq!(
            done(dos.pipe_read(r, 8)),
            Ok(Vec::new()),
            "writer closed and nothing left: EOF"
        );
    }

    #[test]
    fn buffered_data_is_still_delivered_after_the_writer_closes() {
        let (mut dos, w, r) = fixture("PIPE:late");
        let mut heap = GuestHeap::new(0x1_0000, 0x1_8000);
        done(dos.pipe_write(1, w, b"tail")).unwrap();
        dos.close(&mut heap, w);
        assert_eq!(done(dos.pipe_read(r, 8)), Ok(b"tail".to_vec()));
        assert_eq!(done(dos.pipe_read(r, 8)), Ok(Vec::new()));
    }

    #[test]
    fn a_reader_that_opens_first_waits_for_a_writer_instead_of_seeing_eof() {
        let mut mem = FlatMemory::new(0x2_0000);
        let mut heap = GuestHeap::new(0x1_0000, 0x1_8000);
        let mut dos = DosState::new(None);
        let r = dos
            .open(&mut heap, &mut mem, "PIPE:first", MODE_OLDFILE)
            .map(crate::guestmem::addr_from_bptr)
            .unwrap();
        assert!(blocks(dos.pipe_read(r, 1)), "no writer has been opened yet");
        let w = dos
            .open(&mut heap, &mut mem, "PIPE:FIRST", MODE_NEWFILE)
            .map(crate::guestmem::addr_from_bptr)
            .unwrap();
        done(dos.pipe_write(1, w, b"!")).unwrap();
        assert_eq!(done(dos.pipe_read(r, 1)), Ok(b"!".to_vec()));
    }

    #[test]
    fn a_write_bigger_than_the_buffer_goes_through_as_a_reader_drains_it() {
        let (mut dos, w, r) = fixture("PIPE:big");
        let data: Vec<u8> = (0..BUFFER_SIZE + 1000).map(|i| (i % 251) as u8).collect();

        // The first attempt fills the buffer and has to wait for the rest.
        assert!(blocks(dos.pipe_write(7, w, &data)));
        assert_eq!(dos.pipe_write_progress[&(7, w)], BUFFER_SIZE);
        let mut got = done(dos.pipe_read(r, BUFFER_SIZE)).unwrap();
        assert_eq!(got.len(), BUFFER_SIZE);

        // The re-entered call (same data, same task) carries on from
        // where it stopped instead of writing the front again.
        assert_eq!(done(dos.pipe_write(7, w, &data)), Ok(data.len()));
        assert!(dos.pipe_write_progress.is_empty());
        got.extend(done(dos.pipe_read(r, BUFFER_SIZE)).unwrap());
        assert_eq!(got, data);
    }

    #[test]
    fn writing_with_every_reader_gone_fails_instead_of_waiting_forever() {
        let (mut dos, w, r) = fixture("PIPE:gone");
        let mut heap = GuestHeap::new(0x1_0000, 0x1_8000);
        dos.close(&mut heap, r);
        assert_eq!(done(dos.pipe_write(1, w, b"x")), Err(ERROR_WRITE_PROTECTED));
    }

    #[test]
    fn writing_before_any_reader_opens_just_buffers() {
        let mut mem = FlatMemory::new(0x2_0000);
        let mut heap = GuestHeap::new(0x1_0000, 0x1_8000);
        let mut dos = DosState::new(None);
        let w = dos
            .open(&mut heap, &mut mem, "PIPE:early", MODE_NEWFILE)
            .map(crate::guestmem::addr_from_bptr)
            .unwrap();
        assert_eq!(done(dos.pipe_write(1, w, b"abc")), Ok(3));
        let r = dos
            .open(&mut heap, &mut mem, "PIPE:early", MODE_OLDFILE)
            .map(crate::guestmem::addr_from_bptr)
            .unwrap();
        assert_eq!(done(dos.pipe_read(r, 8)), Ok(b"abc".to_vec()));
    }

    #[test]
    fn a_channel_is_gone_once_both_ends_close_and_its_name_starts_fresh() {
        let (mut dos, w, r) = fixture("PIPE:reuse");
        let mut heap = GuestHeap::new(0x1_0000, 0x1_8000);
        let mut mem = FlatMemory::new(0x2_0000);
        done(dos.pipe_write(1, w, b"stale")).unwrap();
        let first_id = dos.pipe_id(w).unwrap();
        dos.close(&mut heap, w);
        dos.close(&mut heap, r);
        assert!(dos.pipes.is_empty());

        let r2 = dos
            .open(&mut heap, &mut mem, "PIPE:reuse", MODE_OLDFILE)
            .map(crate::guestmem::addr_from_bptr)
            .unwrap();
        assert_ne!(dos.pipe_id(r2).unwrap(), first_id, "a new channel");
        assert!(
            blocks(dos.pipe_read(r2, 8)),
            "no stale data, and no stale EOF: it waits for a writer"
        );
    }

    #[test]
    fn the_wrong_end_is_refused_and_readwrite_is_not_supported() {
        let (mut dos, w, r) = fixture("PIPE:ends");
        assert_eq!(done(dos.pipe_read(w, 1)), Err(ERROR_ACTION_NOT_KNOWN));
        assert_eq!(
            done(dos.pipe_write(1, r, b"x")),
            Err(ERROR_ACTION_NOT_KNOWN)
        );
        let mut mem = FlatMemory::new(0x2_0000);
        let mut heap = GuestHeap::new(0x1_0000, 0x1_8000);
        assert_eq!(
            dos.open(
                &mut heap,
                &mut mem,
                "PIPE:ends",
                crate::dosfile::MODE_READWRITE
            ),
            Err(ERROR_ACTION_NOT_KNOWN)
        );
    }

    #[test]
    fn callers_that_cannot_block_get_an_error_not_a_hang() {
        let (mut dos, w, r) = fixture("PIPE:nb");
        assert_eq!(
            dos.read(r, 4),
            Err(ERROR_OBJECT_IN_USE),
            "empty, writer open"
        );
        let big = vec![0u8; BUFFER_SIZE + 1];
        assert_eq!(dos.write(w, &big), Err(ERROR_OBJECT_IN_USE));
        assert!(
            dos.pipe_write_progress.is_empty(),
            "a refused write must not leave a half-written record behind"
        );
        assert_eq!(dos.write(w, b"ok"), Ok(2));
        assert_eq!(dos.read(r, 4), Ok(b"ok".to_vec()));
    }

    // --- end to end: a blocked Read is woken by a Write from another task ---

    /// `move.l #imm32,An`.
    fn move_imm_to_a(n: u16) -> u16 {
        0x207C | (n << 9)
    }
    /// `move.l #imm32,Dn`.
    fn move_imm_to_d(n: u16) -> u16 {
        0x203C | (n << 9)
    }
    /// `jsr <disp16>(a6)`.
    fn jsr_disp16_a6(disp: i32) -> [u16; 2] {
        [0x4EAE, disp as u16]
    }
    fn set_a6(words: &mut Vec<u16>, base: u32) {
        words.extend_from_slice(&[move_imm_to_a(6), (base >> 16) as u16, base as u16]);
    }
    /// `move.l #imm32,Dn` with a placeholder operand; returns the index
    /// of the instruction for [`patch`].
    fn load_d(words: &mut Vec<u16>, n: u16) -> usize {
        words.extend_from_slice(&[move_imm_to_d(n), 0, 0]);
        words.len() - 3
    }
    fn load_a(words: &mut Vec<u16>, n: u16) -> usize {
        words.extend_from_slice(&[move_imm_to_a(n), 0, 0]);
        words.len() - 3
    }
    fn patch(words: &mut [u16], at: usize, value: u32) {
        words[at + 1] = (value >> 16) as u16;
        words[at + 2] = value as u16;
    }
    /// `move.l <abs32>,Dn`.
    fn load_d_from(words: &mut Vec<u16>, n: u16) -> usize {
        words.extend_from_slice(&[0x2039 | (n << 9), 0, 0]);
        words.len() - 3
    }
    /// `move.l D0,<abs32>`.
    fn store_d0_to(words: &mut Vec<u16>) -> usize {
        words.extend_from_slice(&[0x23C0, 0, 0]);
        words.len() - 3
    }

    #[test]
    fn a_read_on_an_empty_pipe_blocks_until_another_task_writes() {
        // Parent opens both ends of PIPE:t, spawns a child that Reads
        // the read end, and dips below its priority so the child runs
        // first: it finds the pipe empty and must block, handing the
        // CPU back. The parent then Writes "AB" and Waits; the Write
        // has to wake the child, whose Read then returns 2 bytes.
        const NP_ENTRY: u32 = 0x8000_0000 + 1000 + 3;
        const MODE_NEWFILE: u32 = 1006;
        const MODE_OLDFILE: u32 = 1005;
        const JOIN: u32 = 0x0001_0000;

        let mut words = Vec::new();
        let mut patches: Vec<(usize, &str)> = Vec::new();
        let mut parent_task_patches = Vec::new();

        // --- parent ---
        set_a6(&mut words, DOS_LIBRARY_BASE);
        patches.push((load_d(&mut words, 1), "name"));
        words.extend_from_slice(&[move_imm_to_d(2), 0, MODE_NEWFILE as u16]);
        words.extend_from_slice(&jsr_disp16_a6(-30)); // Open(NEWFILE)
        patches.push((store_d0_to(&mut words), "w_slot"));
        patches.push((load_d(&mut words, 1), "name"));
        words.extend_from_slice(&[move_imm_to_d(2), 0, MODE_OLDFILE as u16]);
        words.extend_from_slice(&jsr_disp16_a6(-30)); // Open(OLDFILE)
        patches.push((store_d0_to(&mut words), "r_slot"));
        patches.push((load_d(&mut words, 1), "tags"));
        words.extend_from_slice(&jsr_disp16_a6(-498)); // CreateNewProc

        set_a6(&mut words, EXEC_LIBRARY_BASE);
        parent_task_patches.push(load_a(&mut words, 1));
        words.extend_from_slice(&[move_imm_to_d(0), 0xFFFF, 0xFFFF]);
        words.extend_from_slice(&jsr_disp16_a6(-300)); // SetTaskPri(self, -1): child runs
        parent_task_patches.push(load_a(&mut words, 1));
        words.extend_from_slice(&[move_imm_to_d(0), 0, 0]);
        words.extend_from_slice(&jsr_disp16_a6(-300)); // SetTaskPri(self, 0)

        set_a6(&mut words, DOS_LIBRARY_BASE);
        patches.push((load_d_from(&mut words, 1), "w_slot"));
        patches.push((load_d(&mut words, 2), "data"));
        words.extend_from_slice(&[move_imm_to_d(3), 0, 2]);
        words.extend_from_slice(&jsr_disp16_a6(-48)); // Write(w, "AB", 2): wakes the child
        set_a6(&mut words, EXEC_LIBRARY_BASE);
        words.extend_from_slice(&[move_imm_to_d(0), (JOIN >> 16) as u16, JOIN as u16]);
        words.extend_from_slice(&jsr_disp16_a6(-318)); // Wait(JOIN): the child runs
        patches.push((load_d_from(&mut words, 0), "count"));
        words.push(0x4E75); // rts -> exit, code = bytes the child read

        // --- child: Read(r, buf, 2) [blocks]; record the count; Signal ---
        let child_entry = TRAP_TABLE_END + (words.len() as u32) * 2;
        set_a6(&mut words, DOS_LIBRARY_BASE);
        patches.push((load_d_from(&mut words, 1), "r_slot"));
        patches.push((load_d(&mut words, 2), "buf"));
        words.extend_from_slice(&[move_imm_to_d(3), 0, 2]);
        words.extend_from_slice(&jsr_disp16_a6(-42)); // Read: pipe empty, blocks
        patches.push((store_d0_to(&mut words), "count"));
        set_a6(&mut words, EXEC_LIBRARY_BASE);
        parent_task_patches.push(load_a(&mut words, 1));
        words.extend_from_slice(&[move_imm_to_d(0), (JOIN >> 16) as u16, JOIN as u16]);
        words.extend_from_slice(&jsr_disp16_a6(-324)); // Signal(parent, JOIN)
        words.push(0x4E75);

        let entry = TRAP_TABLE_END;
        let code_end = entry + (words.len() as u32) * 2;
        let name = code_end + 16;
        let data = name + 16;
        let buf = data + 16;
        let count = buf + 16;
        let w_slot = count + 16;
        let r_slot = w_slot + 16;
        let tags = r_slot + 16;
        let load_end = tags + 32;

        for (at, what) in patches {
            let value = match what {
                "name" => name,
                "data" => data,
                "buf" => buf,
                "count" => count,
                "w_slot" => w_slot,
                "r_slot" => r_slot,
                "tags" => tags,
                _ => unreachable!(),
            };
            patch(&mut words, at, value);
        }

        let mut mem = FlatMemory::new(0x2_0000);
        for (i, &w) in words.iter().enumerate() {
            mem.write_u16(entry + (i as u32) * 2, w);
        }
        crate::guestmem::write_c_string(&mut mem, name, b"PIPE:t");
        mem.write_u8(data, b'A');
        mem.write_u8(data + 1, b'B');
        mem.write_u32(count, 0xFFFF);
        mem.write_u32(tags, NP_ENTRY);
        mem.write_u32(tags + 4, child_entry);
        mem.write_u32(tags + 8, 0);
        mem.write_u32(tags + 12, 0);

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
        let parent_task = rt.current_task();
        for at in parent_task_patches {
            let addr = entry + (at as u32) * 2;
            rt.memory_mut().write_u32(addr + 2, parent_task);
        }

        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, 2,
            "the child's Read must have blocked on the empty pipe and been woken by the \
             parent's Write, returning both bytes"
        );
        let mem = rt.memory();
        assert_eq!(mem.read_u8(buf), b'A');
        assert_eq!(mem.read_u8(buf + 1), b'B');
    }
}
