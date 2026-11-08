//! Memory-mapping observations; never change the target's mapping or contents.

use crate::syscall::{Call, result_fields};
use crate::timing::OperationTiming;
use nix::libc;
use serde_json::{Value, json};
use std::os::unix::ffi::OsStrExt;

pub const SYSCALLS: &[i64] = &[libc::SYS_mmap];

pub struct Attempt {
    data: Value,
    timing: OperationTiming,
}

pub fn enter(call: &Call) -> Option<Attempt> {
    if call.number != libc::SYS_mmap {
        return None;
    }
    let timing = OperationTiming::begin("syscall_entry");
    let anonymous = call.args[3] & libc::MAP_ANONYMOUS as u64 != 0;
    let fd = call.args[4] as i32;
    let mut data = json!({
        "syscall": "mmap", "address": format!("{:#x}", call.args[0]),
        "length": call.args[1], "prot": call.args[2], "flags": call.args[3],
        "fd": fd, "offset": call.args[5] as i64, "anonymous": anonymous,
    });
    crate::process::fields(call.process, &mut data);
    if !anonymous {
        // Capture before resuming: the FD may close immediately after mmap.
        // This path is a best-effort snapshot, not an immutable file identity.
        match std::fs::read_link(format!("/proc/{}/fd/{fd}", call.tid)) {
            Ok(path) => {
                data["resolved_path"] = path.to_string_lossy().into_owned().into();
                data["path_bytes"] = json!(path.as_os_str().as_bytes());
                data["path_capture"] = "complete".into();
            }
            Err(error) => {
                data["path_capture"] = "unreadable".into();
                data["capture_error"] = error.to_string().into();
            }
        }
    }
    Some(Attempt { data, timing })
}

impl Attempt {
    pub fn finish(mut self, result: i64) -> Value {
        result_fields(&mut self.data, result);
        self.data["mapped_address"] = if result >= 0 {
            format!("{result:#x}").into()
        } else {
            Value::Null
        };
        self.timing.complete(&mut self.data);
        self.data
    }
}
