//! Preserve each overlay root's directory metadata on its initially empty upper.
use nix::{errno::Errno, libc};
use std::{ffi::CString, fs, io, os::unix::fs::MetadataExt, path::Path};

pub fn copy_directory(source: &Path, target: &Path) -> io::Result<()> {
    let source_c = CString::new(source.as_os_str().as_encoded_bytes())?;
    let target_c = CString::new(target.as_os_str().as_encoded_bytes())?;
    let metadata = fs::metadata(source)?;
    Errno::result(unsafe { libc::chown(target_c.as_ptr(), metadata.uid(), metadata.gid()) })?;
    fs::set_permissions(target, metadata.permissions())?;
    // ACLs are xattrs too; silently dropping one would change access checks.
    let size =
        Errno::result(unsafe { libc::listxattr(source_c.as_ptr(), std::ptr::null_mut(), 0) })?
            as usize;
    let mut names = vec![0u8; size];
    let length = Errno::result(unsafe {
        libc::listxattr(source_c.as_ptr(), names.as_mut_ptr().cast(), size)
    })? as usize;
    let names = names
        .get(..length)
        .ok_or_else(|| io::Error::other("directory xattrs changed during root preparation"))?;
    for name in names.split(|b| *b == 0).filter(|s| !s.is_empty()) {
        let name = CString::new(name)?;
        let size = Errno::result(unsafe {
            libc::getxattr(source_c.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0)
        })? as usize;
        let mut value = vec![0u8; size];
        let length = Errno::result(unsafe {
            libc::getxattr(
                source_c.as_ptr(),
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                size,
            )
        })? as usize;
        let value = value
            .get(..length)
            .ok_or_else(|| io::Error::other("directory xattr changed during root preparation"))?;
        Errno::result(unsafe {
            libc::setxattr(
                target_c.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        })?;
    }
    let times = [
        libc::timespec {
            tv_sec: metadata.atime(),
            tv_nsec: metadata.atime_nsec(),
        },
        libc::timespec {
            tv_sec: metadata.mtime(),
            tv_nsec: metadata.mtime_nsec(),
        },
    ];
    Errno::result(unsafe {
        libc::utimensat(libc::AT_FDCWD, target_c.as_ptr(), times.as_ptr(), 0)
    })?;
    Ok(())
}
