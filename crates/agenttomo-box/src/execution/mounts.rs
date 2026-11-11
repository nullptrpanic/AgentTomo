//! Mount primitives shared by task root and certificate setup.
use nix::{errno::Errno, libc};
use std::{
    ffi::CString,
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::Path,
};

pub fn mount(
    source: &str,
    target: &Path,
    kind: &str,
    flags: libc::c_ulong,
    options: &str,
) -> io::Result<()> {
    mount_bytes(source.as_bytes(), target, kind, flags, options)
}

fn mount_bytes(
    source: &[u8],
    target: &Path,
    kind: &str,
    flags: libc::c_ulong,
    options: &str,
) -> io::Result<()> {
    let source = CString::new(source)?;
    let path = CString::new(target.as_os_str().as_encoded_bytes())?;
    let kind = CString::new(kind)?;
    let options = CString::new(options)?;
    Errno::result(unsafe {
        libc::mount(
            source.as_ptr(),
            path.as_ptr(),
            kind.as_ptr(),
            flags,
            options.as_ptr().cast(),
        )
    })
    .map(|_| ())
    .map_err(|e| io::Error::other(format!("mount {}: {e}", target.display())))
}

pub fn private_namespace() -> io::Result<()> {
    Errno::result(unsafe { libc::unshare(libc::CLONE_NEWNS) })?;
    mount("", Path::new("/"), "", libc::MS_REC | libc::MS_PRIVATE, "")
}

pub fn detached_tree(source: &Path) -> io::Result<OwnedFd> {
    let source = CString::new(source.as_os_str().as_encoded_bytes())?;
    let fd = Errno::result(unsafe {
        libc::syscall(
            libc::SYS_open_tree,
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::OPEN_TREE_CLONE | libc::OPEN_TREE_CLOEXEC | libc::AT_RECURSIVE as u32,
        )
    })?;
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

pub fn attach_tree(tree: &OwnedFd, target: &Path) -> io::Result<()> {
    let target = CString::new(target.as_os_str().as_encoded_bytes())?;
    Errno::result(unsafe {
        libc::syscall(
            libc::SYS_move_mount,
            tree.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_FDCWD,
            target.as_ptr(),
            libc::MOVE_MOUNT_F_EMPTY_PATH,
        )
    })?;
    Ok(())
}
