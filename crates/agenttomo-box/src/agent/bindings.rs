//! Bind a process once. The active-call fallback is explicitly inferred.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Scope {
    pub agent_instance_id: String,
    pub task_id: String,
}
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Binding {
    #[serde(flatten)]
    pub scope: Scope,
    pub agenttomo_toolcall_id: Option<String>,
    pub association: &'static str,
    pub external_execution_id: Option<String>,
}
#[derive(Default)]
struct State {
    processes: HashMap<u64, Binding>,
    active: HashMap<String, (String, String)>,
    calls: HashMap<(String, String), (String, Scope)>,
    results: std::collections::HashSet<(String, String)>,
    live: HashMap<u64, OwnedFd>,
    cancelled: std::collections::HashSet<(String, String)>,
}
#[derive(Clone, Default)]
pub struct Hub(Arc<Mutex<State>>);

impl Hub {
    pub fn track(&self, identity: crate::process::Identity) -> std::io::Result<()> {
        let mut state = self.0.lock().unwrap();
        let Some(binding) = state.processes.get(&identity.process_instance_id) else {
            return Ok(());
        };
        let cancelled = state.cancelled.contains(&(
            binding.scope.agent_instance_id.clone(),
            binding.scope.task_id.clone(),
        ));
        if state.live.contains_key(&identity.process_instance_id) {
            return Ok(());
        }
        let fd = unsafe { nix::libc::syscall(nix::libc::SYS_pidfd_open, identity.pid, 0) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        if cancelled {
            let _ = kill(&fd);
        }
        state.live.insert(identity.process_instance_id, fd);
        Ok(())
    }
    pub fn exited(&self, identity: crate::process::Identity) {
        self.0
            .lock()
            .unwrap()
            .live
            .remove(&identity.process_instance_id);
    }
    pub fn cancel(&self, scope: &Scope) -> std::io::Result<()> {
        let mut state = self.0.lock().unwrap();
        state
            .cancelled
            .insert((scope.agent_instance_id.clone(), scope.task_id.clone()));
        let mut failure = None;
        for (instance, fd) in &state.live {
            if state
                .processes
                .get(instance)
                .is_some_and(|binding| &binding.scope == scope)
                && let Err(error) = kill(fd)
            {
                failure = Some(error);
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
    pub fn exec(&self, instance: u64, env: &Value, runtime: bool) {
        let mut state = self.0.lock().unwrap();
        let inherited = state
            .processes
            .get(&instance)
            .map(|binding| binding.scope.clone());
        let scope = match (
            env["AGENTTOMO_AGENT_ID"].as_str(),
            env["AGENTTOMO_TASK_ID"].as_str(),
        ) {
            (Some(agent), Some(task)) => Scope {
                agent_instance_id: agent.to_owned(),
                task_id: task.to_owned(),
            },
            _ => match inherited {
                Some(scope) => scope,
                None => return,
            },
        };
        let external = env["CODEX_TOOL_CALL_ID"].as_str().map(str::to_owned);
        let explicit = env["AGENTTOMO_TOOLCALL_ID"].as_str().map(str::to_owned);
        // A known runtime helper can clear a tentative birth binding. Worker
        // bindings otherwise remain frozen while later execs add their aliases.
        if let Some(binding) = state.processes.get_mut(&instance)
            && binding.agenttomo_toolcall_id.is_some()
            && explicit.is_none()
            && !(runtime && binding.association == "inferred_process_birth")
        {
            if binding.association == "inferred_process_birth" {
                binding.association = "inferred_active_call";
            }
            if let Some(external) = external {
                binding.external_execution_id = Some(external);
            }
            return;
        }
        let active = state
            .active
            .get(&scope.agent_instance_id)
            .filter(|(active_task, _)| active_task == &scope.task_id)
            .map(|(_, call)| call.clone());
        let tool = if runtime {
            explicit.clone()
        } else {
            explicit.clone().or(active)
        };
        let inferred = tool.is_some();
        state.processes.insert(
            instance,
            Binding {
                scope,
                agenttomo_toolcall_id: tool,
                association: if explicit.is_some() {
                    "explicit_environment"
                } else if inferred {
                    "inferred_active_call"
                } else {
                    "agent_runtime"
                },
                external_execution_id: external,
            },
        );
    }
    pub fn fork(&self, parent: u64, child: u64) {
        let mut state = self.0.lock().unwrap();
        if let Some(mut binding) = state.processes.get(&parent).cloned() {
            if binding.agenttomo_toolcall_id.is_some() {
                binding.association = if matches!(
                    binding.association,
                    "explicit_environment" | "inherited_explicit"
                ) {
                    "inherited_explicit"
                } else {
                    "inherited_inferred"
                };
            } else if let Some((task, tool)) = state.active.get(&binding.scope.agent_instance_id)
                && task == &binding.scope.task_id
            {
                binding.agenttomo_toolcall_id = Some(tool.clone());
                binding.association = "inferred_process_birth";
            }
            state.processes.entry(child).or_insert(binding);
        }
    }
    pub fn binding(&self, instance: u64) -> Option<Binding> {
        self.0.lock().unwrap().processes.get(&instance).cloned()
    }
    pub fn fields(&self, data: &mut Value) {
        if let Some(binding) = data["process_instance_id"]
            .as_u64()
            .and_then(|id| self.binding(id))
        {
            data["agent"] = serde_json::to_value(binding).unwrap();
        }
    }
    pub fn call(&self, scope: &Scope, external: &str, activate: bool) -> (String, Scope) {
        let mut state = self.0.lock().unwrap();
        let result = state
            .calls
            .entry((scope.agent_instance_id.clone(), external.to_owned()))
            .or_insert_with(|| {
                (
                    format!("tool-{}", uuid::Uuid::new_v4().simple()),
                    scope.clone(),
                )
            })
            .clone();
        if activate {
            state.active.insert(
                scope.agent_instance_id.clone(),
                (scope.task_id.clone(), result.0.clone()),
            );
        }
        result
    }
    pub fn first_result(&self, scope: &Scope, external: &str) -> bool {
        self.0
            .lock()
            .unwrap()
            .results
            .insert((scope.agent_instance_id.clone(), external.to_owned()))
    }
    pub fn returned(&self, scope: &Scope, tool: &str) {
        let mut state = self.0.lock().unwrap();
        if state
            .active
            .get(&scope.agent_instance_id)
            .is_some_and(|(task, id)| task == &scope.task_id && id == tool)
        {
            state.active.remove(&scope.agent_instance_id);
        }
    }
}

fn kill(fd: &OwnedFd) -> std::io::Result<()> {
    let result = unsafe {
        nix::libc::syscall(
            nix::libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            nix::libc::SIGKILL,
            std::ptr::null::<nix::libc::siginfo_t>(),
            0,
        )
    };
    if result < 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(nix::libc::ESRCH) {
            return Err(error);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn runtime_helpers_stay_unassigned_and_workers_bind_without_environment_copying() {
        let hub = Hub::default();
        let scope = Scope {
            agent_instance_id: "a".into(),
            task_id: "t".into(),
        };
        let (tool, _) = hub.call(&scope, "call", true);
        hub.exec(
            1,
            &json!({"AGENTTOMO_AGENT_ID":"a","AGENTTOMO_TASK_ID":"t"}),
            true,
        );
        assert_eq!(hub.binding(1).unwrap().agenttomo_toolcall_id, None);
        hub.fork(1, 2);
        assert_eq!(
            hub.binding(2).unwrap().agenttomo_toolcall_id.as_deref(),
            Some(tool.as_str()),
            "a fork-only worker must bind before any exec"
        );
        hub.exec(2, &json!({}), false);
        assert_eq!(
            hub.binding(2).unwrap().agenttomo_toolcall_id.as_deref(),
            Some(tool.as_str())
        );
    }
    #[tokio::test]
    async fn cancellation_uses_pinned_processes_and_catches_later_children() {
        use std::os::unix::process::ExitStatusExt;
        let hub = Hub::default();
        let scope = Scope {
            agent_instance_id: "a".into(),
            task_id: "t".into(),
        };
        hub.exec(
            1,
            &json!({"AGENTTOMO_AGENT_ID":"a","AGENTTOMO_TASK_ID":"t"}),
            true,
        );
        for instance in [1, 2] {
            let mut child = tokio::process::Command::new("/bin/sleep")
                .arg("30")
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            if instance == 2 {
                hub.fork(1, 2);
            }
            let identity = crate::process::Identity {
                pid: child.id().unwrap() as i32,
                process_instance_id: instance,
            };
            hub.track(identity).unwrap();
            if instance == 1 {
                hub.cancel(&scope).unwrap();
            }
            let status = tokio::time::timeout(std::time::Duration::from_secs(2), child.wait())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(status.signal(), Some(nix::libc::SIGKILL));
            hub.exited(identity);
        }
    }
    #[test]
    fn a_later_exec_adds_the_external_alias_without_rebinding_the_process() {
        let hub = Hub::default();
        let scope = Scope {
            agent_instance_id: "a".into(),
            task_id: "t".into(),
        };
        let (first, _) = hub.call(&scope, "call1", true);
        let env = json!({"AGENTTOMO_AGENT_ID":"a","AGENTTOMO_TASK_ID":"t"});
        hub.exec(1, &env, false);
        hub.call(&scope, "call2", true);
        hub.exec(1, &json!({"AGENTTOMO_AGENT_ID":"a","AGENTTOMO_TASK_ID":"t","CODEX_TOOL_CALL_ID":"exec-real"}), false);
        let binding = hub.binding(1).unwrap();
        assert_eq!(
            binding.agenttomo_toolcall_id.as_deref(),
            Some(first.as_str())
        );
        assert_eq!(binding.external_execution_id.as_deref(), Some("exec-real"));
    }
    #[test]
    fn binding_is_frozen_across_exec_fork_and_later_calls() {
        let hub = Hub::default();
        let scope = Scope {
            agent_instance_id: "a".into(),
            task_id: "t".into(),
        };
        let (first, _) = hub.call(&scope, "model1", true);
        let env =
            json!({"AGENTTOMO_AGENT_ID":"a","AGENTTOMO_TASK_ID":"t","CODEX_TOOL_CALL_ID":"exec1"});
        hub.exec(1, &env, false);
        hub.call(&scope, "model2", true);
        hub.exec(1, &env, false);
        hub.fork(1, 2);
        assert_eq!(
            hub.binding(2).unwrap().agenttomo_toolcall_id.as_deref(),
            Some(first.as_str())
        );
        assert_eq!(hub.binding(2).unwrap().association, "inherited_inferred");
        hub.exec(3, &json!({"AGENTTOMO_AGENT_ID":"a","AGENTTOMO_TASK_ID":"other","CODEX_TOOL_CALL_ID":"exec2"}), false);
        assert_eq!(hub.binding(3).unwrap().agenttomo_toolcall_id, None);
    }
    #[test]
    fn aliases_are_scoped_and_replayed_results_are_deduplicated() {
        let hub = Hub::default();
        let scope = Scope {
            agent_instance_id: "a".into(),
            task_id: "t".into(),
        };
        let other = Scope {
            agent_instance_id: "b".into(),
            task_id: "t".into(),
        };
        assert_ne!(
            hub.call(&scope, "x", false).0,
            hub.call(&other, "x", false).0
        );
        assert!(hub.first_result(&scope, "x"));
        assert!(!hub.first_result(&scope, "x"));
    }
}
