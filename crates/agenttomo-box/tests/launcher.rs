#![cfg(target_os = "linux")]

use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use serde_json::Value;

struct TestDir(PathBuf);

#[test]
fn run_subcommand_executes_the_existing_launcher() {
    let dir = TestDir::new();
    let output = Command::new(env!("CARGO_BIN_EXE_agenttomo-box"))
        .args([
            "run",
            "-c",
            "printf subcommand; exit 23",
            "--output-dir",
            "run",
        ])
        .current_dir(&dir.0)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(23));
    assert_eq!(output.stdout, b"subcommand");
    assert_eq!(dir.read("stdout.log"), b"subcommand");
}

#[test]
fn public_help_describes_run_and_playground() {
    let output = Command::new(env!("CARGO_BIN_EXE_agenttomo-box"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("run"));
    assert!(help.contains("playground"));
    assert!(!help.contains("__collector"));
    assert!(!help.contains("__tracee"));
}

impl TestDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "agenttomo-launcher-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agenttomo-box"));
        command.arg("run");
        command.current_dir(&self.0);
        command
    }

    fn read(&self, file: &str) -> Vec<u8> {
        fs::read(self.0.join("run").join(file)).unwrap()
    }

    fn compile_fixture(&self) {
        let compiled = Command::new("cc")
            .args(["-Wall", "-Wextra", "-Werror", "-pthread"])
            .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lifetime.c"))
            .arg("-o")
            .arg(self.0.join("lifetime"))
            .output()
            .expect("lifecycle tests require a C compiler");
        assert!(
            compiled.status.success(),
            "{}",
            String::from_utf8_lossy(&compiled.stderr)
        );
    }

    fn fixture(&self, mode: &str) -> Running {
        self.compile_fixture();
        Running(
            self.command()
                .args([
                    "-c",
                    &format!("exec ./lifetime {mode}"),
                    "--output-dir",
                    "run",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        )
    }

    fn pid(&self, file: &str) -> i32 {
        let mut result = None;
        wait_for(
            || {
                result = fs::read_to_string(self.0.join(file))
                    .ok()
                    .and_then(|s| s.trim().parse().ok());
                result.is_some()
            },
            "fixture did not publish its PID",
        );
        result.unwrap()
    }

    fn collection_finished(&self) {
        wait_for(
            || self.collection_state().as_deref() == Some("complete"),
            "collector did not complete",
        );
    }

    fn collection_state(&self) -> Option<String> {
        let value: Value =
            serde_json::from_slice(&fs::read(self.0.join("run/collection-status.json")).ok()?)
                .ok()?;
        value["state"].as_str().map(str::to_owned)
    }

    fn events(&self) -> Vec<Value> {
        String::from_utf8(self.read("events.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

fn wait_for(mut condition: impl FnMut() -> bool, message: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "{message}");
        thread::sleep(Duration::from_millis(10));
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::write(self.0.join("release"), b"");
        // Normal foreground completion can precede the worker's final rename.
        // Keep fixture gates and log directories alive until that drain finishes.
        let mut directories = vec![self.0.clone(), self.0.join(".agenttomo/runs")];
        let mut statuses = Vec::new();
        for parent in directories.drain(..) {
            if let Ok(entries) = fs::read_dir(parent) {
                statuses.extend(
                    entries
                        .flatten()
                        .map(|e| e.path().join("collection-status.json")),
                );
            }
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline
            && statuses.iter().any(|path| {
                fs::read(path)
                    .ok()
                    .and_then(|s| serde_json::from_slice::<Value>(&s).ok())
                    .is_some_and(|s| s["state"] == "running")
            })
        {
            thread::sleep(Duration::from_millis(10));
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Running(Child);

impl Running {
    fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "launcher did not finish");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn read_all(mut reader: impl Read) -> Vec<u8> {
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).unwrap();
    bytes
}

fn run(dir: &TestDir, args: &[&str], input: &[u8]) -> Output {
    let input_path = dir.0.join("input");
    fs::write(&input_path, input).unwrap();
    let mut child = Running(
        dir.command()
            .args(args)
            .stdin(fs::File::open(input_path).unwrap())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stdout = child.0.stdout.take().unwrap();
    let stderr = child.0.stderr.take().unwrap();
    let stdout = thread::spawn(move || read_all(stdout));
    let stderr = thread::spawn(move || read_all(stderr));
    Output {
        status: child.wait(),
        stdout: stdout.join().unwrap(),
        stderr: stderr.join().unwrap(),
    }
}

#[test]
fn records_binary_streams_and_preserves_command_exit_code() {
    let dir = TestDir::new();
    let command = "cat; printf '\\000\\377error' >&2; exit 23";
    let input = b"input\x00\xff\n";
    let output = run(&dir, &["-c", command, "--output-dir", "run"], input);

    assert_eq!(output.status.code(), Some(23));
    assert_eq!(output.stdout, input);
    assert_eq!(output.stderr, b"\x00\xfferror");
    assert_eq!(dir.read("stdin.log"), input);
    assert_eq!(dir.read("stdout.log"), input);
    assert_eq!(dir.read("stderr.log"), b"\x00\xfferror");
    assert_eq!(dir.read("command.txt"), command.as_bytes());
    assert_eq!(dir.read("exit-code.txt"), b"23\n");
}

#[test]
fn forwards_output_before_input_eof_and_exits_with_stdin_still_open() {
    let dir = TestDir::new();
    let mut child = Running(
        dir.command()
            .args([
                "-c",
                "printf 'ready\\n'; IFS= read -r line; printf 'got:%s\\n' \"$line\"",
                "--output-dir",
                "run",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stdout = child.0.stdout.take().unwrap();
    let (ready, received) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut stdout = BufReader::new(stdout);
        let mut line = Vec::new();
        stdout.read_until(b'\n', &mut line).unwrap();
        ready.send(line).unwrap();
        read_all(stdout)
    });
    assert_eq!(
        received.recv_timeout(Duration::from_secs(5)).unwrap(),
        b"ready\n"
    );
    let mut stdin = child.0.stdin.take().unwrap();
    stdin.write_all(b"hello\n").unwrap();
    assert!(child.wait().success());
    assert_eq!(reader.join().unwrap(), b"got:hello\n");
    assert_eq!(dir.read("stdin.log"), b"hello\n");
    assert_eq!(dir.read("stdout.log"), b"ready\ngot:hello\n");
    drop(stdin);
}

#[test]
fn drains_large_stdout_and_stderr_without_pipe_deadlock() {
    let dir = TestDir::new();
    let input = vec![b'x'; 512 * 1024];
    let output = run(
        &dir,
        &[
            "-c",
            "cat; head -c 524288 /dev/zero >&2",
            "--output-dir",
            "run",
        ],
        &input,
    );

    assert!(output.status.success());
    assert_eq!(output.stdout.len(), input.len());
    assert!(output.stdout == input, "stdout content mismatch");
    assert_eq!(output.stderr.len(), 512 * 1024);
    assert!(output.stderr.iter().all(|byte| *byte == 0));
    assert!(dir.read("stdin.log") == input, "stdin log mismatch");
    assert!(dir.read("stdout.log") == input, "stdout log mismatch");
    assert!(
        dir.read("stderr.log") == vec![0; 512 * 1024],
        "stderr log mismatch"
    );
}

#[test]
fn allows_the_command_to_close_stdin_early() {
    let dir = TestDir::new();
    let output = run(
        &dir,
        &["-c", "exec 0<&-; printf done", "--output-dir", "run"],
        &vec![b'x'; 512 * 1024],
    );
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"done");
}

#[test]
fn rejects_existing_output_directory_before_running_command() {
    let dir = TestDir::new();
    fs::create_dir(dir.0.join("run")).unwrap();
    fs::write(dir.0.join("run/stdout.log"), b"previous run").unwrap();
    let output = run(
        &dir,
        &["-c", "touch should-not-exist", "--output-dir", "run"],
        b"",
    );
    assert!(!output.status.success());
    assert!(!dir.0.join("should-not-exist").exists());
    assert_eq!(dir.read("stdout.log"), b"previous run");
}

#[test]
fn reports_a_closed_output_pipe_without_waiting_for_stdin() {
    let dir = TestDir::new();
    let mut child = Running(
        dir.command()
            .args(["-c", "while :; do printf x; done", "--output-dir", "run"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    drop(child.0.stdout.take());
    assert_eq!(child.wait().code(), Some(125));
    let stderr = read_all(child.0.stderr.take().unwrap());
    assert!(String::from_utf8_lossy(&stderr).contains("stdout recording failed"));
    assert!(dir.read("exit-code.txt").is_empty());
}

#[test]
fn uses_separate_default_run_directories() {
    let dir = TestDir::new();
    for _ in 0..2 {
        let output = run(&dir, &["-c", "printf hello"], b"");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"hello");
        assert!(output.stderr.is_empty());
    }
    let runs: Vec<_> = fs::read_dir(dir.0.join(".agenttomo/runs"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(runs.len(), 2);
    for path in runs {
        assert_eq!(fs::read(path.join("stdout.log")).unwrap(), b"hello");
    }
}

#[test]
fn validates_arguments_before_starting_a_command() {
    let dir = TestDir::new();
    for args in [
        vec![],
        vec!["-c"],
        vec!["-c", "touch should-not-exist", "--unknown"],
        vec!["-c", "touch should-not-exist", "--output-dir"],
        vec!["-c", "touch should-not-exist", "-c", "printf ignored"],
        vec![
            "-c",
            "touch should-not-exist",
            "--output-dir",
            "run",
            "--output-dir",
            "another-run",
        ],
    ] {
        let output = run(&dir, &args, b"");
        assert_eq!(output.status.code(), Some(2));
        assert!(!dir.0.join("should-not-exist").exists());
        assert!(!dir.0.join(".agenttomo").exists());
    }
}

#[test]
fn accepts_an_empty_shell_command() {
    let dir = TestDir::new();
    let output = run(&dir, &["-c", "", "--output-dir", "run"], b"");
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(dir.read("command.txt").is_empty());
    assert_eq!(dir.read("exit-code.txt"), b"0\n");
}

#[test]
fn preserves_non_utf8_command_and_directory_arguments() {
    let dir = TestDir::new();
    let command = OsString::from_vec(b"printf '\xff'".to_vec());
    let output_dir = dir.0.join(OsString::from_vec(b"run-\xff".to_vec()));
    let output = dir
        .command()
        .arg("-c")
        .arg(&command)
        .arg("--output-dir")
        .arg(&output_dir)
        .output()
        .unwrap();

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"\xff");
    assert_eq!(
        fs::read(output_dir.join("command.txt")).unwrap(),
        command.as_encoded_bytes()
    );
    assert_eq!(fs::read(output_dir.join("stdout.log")).unwrap(), b"\xff");
}

#[test]
fn reports_child_signal_as_shell_exit_code() {
    let dir = TestDir::new();
    let output = run(&dir, &["-c", "kill -TERM $$", "--output-dir", "run"], b"");
    assert_eq!(output.status.code(), Some(143));
    assert_eq!(dir.read("exit-code.txt"), b"143\n");
}

#[test]
fn help_has_no_run_side_effects() {
    let dir = TestDir::new();
    let output = run(&dir, &["--help"], b"");
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("-c"));
    assert!(!dir.0.join(".agenttomo").exists());
}

#[test]
fn returns_root_status_while_a_descendant_keeps_output_open() {
    let dir = TestDir::new();
    let mut child = Running(
        dir.command()
            .args([
                "-c",
                "(while [ ! -f release ]; do sleep 0.02; done; exec /bin/echo later) & printf 'before\\n'; exit 23",
                "--output-dir", "run",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn().unwrap(),
    );
    let stdout = child.0.stdout.take().unwrap();
    let stderr = child.0.stderr.take().unwrap();
    let (done, output) = mpsc::channel();
    let reader = thread::spawn(move || done.send((read_all(stdout), read_all(stderr))).unwrap());
    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            break None;
        }
        thread::sleep(Duration::from_millis(10));
    };
    // Release even on failure, so the old launcher's descendant cannot leak.
    let foreground_output = output.recv_timeout(Duration::from_millis(500));
    fs::write(dir.0.join("release"), b"").unwrap();
    child.wait();
    reader.join().unwrap();
    assert_eq!(
        status.and_then(|s| s.code()),
        Some(23),
        "foreground waited for a surviving descendant"
    );
    let (stdout, stderr) = foreground_output.expect("collector retained caller output pipes");
    assert_eq!(stdout, b"before\n");
    assert!(stderr.is_empty(), "{stderr:?}");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if fs::read_to_string(dir.0.join("run/collection-status.json"))
            .is_ok_and(|status| status.contains("\"complete\""))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "collector did not finish after descendant exit"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(dir.read("exit-code.txt"), b"23\n");
    assert_eq!(dir.read("stdout.log"), b"before\nlater\n");
    let events = String::from_utf8(dir.read("events.jsonl")).unwrap();
    assert!(events.contains("process.fork") || events.contains("process.vfork"));
    assert!(
        events.contains("/bin/echo"),
        "missing descendant exec after root exit: {events}"
    );
}

#[test]
fn traces_double_fork_and_setsid_after_root_exit_with_closed_stdio() {
    let dir = TestDir::new();
    let mut child = dir.fixture("daemon");
    assert_eq!(child.wait().code(), Some(27));
    assert!(read_all(child.0.stdout.take().unwrap()).is_empty());
    assert!(read_all(child.0.stderr.take().unwrap()).is_empty());
    assert_eq!(dir.collection_state().as_deref(), Some("running"));
    let pid = dir.pid("descendant.pid");
    let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    assert!(
        status
            .lines()
            .any(|line| line.starts_with("TracerPid:")
                && line.split_whitespace().nth(1) != Some("0")),
        "descendant detached: {status}"
    );
    fs::write(dir.0.join("release"), b"").unwrap();
    dir.collection_finished();
    let events = dir.events();
    let root_pid = dir.pid("root.pid");
    let root_exit = events
        .iter()
        .position(|e| e["kind"] == "process.exit" && e["data"]["pid"] == root_pid)
        .unwrap();
    let later_exec = events
        .iter()
        .position(|e| e["kind"] == "process.exec" && e["data"]["argv"][0] == "survivor-after-root")
        .unwrap();
    assert!(later_exec > root_exit);
    assert!(
        events
            .iter()
            .any(|e| e["kind"] == "process.exit" && e["data"]["pid"] == pid)
    );
    assert!(
        events
            .iter()
            .filter(|e| e["kind"] == "process.fork")
            .count()
            >= 2
    );
    assert!(
        !PathBuf::from(format!("/proc/{pid}")).exists(),
        "adopted orphan was not reaped"
    );
}

#[test]
fn waits_for_root_process_when_only_its_leader_thread_exits() {
    for mode in ["thread", "thread-exec"] {
        let dir = TestDir::new();
        let mut child = dir.fixture(mode);
        let root_pid = dir.pid("root.pid");
        wait_for(
            || {
                fs::read_to_string(format!("/proc/{root_pid}/status")).is_ok_and(|s| {
                    s.lines()
                        .any(|l| l.starts_with("State:") && l.contains('Z'))
                })
            },
            "leader thread did not exit",
        );
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "foreground treated a thread exit as process completion"
        );
        fs::write(dir.0.join("release"), b"").unwrap();
        assert!(child.wait().success());
        assert_eq!(read_all(child.0.stdout.take().unwrap()), b"thread-output\n");
        dir.collection_finished();
        let events = dir.events();
        assert!(events.iter().all(|e| e["kind"] != "process.clone"));
        assert_eq!(
            events
                .iter()
                .filter(|e| e["kind"] == "process.exit")
                .count(),
            1
        );
        assert!(
            events
                .iter()
                .filter(|e| e["kind"] == "process.exit")
                .all(|e| e["data"]["pid"] == root_pid)
        );
        if mode == "thread-exec" {
            let instances: std::collections::HashSet<_> = events
                .iter()
                .filter(|e| e["kind"] == "process.exec")
                .map(|e| {
                    assert_eq!(e["data"]["pid"], root_pid);
                    e["data"]["process_instance_id"].as_u64().unwrap()
                })
                .collect();
            assert_eq!(
                instances.len(),
                1,
                "exec from a worker must retain the process identity"
            );
            assert!(
                events
                    .iter()
                    .any(|e| e["kind"] == "process.exec" && e["data"]["argv"][0] == "thread-exec")
            );
        }
    }
}

#[test]
fn preserves_sigstop_and_sigcont() {
    let dir = TestDir::new();
    let mut child = dir.fixture("stop");
    let pid = dir.pid("root.pid");
    let mut output = BufReader::new(child.0.stdout.take().unwrap());
    let mut line = String::new();
    output.read_line(&mut line).unwrap();
    assert_eq!(line, "ready\n");
    wait_for(
        || {
            fs::read_to_string(format!("/proc/{pid}/status")).is_ok_and(|s| {
                s.lines().any(|l| {
                    l.starts_with("State:")
                        && matches!(l.split_whitespace().nth(1), Some("T" | "t"))
                })
            })
        },
        "target did not remain stopped",
    );
    assert!(child.0.try_wait().unwrap().is_none());
    kill(Pid::from_raw(pid), Signal::SIGCONT).unwrap();
    assert!(child.wait().success());
    assert_eq!(read_all(output), b"resumed\n");
    dir.collection_finished();
}

#[test]
fn continuous_descendant_output_does_not_postpone_root_exit() {
    let dir = TestDir::new();
    let mut child = dir.fixture("busy");
    let output = child.0.stdout.take().unwrap();
    let reader = thread::spawn(move || read_all(output));
    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            break None;
        }
        thread::sleep(Duration::from_millis(10));
    };
    fs::write(dir.0.join("release"), b"").unwrap();
    child.wait();
    let output = reader.join().unwrap();
    assert_eq!(status.and_then(|s| s.code()), Some(31));
    assert!(
        output
            .windows(b"root-output\n".len())
            .any(|w| w == b"root-output\n")
    );
    dir.collection_finished();
    assert!(dir.read("stdout.log").starts_with(&output));
}

#[test]
fn forwards_sigterm_to_the_root_process_group() {
    let dir = TestDir::new();
    let mut child = dir.fixture("wait");
    dir.pid("root.pid");
    kill(Pid::from_raw(child.0.id() as i32), Signal::SIGTERM).unwrap();
    assert_eq!(child.wait().code(), Some(143));
    dir.collection_finished();
    assert_eq!(dir.read("exit-code.txt"), b"143\n");
}

#[test]
fn unexpected_foreground_death_terminates_the_tracee() {
    let dir = TestDir::new();
    let mut child = dir.fixture("wait");
    let pid = dir.pid("root.pid");
    child.0.kill().unwrap();
    child.wait();
    wait_for(
        || {
            fs::read_to_string(format!("/proc/{pid}/status")).map_or(true, |s| {
                s.lines()
                    .any(|l| l.starts_with("State:") && l.contains('Z'))
            })
        },
        "tracee survived loss of its foreground client",
    );
    wait_for(
        || dir.collection_state().as_deref() == Some("failed"),
        "collector did not record the disconnect",
    );
}

#[test]
fn process_events_exclude_the_internal_launch_helper() {
    for _ in 0..12 {
        let dir = TestDir::new();
        let output = run(&dir, &["-c", "", "--output-dir", "run"], b"");
        assert!(output.status.success());
        dir.collection_finished();
        let execs: Vec<_> = dir
            .events()
            .into_iter()
            .filter(|e| e["kind"] == "process.exec")
            .collect();
        assert_eq!(execs.len(), 1, "unexpected helper exec: {execs:?}");
        assert_eq!(execs[0]["data"]["argv"][0], "/bin/sh");
    }
}

#[test]
fn foreground_death_is_detected_while_target_stdin_is_blocked() {
    let dir = TestDir::new();
    let mut child = Running(
        dir.command()
            .args([
                "-c",
                "echo $$ > root.pid; while [ ! -f release ]; do sleep 0.02; done",
                "--output-dir",
                "run",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut input = child.0.stdin.take().unwrap();
    let writer = thread::spawn(move || {
        let _ = input.write_all(&vec![b'x'; 1024 * 1024]);
    });
    let pid = dir.pid("root.pid");
    let target_input = fs::File::open(format!("/proc/{pid}/fd/0")).unwrap();
    let capacity =
        nix::fcntl::fcntl(&target_input, nix::fcntl::FcntlArg::F_GETPIPE_SZ).unwrap() as u64;
    drop(target_input);
    // A chunk beyond pipe capacity is logged before its blocked write.
    wait_for(
        || fs::metadata(dir.0.join("run/stdin.log")).is_ok_and(|m| m.len() > capacity),
        "input never filled target pipe",
    );
    child.0.kill().unwrap();
    child.wait();
    writer.join().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let stopped = loop {
        let stopped = fs::read_to_string(format!("/proc/{pid}/status")).map_or(true, |s| {
            s.lines()
                .any(|l| l.starts_with("State:") && l.contains('Z'))
        });
        if stopped || Instant::now() >= deadline {
            break stopped;
        }
        thread::sleep(Duration::from_millis(10));
    };
    fs::write(dir.0.join("release"), b"").unwrap();
    assert!(stopped, "blocked stdin hid the foreground disconnect");
}

#[test]
fn refuses_to_execute_the_command_when_ptrace_is_denied() {
    let dir = TestDir::new();
    dir.compile_fixture();
    let output = Command::new(dir.0.join("lifetime"))
        .args(["deny-ptrace", env!("CARGO_BIN_EXE_agenttomo-box")])
        .current_dir(&dir.0)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(125), "{:?}", output.stderr);
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot trace child"));
    assert!(!dir.0.join("should-not-exist").exists());
    assert_eq!(dir.collection_state().as_deref(), Some("failed"));
    assert!(dir.read("exit-code.txt").is_empty());
}
