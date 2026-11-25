//! Only the launch helper changes mounts; the collector retains the real trust
//! store for upstream verification. Upper/work live in its private tmpfs.
use super::mounts::mount;
use nix::libc;
use std::{
    fs, io,
    io::Read,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
};

const CERTS: &str = "/etc/ssl/certs";
const BUNDLE: &str = "ca-certificates.crt";
pub const PRIVATE_PATHS: &[&str] = &[
    CERTS,
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/ssl/certs/agenttomo-task-ca.pem",
];

pub struct Overlay {
    directory: Option<PathBuf>,
    pem: String,
}

impl Overlay {
    pub fn new(pem: &str, create_mountpoint: bool) -> io::Result<Self> {
        let directory = if create_mountpoint {
            let directory = fs::canonicalize(std::env::temp_dir())?
                .join(format!("agenttomo-trust-{}", uuid::Uuid::new_v4()));
            fs::DirBuilder::new().mode(0o700).create(&directory)?;
            Some(directory)
        } else {
            None
        };
        Ok(Self {
            directory,
            pem: pem.to_owned(),
        })
    }

    #[cfg(test)]
    pub fn path(&self) -> Option<&Path> {
        self.directory.as_deref()
    }

    pub fn handoff(&self) -> (Option<PathBuf>, String) {
        (self.directory.clone(), self.pem.clone())
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        // The collector only creates an empty mountpoint anchor; never recursively delete.
        if let Some(directory) = &self.directory {
            let _ = fs::remove_dir(directory);
        }
    }
}

pub fn receive(control: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut length = [0; 4];
    control.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if !(1..=65536).contains(&length) {
        return Err(io::Error::other(
            "invalid task CA length in startup handoff",
        ));
    }
    let mut pem = vec![0; length];
    control.read_exact(&mut pem)?;
    Ok(pem)
}

pub fn install(directory: &Path, ca: &[u8]) -> io::Result<()> {
    let install = || -> io::Result<()> {
        super::mounts::private_namespace()?;
        mount(
            "tmpfs",
            directory,
            "tmpfs",
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
            "size=4m,mode=0700",
        )?;
        let upper = directory.join("upper");
        let work = directory.join("work");
        fs::create_dir(&upper)?;
        fs::set_permissions(&upper, fs::metadata(CERTS)?.permissions())?;
        fs::create_dir(&work)?;
        inject(&upper, ca)?;
        let options = format!(
            "lowerdir={CERTS},upperdir={},workdir={},userxattr",
            super::rootfs::escape(&upper)?,
            super::rootfs::escape(&work)?
        );
        mount(
            "overlay",
            Path::new(CERTS),
            "overlay",
            libc::MS_NOSUID | libc::MS_NODEV,
            &options,
        )?;
        Ok(())
    };
    install().map_err(|error| io::Error::other(format!("cannot inject task CA through OverlayFS into {CERTS}/{BUNDLE}: {error}; capture requires user/mount namespaces and OverlayFS (no fallback)")))
}

pub fn inject(destination: &Path, ca: &[u8]) -> io::Result<()> {
    let mut bundle = fs::read(Path::new(CERTS).join(BUNDLE))?;
    if !bundle.ends_with(b"\n") {
        bundle.push(b'\n');
    }
    bundle.extend_from_slice(ca);
    fs::write(destination.join(BUNDLE), bundle)?;
    fs::write(destination.join("agenttomo-task-ca.pem"), ca)?;
    for path in [BUNDLE, "agenttomo-task-ca.pem"] {
        fs::set_permissions(destination.join(path), fs::Permissions::from_mode(0o644))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_mountpoint_never_contains_an_archived_ca() {
        let overlay = Overlay::new("public CA fixture", true).unwrap();
        let path = overlay.path().unwrap().to_owned();
        assert_eq!(fs::read_dir(&path).unwrap().count(), 0);
        drop(overlay);
        assert!(!path.exists());

        let root_overlay_mode = Overlay::new("public CA fixture", false).unwrap();
        assert!(root_overlay_mode.path().is_none());
    }

    #[test]
    fn startup_handoff_is_bounded_and_leaves_the_next_gate_byte_unread() {
        let pem = b"public certificate";
        let mut bytes = (pem.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(pem);
        bytes.push(b'G');
        let mut cursor = std::io::Cursor::new(bytes);
        assert_eq!(receive(&mut cursor).unwrap(), pem);
        let mut next = [0];
        cursor.read_exact(&mut next).unwrap();
        assert_eq!(&next, b"G");
        for length in [0_u32, 65537, u32::MAX] {
            assert!(receive(&mut length.to_be_bytes().as_slice()).is_err());
        }
        assert!(receive(&mut [0, 0, 0, 5, b'x'].as_slice()).is_err());
    }
}
