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
}

impl DosState {
    /// The running process's fields, copied out of `self`. With
    /// [`Self::load_process`] this is the only place a field has to be
    /// listed, besides [`ProcessState`] itself.
    fn save_process(&self) -> ProcessState {
        ProcessState {
            io_err: self.io_err,
            cwd: self.vfs.as_ref().map(|vfs| vfs.cwd().to_string()),
            current_dir_lock: self.current_dir_lock,
        }
    }

    /// Makes `state` the running process's fields.
    fn load_process(&mut self, state: ProcessState) {
        self.io_err = state.io_err;
        if let (Some(vfs), Some(cwd)) = (self.vfs.as_mut(), state.cwd.as_deref()) {
            vfs.restore_cwd(cwd);
        }
        self.current_dir_lock = state.current_dir_lock;
    }

    /// The state a new process starts with, parked until it first runs:
    /// the current directory is `np_current_dir`'s lock as given, or a
    /// `DupLock` of the caller's (real `CreateNewProc`'s default; `0`
    /// when the caller has none). Returns the `pr_CurrentDir` BPTR to
    /// store in the new process too.
    pub(crate) fn new_process_state(
        &mut self,
        heap: &mut GuestHeap,
        mem: &mut dyn AddressSpace,
        np_current_dir: Option<u32>,
    ) -> (ProcessState, u32) {
        let caller_cwd = self.vfs.as_ref().map(|vfs| vfs.cwd().to_string());
        let (lock, cwd) = match np_current_dir.map(addr_from_bptr) {
            Some(addr) if addr != 0 => {
                let path = self.lock_amiga_path(addr).or(caller_cwd);
                (Some(addr), path)
            }
            _ => {
                let dup = self
                    .current_dir_lock
                    .and_then(|addr| self.dup_lock(heap, mem, addr).ok())
                    .filter(|&addr| addr != 0);
                (dup, caller_cwd)
            }
        };
        let state = ProcessState {
            cwd,
            current_dir_lock: lock,
            ..ProcessState::default()
        };
        (state, lock.map(bptr_from_addr).unwrap_or(0))
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
        let next = self.parked_procs.remove(&incoming).unwrap_or_default();
        self.load_process(next);
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
