#![cfg(target_os = "linux")]

use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::{io::AsyncWriteExt, process::Command, time::timeout};

struct Container(String);
impl Drop for Container {
    fn drop(&mut self) {
        let _ = std::process::Command::new("docker")
            .args(["rm", "--force", &self.0])
            .output();
    }
}

struct ReadonlyFixture(PathBuf);
impl Drop for ReadonlyFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// Runs the real launcher inside the same capability model as Playground.
// Outside assertions distinguish selected private directories from shared paths.
#[tokio::test]
#[ignore = "requires Docker and the local agenttomo-playground:24.04 image"]
async fn root_overlay_covers_only_whole_top_level_directories_and_ssl() {
    run_fixture("root_overlay.sh", "ROOT_OVERLAY_OK").await;
}

#[tokio::test]
#[ignore = "requires Docker and the local agenttomo-playground:24.04 image"]
async fn root_overlay_rejects_invalid_layouts_before_user_code() {
    run_fixture("root_overlay_failures.sh", "ROOT_OVERLAY_FAILURES_OK").await;
}

#[tokio::test]
#[ignore = "requires Docker and the local agenttomo-playground:24.04 image"]
async fn root_overlay_lasts_until_descendants_finish_and_bounds_writes() {
    run_fixture("root_overlay_lifetime.sh", "ROOT_OVERLAY_LIFETIME_OK").await;
}

#[tokio::test]
#[ignore = "requires Docker and the local agenttomo-playground:24.04 image"]
async fn exempting_tmp_does_not_expose_the_overlay_backing_store() {
    run_fixture("root_overlay_tmp.sh", "ROOT_OVERLAY_TMP_OK").await;
}

async fn run_fixture(fixture: &str, marker: &str) {
    let container = Container(format!("agenttomo-root-test-{}", uuid::Uuid::new_v4()));
    let readonly = ReadonlyFixture(
        std::env::temp_dir().join(format!("agenttomo-root-source-{}", uuid::Uuid::new_v4())),
    );
    std::fs::create_dir_all(readonly.0.join("data")).unwrap();
    std::fs::write(readonly.0.join("license"), "readonly-original").unwrap();
    let binary = std::fs::canonicalize(env!("CARGO_BIN_EXE_agenttomo-box")).unwrap();
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture);
    let mut child = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--init",
            "-i",
            "--name",
            &container.0,
            "--pull=never",
            "--network=none",
            "--user=0:0",
            "--cap-drop=ALL",
            "--cap-add=SETUID",
            "--cap-add=SETGID",
            "--cap-add=SETFCAP",
            "--cap-add=DAC_OVERRIDE",
            "--security-opt=no-new-privileges",
            "--security-opt=seccomp=unconfined",
            "--security-opt=apparmor=unconfined",
            "--mount",
        ])
        .arg(format!(
            "type=bind,src={},dst=/agenttomo-box,readonly",
            binary.display()
        ))
        .arg("--mount")
        .arg(format!(
            "type=bind,src={},dst=/readonly,readonly",
            readonly.0.display()
        ))
        .args(["--tmpfs", "/readonly/data:rw,mode=1777"])
        .args(["--entrypoint=/bin/sh", "agenttomo-playground:24.04", "-eu"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&std::fs::read(script).unwrap())
        .await
        .unwrap();
    let output = timeout(Duration::from_secs(90), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains(marker));
}
