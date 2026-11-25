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
    // An exited child may still have a creation stop pending on its creator.
    unparented_exits: HashMap<(Pid, u64), Identity>,
}
struct Tracee {
    identity: Identity,
    linked: bool,
    parent: Option<Identity>,
}
struct Group {
    identity: Identity,
    members: HashSet<Pid>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Child {
    pub identity: Identity,
    pub exited: bool,
}

impl Registry {
    pub fn observe(&mut self, handle: Pid) -> Option<Identity> {
        if let Some(tracee) = self.tracees.get(&handle) {
            return Some(tracee.identity);
        }
        let status = std::fs::read_to_string(format!("/proc/{handle}/status")).ok()?;
        let field = |name: &str| {
            status
                .lines()
                .find_map(|line| line.strip_prefix(name)?.trim().parse::<i32>().ok())
        };
        let pid = field("Tgid:")?;
        if pid <= 0 {
            return None;
        }
        let identity = self.register(handle, pid);
        let parent = if handle.as_raw() == pid {
            field("PPid:").and_then(|pid| self.groups.get(&pid).map(|group| group.identity))
        } else {
            Some(identity)
        };
        self.tracees.get_mut(&handle)?.parent = parent;
        Some(identity)
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
            parent: None,
        });
        identity
    }

    pub fn child(&mut self, handle: Pid, creator: Option<Identity>) -> Option<Child> {
        // A child can finish before waitpid reports the parent's creation stop.
        // CLONE_PARENT gives the child the creator's own parent.
        let shared_parent = creator.and_then(|p| self.tracees.get(&Pid::from_raw(p.pid))?.parent);
        for owner in [creator, shared_parent].into_iter().flatten() {
            if let Some(identity) = self
                .unparented_exits
                .remove(&(handle, owner.process_instance_id))
            {
                return Some(Child {
                    identity,
                    exited: true,
                });
            }
        }
        let identity = self.observe(handle)?;
        self.tracees.get_mut(&handle).unwrap().linked = true;
        Some(Child {
            identity,
            exited: false,
        })
    }

    pub fn is_live_pid(&self, pid: i32) -> bool {
        self.groups.contains_key(&pid)
    }

    pub fn exec(&mut self, handle: Pid, former: Pid) -> Identity {
        if let Some(mut tracee) = self.tracees.remove(&former) {
            let identity = tracee.identity;
            if former != handle {
                if let Some(leader) = self.tracees.remove(&handle) {
                    tracee.parent = leader.parent;
                } else if tracee.parent == Some(identity) {
                    tracee.parent = None;
                }
            }
            // exec destroys all other threads in the group, so no pending
            // pre-exec creation stop from this process can still arrive.
            self.unparented_exits
                .retain(|(_, owner), _| *owner != identity.process_instance_id);
            let group = self.groups.get_mut(&identity.pid).unwrap();
            group.members.retain(|member| *member == handle);
            group.members.insert(handle);
            self.tracees
                .retain(|tid, entry| *tid == handle || entry.identity != identity);
            self.tracees.insert(handle, tracee);
            identity
        } else {
            // A successful exec is always reported at the process leader PID.
            self.register(handle, handle.as_raw())
        }
    }

    pub fn exit(&mut self, handle: Pid) -> Option<Identity> {
        let tracee = self.tracees.remove(&handle)?;
        if !tracee.linked
            && let Some(parent) = tracee.parent
            && self
                .groups
                .get(&parent.pid)
                .is_some_and(|group| group.identity == parent)
        {
            self.unparented_exits
                .insert((handle, parent.process_instance_id), tracee.identity);
        }
        let group = self.groups.get_mut(&tracee.identity.pid).unwrap();
        group.members.remove(&handle);
        if !group.members.is_empty() {
            return None;
        }
        self.groups.remove(&tracee.identity.pid);
        self.unparented_exits
            .retain(|(_, owner), _| *owner != tracee.identity.process_instance_id);
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
        registry.child(leader, None);
        registry.child(worker, Some(identity));
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
        registry.child(leader, None);
        registry.child(worker, Some(identity));
        assert_eq!(registry.exit(leader), None);
        assert_eq!(registry.exec(leader, worker), identity);
        assert_eq!(registry.exit(leader), Some(identity));
    }

    #[test]
    fn late_creation_retains_the_exited_child_process_identity() {
        let mut registry = Registry::default();
        let parent = Pid::from_raw(10);
        let creator = registry.register(parent, 10);
        registry.child(parent, None);
        let child = Pid::from_raw(11);
        let identity = registry.register(child, 11);
        registry.tracees.get_mut(&child).unwrap().parent = Some(creator);
        assert_eq!(registry.exit(child), Some(identity));
        let reused = registry.register(child, 11);
        assert_ne!(reused, identity);
        assert_eq!(
            registry.child(child, Some(creator)),
            Some(Child {
                identity,
                exited: true
            })
        );
        assert_eq!(
            registry.child(child, Some(creator)),
            Some(Child {
                identity: reused,
                exited: false
            })
        );
    }

    #[test]
    fn orphaned_and_pre_exec_exits_cannot_claim_a_reused_handle() {
        let mut registry = Registry::default();
        let parent = Pid::from_raw(10);
        let worker = Pid::from_raw(11);
        let child = Pid::from_raw(12);
        let creator = registry.register(parent, 10);
        registry.child(parent, None);
        registry.register(worker, 10);
        registry.child(worker, Some(creator));
        registry.register(child, 12);
        registry.tracees.get_mut(&child).unwrap().parent = Some(creator);
        registry.exit(child);
        registry.exec(parent, worker);
        let reused = registry.register(child, 12);
        assert_eq!(
            registry.child(child, Some(creator)),
            Some(Child {
                identity: reused,
                exited: false
            })
        );
        registry.exit(child);

        registry.register(child, 12);
        registry.tracees.get_mut(&child).unwrap().parent = Some(creator);
        registry.exit(parent);
        registry.exit(child);
        assert!(registry.unparented_exits.is_empty());
    }

    #[test]
    fn main_and_nonleader_exec_clean_unparented_exits_and_preserve_parent() {
        let mut registry = Registry::default();
        let grandparent = Pid::from_raw(5);
        let leader = Pid::from_raw(10);
        let worker = Pid::from_raw(11);
        let child = Pid::from_raw(12);
        let gp = registry.register(grandparent, 5);
        registry.child(grandparent, None);
        let creator = registry.register(leader, 10);
        registry.tracees.get_mut(&leader).unwrap().parent = Some(gp);
        registry.child(leader, Some(gp));
        registry.register(child, 12);
        registry.tracees.get_mut(&child).unwrap().parent = Some(creator);
        registry.exit(child);
        // Main-thread exec must clear stale unparented exits too.
        assert_eq!(registry.exec(leader, leader), creator);
        assert!(registry.unparented_exits.is_empty());

        // Non-leader exec must keep the leader's parent rather than self-referencing.
        registry.register(worker, 10);
        registry.tracees.get_mut(&worker).unwrap().parent = Some(creator);
        assert_eq!(registry.exec(leader, worker), creator);
        assert_eq!(registry.tracees[&leader].parent, Some(gp));
    }
}
