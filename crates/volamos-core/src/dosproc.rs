//! Per-process `dos.library` state (issue #326).
//!
//! [`DosState`] is one instance for the whole runtime, but on real
//! AmigaOS several of its fields belong to the *process* that made the
//! call: `IoErr()` is `pr_Result2`, the standard streams are `pr_CIS`/
//! `pr_COS`, the current directory is `pr_CurrentDir`, and so on. With
//! only one task they coincide; once `CreateNewProc` runs a second one,
//! a child's failed call would otherwise overwrite its parent's `IoErr()`.
//!
//! The fix is a context switch of those fields rather than a rewrite of
//! every handler that reads them (`io_err` alone has ~190 uses): the
//! running task's values live in [`DosState`] as before, and when the
//! scheduler commits a task switch ([`DosState::switch_process`]) the
//! outgoing task's values are parked in [`DosState::parked_procs`], keyed
//! by task address, and the incoming task's are loaded (a task that has
//! never run starts from [`ProcessState::default`]). A task that has
//! exited is dropped instead of parked. Exiting is the only way a task
//! leaves the scheduler (`RemTask` of another task is a no-op here), so
//! nothing else needs to clean the map.
//!
//! Not per-process on real AmigaOS either, and so not in
//! [`ProcessState`]: file handles, locks, seglists, pipes, volumes and
//! assigns -- system objects any process can reach. `RunCommand` and
//! `SystemTagList` run on the calling task and share its state, as on
//! real AmigaOS.

use std::collections::HashMap;

use crate::dosfile::DosState;
use crate::guestmem::{GuestHeap, addr_from_bptr, bptr_from_addr};
use crate::memory::AddressSpace;

/// The fields of [`DosState`] that belong to one process. Grows one
/// field per step of #326.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ProcessState {
    /// `pr_Result2`: this process's `IoErr()`.
    pub(crate) io_err: i32,
    /// `pr_CurrentDir`, as the Amiga path the VFS resolves relative
    /// names against. `None` (never run, or no VFS) leaves the VFS's as
    /// it is.
    pub(crate) cwd: Option<String>,
    /// The lock `CurrentDir` last installed, which `CurrentDir` hands
    /// back as the previous one.
    pub(crate) current_dir_lock: Option<u32>,
    /// What `PROGDIR:` resolves to (`pr_HomeDir`'s path); `None` leaves
    /// it unresolvable.
    pub(crate) progdir: Option<String>,
    /// `pr_LocalVars`: the local shell variables, and the guest
    /// `struct LocalVar` blocks `FindVar` has handed out for them (see
    /// `crate::dosvar`).
    pub(crate) local_vars: HashMap<String, Vec<u8>>,
    pub(crate) local_var_nodes: HashMap<String, [u32; 4]>,
    /// `pr_FileSystemTask`: `GetFileSysTask`/`SetFileSysTask`'s value.
    pub(crate) file_sys_task: u32,
    /// The buffered standard input (`ReadArgs`' and `FGetC`'s source on
    /// `Input()`) and how much of it has been read. On real AmigaOS the
    /// buffer is the `FileHandle`'s, so a process that did not read
    /// `Input()` cannot see or consume another's.
    pub(crate) input_buf: Vec<u8>,
    pub(crate) input_pos: usize,
}

impl DosState {
    /// The running process's fields, copied out of `self`. With
    /// [`Self::load_process`] this is the only place a field has to be
    /// listed, besides [`ProcessState`] itself.
    fn save_process(&mut self) -> ProcessState {
        ProcessState {
            io_err: self.io_err,
            cwd: self.vfs.as_ref().map(|vfs| vfs.cwd().to_string()),
            current_dir_lock: self.current_dir_lock,
            progdir: self
                .vfs
                .as_ref()
                .and_then(|vfs| vfs.assign_target("PROGDIR")),
            // Moved, not copied: `load_process` puts the next ones in.
            local_vars: std::mem::take(&mut self.local_vars),
            local_var_nodes: std::mem::take(&mut self.local_var_nodes),
            file_sys_task: self.current_file_sys_task,
            input_buf: std::mem::take(&mut self.input_buf),
            input_pos: self.input_pos,
        }
    }

    /// Makes `state` the running process's fields.
    fn load_process(&mut self, state: ProcessState) {
        self.io_err = state.io_err;
        if let (Some(vfs), Some(cwd)) = (self.vfs.as_mut(), state.cwd.as_deref()) {
            vfs.restore_cwd(cwd);
        }
        self.current_dir_lock = state.current_dir_lock;
        self.local_vars = state.local_vars;
        self.local_var_nodes = state.local_var_nodes;
        self.current_file_sys_task = state.file_sys_task;
        self.input_buf = state.input_buf;
        self.input_pos = state.input_pos;
        if let Some(vfs) = self.vfs.as_mut() {
            match state.progdir {
                Some(path) => vfs.set_assign("PROGDIR", vec![path]),
                None => vfs.remove_assign("PROGDIR"),
            }
        }
    }

    /// The state a new process starts with, parked until it first runs:
    /// the current directory is `np_current_dir`'s lock as given (`0`: none,
    /// i.e. the starting directory), or a `DupLock` of the caller's (real
    /// `CreateNewProc`'s default; none if the caller has no lock). Returns the `pr_CurrentDir` BPTR to
    /// store in the new process too.
    pub(crate) fn new_process_state(
        &mut self,
        heap: &mut GuestHeap,
        mem: &mut dyn AddressSpace,
        np_current_dir: Option<u32>,
        np_home_dir: Option<u32>,
        caller_home_dir: u32,
        copy_vars: bool,
    ) -> (ProcessState, u32, u32) {
        let caller_cwd = self.vfs.as_ref().map(|vfs| vfs.cwd().to_string());
        let (lock, cwd) = match np_current_dir.map(addr_from_bptr) {
            // Given: used as is.
            Some(addr) if addr != 0 => {
                let path = self.lock_amiga_path(addr).or(caller_cwd);
                (Some(addr), path)
            }
            // An explicit 0: no current directory, i.e. where the program
            // started.
            Some(_) => (None, self.initial_cwd.clone().or(caller_cwd)),
            // Not given: a `DupLock` of the caller's.
            None => {
                let dup = self
                    .current_dir_lock
                    .and_then(|addr| self.dup_lock(heap, mem, addr).ok())
                    .filter(|&addr| addr != 0);
                (dup, caller_cwd)
            }
        };
        // The home directory (`PROGDIR:`) the same way: as given (`0`:
        // none), or a `DupLock` of the caller's.
        let caller_progdir = self
            .vfs
            .as_ref()
            .and_then(|vfs| vfs.assign_target("PROGDIR"));
        let (home_lock, progdir) = match np_home_dir.map(addr_from_bptr) {
            Some(addr) if addr != 0 => (addr, self.lock_amiga_path(addr)),
            Some(_) => (0, None),
            None => {
                let dup = self
                    .dup_lock(heap, mem, addr_from_bptr(caller_home_dir))
                    .unwrap_or(0);
                // `PROGDIR:` and `pr_HomeDir` agree: no lock, no assign.
                (dup, if dup != 0 { caller_progdir } else { None })
            }
        };
        let state = ProcessState {
            cwd,
            current_dir_lock: lock,
            progdir,
            // `NP_CopyVars`: the caller's local variables, by value. The
            // `FindVar` structures are the caller's own and are not shared.
            local_vars: if copy_vars {
                self.local_vars.clone()
            } else {
                HashMap::new()
            },
            // `pr_FileSystemTask` is the caller's; the input buffer starts
            // empty (the child reads its own `Input()`).
            file_sys_task: self.current_file_sys_task,
            ..ProcessState::default()
        };
        (
            state,
            lock.map(bptr_from_addr).unwrap_or(0),
            if home_lock == 0 {
                0
            } else {
                bptr_from_addr(home_lock)
            },
        )
    }

    /// Parks the running process's fields as `outgoing`'s (unless it has
    /// exited) and loads `incoming`'s. Called when the scheduler commits
    /// a task switch.
    pub(crate) fn switch_process(&mut self, outgoing: u32, outgoing_exited: bool, incoming: u32) {
        if outgoing == incoming {
            return;
        }
        if !outgoing_exited {
            let current = self.save_process();
            self.parked_procs.insert(outgoing, current);
        }
        match self.parked_procs.remove(&incoming) {
            Some(next) => self.load_process(next),
            // A task nothing parked state for (`spawn_process` always
            // does, so this is a task made some other way): a clean
            // `IoErr()` and no local variables, but the directories and
            // `PROGDIR:` stay as they are rather than being wiped.
            None => {
                self.io_err = 0;
                self.local_vars.clear();
                self.local_var_nodes.clear();
                self.input_buf.clear();
                self.input_pos = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_process_keeps_its_own_io_err_across_switches_326() {
        let mut dos = DosState::new(None);
        dos.set_io_err(111); // task 1 running
        dos.switch_process(1, false, 2);
        assert_eq!(dos.io_err(), 0, "a task that never ran starts clean");
        dos.set_io_err(222);
        dos.switch_process(2, false, 1);
        assert_eq!(dos.io_err(), 111);
        dos.switch_process(1, false, 2);
        assert_eq!(dos.io_err(), 222);
    }

    #[test]
    fn a_task_with_no_parked_state_keeps_the_directories_326() {
        let mut dos = DosState::new(None);
        dos.set_io_err(5);
        dos.local_vars.insert("V".to_string(), b"x".to_vec());
        dos.switch_process(1, false, 2);
        assert_eq!(dos.io_err(), 0);
        assert!(dos.local_vars.is_empty());
        assert!(dos.parked_procs.contains_key(&1));
    }

    #[test]
    fn a_new_process_copies_its_callers_local_vars_but_not_their_nodes_326() {
        let mut dos = DosState::new(None);
        dos.local_vars.insert("V".to_string(), b"x".to_vec());
        dos.local_var_nodes.insert("V".to_string(), [1, 2, 3, 4]);
        let mut heap = GuestHeap::new(0x1000, 0x1000);
        let mut mem = crate::memory::FlatMemory::new(0x4000);
        let (copied, ..) = dos.new_process_state(&mut heap, &mut mem, None, None, 0, true);
        assert_eq!(
            copied.local_vars.get("V").map(Vec::as_slice),
            Some(&b"x"[..])
        );
        assert!(copied.local_var_nodes.is_empty());
        let (empty, ..) = dos.new_process_state(&mut heap, &mut mem, None, None, 0, false);
        assert!(empty.local_vars.is_empty());
    }

    #[test]
    fn each_process_keeps_its_own_buffered_input_326() {
        let mut dos = DosState::new(None);
        dos.input_buf = b"PARENT ARGS\n".to_vec();
        dos.input_pos = 7;
        dos.switch_process(1, false, 2);
        assert!(dos.input_buf.is_empty(), "a new process has read nothing");
        assert_eq!(dos.input_pos, 0);
        dos.input_buf = b"child\n".to_vec();
        dos.input_pos = 2;
        dos.switch_process(2, false, 1);
        assert_eq!(dos.input_buf, b"PARENT ARGS\n");
        assert_eq!(dos.input_pos, 7);
        dos.switch_process(1, false, 2);
        assert_eq!(
            (dos.input_buf.as_slice(), dos.input_pos),
            (&b"child\n"[..], 2)
        );
    }

    #[test]
    fn a_new_process_inherits_its_callers_file_system_task_326() {
        let mut dos = DosState::new(None);
        dos.current_file_sys_task = 0x1234;
        let mut heap = GuestHeap::new(0x1000, 0x1000);
        let mut mem = crate::memory::FlatMemory::new(0x4000);
        let (state, ..) = dos.new_process_state(&mut heap, &mut mem, None, None, 0, true);
        assert_eq!(state.file_sys_task, 0x1234);
        assert!(state.input_buf.is_empty());
    }

    #[test]
    fn an_exited_process_is_dropped_not_parked_326() {
        let mut dos = DosState::new(None);
        dos.set_io_err(5);
        dos.switch_process(1, false, 2);
        dos.set_io_err(9);
        dos.switch_process(2, true, 1);
        assert_eq!(dos.io_err(), 5);
        assert!(!dos.parked_procs.contains_key(&2));
    }
}
