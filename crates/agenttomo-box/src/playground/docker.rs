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

pub async fn check(image: &str) -> io::Result<String> {
    let output =
        execute(Command::new("docker").args(["image", "inspect", "--format", "{{.Id}}", image]))
            .await?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "cannot inspect local Docker image {image}; check Docker access and install the image first (no automatic pull): {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8(output.stdout)
        .map_err(io::Error::other)?
        .trim()
        .to_owned())
}

pub async fn supports_browser(image: &str) -> io::Result<bool> {
    let name = format!("agenttomo-probe-{}", uuid::Uuid::new_v4().simple());
    let output = execute(
        Command::new("docker")
            .args([
                "run",
                "--pull=never",
                "--rm",
                "--name",
                &name,
                "--network=none",
                "--read-only",
                "--cap-drop=ALL",
                "--security-opt=no-new-privileges",
                "--user",
            ])
            .arg(format!(
                "{}:{}",
                nix::unistd::getuid(),
                nix::unistd::getgid()
            ))
            .args([
                "--entrypoint",
                "/bin/sh",
                image,
                "-c",
                r#"if [ ! -e /usr/local/bin/agenttomo-browser-session ] &&
   [ ! -e /usr/local/bin/agenttomo-desktop ] &&
   [ ! -e /usr/local/bin/agenttomo-chrome ]; then
    exit 10
fi
for helper in agenttomo-browser-session agenttomo-desktop agenttomo-chrome; do
    if [ ! -x "/usr/local/bin/$helper" ]; then
        printf 'Missing executable browser helper: /usr/local/bin/%s\n' "$helper" >&2
        exit 1
    fi
done"#,
            ]),
    )
    .await;
    // Also remove the named probe after a timeout or failed Docker client.
    remove(&name).await?;
    let output = output?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(10) => Ok(false),
        _ => Err(io::Error::other(format!(
            "cannot detect browser support in Docker image {image}: {}",
            command_failure(&output)
        ))),
    }
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

pub struct Create<'a> {
    pub name: &'a str,
    pub image: &'a str,
    pub binary: &'a Path,
    pub tmpfs_size_mib: u32,
    pub preview_limits: crate::recording::file_preview::Limits,
    pub http: crate::proxy::http::Options,
    pub browser: bool,
    pub rootfs: &'a crate::execution::rootfs::Options,
    pub codex: Option<&'a crate::agent::codex::Installation>,
    pub agent_id: &'a str,
}
pub async fn create(options: Create<'_>) -> io::Result<()> {
    let Create {
        name,
        image,
        binary,
        tmpfs_size_mib,
        preview_limits,
        http,
        browser,
        rootfs,
        codex,
        agent_id,
    } = options;
    let mut rootfs = rootfs.clone();
    if rootfs.enabled {
        rootfs.exemptions.push("/records".into());
        if browser {
            rootfs
                .exemptions
                .extend(["/tmp/.X11-unix".into(), "/tmp/agenttomo-desktop".into()]);
        }
    }
    let mut command = Command::new("docker");
    command
            .args([
                "create",
                "--pull=never",
                "--name",
                name,
                "--label",
                "io.agenttomo.playground=true",
                "--interactive",
                "--log-driver=none",
                "--init",
                "--cap-drop=ALL",
                // Root bootstrap needs credential mapping; DAC_OVERRIDE lets
                // the collector inspect the browser's private files. Target
                // capabilities are restricted in its own user namespace.
                "--cap-add=SETUID",
                "--cap-add=SETGID",
                "--cap-add=SETFCAP",
                "--cap-add=DAC_OVERRIDE",
                "--security-opt=no-new-privileges",
                // This development playground must allow nested user/net
                // namespaces. Network/mount administration is only available
                // in a new namespace owned by the launch helper.
                "--security-opt=seccomp=unconfined",
                "--security-opt=apparmor=unconfined",
                "--workdir",
                "/root",
                "--env",
                "HOME=/root",
                "--env",
                "USER=root",
                "--env",
                "LOGNAME=root",
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
            .args(if browser && rootfs.enabled {
                vec![
                    "--tmpfs", "/tmp/.X11-unix:rw,nosuid,nodev,noexec,size=1m,mode=1777",
                    "--tmpfs", "/tmp/agenttomo-desktop:rw,nosuid,nodev,noexec,size=16m,mode=0711,uid=65534,gid=65534",
                ]
            } else { vec![] })
            .arg("--user")
            .arg("0:0")
            .arg("--mount")
            .arg(mount(binary, "/agenttomo-box", true)?)
            .arg("--tmpfs")
            // Codex installs its interactive daemon under its private home here.
            .arg(format!("/records:rw,exec,nosuid,nodev,size={tmpfs_size_mib}m,mode=0711"))
            ;
    if let Some(codex) = codex {
        command
            .arg("--mount")
            .arg(mount(&codex.distribution, "/opt/agenttomo-codex", true)?)
            .arg("--env")
            .arg("AGENTTOMO_WORKSPACE=1")
            .arg("--env")
            .arg("SSL_CERT_FILE=/records/control/upstream-certs.pem")
            .arg("--env")
            .arg(format!("AGENTTOMO_AGENT_ID={agent_id}"));
    }
    let output = execute(
        command
            .args([
                "--entrypoint",
                "/agenttomo-box",
                image,
                "__collector",
                "--terminal",
                "--output-dir",
                &format!("/records/{agent_id}/run"),
                "-c",
                if codex.is_some() && browser {
                    "exec /agenttomo-box __workspace --browser"
                } else if codex.is_some() {
                    "exec /agenttomo-box __workspace"
                } else if browser {
                    "exec /usr/local/bin/agenttomo-browser-session"
                } else {
                    "exec /bin/bash --noprofile --norc -i"
                },
            ])
            .arg("--playground-session")
            .arg(agent_id)
            .arg("--file-preview-max-bytes")
            .arg(preview_limits.max_bytes.to_string())
            .arg("--file-preview-total-bytes")
            .arg(preview_limits.total_bytes.to_string())
            .args(http.arguments())
            .args(rootfs.arguments()),
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
