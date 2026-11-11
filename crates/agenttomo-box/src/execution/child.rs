//! The gated helper execs user code only after the tracer is attached.

use std::ffi::OsStr;
use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;

pub fn exec(
    command: &OsStr,
    trust_overlay: Option<&std::path::Path>,
    root_session: bool,
    root_overlay: Option<&std::path::Path>,
    rootfs: &super::rootfs::Options,
) -> u8 {
    // fd 3 is a dedicated, unbuffered gate, separate from the target's streams.
    let mut control = unsafe { UnixStream::from_raw_fd(3) };
    let ca = if trust_overlay.is_some() {
        match super::trust::receive(&mut control) {
            Ok(ca) => ca,
            Err(error) => {
                let _ = write!(control, "Ecannot receive task CA: {error}");
                return 125;
            }
        }
    } else {
        Vec::new()
    };
    if let Err(error) = crate::network::namespace::prepare(&control, root_session, || {
        if rootfs.enabled {
            let directory = root_overlay
                .ok_or_else(|| std::io::Error::other("root overlay storage is missing"))?;
            let private_paths = if trust_overlay.is_some() {
                super::trust::PRIVATE_PATHS
                    .iter()
                    .map(std::path::Path::new)
                    .collect()
            } else {
                vec![]
            };
            super::rootfs::install(directory, rootfs, &private_paths)?;
        }
        if let Some(directory) = trust_overlay {
            if rootfs.enabled {
                super::trust::inject(std::path::Path::new("/etc/ssl/certs"), &ca)?;
            } else {
                super::trust::install(directory, &ca)?;
            }
        }
        Ok(())
    }) {
        let _ = write!(control, "E{error}");
        return 125;
    }
    if control.write_all(b"R").is_err() {
        return 125;
    }
    let mut gate = [0];
    if control.read_exact(&mut gate).is_err() || gate != *b"G" {
        return 125;
    }
    if let Err(error) = crate::syscall::install(&crate::collectors::watched_syscalls()) {
        let _ = write!(control, "Ecannot install syscall filter: {error}");
        return 125;
    }
    if control.write_all(b"F").is_err() {
        return 125;
    }
    drop(control);
    let error = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .exec();
    eprintln!("agenttomo-box: cannot execute shell: {error}");
    125
}
