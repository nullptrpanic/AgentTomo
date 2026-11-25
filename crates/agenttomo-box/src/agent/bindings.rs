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
}
#[derive(Default)]
struct State {
    foreground: Option<Scope>,
    launcher: Option<u64>,
    launch_children: std::collections::HashSet<u64>,
    chat_foreground: bool,
    conversation_runtime: Option<u64>,
    processes: HashMap<u64, Binding>,
    active: HashMap<String, (String, String)>,
    calls: HashMap<(String, String), (String, Scope)>,
    results: std::collections::HashSet<(String, String)>,
    live: HashMap<u64, OwnedFd>,
    cancelled: std::collections::HashSet<(String, String)>,
}
impl State {
    fn binding(&self, instance: u64) -> Option<Binding> {
        let mut binding = self.processes.get(&instance)?.clone();
        if binding.association == "conversation_runtime" {
            if self.conversation_runtime != Some(instance) || !self.chat_foreground {
                return None;
            }
            binding.scope = self
                .foreground
                .as_ref()
                .filter(|scope| scope.agent_instance_id == binding.scope.agent_instance_id)?
                .clone();
        }
        Some(binding)
    }
}
#[derive(Clone, Default)]
pub struct Hub {
    state: Arc<Mutex<State>>,
    runtimes: Arc<[String]>,
}

impl Hub {
    pub fn workspace_launcher(&self, instance: u64) {
        self.state.lock().unwrap().launcher = Some(instance);
    }
    pub fn begin_turn(&self, scope: &Scope, chat: bool) {
        let mut state = self.state.lock().unwrap();
        state.foreground = Some(scope.clone());
        state.chat_foreground = chat;
        if chat
            && let Some(id) = state.conversation_runtime
            && let Some(binding) = state.processes.get_mut(&id)
        {
            binding.scope = scope.clone();
        }
    }
    pub fn finish_turn(&self, scope: &Scope) {
        let mut state = self.state.lock().unwrap();
        if state.foreground.as_ref() == Some(scope) {
            state.foreground = None;
        }
    }
    pub fn with_runtimes(runtimes: Vec<String>) -> Self {
        Self {
            runtimes: runtimes.into(),
            ..Self::default()
        }
    }
    pub fn observe_exec(&self, instance: u64, env: &Value, tid: nix::unistd::Pid) {
        use std::os::unix::fs::MetadataExt;
        let executable = std::fs::metadata(format!("/proc/{tid}/exe"));
        let runtime = executable.is_ok_and(|exe| {
            self.runtimes.iter().any(|path| {
                std::fs::metadata(path)
                    .is_ok_and(|trusted| exe.dev() == trusted.dev() && exe.ino() == trusted.ino())
            })
        });
        self.exec(instance, env, runtime);
    }
    pub fn track(&self, identity: crate::process::Identity) -> std::io::Result<()> {
        let mut state = self.state.lock().unwrap();
        let Some(binding) = state.binding(identity.process_instance_id) else {
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
            let error = std::io::Error::last_os_error();
            if cancelled {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(identity.pid),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
            return if error.raw_os_error() == Some(nix::libc::ESRCH) {
                Ok(())
            } else {
                Err(error)
            };
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        if cancelled {
            let _ = kill(&fd);
        }
        state.live.insert(identity.process_instance_id, fd);
        Ok(())
    }
    pub fn exited(&self, identity: crate::process::Identity) {
        let mut state = self.state.lock().unwrap();
        let instance = identity.process_instance_id;
        state.live.remove(&instance);
        state.launch_children.remove(&instance);
        if state.conversation_runtime == Some(instance) {
            // Late events from a stopped process keep its last active turn.
            if let Some(mut binding) = state.processes.get(&instance).cloned() {
                binding.association = "agent_runtime";
                state.processes.insert(instance, binding);
            }
            state.conversation_runtime = None;
        }
    }
    pub fn cancel(&self, scope: &Scope) -> std::io::Result<()> {
        let mut state = self.state.lock().unwrap();
        state
            .cancelled
            .insert((scope.agent_instance_id.clone(), scope.task_id.clone()));
        let mut failure = None;
        for (instance, fd) in &state.live {
            if state
                .binding(*instance)
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
        let mut state = self.state.lock().unwrap();
        let inherited = state
            .processes
            .get(&instance)
            .map(|binding| binding.scope.clone());
        let launch = state.launch_children.remove(&instance);
        let scope = if let Some(scope) = inherited.as_ref() {
            scope.clone()
        } else {
            let (Some(agent), Some(task)) = (
                env["AGENTTOMO_AGENT_ID"].as_str(),
                env["AGENTTOMO_TASK_ID"].as_str(),
            ) else {
                return;
            };
            let scope = Scope {
                agent_instance_id: agent.into(),
                task_id: task.into(),
            };
            if state.launcher.is_some() && (!launch || state.foreground.as_ref() != Some(&scope)) {
                return;
            }
            scope
        };
        let explicit = env["AGENTTOMO_TOOLCALL_ID"]
            .as_str()
            .filter(|id| {
                state.launcher.is_none()
                    || state
                        .active
                        .get(&scope.agent_instance_id)
                        .is_some_and(|(task, tool)| task == &scope.task_id && tool == *id)
            })
            .map(str::to_owned);
        // A known runtime helper can clear a tentative birth binding. Worker
        // bindings otherwise remain frozen across later execs.
        if let Some(binding) = state.processes.get_mut(&instance)
            && binding.agenttomo_toolcall_id.is_some()
            && !(runtime && binding.association == "inferred_process_birth")
        {
            if binding.association == "inferred_process_birth" {
                binding.association = "inferred_active_call";
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
        let conversation = runtime
            && inherited.is_none()
            && state.chat_foreground
            && env["AGENTTOMO_CONVERSATION_ID"].as_str() == Some(scope.task_id.as_str())
            && state.foreground.as_ref() == Some(&scope);
        if conversation {
            state.conversation_runtime = Some(instance);
        }
        state.processes.insert(
            instance,
            Binding {
                scope,
                agenttomo_toolcall_id: tool,
                association: if conversation {
                    "conversation_runtime"
                } else if explicit.is_some() {
                    "explicit_environment"
                } else if inferred {
                    "inferred_active_call"
                } else {
                    "agent_runtime"
                },
            },
        );
    }
    pub fn fork(&self, parent: u64, child: u64) {
        let mut state = self.state.lock().unwrap();
        if parent == child {
            return;
        }
        if state.launcher == Some(parent) {
            state.launch_children.insert(child);
        }
        if let Some(mut binding) = state.binding(parent) {
            if binding.association == "conversation_runtime" {
                binding.association = "agent_runtime";
            }
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
        self.state.lock().unwrap().binding(instance)
    }
    pub fn fields(&self, data: &mut Value) {
        if data.get("agent").is_some() {
            return;
        }
        if let Some(binding) = data["process_instance_id"]
            .as_u64()
            .and_then(|id| self.binding(id))
        {
            data["agent"] = serde_json::to_value(binding).unwrap();
        }
    }
    pub fn call(&self, scope: &Scope, external: &str, activate: bool) -> (String, Scope) {
        let mut state = self.state.lock().unwrap();
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
        self.state
            .lock()
            .unwrap()
            .results
            .insert((scope.agent_instance_id.clone(), external.to_owned()))
    }
    pub fn returned(&self, scope: &Scope, tool: &str) {
        let mut state = self.state.lock().unwrap();
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
    fn conversation_socket_owner_tracks_each_turn_and_never_steals_terminal_traffic() {
        let hub = Hub::with_runtimes(vec!["/proc/self/exe".into()]);
        let first = Scope {
            agent_instance_id: "a".into(),
            task_id: "one".into(),
        };
        let second = Scope {
            agent_instance_id: "a".into(),
            task_id: "two".into(),
        };
        let env = json!({"AGENTTOMO_AGENT_ID":"a","AGENTTOMO_TASK_ID":"one","AGENTTOMO_CONVERSATION_ID":"one"});
        hub.begin_turn(&first, true);
        hub.observe_exec(11, &env, nix::unistd::getpid());
        let captured = hub.binding(11).unwrap().scope;
        hub.finish_turn(&first);
        assert!(hub.binding(11).is_none());
        hub.begin_turn(&second, true);
        hub.observe_exec(12, &json!({}), nix::unistd::getpid());
        assert!(hub.binding(12).is_none());
        assert_eq!(hub.binding(11).unwrap().scope, second);
        assert_eq!(captured, first);
        hub.exited(crate::process::Identity {
            pid: 42,
            process_instance_id: 11,
        });
        hub.finish_turn(&second);
        hub.begin_turn(&first, true);
        assert_eq!(hub.binding(11).unwrap().scope, second);
    }

    #[test]
    fn workspace_requires_a_managed_launch_and_preserves_inherited_turns() {
        let hub = Hub::default();
        let scope = Scope {
            agent_instance_id: "a".into(),
            task_id: "first".into(),
        };
        let next = Scope {
            task_id: "second".into(),
            ..scope.clone()
        };
        let env = json!({"AGENTTOMO_AGENT_ID":"a","AGENTTOMO_TASK_ID":"first","AGENTTOMO_CONVERSATION_ID":"first"});
        hub.workspace_launcher(1);
        hub.begin_turn(&scope, true);
        hub.exec(2, &env, true);
        assert!(hub.binding(2).is_none());
        hub.fork(1, 3);
        hub.exec(3, &env, true);
        hub.finish_turn(&scope);
        hub.begin_turn(&next, false);
        assert!(hub.binding(3).is_none());
        hub.finish_turn(&next);
        hub.begin_turn(&next, true);
        hub.fork(3, 4);
        hub.exec(4, &env, false);
        assert_eq!(hub.binding(4).unwrap().scope, next);
    }
    #[test]
    fn tracking_a_process_that_already_exited_keeps_its_historical_binding() {
        let mut child = std::process::Command::new("/bin/true").spawn().unwrap();
        let identity = crate::process::Identity {
            pid: child.id() as i32,
            process_instance_id: 123,
        };
        child.wait().unwrap();
        let hub = Hub::default();
        hub.exec(
            123,
            &json!({"AGENTTOMO_AGENT_ID":"fixture","AGENTTOMO_TASK_ID":"t"}),
            false,
        );
        hub.track(identity).unwrap();
        let state = hub.state.lock().unwrap();
        assert!(state.processes.contains_key(&123));
        assert!(!state.live.contains_key(&123));
    }
    #[test]
    fn runtime_helpers_keep_later_workers_on_their_current_calls() {
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
        hub.exec(2, &json!({}), true);
        assert_eq!(hub.binding(2).unwrap().agenttomo_toolcall_id, None);
        hub.fork(2, 3);
        assert_eq!(
            hub.binding(3).unwrap().agenttomo_toolcall_id.as_deref(),
            Some(tool.as_str()),
            "a fork-only worker must bind before any exec"
        );
        hub.exec(3, &json!({}), false);
        let (next, _) = hub.call(&scope, "next-call", true);
        hub.fork(2, 4);
        hub.exec(4, &json!({}), false);
        assert_eq!(
            hub.binding(3).unwrap().agenttomo_toolcall_id.as_deref(),
            Some(tool.as_str())
        );
        assert_eq!(
            hub.binding(4).unwrap().agenttomo_toolcall_id.as_deref(),
            Some(next.as_str()),
            "a persistent runtime helper must not pass its first call to later workers"
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
    fn binding_is_frozen_across_exec_fork_and_later_calls() {
        let hub = Hub::default();
        let scope = Scope {
            agent_instance_id: "a".into(),
            task_id: "t".into(),
        };
        let (first, _) = hub.call(&scope, "model1", true);
        let env = json!({"AGENTTOMO_AGENT_ID":"a","AGENTTOMO_TASK_ID":"t"});
        hub.exec(1, &env, false);
        hub.call(&scope, "model2", true);
        hub.exec(1, &env, false);
        hub.fork(1, 2);
        assert_eq!(
            hub.binding(2).unwrap().agenttomo_toolcall_id.as_deref(),
            Some(first.as_str())
        );
        assert_eq!(hub.binding(2).unwrap().association, "inherited_inferred");
        hub.exec(
            3,
            &json!({"AGENTTOMO_AGENT_ID":"a","AGENTTOMO_TASK_ID":"other"}),
            false,
        );
        assert_eq!(hub.binding(3).unwrap().agenttomo_toolcall_id, None);
    }
    #[test]
    fn model_call_ids_are_scoped_and_replayed_results_are_deduplicated() {
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
