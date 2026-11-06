//! Process ownership shared by observations and proxies. PID is the kernel
//! thread-group ID; the run-local instance separates successive uses of a PID.
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Identity {
    pub pid: i32,
    pub process_instance_id: u64,
}

pub fn fields(identity: Option<Identity>, data: &mut Value) {
    data["pid"] = json!(identity.map(|p| p.pid));
    data["process_instance_id"] = json!(identity.map(|p| p.process_instance_id));
}
