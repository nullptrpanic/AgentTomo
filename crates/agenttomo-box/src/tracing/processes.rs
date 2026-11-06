//! Translate private ptrace thread handles into process lifetimes. Only process
//! identities cross into collectors; thread creation/exit stays inside here.
use crate::process::Identity;
use nix::unistd::Pid;
use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub struct Registry {
    next: u64,
    tracees: HashMap<Pid, Tracee>,
    groups: HashMap<i32, Group>,
    unparented_exits: HashMap<Pid, Identity>,
}
struct Tracee {
    identity: Identity,
    linked: bool,
}
struct Group {
    identity: Identity,
    members: HashSet<Pid>,
}

impl Registry {
    pub fn observe(&mut self, handle: Pid) -> Option<Identity> {
        if let Some(tracee) = self.tracees.get(&handle) {
            return Some(tracee.identity);
        }
        let pid = std::fs::read_to_string(format!("/proc/{handle}/status"))
            .ok()?
            .lines()
            .find_map(|line| line.strip_prefix("Tgid:")?.trim().parse::<i32>().ok())?;
        (pid > 0).then(|| self.register(handle, pid))
    }

    fn register(&mut self, handle: Pid, pid: i32) -> Identity {
        let group = self.groups.entry(pid).or_insert_with(|| {
            self.next += 1;
            Group {
                identity: Identity {
                    pid,
                    process_instance_id: self.next,
                },
                members: HashSet::new(),
            }
        });
        group.members.insert(handle);
        let identity = group.identity;
        self.tracees.entry(handle).or_insert(Tracee {
            identity,
            linked: false,
        });
        identity
    }

    pub fn child(&mut self, handle: Pid) -> Option<Identity> {
        // A child can finish before waitpid reports the parent's creation stop.
        if let Some(identity) = self.unparented_exits.remove(&handle) {
            return Some(identity);
        }
        let identity = self.observe(handle)?;
        self.tracees.get_mut(&handle).unwrap().linked = true;
        Some(identity)
    }

    pub fn exec(&mut self, handle: Pid, former: Pid) -> Identity {
        if let Some(tracee) = self.tracees.remove(&former) {
            if former != handle {
                self.tracees.remove(&handle);
            }
            let group = self.groups.get_mut(&tracee.identity.pid).unwrap();
            group.members.remove(&former);
            group.members.insert(handle);
            let identity = tracee.identity;
            self.tracees.insert(handle, tracee);
            identity
        } else {
            // A successful exec is always reported at the process leader PID.
            self.register(handle, handle.as_raw())
        }
    }

    pub fn exit(&mut self, handle: Pid) -> Option<Identity> {
        let tracee = self.tracees.remove(&handle)?;
        if !tracee.linked {
            self.unparented_exits.insert(handle, tracee.identity);
        }
        let group = self.groups.get_mut(&tracee.identity.pid).unwrap();
        group.members.remove(&handle);
        if !group.members.is_empty() {
            return None;
        }
        self.groups.remove(&tracee.identity.pid);
        Some(tracee.identity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threads_share_identity_until_final_exit_and_pid_reuse_gets_a_new_instance() {
        let mut registry = Registry::default();
        let leader = Pid::from_raw(10);
        let worker = Pid::from_raw(11);
        let identity = registry.register(leader, 10);
        assert_eq!(registry.register(worker, 10), identity);
        registry.child(leader);
        registry.child(worker);
        assert_eq!(registry.exit(leader), None);
        assert_eq!(registry.exit(worker), Some(identity));
        assert_ne!(registry.register(leader, 10), identity);
    }

    #[test]
    fn nonleader_exec_preserves_the_process_instance() {
        let mut registry = Registry::default();
        let leader = Pid::from_raw(10);
        let worker = Pid::from_raw(11);
        let identity = registry.register(leader, 10);
        registry.register(worker, 10);
        registry.child(leader);
        registry.child(worker);
        assert_eq!(registry.exit(leader), None);
        assert_eq!(registry.exec(leader, worker), identity);
        assert_eq!(registry.exit(leader), Some(identity));
    }

    #[test]
    fn late_creation_retains_the_exited_child_process_identity() {
        let mut registry = Registry::default();
        let child = Pid::from_raw(11);
        let identity = registry.register(child, 11);
        assert_eq!(registry.exit(child), Some(identity));
        assert_eq!(registry.child(child), Some(identity));
        assert_ne!(registry.register(child, 11), identity);
    }
}
