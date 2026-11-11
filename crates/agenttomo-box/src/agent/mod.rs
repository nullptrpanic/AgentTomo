//! Model-wire evidence and process correlation. Agent launch adapters are separate.
mod bindings;
pub mod codex;
mod responses;
pub mod store;
mod wire;
pub mod workspace;

pub use bindings::{Hub, Scope};
pub use responses::Event;
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
                "AGENTTOMO_AGENT_ID"
                    | "AGENTTOMO_TASK_ID"
                    | "AGENTTOMO_TOOLCALL_ID"
                    | "CODEX_TOOL_CALL_ID"
            )
            && value.len() <= 256
        {
            data[name] = value.into();
        }
    }
    data
}
