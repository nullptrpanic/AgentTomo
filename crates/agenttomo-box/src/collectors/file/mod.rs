//! File-open observation only; no redirection and no dependency on networking.

use crate::syscall::{Call, memory, result_fields};
use nix::libc;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Instant;

pub const SYSCALLS: &[(i64, &str)] = &[
    #[cfg(target_arch = "x86_64")]
    (libc::SYS_open, "open"),
    #[cfg(target_arch = "x86_64")]
    (libc::SYS_creat, "creat"),
    (libc::SYS_openat, "openat"),
    (libc::SYS_openat2, "openat2"),
];

pub struct Attempt {
    pub data: Value,
    started: Instant,
}

pub fn enter(call: &Call) -> Option<Attempt> {
    let name = SYSCALLS
        .iter()
        .find(|(number, _)| *number == call.number)?
        .1;
    let at = matches!(name, "openat" | "openat2");
    let dirfd = if at {
        call.args[0] as i32
    } else {
        libc::AT_FDCWD
    };
    let path_pointer = call.args[usize::from(at)];
    let mut data =
        json!({"tid":call.tid.as_raw(),"task_id":call.task_id,"syscall":name,"dirfd":dirfd});
    match memory::c_string(call.tid, path_pointer, 4096) {
        Ok((bytes, truncated)) => {
            data["path"] = String::from_utf8_lossy(&bytes).into_owned().into();
            data["path_bytes"] = json!(bytes);
            data["path_capture"] = if truncated { "truncated" } else { "complete" }.into();
        }
        Err(error) => {
            data["path_capture"] = "unreadable".into();
            data["capture_error"] = error.to_string().into();
        }
    }
    if !data["path"]
        .as_str()
        .is_some_and(|path| path.starts_with('/'))
    {
        let link = if dirfd == libc::AT_FDCWD {
            format!("/proc/{}/cwd", call.tid)
        } else {
            format!("/proc/{}/fd/{dirfd}", call.tid)
        };
        if let Ok(path) = std::fs::read_link(link) {
            data["base_path"] = path.to_string_lossy().into_owned().into();
        }
    }
    if name == "openat2" {
        data["how_size"] = call.args[3].into();
        if call.args[3] >= 24 {
            match memory::read(call.tid, call.args[2], 24) {
                Ok(bytes) => {
                    for (index, field) in ["flags", "mode", "resolve"].iter().enumerate() {
                        data[*field] =
                            u64::from_ne_bytes(bytes[index * 8..index * 8 + 8].try_into().unwrap())
                                .into();
                    }
                }
                Err(error) => {
                    data["how_capture_error"] = error.to_string().into();
                }
            }
        }
    } else if name == "creat" {
        data["flags"] = (libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC).into();
        data["mode"] = call.args[1].into();
    } else {
        let flags = call.args[1 + usize::from(at)] as i32;
        data["flags"] = flags.into();
        if flags & libc::O_CREAT != 0 || flags & libc::O_TMPFILE == libc::O_TMPFILE {
            data["mode"] = (call.args[2 + usize::from(at)] as u32).into();
        }
    }
    Some(Attempt {
        data,
        started: Instant::now(),
    })
}

impl Attempt {
    pub fn finish(mut self, result: i64) -> Value {
        result_fields(&mut self.data, result);
        self.data["duration_ns"] = json!(self.started.elapsed().as_nanos());
        if result >= 0 {
            let tid = self.data["tid"].as_i64().unwrap();
            if let Ok(path) = std::fs::read_link(PathBuf::from(format!("/proc/{tid}/fd/{result}")))
            {
                self.data["resolved_path"] = path.to_string_lossy().into_owned().into();
            }
        }
        self.data
    }
}
