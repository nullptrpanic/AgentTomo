//! Model-wire evidence and process correlation. Agent launch adapters are separate.
mod adapter;
mod bindings;
mod codex;
mod protocol;
mod responses;
pub mod store;
mod wire;
pub mod workspace;

pub use adapter::{Adapter, Installation, ObservationRules, discover, observation_rules};
pub use bindings::{Hub, Scope};
pub use protocol::{Event, Protocol};
pub use wire::Observer;

pub const MAX_CONTENT: usize = 32 * 1024 * 1024;

pub fn now() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

/// Read only correlation variables. Other environment values never enter logs.
pub fn environment(pid: nix::unistd::Pid) -> serde_json::Value {
    use std::io::Read;
    let mut data = serde_json::json!({});
    let Ok(file) = std::fs::File::open(format!("/proc/{pid}/environ")) else {
        return data;
    };
    let mut bytes = Vec::new();
    if file.take(4 * 1024 * 1024).read_to_end(&mut bytes).is_err() {
        return data;
    }
    for entry in bytes.split(|byte| *byte == 0) {
        let Some((name, value)) = entry
            .iter()
            .position(|byte| *byte == b'=')
            .map(|index| (&entry[..index], &entry[index + 1..]))
        else {
            continue;
        };
        if let (Ok(name), Ok(value)) = (std::str::from_utf8(name), std::str::from_utf8(value))
            && matches!(
                name,
                "AGENTTOMO_AGENT_ID" | "AGENTTOMO_TASK_ID" | "AGENTTOMO_TOOLCALL_ID"
            )
            && value.len() <= 256
        {
            data[name] = value.into();
        }
    }
    data
}

#[cfg(test)]
mod tests {
    #[test]
    fn runtime_rules_are_supplied_by_the_adapter_and_match_executable_names() {
        let hub = super::Hub::with_runtimes(vec!["example-agent".into(), "example-helper".into()]);
        let scope = super::Scope {
            agent_instance_id: "a".into(),
            task_id: "t".into(),
        };
        let env = serde_json::json!({"AGENTTOMO_AGENT_ID":"a","AGENTTOMO_TASK_ID":"t"});
        let (call, _) = hub.call(&scope, "call", true);
        hub.observe_exec(1, &env, &serde_json::json!(["/opt/example-agent", "run"]));
        assert_eq!(hub.binding(1).unwrap().association, "agent_runtime");
        hub.fork(1, 2);
        hub.observe_exec(2, &env, &serde_json::json!(["/opt/example-helper"]));
        assert!(hub.binding(2).unwrap().agenttomo_toolcall_id.is_none());
        hub.fork(2, 3);
        hub.observe_exec(
            3,
            &env,
            &serde_json::json!(["/usr/bin/curl", "example-agent"]),
        );
        assert_eq!(hub.binding(3).unwrap().agenttomo_toolcall_id, Some(call));
        // A different application's name is not implicitly a runtime.
        hub.observe_exec(4, &env, &serde_json::json!(["/usr/bin/codex"]));
        assert_ne!(hub.binding(4).unwrap().association, "agent_runtime");
    }

    #[tokio::test]
    async fn process_events_collect_only_agenttomo_correlation_variables() {
        use std::process::Stdio;
        use tokio::io::AsyncReadExt;

        let mut child = tokio::process::Command::new("/bin/sh")
            .args(["-c", "printf R; read -r value"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .env_clear()
            .envs([
                ("AGENTTOMO_AGENT_ID", "agent"),
                ("AGENTTOMO_TASK_ID", "task"),
                ("AGENTTOMO_TOOLCALL_ID", "tool"),
                ("CODEX_TOOL_CALL_ID", "unused-execution-id"),
                ("OPENAI_API_KEY", "unused-test-key"),
            ])
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        // Wait for exec to finish before reading the child's /proc environment.
        assert_eq!(
            child.stdout.as_mut().unwrap().read_u8().await.unwrap(),
            b'R'
        );
        let captured = super::environment(nix::unistd::Pid::from_raw(child.id().unwrap() as i32));
        child.kill().await.unwrap();
        assert_eq!(
            captured,
            serde_json::json!({
                "AGENTTOMO_AGENT_ID": "agent",
                "AGENTTOMO_TASK_ID": "task",
                "AGENTTOMO_TOOLCALL_ID": "tool",
            })
        );
    }
}
