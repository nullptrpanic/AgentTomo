//! Whole top-level directories plus /etc/ssl. Mount boundaries are left intact;
//! skipped directories retain the original view instead of being subdivided.
use super::metadata;
use crate::execution::mounts;
use nix::libc;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs, io,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
};

pub struct Layout {
    mounts: BTreeMap<PathBuf, libc::c_ulong>,
    exemptions: Vec<PathBuf>,
    overlay_dirs: Vec<PathBuf>,
}

impl Layout {
    pub fn read(requested: &[PathBuf]) -> io::Result<Self> {
        let mounts = parse_mounts(&fs::read("/proc/self/mountinfo")?)?;
        let mut exemptions = Vec::new();
        for path in ["/dev", "/proc", "/sys", "/run"] {
            if Path::new(path).exists() {
                exemptions.push(fs::canonicalize(path)?);
            }
        }
        for path in requested {
            if !path.is_absolute()
                || path
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err(io::Error::other(format!(
                    "overlay exemption must be an absolute path without '..': {}",
                    path.display()
                )));
            }
            let path = fs::canonicalize(path).map_err(|e| {
                io::Error::other(format!("overlay exemption {}: {e}", path.display()))
            })?;
            if path == Path::new("/") {
                return Err(io::Error::other("cannot exempt the entire root"));
            }
            exemptions.push(path);
        }
        exemptions.sort();
        exemptions.dedup();
        let mut layout = Self {
            mounts,
            exemptions,
            overlay_dirs: Vec::new(),
        };
        for entry in fs::read_dir("/")? {
            let entry = entry?;
            if entry.file_type()?.is_dir() && layout.can_overlay(&entry.path()) {
                layout.overlay_dirs.push(entry.path());
            }
        }
        match fs::canonicalize("/etc/ssl") {
            Ok(ssl) => {
                if !layout.overlay_dirs.iter().any(|p| ssl.starts_with(p)) && !layout.exempted(&ssl)
                {
                    if !ssl.is_dir() || !layout.can_overlay(&ssl) {
                        return Err(io::Error::other(
                            "cannot overlay /etc/ssl as a whole: read-only path or nested mounts",
                        ));
                    }
                    layout.overlay_dirs.push(ssl);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        layout.overlay_dirs.sort();
        Ok(layout)
    }

    fn flags(&self, path: &Path) -> libc::c_ulong {
        self.mounts
            .iter()
            .filter(|(mount, _)| path.starts_with(mount))
            .max_by_key(|(mount, _)| mount.as_os_str().len())
            .map_or(0, |(_, flags)| *flags)
    }

    fn exempted(&self, path: &Path) -> bool {
        self.exemptions
            .iter()
            .any(|p| path.starts_with(p) || p.starts_with(path))
    }

    fn can_overlay(&self, path: &Path) -> bool {
        self.flags(path) & libc::MS_RDONLY == 0
            && !self.exempted(path)
            && !self.mounts.keys().any(|p| p != path && p.starts_with(path))
    }

    pub fn require_private(&self, path: &Path) -> io::Result<()> {
        let path = if fs::symlink_metadata(path).is_ok() {
            fs::canonicalize(path)?
        } else {
            fs::canonicalize(
                path.parent()
                    .ok_or_else(|| io::Error::other("private path has no parent"))?,
            )?
            .join(
                path.file_name()
                    .ok_or_else(|| io::Error::other("private path has no filename"))?,
            )
        };
        if !self.overlay_dirs.iter().any(|p| path.starts_with(p)) {
            return Err(io::Error::other(format!(
                "{} must stay private for CA injection; it is outside the selected overlays",
                path.display()
            )));
        }
        Ok(())
    }

    pub fn assemble(&self, storage: &Path, root: &Path) -> io::Result<()> {
        for (segment, source) in self.overlay_dirs.iter().enumerate() {
            // Resolve lower through the saved root so a /tmp overlay cannot
            // accidentally include the backing tmpfs mounted in the old root.
            let target = root.join(source.strip_prefix("/").unwrap());
            let slot = storage.join(format!("segment-{segment}"));
            fs::create_dir(&slot)?;
            let upper = slot.join("upper");
            let work = slot.join("work");
            fs::create_dir(&upper)?;
            fs::create_dir(&work)?;
            // OverlayFS takes its root metadata from upper, even when empty.
            metadata::copy_directory(&target, &upper)?;
            let options = format!(
                "lowerdir={},upperdir={},workdir={},userxattr",
                escape(&target)?,
                escape(&upper)?,
                escape(&work)?
            );
            mounts::mount("overlay", &target, "overlay", self.flags(source), &options)
                .map_err(|e| io::Error::other(format!("overlay {}: {e}", source.display())))?;
        }
        Ok(())
    }
}

fn escape(path: &Path) -> io::Result<String> {
    let path = path
        .to_str()
        .ok_or_else(|| io::Error::other("OverlayFS paths must be UTF-8"))?;
    Ok(path
        .replace('\\', "\\\\")
        .replace(',', "\\,")
        .replace(':', "\\:"))
}

fn decode(value: &[u8]) -> io::Result<PathBuf> {
    let mut bytes = Vec::new();
    let mut i = 0;
    while i < value.len() {
        if value[i] == b'\\' {
            let octal = value
                .get(i + 1..i + 4)
                .ok_or_else(|| io::Error::other("invalid mountinfo escape"))?;
            if !octal.iter().all(|b| (b'0'..=b'7').contains(b)) {
                return Err(io::Error::other("invalid mountinfo escape"));
            }
            bytes.push(((octal[0] - b'0') << 6) | ((octal[1] - b'0') << 3) | (octal[2] - b'0'));
            i += 4;
        } else {
            bytes.push(value[i]);
            i += 1;
        }
    }
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

fn parse_mounts(input: &[u8]) -> io::Result<BTreeMap<PathBuf, libc::c_ulong>> {
    let mut mounts = BTreeMap::new();
    for line in input.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        let fields: Vec<_> = line.split(|b| *b == b' ').collect();
        if fields.len() < 10 {
            return Err(io::Error::other("invalid /proc/self/mountinfo"));
        }
        let mut flags = 0;
        for option in fields[5].split(|b| *b == b',') {
            flags |= match option {
                b"ro" => libc::MS_RDONLY,
                b"nosuid" => libc::MS_NOSUID,
                b"nodev" => libc::MS_NODEV,
                b"noexec" => libc::MS_NOEXEC,
                _ => 0,
            };
        }
        mounts.insert(decode(fields[4])?, flags);
    }
    if !mounts.contains_key(Path::new("/")) {
        return Err(io::Error::other("mountinfo has no root"));
    }
    Ok(mounts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_directory_selection_stops_at_mounts_and_exemptions() {
        let layout = Layout {
            mounts: parse_mounts(b"1 0 0:1 / / rw - overlay overlay rw\n2 1 0:2 / /readonly ro - tmpfs tmpfs ro\n3 2 0:3 / /readonly/data rw - tmpfs tmpfs rw\n").unwrap(),
            exemptions: vec![PathBuf::from("/tmp/shared")],
            overlay_dirs: vec![],
        };
        assert!(!layout.can_overlay(Path::new("/readonly")));
        assert!(!layout.can_overlay(Path::new("/tmp")));
        assert!(!layout.can_overlay(Path::new("/tmp/shared")));
        assert!(layout.can_overlay(Path::new("/root")));
        assert!(layout.can_overlay(Path::new("/etc/ssl")));
    }
}
