//! Linux process ownership. Every ptrace/wait operation stays on this OS thread.

mod processes;
mod syscalls;

use std::collections::HashSet;
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{ChildStderr, ChildStdin, ChildStdout, Command, Stdio};

use crate::terminal;
use nix::errno::Errno;
use nix::sys::ptrace::{self, Options};
use nix::sys::signal::Signal;
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

pub struct Pipes {
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
    pub stderr: ChildStderr,
}

pub enum Streams {
    Pipes(Pipes),
    Terminal(OwnedFd),
}

pub enum Event {
    Started { pid: i32, streams: Streams },
    Process { kind: &'static str, data: Value },
    FileOpen(crate::collectors::file::Opened),
    RootExit(u8),
    Finished,
    Failed(String),
}

fn emit(sender: &mpsc::Sender<Event>, event: Event) -> io::Result<()> {
    sender
        .blocking_send(event)
        .map_err(|_| io::Error::other("collector stopped receiving process events"))
}

pub fn spawn(
    command: OsString,
    with_terminal: bool,
    sender: mpsc::Sender<Event>,
    registry: crate::network::association::Registry,
    network: oneshot::Sender<crate::network::Endpoints>,
    capture_files: bool,
    trust_overlay: Option<std::path::PathBuf>,
) -> io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("agenttomo-tracer".into())
        .spawn(move || {
            if let Err(error) = run(
                command,
                with_terminal,
                &sender,
                registry,
                network,
                capture_files,
                trust_overlay,
            ) {
                let _ = emit(&sender, Event::Failed(error.to_string()));
            }
            // EXITKILL applies to this tracing thread, including on error/panic.
        })
}

fn run(
    command: OsString,
    with_terminal: bool,
    sender: &mpsc::Sender<Event>,
    registry: crate::network::association::Registry,
    network: oneshot::Sender<crate::network::Endpoints>,
    capture_files: bool,
    trust_overlay: Option<std::path::PathBuf>,
) -> io::Result<()> {
    // The helper cannot exec user code until we arm ptrace and send its gate byte.
    // SEIZE supports group-stop/LISTEN and avoids a pre_exec SIGSTOP/spawn deadlock.
    let (mut control, child_control) = UnixStream::pair()?;
    let mut launch = Command::new(std::env::current_exe()?);
    launch.arg("__tracee").arg("-c").arg(command);
    if let Some(path) = trust_overlay {
        launch.arg("--trust-overlay").arg(path);
    }
    let master = if with_terminal {
        let pair = terminal::open()?;
        launch
            .stdin(Stdio::from(pair.slave.try_clone()?))
            .stdout(Stdio::from(pair.slave.try_clone()?))
            .stderr(Stdio::from(pair.slave));
        Some(pair.master)
    } else {
        launch
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        None
    };
    let gate_fd = child_control.as_raw_fd();
    // Only syscalls run here. Reserve fd 3 across the helper exec and close it
    // in the helper before user code runs; all original endpoints are CLOEXEC.
    unsafe {
        launch.pre_exec(move || {
            nix::errno::Errno::result(nix::libc::dup2(gate_fd, 3))?;
            nix::errno::Errno::result(nix::libc::fcntl(3, nix::libc::F_SETFD, 0))?;
            if with_terminal {
                terminal::acquire_controlling_terminal()?;
            }
            Ok(())
        });
    }
    let mut child = launch.spawn()?;
    drop(child_control);
    drop(launch);
    let root = Pid::from_raw(child.id() as i32);
    control.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
    let endpoints = match crate::network::namespace::receive(&mut control) {
        Ok(endpoints) => endpoints,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    let netns = std::fs::read_link(format!("/proc/{root}/ns/net"))?;
    network
        .send(endpoints)
        .map_err(|_| io::Error::other("network proxy stopped during startup"))?;
    let mut ready = [0];
    if control.read_exact(&mut ready).is_err() || ready != *b"R" {
        let _ = child.kill();
        let _ = child.wait();
        return Err(io::Error::other(
            "launch helper failed before its startup gate",
        ));
    }
    let options = Options::PTRACE_O_TRACEFORK
        | Options::PTRACE_O_TRACEVFORK
        | Options::PTRACE_O_TRACECLONE
        | Options::PTRACE_O_TRACEEXEC
        | Options::PTRACE_O_TRACEEXIT
        | Options::PTRACE_O_TRACESECCOMP
        | Options::PTRACE_O_TRACESYSGOOD
        | Options::PTRACE_O_EXITKILL;
    if let Err(error) = ptrace::seize(root, options) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(io::Error::other(format!(
            "cannot trace child {root}: {error}"
        )));
    }
    control.write_all(b"G")?;
    if crate::syscall::SUPPORTED {
        // The acknowledgement itself is a send/write on the private gate.
        // Drive that stop before waiting for its byte; do not expose it as a
        // target event. A failed filter installation instead reaches EXIT.
        match waitpid(root, Some(WaitPidFlag::__WALL))? {
            WaitStatus::PtraceEvent(pid, _, nix::libc::PTRACE_EVENT_SECCOMP) => {
                let call = crate::syscall::Call::read(pid, None)?;
                if call.args[0] != 3 {
                    return Err(io::Error::other(
                        "unexpected syscall before startup acknowledgement",
                    ));
                }
                ptrace::cont(pid, None)?;
            }
            WaitStatus::PtraceEvent(pid, _, nix::libc::PTRACE_EVENT_EXIT) => {
                ptrace::cont(pid, None)?;
            }
            WaitStatus::Exited(_, _) => {}
            _ => {
                return Err(io::Error::other(
                    "helper stopped before startup acknowledgement",
                ));
            }
        }
    }
    let mut installed = [0];
    if control.read_exact(&mut installed).is_err() || installed != *b"F" {
        let mut error = String::new();
        let _ = control.take(4096).read_to_string(&mut error);
        let _ = child.kill();
        let _ = child.wait();
        return Err(io::Error::other(if error.is_empty() {
            "launch helper failed to install syscall collection".into()
        } else {
            error
        }));
    }
    drop(control);
    let streams = match master {
        Some(master) => Streams::Terminal(master),
        None => Streams::Pipes(Pipes {
            stdin: child.stdin.take().expect("piped stdin"),
            stdout: child.stdout.take().expect("piped stdout"),
            stderr: child.stderr.take().expect("piped stderr"),
        }),
    };
    emit(
        sender,
        Event::Started {
            pid: root.as_raw(),
            streams,
        },
    )?;

    let mut known = HashSet::from([root]);
    let mut newborn = HashSet::new();
    let mut root_exited = false;
    let mut calls = syscalls::Dispatch::new(registry, netns, capture_files);
    let mut processes = processes::Registry::default();
    processes.child(root);
    loop {
        let status = match waitpid(None, Some(WaitPidFlag::__WALL)) {
            Err(Errno::EINTR) => continue,
            Err(Errno::ECHILD) if root_exited => break,
            result => result?,
        };
        match status {
            WaitStatus::Exited(pid, code) => {
                calls.forget(pid);
                known.remove(&pid);
                newborn.remove(&pid);
                if let Some(identity) = processes.exit(pid) {
                    let mut data = json!({"exit_code": code});
                    crate::process::fields(Some(identity), &mut data);
                    emit(
                        sender,
                        Event::Process {
                            kind: "process.exit",
                            data,
                        },
                    )?;
                }
                if pid == root {
                    root_exited = true;
                    emit(sender, Event::RootExit(code as u8))?;
                }
            }
            WaitStatus::Signaled(pid, signal, core_dumped) => {
                calls.forget(pid);
                known.remove(&pid);
                newborn.remove(&pid);
                if let Some(identity) = processes.exit(pid) {
                    let mut data = json!({"signal": signal as i32, "core_dumped": core_dumped});
                    crate::process::fields(Some(identity), &mut data);
                    emit(
                        sender,
                        Event::Process {
                            kind: "process.exit",
                            data,
                        },
                    )?;
                }
                if pid == root {
                    root_exited = true;
                    emit(sender, Event::RootExit(128 + signal as u8))?;
                }
            }
            WaitStatus::PtraceEvent(pid, signal, event) => {
                let first_stop = known.insert(pid) | newborn.remove(&pid);
                if event != nix::libc::PTRACE_EVENT_EXEC {
                    processes.observe(pid);
                }
                match event {
                    nix::libc::PTRACE_EVENT_SECCOMP => {
                        if let Err(error) = calls.enter(pid, processes.observe(pid)) {
                            if error.raw_os_error() != Some(nix::libc::ESRCH) {
                                return Err(error);
                            }
                            calls.forget(pid);
                        }
                    }
                    nix::libc::PTRACE_EVENT_FORK
                    | nix::libc::PTRACE_EVENT_VFORK
                    | nix::libc::PTRACE_EVENT_CLONE => {
                        let new = Pid::from_raw(ptrace::getevent(pid)? as i32);
                        if known.insert(new) {
                            newborn.insert(new);
                        }
                        let kind = match event {
                            nix::libc::PTRACE_EVENT_FORK => "process.fork",
                            nix::libc::PTRACE_EVENT_VFORK => "process.vfork",
                            _ => "process.clone",
                        };
                        let parent = processes.observe(pid);
                        let child = processes.child(new);
                        if parent.is_none() || child.is_none() || parent != child {
                            let mut data = json!({
                                "parent_pid": parent.map(|p| p.pid),
                                "child_pid": child.map(|p| p.pid),
                                "child_process_instance_id": child.map(|p| p.process_instance_id),
                            });
                            crate::process::fields(parent, &mut data);
                            emit(sender, Event::Process { kind, data })?;
                        }
                    }
                    nix::libc::PTRACE_EVENT_EXEC => {
                        let old = Pid::from_raw(ptrace::getevent(pid)? as i32);
                        if old != pid {
                            calls.forget(old);
                            known.remove(&old);
                            newborn.remove(&old);
                        }
                        let identity = processes.exec(pid, old);
                        let mut data = exec_context(pid);
                        crate::process::fields(Some(identity), &mut data);
                        emit(
                            sender,
                            Event::Process {
                                kind: "process.exec",
                                data,
                            },
                        )?;
                    }
                    nix::libc::PTRACE_EVENT_STOP if !first_stop && signal != Signal::SIGTRAP => {
                        listen(pid)?;
                        continue;
                    }
                    _ => {}
                }
                resume(&calls, pid, None)?;
            }
            WaitStatus::PtraceSyscall(pid) => {
                match calls.exit(pid) {
                    Ok(Some(event)) => emit(sender, event)?,
                    Err(error) if error.raw_os_error() == Some(nix::libc::ESRCH) => {
                        calls.forget(pid)
                    }
                    Err(error) => return Err(error),
                    Ok(None) => {}
                }
                resume(&calls, pid, None)?;
            }
            WaitStatus::Stopped(pid, signal) => {
                resume(&calls, pid, Some(signal))?;
            }
            _ => {}
        }
    }
    emit(sender, Event::Finished)
}

fn resume(calls: &syscalls::Dispatch, pid: Pid, signal: Option<Signal>) -> io::Result<()> {
    match calls.resume(pid, signal) {
        // A concurrent group exit can kill this task before it is resumed.
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn listen(pid: Pid) -> io::Result<()> {
    // nix exposes SEIZE but not LISTEN. This request has no pointer arguments.
    let result = unsafe { nix::libc::ptrace(nix::libc::PTRACE_LISTEN, pid.as_raw(), 0, 0) };
    if result == -1 && Errno::last() != Errno::ESRCH {
        return Err(Errno::last().into());
    }
    Ok(())
}

fn exec_context(pid: Pid) -> Value {
    let mut data = json!({});
    let path = format!("/proc/{pid}/cmdline");
    let mut raw = Vec::new();
    match std::fs::File::open(path)
        .and_then(|file| file.take(1024 * 1024 + 1).read_to_end(&mut raw))
    {
        Ok(_) => {
            let truncated = raw.len() > 1024 * 1024;
            raw.truncate(1024 * 1024);
            data["argv"] = json!(
                raw.strip_suffix(&[0])
                    .unwrap_or(&raw)
                    .split(|byte| *byte == 0)
                    .map(|arg| String::from_utf8_lossy(arg).into_owned())
                    .collect::<Vec<_>>()
            );
            data["argv_bytes"] = json!(raw);
            data["capture"] = json!(if truncated { "truncated" } else { "complete" });
        }
        Err(error) => {
            data["capture"] = json!("unreadable");
            data["error"] = json!(error.to_string());
        }
    }
    data
}
