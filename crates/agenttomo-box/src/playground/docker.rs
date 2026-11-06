//! Docker owns container isolation; this adapter owns only named playground containers.

use std::io;
use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::time::timeout;

async fn execute(command: &mut Command) -> io::Result<Output> {
    // Terminal Ctrl-C targets the foreground process group. Only the server
    // should handle it; repeated interrupts must not abort Docker cleanup.
    command
        .process_group(0)
        .stdin(Stdio::null())
        .kill_on_drop(true);
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

pub async fn create(
    name: &str,
    image: &str,
    binary: &Path,
    directory: &Path,
    preview_limits: crate::recording::file_preview::Limits,
    http: crate::proxy::http::Options,
    browser: bool,
) -> io::Result<()> {
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
                // This development playground must allow nested user/net
                // namespaces. Keep host capabilities dropped; rules are only
                // configured in a new namespace owned by the launch helper.
                "--security-opt=seccomp=unconfined",
                "--security-opt=apparmor=unconfined",
                "--workdir",
                "/tmp",
                "--env",
                "HOME=/tmp",
                "--env",
                "TERM=xterm-256color",
                "--env",
                "PS1=\\u@playground:\\w\\$ ",
            ])
            .args(if browser {
                vec!["--shm-size=256m"]
            } else {
                vec![]
            })
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
                if browser {
                    "exec /usr/local/bin/agenttomo-browser-session"
                } else {
                    "exec /bin/bash --noprofile --norc -i"
                },
            ])
            .arg("--file-preview-max-bytes")
            .arg(preview_limits.max_bytes.to_string())
            .arg("--file-preview-total-bytes")
            .arg(preview_limits.total_bytes.to_string())
            .args(http.arguments()),
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
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
}

pub async fn remove(name: &str) -> io::Result<()> {
    let failure =
        match execute(Command::new("docker").args(["rm", "--force", "--volumes", name])).await {
            Ok(output) if output.status.success() => return Ok(()),
            Ok(output) => command_failure(&output),
            Err(error) => error.to_string(),
        };
    // A failed/killed CLI does not undo a deletion accepted by the daemon.
    // Only a successful query can prove absence; connection/permission errors
    // must remain errors. Compare exact names because Docker's filter is partial.
    let verification = match execute(Command::new("docker").args([
        "container",
        "ls",
        "--all",
        "--filter",
        &format!("name={name}"),
        "--format",
        "{{.Names}}",
    ]))
    .await
    {
        Ok(output) if output.status.success() => {
            if !String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line == name)
            {
                return Ok(());
            }
            "container still exists".to_owned()
        }
        Ok(output) => format!(
            "cannot verify container state: {}",
            command_failure(&output)
        ),
        Err(error) => format!("cannot verify container state: {error}"),
    };
    Err(io::Error::other(format!(
        "cannot remove playground container {name}: {failure}; {verification}"
    )))
}

fn command_failure(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.trim().is_empty() {
        output.status.to_string()
    } else {
        format!("{}: {}", output.status, stderr.trim())
    }
}
