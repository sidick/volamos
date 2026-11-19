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
//! exited is dropped instead of parked.
//!
//! Not per-process on real AmigaOS either, and so not in
//! [`ProcessState`]: file handles, locks, seglists, pipes, volumes and
//! assigns -- system objects any process can reach. `RunCommand` and
//! `SystemTagList` run on the calling task and share its state, as on
//! real AmigaOS.

use crate::dosfile::DosState;

/// The fields of [`DosState`] that belong to one process. Grows one
/// field per step of #326.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ProcessState {
    /// `pr_Result2`: this process's `IoErr()`.
    pub(crate) io_err: i32,
}

impl DosState {
    /// Parks the running process's fields as `outgoing`'s (unless it has
    /// exited) and loads `incoming`'s. Called when the scheduler commits
    /// a task switch.
    pub(crate) fn switch_process(&mut self, outgoing: u32, outgoing_exited: bool, incoming: u32) {
        if outgoing == incoming {
            return;
        }
        let current = ProcessState {
            io_err: self.io_err,
        };
        if !outgoing_exited {
            self.parked_procs.insert(outgoing, current);
        }
        let next = self.parked_procs.remove(&incoming).unwrap_or_default();
        self.io_err = next.io_err;
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
