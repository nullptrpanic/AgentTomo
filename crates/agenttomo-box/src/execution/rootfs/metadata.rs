//! Preserve each overlay root's directory metadata on its initially empty upper.
use nix::{errno::Errno, libc};
use std::{ffi::CString, fs, io, os::unix::fs::MetadataExt, path::Path};

pub fn copy_directory(source: &Path, target: &Path) -> io::Result<()> {
    let source_c = CString::new(source.as_os_str().as_encoded_bytes())?;
    let target_c = CString::new(target.as_os_str().as_encoded_bytes())?;
    let metadata = fs::metadata(source)?;
    let current = fs::metadata(target)?;
    if current.uid() != metadata.uid() || current.gid() != metadata.gid() {
        Errno::result(unsafe { libc::chown(target_c.as_ptr(), metadata.uid(), metadata.gid()) })
            .map_err(|e| {
                io::Error::other(format!(
                    "cannot preserve owner {}:{} of {}; check the user namespace ID mapping: {e}",
                    metadata.uid(),
                    metadata.gid(),
                    source.display()
                ))
            })?;
    }
    fs::set_permissions(target, metadata.permissions())?;
    // ACLs are xattrs too; silently dropping one would change access checks.
    let size =
        match Errno::result(unsafe { libc::listxattr(source_c.as_ptr(), std::ptr::null_mut(), 0) })
        {
            Ok(size) => size as usize,
            Err(Errno::EOPNOTSUPP) => 0,
            Err(error) => return Err(error.into()),
        };
    let mut names = vec![0u8; size];
    let length = if size == 0 {
        0
    } else {
        Errno::result(unsafe {
            libc::listxattr(source_c.as_ptr(), names.as_mut_ptr().cast(), size)
        })? as usize
    };
    let names = names
        .get(..length)
        .ok_or_else(|| io::Error::other("directory xattrs changed during root preparation"))?;
    for name in names.split(|b| *b == 0).filter(|s| !s.is_empty()) {
        if name.starts_with(b"trusted.overlay.") || name.starts_with(b"user.overlay.") {
            continue;
        }
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
        if let Err(error) = Errno::result(unsafe {
            libc::setxattr(
                target_c.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        }) {
            // A filesystem may assign the correct security label itself while
            // disallowing explicit relabelling in this user namespace.
            let mut actual = vec![0u8; value.len()];
            let length = unsafe {
                libc::getxattr(
                    target_c.as_ptr(),
                    name.as_ptr(),
                    actual.as_mut_ptr().cast(),
                    actual.len(),
                )
            };
            if length != value.len() as isize || actual != value {
                return Err(error.into());
            }
        }
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
