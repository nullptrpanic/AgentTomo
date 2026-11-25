//! Bounded observations of the public report in one active sample. Inspect the
//! published inode while the mutating thread is still at its syscall exit stop.
use crate::{
    agent::{Hub, Scope},
    process::Identity,
    syscall::{Call, memory},
};
use nix::{
    fcntl::{OFlag, OpenHow, ResolveFlag, openat, openat2},
    libc,
    sys::stat::Mode,
    unistd::Pid,
};
use serde_json::{Value, json};
use std::{
    ffi::OsStr,
    fs::{File, OpenOptions},
    io::Read,
    os::fd::AsRawFd,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

// write and writev are already observed by the network collector.
pub const EXTRA_SYSCALLS: &[i64] = &[
    libc::SYS_pwrite64,
    libc::SYS_pwritev,
    libc::SYS_pwritev2,
    libc::SYS_ftruncate,
    libc::SYS_truncate,
    libc::SYS_copy_file_range,
    libc::SYS_sendfile,
    libc::SYS_splice,
    libc::SYS_renameat,
    libc::SYS_renameat2,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_rename,
];

#[derive(Clone, Default)]
pub struct Watch(Arc<Mutex<Option<Arc<Trial>>>>);

struct Trial {
    scope: Scope,
    path: PathBuf,
    marker: String,
    evidence: Mutex<Vec<Value>>,
}

impl Watch {
    pub fn arm(&self, scope: &Scope, path: PathBuf, marker: String) {
        *self.0.lock().unwrap() = Some(Arc::new(Trial {
            scope: scope.clone(),
            path,
            marker,
            evidence: Default::default(),
        }));
    }

    pub fn finish(&self, scope: &Scope) -> Option<Vec<Value>> {
        let mut active = self.0.lock().unwrap();
        active
            .take_if(|trial| &trial.scope == scope)
            .map(|trial| std::mem::take(&mut *trial.evidence.lock().unwrap()))
    }

    fn scoped(&self, call: &Call, agents: &Hub) -> Option<(Arc<Trial>, Identity)> {
        let trial = self.0.lock().unwrap().clone()?;
        if trial.evidence.lock().unwrap().len() >= 2 {
            return None;
        }
        let identity = call.process?;
        (agents.binding(identity.process_instance_id)?.scope == trial.scope)
            .then_some((trial, identity))
    }

    pub fn enter(&self, call: &Call, agents: &Hub) -> Option<Mutation> {
        let (trial, identity) = self.scoped(call, agents)?;
        let args = call.args;
        let fd = |value| pin(&fd_path(call.tid, value), 0);
        let (name, pin, rename) = match call.number {
            libc::SYS_write => ("write", fd(args[0])?, false),
            libc::SYS_writev => ("writev", fd(args[0])?, false),
            libc::SYS_pwrite64 => ("pwrite64", fd(args[0])?, false),
            libc::SYS_pwritev => ("pwritev", fd(args[0])?, false),
            libc::SYS_pwritev2 => ("pwritev2", fd(args[0])?, false),
            libc::SYS_ftruncate => ("ftruncate", fd(args[0])?, false),
            libc::SYS_copy_file_range => ("copy_file_range", fd(args[2])?, false),
            libc::SYS_sendfile => ("sendfile", fd(args[0])?, false),
            libc::SYS_splice => ("splice", fd(args[2])?, false),
            libc::SYS_truncate => (
                "truncate",
                argument_pin(call.tid, libc::AT_FDCWD, args[0], false)?,
                false,
            ),
            #[cfg(target_arch = "x86_64")]
            libc::SYS_rename => (
                "rename",
                trial.rename_source(call, libc::AT_FDCWD, args[0], libc::AT_FDCWD, args[1])?,
                true,
            ),
            libc::SYS_renameat | libc::SYS_renameat2 => {
                let name = if call.number == libc::SYS_renameat {
                    "renameat"
                } else {
                    "renameat2"
                };
                (
                    name,
                    trial
                        .rename_source(call, args[0] as i32, args[1], args[2] as i32, args[3])
                        .or_else(|| {
                            if call.number == libc::SYS_renameat2
                                && args[4] as u32 & libc::RENAME_EXCHANGE != 0
                            {
                                trial.rename_source(
                                    call,
                                    args[2] as i32,
                                    args[3],
                                    args[0] as i32,
                                    args[1],
                                )
                            } else {
                                None
                            }
                        })?,
                    true,
                )
            }
            _ => return None,
        };
        let metadata = pin.metadata().ok()?;
        if !metadata.is_file()
            || matches!(call.number, libc::SYS_ftruncate | libc::SYS_truncate)
                && metadata.len() == args[1]
        {
            return None;
        }
        let already_public = trial
            .report(call.tid)
            .is_some_and(|published| same_file(&pin, &published));
        // A rename of the public inode onto itself changes nothing. An fd write
        // must already address the published inode before the call starts.
        if (rename && already_public) || (!rename && !already_public) {
            return None;
        }
        // Publishing a new inode supplies the report. An in-place edit must
        // introduce the marker, even when another task pre-populated this file.
        let can_introduce_marker = rename
            || !read_report(&pin)
                .is_some_and(|report| report["diagnostic_token"] == trial.marker.as_str());
        Some(Mutation {
            trial,
            identity,
            tid: call.tid,
            name,
            pin,
            can_introduce_marker,
        })
    }

    pub fn opened(&self, call: &Call, agents: &Hub, opened: &Value) -> Option<Value> {
        let flags = opened["flags"].as_i64()?;
        if flags & i64::from(libc::O_TRUNC) == 0 || flags & i64::from(libc::O_PATH) != 0 {
            return None;
        }
        let fd = opened["result"].as_i64().filter(|fd| *fd >= 0)?;
        let (trial, identity) = self.scoped(call, agents)?;
        let pin = pin(&fd_path(call.tid, fd as u64), 0)?;
        Mutation {
            trial,
            identity,
            tid: call.tid,
            name: "open(O_TRUNC)",
            pin,
            can_introduce_marker: false,
        }
        .finish(fd)
    }
}

fn fd_path(tid: Pid, fd: u64) -> PathBuf {
    format!("/proc/{tid}/fd/{}", fd as i32).into()
}

fn argument_path(tid: Pid, dirfd: i32, pointer: u64) -> Option<PathBuf> {
    let (bytes, truncated) = memory::c_string(tid, pointer, 4096).ok()?;
    if truncated {
        return None;
    }
    let path = Path::new(OsStr::from_bytes(&bytes));
    if path.is_absolute() {
        return Some(path.to_owned());
    }
    let base = if dirfd == libc::AT_FDCWD {
        PathBuf::from(format!("/proc/{tid}/cwd"))
    } else {
        fd_path(tid, dirfd as u64)
    };
    let base = pin(&base, 0)?;
    let root = pin(Path::new(&format!("/proc/{tid}/root")), 0)?;
    let location =
        |file: &File| std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).ok();
    let base_name = location(&base)?;
    let relative = base_name.strip_prefix(location(&root)?).ok()?;
    let base_path = Path::new("/").join(relative);
    same_file(&base, &rooted(&root, &base_path, OFlag::empty())?).then(|| base_path.join(path))
}

fn rooted(root: &File, path: &Path, flags: OFlag) -> Option<File> {
    let how = OpenHow::new()
        .flags(OFlag::O_PATH | OFlag::O_CLOEXEC | flags)
        .resolve(ResolveFlag::RESOLVE_IN_ROOT | ResolveFlag::RESOLVE_NO_MAGICLINKS);
    match openat2(root, path, how) {
        Ok(fd) => Some(fd.into()),
        Err(nix::errno::Errno::ENOSYS) => rooted_fallback(root, path, flags),
        Err(_) => None,
    }
}

fn rooted_fallback(root: &File, path: &Path, flags: OFlag) -> Option<File> {
    // On older kernels walk simple paths with pinned directory handles.
    let mut dirs = vec![root.try_clone().ok()?];
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                if !dirs.last()?.metadata().ok()?.is_dir() {
                    return None;
                }
                if dirs.len() > 1 {
                    dirs.pop();
                }
            }
            Component::Normal(name) => {
                if !dirs.last()?.metadata().ok()?.is_dir() {
                    return None;
                }
                let fd = openat(
                    dirs.last()?,
                    Path::new(name),
                    OFlag::O_PATH | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                    Mode::empty(),
                )
                .ok()?;
                dirs.push(fd.into());
            }
            Component::Prefix(_) => return None,
        }
    }
    let resolved = dirs.pop()?;
    if flags.contains(OFlag::O_DIRECTORY) && !resolved.metadata().ok()?.is_dir() {
        return None;
    }
    Some(resolved)
}

fn argument_pin(tid: Pid, dirfd: i32, pointer: u64, nofollow: bool) -> Option<File> {
    let path = argument_path(tid, dirfd, pointer)?;
    let root = pin(Path::new(&format!("/proc/{tid}/root")), 0)?;
    rooted(
        &root,
        &path,
        if nofollow {
            OFlag::O_NOFOLLOW
        } else {
            OFlag::empty()
        },
    )
}

fn pin(path: &Path, flags: i32) -> Option<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | flags)
        .open(path)
        .ok()
}

fn same_file(a: &File, b: &File) -> bool {
    a.metadata()
        .ok()
        .zip(b.metadata().ok())
        .is_some_and(|(a, b)| a.dev() == b.dev() && a.ino() == b.ino())
}

impl Trial {
    fn lookup(&self, tid: Pid, path: &Path) -> Option<File> {
        let mut file = pin(Path::new(&format!("/proc/{tid}/root")), 0)?;
        let flags = OFlag::O_PATH | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
        for component in path.strip_prefix("/").ok()?.components() {
            let Component::Normal(name) = component else {
                return None;
            };
            file = openat(&file, Path::new(name), flags, Mode::empty())
                .ok()?
                .into();
        }
        Some(file)
    }

    fn report(&self, tid: Pid) -> Option<File> {
        self.lookup(tid, &self.path)
    }

    fn rename_source(
        &self,
        call: &Call,
        from_fd: i32,
        from: u64,
        to_fd: i32,
        to: u64,
    ) -> Option<File> {
        let dest = argument_path(call.tid, to_fd, to)?;
        if dest.file_name() != self.path.file_name() {
            return None;
        }
        let root = pin(Path::new(&format!("/proc/{}/root", call.tid)), 0)?;
        let parent = rooted(&root, dest.parent()?, OFlag::O_DIRECTORY)?;
        if !same_file(&parent, &self.lookup(call.tid, self.path.parent()?)?) {
            return None;
        }
        argument_pin(call.tid, from_fd, from, true)
    }
}

pub struct Mutation {
    trial: Arc<Trial>,
    identity: Identity,
    tid: Pid,
    name: &'static str,
    pin: File,
    can_introduce_marker: bool,
}

fn read_report(pin: &File) -> Option<Value> {
    let file = super::preview::read_handle(pin, &mut Value::Null).ok()?;
    let mut bytes = Vec::new();
    if file.take(8193).read_to_end(&mut bytes).ok()? > 8192 {
        return None;
    }
    if bytes.is_empty() {
        Some(Value::Null)
    } else {
        serde_json::from_slice(&bytes).ok()
    }
}

impl Mutation {
    pub fn finish(self, result: i64) -> Option<Value> {
        let zero_is_success = matches!(
            self.name,
            "open(O_TRUNC)" | "ftruncate" | "truncate" | "rename" | "renameat" | "renameat2"
        );
        if result < 0 || result == 0 && !zero_is_success {
            return None;
        }
        let public = self.trial.report(self.tid)?;
        if !same_file(&self.pin, &public) {
            return None;
        }
        let report = read_report(&public)?;
        let observed_marker = report["diagnostic_token"]
            .as_str()
            .filter(|value| *value == self.trial.marker);
        let matched = observed_marker.is_some();
        if matched && !self.can_introduce_marker {
            return None;
        }
        let mut evidence = self.trial.evidence.lock().unwrap();
        if !matches!((evidence.len(), matched), (0, true) | (1, false)) {
            return None;
        }
        let mut event = json!({"task_id":self.trial.scope.task_id,"agent_instance_id":self.trial.scope.agent_instance_id,
            "path":self.trial.path,"syscall":self.name,"result":result,"matched":matched,
            "captured_unix_ns":crate::agent::now()});
        crate::process::fields(Some(self.identity), &mut event);
        if let Some(marker) = observed_marker {
            event["observed_marker"] = marker.into();
        }
        evidence.push(event.clone());
        Some(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn absolute_symlinks_and_parent_components_resolve_inside_the_selected_root() {
        let dir =
            std::env::temp_dir().join(format!("agenttomo-report-path-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("public")).unwrap();
        let report = dir.join("public/report");
        std::fs::write(&report, "fixture").unwrap();
        std::os::unix::fs::symlink("/public", dir.join("current")).unwrap();
        let root = File::open(&dir).unwrap();
        // openat2 appeared in Linux 5.6. Older kernels decline symlink evidence.
        if openat2(&root, ".", OpenHow::new().flags(OFlag::O_PATH)).is_ok() {
            let observed = rooted(&root, Path::new("/../current/report"), OFlag::empty()).unwrap();
            assert!(same_file(&observed, &File::open(report).unwrap()));
        }
        assert!(rooted_fallback(&root, Path::new("/public/report/.."), OFlag::empty()).is_none());
        assert!(rooted_fallback(&root, Path::new("/public/report"), OFlag::O_DIRECTORY).is_none());
        assert!(rooted_fallback(&root, Path::new("/public"), OFlag::O_DIRECTORY).is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
