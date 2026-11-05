//! Session-local lifetime IDs keep delayed observations separate from TID reuse.

use nix::unistd::Pid;
use std::collections::HashMap;

#[derive(Default)]
pub struct Tasks {
    next: u64,
    ids: HashMap<Pid, Task>,
    unparented_exits: HashMap<Pid, u64>,
}

struct Task {
    id: u64,
    linked: bool,
}

impl Tasks {
    pub fn id(&mut self, tid: Pid) -> u64 {
        self.ids
            .entry(tid)
            .or_insert_with(|| {
                self.next += 1;
                Task {
                    id: self.next,
                    linked: false,
                }
            })
            .id
    }
    pub fn child(&mut self, tid: Pid) -> u64 {
        // waitpid may deliver a child's entire lifetime before the parent's
        // creation stop. Its creation record must still carry that same ID.
        if let Some(id) = self.unparented_exits.remove(&tid) {
            return id;
        }
        let id = self.id(tid);
        self.ids.get_mut(&tid).unwrap().linked = true;
        id
    }
    pub fn exec(&mut self, tid: Pid, former: Pid) -> u64 {
        if tid != former
            && let Some(task) = self.ids.remove(&former)
        {
            self.ids.insert(tid, task);
        }
        self.id(tid)
    }
    pub fn remove(&mut self, tid: Pid) {
        if let Some(task) = self.ids.remove(&tid)
            && !task.linked
        {
            self.unparented_exits.insert(tid, task.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_creation_links_the_child_that_already_exited() {
        let mut tasks = Tasks::default();
        let child = Pid::from_raw(11);
        let early_id = tasks.id(child);
        tasks.remove(child);
        assert_eq!(tasks.child(child), early_id);
        assert_ne!(
            tasks.child(child),
            early_id,
            "a later lifetime needs a new identity"
        );
    }

    #[test]
    fn exec_moves_thread_identity_without_merging_the_leader() {
        let mut tasks = Tasks::default();
        let leader = Pid::from_raw(10);
        let thread = Pid::from_raw(11);
        let leader_id = tasks.child(leader);
        let thread_id = tasks.child(thread);
        assert_eq!(tasks.exec(leader, thread), thread_id);
        assert_ne!(leader_id, thread_id);
        tasks.remove(leader);
        assert_ne!(tasks.child(leader), thread_id);
    }
}
