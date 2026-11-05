//! Docker owns container isolation; this adapter owns only named playground containers.

use std::io;
use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::time::timeout;

async fn execute(command: &mut Command) -> io::Result<Output> {
    command.stdin(Stdio::null()).kill_on_drop(true);
    timeout(Duration::from_secs(30), command.output())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Docker command timed out"))?
}

pub async fn check(image: &str) -> io::Result<()> {
    let output =
        execute(Command::new("docker").args(["image", "inspect", "--format", "{{.Id}}", image]))
            .await?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "cannot inspect local Docker image {image}; check Docker access and install the image first (no automatic pull): {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

fn mount(source: &Path, destination: &str, readonly: bool) -> io::Result<String> {
    let source = source.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Docker bind paths must be UTF-8",
        )
    })?;
    // --mount is CSV, not shell syntax. Quote commas and quotes in host paths.
    Ok(format!(
        "type=bind,\"src={}\",dst={destination}{}",
        source.replace('"', "\"\""),
        if readonly { ",readonly" } else { "" }
    ))
}

pub async fn create(name: &str, image: &str, binary: &Path, directory: &Path) -> io::Result<()> {
    let output = execute(
        Command::new("docker")
            .args([
                "create",
                "--pull=never",
                "--name",
                name,
                "--label",
                "io.agenttomo.playground=true",
                "--interactive",
                "--init",
                "--cap-drop=ALL",
                "--security-opt=no-new-privileges",
                "--workdir",
                "/tmp",
                "--env",
                "HOME=/tmp",
                "--env",
                "TERM=xterm-256color",
                "--env",
                "PS1=\\u@playground:\\w\\$ ",
            ])
            .arg("--user")
            .arg(format!(
                "{}:{}",
                nix::unistd::getuid(),
                nix::unistd::getgid()
            ))
            .arg("--mount")
            .arg(mount(binary, "/agenttomo-box", true)?)
            .arg("--mount")
            .arg(mount(directory, "/records", false)?)
            .args([
                "--entrypoint",
                "/agenttomo-box",
                image,
                "__collector",
                "--terminal",
                "--output-dir",
                "/records/run",
                "-c",
                "exec /bin/bash --noprofile --norc -i",
            ]),
    )
    .await?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "Docker create failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

pub fn start(name: &str) -> io::Result<Child> {
    Command::new("docker")
        .args(["start", "--attach", "--interactive", name])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
}

pub async fn remove(name: &str) -> io::Result<()> {
    let output = execute(Command::new("docker").args(["rm", "--force", "--volumes", name])).await?;
    if output.status.success()
        || String::from_utf8_lossy(&output.stderr).contains("No such container")
    {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "cannot remove playground container {name}: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}
