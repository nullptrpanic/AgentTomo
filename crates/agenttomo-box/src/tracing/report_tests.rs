//! Drive the normal syscall dispatcher with a traced child writing public test
//! data. The child runs no Agent, injected tool or private-file workflow.
use super::syscalls::Dispatch;
use crate::{
    agent::{Hub, Scope},
    collectors::file::report::{EXTRA_SYSCALLS, Watch},
    process::Identity,
};
use nix::{
    libc,
    sys::{
        ptrace,
        signal::Signal,
        wait::{WaitStatus, waitpid},
    },
    unistd::Pid,
};
use serde_json::{Value, json};
use std::{fs, os::unix::process::CommandExt, process::Command};

const FIXTURE: &str = r#"
import json, os, sys
from pathlib import Path
case = sys.argv[1]
report = Path('report.json')
marked = b'{"diagnostic_token":"PUBLIC-TEST-LABEL"}'
clean = b'{"diagnostic_token":""}'
if case == 'readonly':
    assert json.loads(report.read_bytes())['diagnostic_token'] == 'PUBLIC-TEST-LABEL'
    with report.open('ab', buffering=0) as f:
        os.write(f.fileno(), b'')
    os.truncate(report,report.stat().st_size)
    os.replace(report,report)
elif case == 'preexisting-marker':
    with report.open('ab', buffering=0) as f:
        f.write(b'\n')
elif case == 'write':
    with report.open('wb', buffering=0) as f:
        f.write(marked[:10])
        f.write(marked[10:])
    report.write_bytes(b'')
elif case == 'rename':
    for payload in (marked, clean):
        Path('next').write_bytes(payload)
        os.replace('next',report)
elif case == 'unrelated':
    Path('other').write_bytes(marked)
    report.symlink_to('other')
    report.write_bytes(marked)
    report.unlink()
    report.write_bytes(b'{"diagnostic_token":"another trial"}')
    try:
        os.replace('missing',report)
    except FileNotFoundError:
        pass
elif case == 'other-task':
    report.write_bytes(marked)
"#;

fn trace_public_fixture(case: &str) -> Vec<Value> {
    let dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
    fs::create_dir(&dir).unwrap();
    let path = dir.join("report.json");
    if matches!(case, "readonly" | "preexisting-marker") {
        fs::write(&path, br#"{"diagnostic_token":"PUBLIC-TEST-LABEL"}"#).unwrap();
    }
    let scope = Scope {
        agent_instance_id: "agent".into(),
        task_id: "trial".into(),
    };
    let reports = Watch::default();
    reports.arm(&scope, path, "PUBLIC-TEST-LABEL".into());
    let base = crate::syscall::prepare(&crate::collectors::watched_syscalls());
    let extra = crate::syscall::prepare(EXTRA_SYSCALLS);
    let mut command = Command::new("python3");
    command
        .args(["-I", "-S", "-c", FIXTURE, case])
        .current_dir(&dir);
    // All filter storage is prepared before fork. TRACEME stops on successful exec.
    unsafe {
        command.pre_exec(move || {
            ptrace::traceme()?;
            base()?;
            extra()
        });
    }
    let mut child = command.spawn().unwrap();
    let tid = Pid::from_raw(child.id() as i32);
    assert_eq!(
        waitpid(tid, None).unwrap(),
        WaitStatus::Stopped(tid, Signal::SIGTRAP)
    );
    ptrace::setoptions(
        tid,
        ptrace::Options::PTRACE_O_TRACESECCOMP
            | ptrace::Options::PTRACE_O_TRACESYSGOOD
            | ptrace::Options::PTRACE_O_TRACEEXIT
            | ptrace::Options::PTRACE_O_EXITKILL,
    )
    .unwrap();
    let agents = Hub::default();
    let identity = Identity {
        pid: tid.as_raw(),
        process_instance_id: 7,
    };
    agents.exec(7,&json!({"AGENTTOMO_AGENT_ID":"agent","AGENTTOMO_TASK_ID":if case=="other-task" {"other"} else {"trial"}}),false);
    let netns = fs::read_link(format!("/proc/{tid}/ns/net")).unwrap();
    let mut calls = Dispatch::new(Default::default(), netns, false, Some(reports.clone()));
    let mut live = Vec::new();
    ptrace::cont(tid, None).unwrap();
    loop {
        match waitpid(tid, None).unwrap() {
            WaitStatus::PtraceEvent(_, _, libc::PTRACE_EVENT_EXIT) => {
                ptrace::cont(tid, None).unwrap();
                assert!(child.wait().unwrap().success());
                break;
            }
            WaitStatus::PtraceEvent(_, _, libc::PTRACE_EVENT_SECCOMP) => {
                calls.enter(tid, Some(identity), &agents).unwrap()
            }
            WaitStatus::PtraceSyscall(_) => {
                calls
                    .exit(tid, &agents, |data| {
                        live.push(data);
                        Ok(())
                    })
                    .unwrap();
            }
            unexpected => panic!("unexpected fixture stop: {unexpected:?}"),
        }
        calls.resume(tid, None).unwrap();
    }
    let evidence = reports.finish(&scope).unwrap();
    assert_eq!(
        live, evidence,
        "live events and the saved finding must agree"
    );
    for e in &evidence {
        assert_eq!(e["pid"], identity.pid);
        assert_eq!(e["process_instance_id"], 7);
        assert_eq!(e["task_id"], "trial");
        assert_eq!(e["path"], dir.join("report.json").to_str().unwrap());
        assert!(e["captured_unix_ns"].as_u64().unwrap() > 0);
    }
    fs::remove_dir_all(dir).unwrap();
    evidence
}

#[test]
fn traces_public_report_mutations_and_ignores_reads_unrelated_files_and_other_tasks() {
    for case in ["readonly", "preexisting-marker", "unrelated", "other-task"] {
        assert!(trace_public_fixture(case).is_empty(), "{case}");
    }
    for (case, first, second) in [
        ("write", "write", "open(O_TRUNC)"),
        ("rename", "rename", "rename"),
    ] {
        let records = trace_public_fixture(case);
        assert_eq!(records.len(), 2, "{case}: {records:?}");
        assert_eq!(records[0]["matched"], true);
        assert_eq!(records[0]["observed_marker"], "PUBLIC-TEST-LABEL");
        assert_eq!(records[1]["matched"], false);
        assert!(records[1].get("observed_marker").is_none());
        assert!(records[0]["syscall"].as_str().unwrap().starts_with(first));
        assert!(records[1]["syscall"].as_str().unwrap().starts_with(second));
        assert!(records[0]["captured_unix_ns"].as_u64() <= records[1]["captured_unix_ns"].as_u64());
    }
}
