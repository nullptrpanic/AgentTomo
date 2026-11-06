//! Pin the actual opened inode while the target is stopped. Content reads belong
//! to recording, never to the ptrace thread or a later browser pathname lookup.

use nix::{libc, unistd::Pid};
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io,
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
};

pub fn pin(pid: Pid, fd: i64, enabled: bool, data: &mut Value) -> Option<File> {
    data["preview"] = json!({"status":"unavailable", "source":"open_snapshot"});
    let reason = if fd < 0 {
        Some("open_failed")
    } else if !enabled {
        Some("disabled")
    } else if data["flags"].as_i64().is_none() {
        Some("unknown_flags")
    } else if data["flags"].as_i64().unwrap() & i64::from(libc::O_PATH) != 0 {
        Some("path_only")
    } else if data["flags"].as_i64().unwrap() & i64::from(libc::O_ACCMODE)
        == i64::from(libc::O_WRONLY)
    {
        Some("write_only")
    } else {
        None
    };
    if let Some(reason) = reason {
        data["preview"]["reason"] = reason.into();
        return None;
    }
    match open(pid, fd, &mut data["preview"]) {
        Ok(file) => Some(file),
        Err(error) => {
            if data["preview"]["reason"].is_null() {
                data["preview"]["reason"] = "read_handle_unavailable".into();
                data["preview"]["error"] = error.to_string().into();
            }
            None
        }
    }
}

fn open(pid: Pid, fd: i64, preview: &mut Value) -> io::Result<File> {
    // O_PATH can inspect a FIFO/device without opening it for I/O. Keep the pin
    // alive until the independent read handle has been opened through our FD.
    let pin = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
        .open(format!("/proc/{pid}/fd/{fd}"))?;
    let metadata = pin.metadata()?;
    if !metadata.is_file() {
        preview["reason"] = "not_regular".into();
        return Err(io::Error::other("not a regular file"));
    }
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    nix::errno::Errno::result(unsafe { libc::fstatfs(pin.as_raw_fd(), stat.as_mut_ptr()) })?;
    let magic = unsafe { stat.assume_init() }.f_type;
    // Local regular files only. In particular, do not read virtual proc/sys
    // files or invoke arbitrary FUSE/network filesystem read implementations.
    if !matches!(
        magic as u64,
        0xef53 | 0x58465342 | 0x9123683e | 0x794c7630 | 0x01021994 | 0x858458f6 | 0x73717368
    ) {
        preview["reason"] = "unsupported_filesystem".into();
        return Err(io::Error::other(
            "filesystem is not supported for snapshots",
        ));
    }
    preview["size_at_open"] = metadata.len().into();
    let path = format!("/proc/self/fd/{}", pin.as_raw_fd());
    let flags = libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOCTTY;
    // Avoid updating atime where permitted; failure does not grant extra rights.
    match OpenOptions::new()
        .read(true)
        .custom_flags(flags | libc::O_NOATIME)
        .open(&path)
    {
        Err(error) if error.raw_os_error() == Some(libc::EPERM) => {
            OpenOptions::new().read(true).custom_flags(flags).open(path)
        }
        result => result,
    }
}
