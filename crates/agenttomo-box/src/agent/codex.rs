//! The only adapter that knows Codex installation, configuration and CLI flags.
pub mod credentials;
use std::{
    io,
    path::{Path, PathBuf},
};

pub fn is_runtime(argv: &serde_json::Value) -> bool {
    argv[0]
        .as_str()
        .and_then(|path| Path::new(path).file_name())
        .is_some_and(|name| name == "codex" || name == "codex-code-mode-host")
}

#[derive(Clone)]
pub struct Installation {
    pub distribution: PathBuf,
    pub home: PathBuf,
}

impl Installation {
    pub fn discover() -> Option<Self> {
        let home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))?;
        let mut candidates = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|directory| directory.join("codex"))
            .collect::<Vec<_>>();
        if let Ok(versions) = std::fs::read_dir("/opt/nvm/versions/node") {
            candidates.extend(
                versions
                    .flatten()
                    .map(|entry| entry.path().join("bin/codex")),
            );
        }
        for candidate in candidates {
            let Ok(candidate) = candidate.canonicalize() else {
                continue;
            };
            if let Some(root) = candidate.parent().and_then(Path::parent) {
                if root.join("bin/codex-code-mode-host").is_file() {
                    return Some(Self {
                        distribution: root.into(),
                        home,
                    });
                }
                // The npm CLI delegates to a platform-specific native package.
                if let Ok(packages) = std::fs::read_dir(root.join("node_modules/@openai")) {
                    for package in packages.flatten() {
                        if let Ok(vendors) = std::fs::read_dir(package.path().join("vendor")) {
                            for vendor in vendors.flatten() {
                                if vendor.path().join("bin/codex").is_file() {
                                    return Some(Self {
                                        distribution: vendor.path(),
                                        home,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
        None
    }

    /// Prepare sandbox configuration and keep authentication grants on the host.
    pub async fn snapshot(&self) -> io::Result<credentials::Prepared> {
        let mut files = std::collections::BTreeMap::new();
        let mut total = 0;
        let entries = match std::fs::read_dir(&self.home) {
            Ok(entries) => entries.collect::<io::Result<Vec<_>>>()?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        for entry in entries {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if configuration_file(name) {
                if !std::fs::metadata(entry.path())?.is_file() {
                    return Err(io::Error::other("invalid Codex configuration file"));
                }
                use tokio::io::AsyncReadExt;
                let mut bytes = Vec::new();
                tokio::fs::File::open(entry.path())
                    .await?
                    .take(16 * 1024 * 1024 + 1)
                    .read_to_end(&mut bytes)
                    .await?;
                total += bytes.len();
                if bytes.len() > 16 * 1024 * 1024 || total > 24 * 1024 * 1024 {
                    return Err(io::Error::other(
                        "Codex configuration exceeds bootstrap size limit",
                    ));
                }
                files.insert(
                    name.to_owned(),
                    String::from_utf8(bytes).map_err(io::Error::other)?,
                );
            }
        }
        credentials::prepare(&self.home, files, std::env::vars().collect())
    }
}

pub const HOME: &str = "/records/codex";

pub fn configuration_file(name: &str) -> bool {
    !name.contains('/') && matches!(name, "auth.json" | "config.toml" | "models_cache.json")
        || (!name.contains('/') && name.ends_with(".config.toml"))
}

pub fn configure(command: &mut tokio::process::Command) -> io::Result<()> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(PathBuf::from("/opt/agenttomo-codex/bin"))
            .chain(std::env::split_paths(&path)),
    )
    .map_err(io::Error::other)?;
    command
        .env("PATH", path)
        .env("CODEX_HOME", HOME)
        .env("CODEX_CA_CERTIFICATE", "/etc/ssl/certs/ca-certificates.crt")
        .env("SSL_CERT_FILE", "/etc/ssl/certs/ca-certificates.crt");
    let environment: Vec<(String, String)> =
        serde_json::from_slice(&std::fs::read("/records/codex/agenttomo-environment.json")?)?;
    command.envs(environment);
    Ok(())
}

pub fn command(scope: &super::Scope, thread: Option<&str>) -> io::Result<tokio::process::Command> {
    let mut command = tokio::process::Command::new("/opt/agenttomo-codex/bin/codex");
    configure(&mut command)?;
    command.arg("exec");
    if let Some(thread) = thread {
        command.arg("resume").arg(thread);
    }
    command
        .args([
            "--skip-git-repo-check",
            "--dangerously-bypass-approvals-and-sandbox",
            "--json",
            "-",
        ])
        .env("AGENTTOMO_AGENT_ID", &scope.agent_instance_id)
        .env("AGENTTOMO_TASK_ID", &scope.task_id);
    Ok(command)
}

#[derive(Default)]
pub struct RuntimeOutput {
    pub thread: Option<String>,
    pub error: Option<String>,
}

/// CLI lifecycle diagnostics only. Model and tool evidence comes from traffic.
pub async fn read_runtime_output(
    input: impl tokio::io::AsyncRead + Unpin,
    diagnostics: tokio::sync::mpsc::Sender<String>,
) -> io::Result<RuntimeOutput> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    let mut input = tokio::io::BufReader::new(input);
    let mut output = RuntimeOutput::default();
    let mut line = Vec::new();
    let mut discarding = false;
    loop {
        line.clear();
        let count = (&mut input)
            .take(64 * 1024)
            .read_until(b'\n', &mut line)
            .await?;
        if count == 0 {
            break;
        }
        let complete = line.ends_with(b"\n");
        if discarding {
            discarding = !complete;
            continue;
        }
        // Drain oversized tool records so later lifecycle errors still arrive.
        if !complete && count == 64 * 1024 {
            discarding = true;
            continue;
        }
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&line) else {
            continue;
        };
        if value["type"] == "thread.started" {
            output.thread = value["thread_id"].as_str().map(str::to_owned);
        }
        let error = match value["type"].as_str() {
            Some("error") => value["message"].as_str(),
            Some("turn.failed") => value["error"]["message"].as_str(),
            _ => None,
        };
        if let Some(error) = error {
            let error = error.chars().take(8192).collect::<String>();
            if output.error.as_ref() != Some(&error) {
                diagnostics
                    .send(error.clone())
                    .await
                    .map_err(io::Error::other)?;
            }
            output.error = Some(error);
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn runtime_errors_arrive_before_exit_after_an_oversized_tool_line() {
        use tokio::io::AsyncWriteExt;
        let (mut write, read) = tokio::io::duplex(256 * 1024);
        let (diagnostics, mut received) = tokio::sync::mpsc::channel(4);
        let output = tokio::spawn(read_runtime_output(read, diagnostics));
        write
            .write_all(b"{\"type\":\"thread.started\",\"thread_id\":\"thread-1\"}\n")
            .await
            .unwrap();
        write.write_all(&vec![b'x'; 128 * 1024]).await.unwrap();
        write
            .write_all(b"\n{\"type\":\"error\",\"message\":\"HTTP 401 Unauthorized\"}\n")
            .await
            .unwrap();
        let error = tokio::time::timeout(std::time::Duration::from_secs(1), received.recv())
            .await
            .unwrap();
        assert_eq!(error.as_deref(), Some("HTTP 401 Unauthorized"));
        write
            .write_all(
                b"{\"type\":\"turn.failed\",\"error\":{\"message\":\"HTTP 401 Unauthorized\"}}\n",
            )
            .await
            .unwrap();
        drop(write);
        let output = output.await.unwrap().unwrap();
        assert_eq!(output.thread.as_deref(), Some("thread-1"));
        assert_eq!(output.error.as_deref(), Some("HTTP 401 Unauthorized"));
        assert!(received.recv().await.is_none());
    }

    #[tokio::test]
    async fn snapshot_follows_configuration_links_without_host_staging_or_source_changes() {
        let root =
            std::env::temp_dir().join(format!("agenttomo-codex-config-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let session = root.join("session");
        tokio::fs::create_dir_all(&home).await.unwrap();
        let source = root.join("source.toml");
        tokio::fs::write(&source, b"model = 'example'\n")
            .await
            .unwrap();
        std::os::unix::fs::symlink(&source, home.join("config.toml")).unwrap();
        let auth = b"{\"OPENAI_API_KEY\":\"local-fixture\"}";
        tokio::fs::write(home.join("auth.json"), auth)
            .await
            .unwrap();
        let installation = Installation {
            distribution: PathBuf::new(),
            home,
        };
        let mut snapshot = installation.snapshot().await.unwrap().files;
        assert!(!snapshot["auth.json"].contains("local-fixture"));
        let config: toml::Value = toml::from_str(&snapshot["config.toml"]).unwrap();
        assert_eq!(config["model"].as_str(), Some("example"));
        snapshot.insert("auth.json".into(), "private change".into());
        assert_eq!(
            tokio::fs::read(installation.home.join("auth.json"))
                .await
                .unwrap(),
            auth
        );
        assert_eq!(
            tokio::fs::read(&source).await.unwrap(),
            b"model = 'example'\n"
        );
        assert!(!session.exists(), "a snapshot must not stage on the host");
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}
