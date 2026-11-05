#![cfg(target_os = "linux")]

use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

struct Worker {
    child: Child,
    input: ChildStdin,
    messages: mpsc::Receiver<Value>,
    directory: PathBuf,
    output: Vec<u8>,
}

impl Worker {
    fn new(command: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "agenttomo-pty-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut child = Command::new(env!("CARGO_BIN_EXE_agenttomo-box"))
            .args(["__collector", "--terminal", "-c", command, "--output-dir"])
            .arg(&directory)
            .env("TERM", "xterm-256color")
            .env("PS1", "test$ ")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (sender, messages) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let Ok(line) = line else { break };
                let message = serde_json::from_str(&line).unwrap();
                if sender.send(message).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            input,
            messages,
            directory,
            output: Vec::new(),
        }
    }

    fn send(&mut self, message: Value) {
        writeln!(self.input, "{message}").unwrap();
        self.input.flush().unwrap();
    }

    fn next(&mut self) -> Value {
        let message = self
            .messages
            .recv_timeout(Duration::from_secs(5))
            .expect("worker did not emit a message");
        assert!(message.get("Error").is_none(), "{message}");
        if let Some(bytes) = message.get("Stdout") {
            self.output
                .extend(serde_json::from_value::<Vec<u8>>(bytes.clone()).unwrap());
        }
        message
    }

    fn until_output(&mut self, text: &str) {
        while !String::from_utf8_lossy(&self.output).contains(text) {
            self.next();
        }
    }

    fn finish(&mut self) -> u8 {
        loop {
            if let Some(code) = self.next().get("RootExit") {
                let code = code.as_u64().unwrap() as u8;
                let deadline = Instant::now() + Duration::from_secs(5);
                while self.child.try_wait().unwrap().is_none() {
                    assert!(Instant::now() < deadline, "worker did not complete");
                    std::thread::sleep(Duration::from_millis(10));
                }
                return code;
            }
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[test]
fn worker_provides_a_real_terminal_and_resizes_it() {
    let mut worker = Worker::new(
        "test -t 0 && test -t 1 && test -t 2 || exit 19; printf 'READY\\n'; read line; stty size; printf 'GOT:%s\\n' \"$line\"",
    );
    worker.until_output("READY");
    worker.send(json!({"Resize": {"rows": 37, "cols": 91}}));
    worker.send(json!({"Bytes": b"hello\n".to_vec()}));
    assert_eq!(worker.finish(), 0);
    let output = String::from_utf8_lossy(&worker.output);
    assert!(output.contains("37 91\r\n"), "{output}");
    assert!(output.contains("GOT:hello\r\n"), "{output}");
    assert_eq!(
        fs::read(worker.directory.join("stdout.log")).unwrap(),
        worker.output
    );
    assert!(
        fs::read(worker.directory.join("stderr.log"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn ctrl_c_interrupts_the_foreground_job_and_keeps_the_shell() {
    let mut worker = Worker::new("exec /bin/bash --noprofile --norc -i");
    worker.until_output("test$ ");
    worker.send(json!({"Bytes": b"sleep 20\n".to_vec()}));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let events = fs::read_to_string(worker.directory.join("events.jsonl")).unwrap();
        if events.contains("\"argv\":[\"sleep\",\"20\"]") {
            break;
        }
        assert!(Instant::now() < deadline, "sleep was not traced");
        std::thread::sleep(Duration::from_millis(10));
    }
    worker.send(json!({"Bytes": [3]}));
    worker.send(json!({"Bytes": b"printf 'AFTER_INTERRUPT\\n'; exit\n".to_vec()}));
    assert_eq!(worker.finish(), 0);
    assert!(String::from_utf8_lossy(&worker.output).contains("AFTER_INTERRUPT\r\n"));
    let events = fs::read_to_string(worker.directory.join("events.jsonl")).unwrap();
    assert!(
        events.contains("\"signal\":2"),
        "foreground job did not receive SIGINT"
    );
}
