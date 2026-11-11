use serde_json::Value;
use std::{
    path::PathBuf,
    process::{Command, Output},
    time::{Duration, Instant},
};

pub struct Fixture(pub PathBuf);

impl Fixture {
    pub fn compile(source: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("agenttomo-collection-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        let output = Command::new("cc")
            .args(["-Wall", "-Wextra", "-Werror", "-pthread"])
            .arg(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join(format!("tests/fixtures/{source}.c")),
            )
            .arg("-o")
            .arg(path.join("fixture"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Self(path)
    }

    pub fn run(&self, command: &str) -> Output {
        Self::wait(self.command(command))
    }

    pub fn command(&self, command: &str) -> Command {
        let mut process = Command::new(env!("CARGO_BIN_EXE_agenttomo-box"));
        process
            .args(["run", "--no-proxy", "--output-dir", "run", "-c", command])
            .current_dir(&self.0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        process
    }

    pub fn wait(mut command: Command) -> Output {
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("command did not finish");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        child.wait_with_output().unwrap()
    }

    pub fn events(&self) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let status = std::fs::read(self.0.join("run/collection-status.json"))
                .ok()
                .and_then(|data| serde_json::from_slice::<Value>(&data).ok());
            if let Some(status) = status {
                assert_ne!(status["state"], "failed", "{status}");
                if status["state"] == "complete" {
                    break;
                }
            }
            assert!(Instant::now() < deadline, "collection did not complete");
            std::thread::sleep(Duration::from_millis(10));
        }
        std::fs::read_to_string(self.0.join("run/events.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
