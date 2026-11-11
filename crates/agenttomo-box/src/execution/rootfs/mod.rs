//! Task-scoped directory CoW. Only the gated, single-threaded launch helper
//! changes mounts; the collector keeps the original root and owns cleanup.

mod layout;
mod metadata;

use nix::{errno::Errno, libc};
use std::{
    ffi::OsString,
    fs, io,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
pub struct Options {
    pub enabled: bool,
    pub size_mib: u32,
    pub exemptions: Vec<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            enabled: false,
            size_mib: 512,
            exemptions: Vec::new(),
        }
    }
}

impl Options {
    pub fn arguments(&self) -> Vec<OsString> {
        if !self.enabled {
            return Vec::new();
        }
        let mut args = vec![
            "--overlay-root".into(),
            "--overlay-root-size-mib".into(),
            self.size_mib.to_string().into(),
        ];
        for path in &self.exemptions {
            args.extend(["--overlay-exempt".into(), path.as_os_str().to_owned()]);
        }
        args
    }
}

pub struct Root {
    directory: PathBuf,
}

impl Root {
    pub fn new() -> io::Result<Self> {
        let directory = fs::canonicalize(std::env::temp_dir())?
            .join(format!("agenttomo-root-{}", uuid::Uuid::new_v4()));
        fs::DirBuilder::new().mode(0o700).create(&directory)?;
        Ok(Self { directory })
    }

    pub fn path(&self) -> &Path {
        &self.directory
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        // The collector never sees the helper's tmpfs, just this empty anchor.
        // Never recursively delete a directory supplied by the workload.
        let _ = fs::remove_dir(&self.directory);
    }
}

pub fn install(directory: &Path, options: &Options, private_paths: &[&Path]) -> io::Result<()> {
    let install = || -> io::Result<()> {
        let cwd = std::env::current_dir()?;
        let layout = layout::Layout::read(&options.exemptions)?;
        for path in private_paths {
            layout.require_private(path)?;
        }
        super::mounts::private_namespace()?;
        // Clone the original mount view before adding backing storage. Even
        // shared /tmp must see only the empty anchor, never upper/work.
        let original_root = super::mounts::detached_tree(Path::new("/"))?;
        super::mounts::mount(
            "tmpfs",
            directory,
            "tmpfs",
            libc::MS_NOSUID | libc::MS_NODEV,
            &format!("size={}m,mode=0700", options.size_mib),
        )?;
        let merged = directory.join("root");
        fs::create_dir(&merged)?;
        super::mounts::attach_tree(&original_root, &merged)?;
        drop(original_root);
        layout.assemble(directory, &merged)?;
        std::env::set_current_dir(&merged)?;
        let dot = c".";
        Errno::result(unsafe { libc::syscall(libc::SYS_pivot_root, dot.as_ptr(), dot.as_ptr()) })?;
        Errno::result(unsafe { libc::umount2(dot.as_ptr(), libc::MNT_DETACH) })?;
        // Never retain an old-root cwd or descriptor in the target.
        std::env::set_current_dir("/")?;
        std::env::set_current_dir(cwd)?;
        Ok(())
    };
    install().map_err(|e| {
        io::Error::other(format!(
            "cannot prepare directory OverlayFS: {e}; target was not started (no fallback)"
        ))
    })
}
