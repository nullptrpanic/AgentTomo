//! The only adapter that knows Codex installation, configuration and CLI flags.
mod credentials;
use std::{
    io,
    path::{Path, PathBuf},
};

use super::adapter::{
    self, Adapter, AgentAdapter, Launch, LaunchRequest, Metadata, ObservationRules, Prepared,
    RuntimeOutput, RuntimeReader,
};
use futures_util::future::BoxFuture;
use std::sync::Arc;

const MODEL: &str = "gpt-6-sol";
const REASONING_EFFORT: &str = "high";

pub struct Codex;

impl AgentAdapter for Codex {
    fn metadata(&self) -> Metadata {
        Metadata {
            supports_security_tests: true,
            id: "codex",
            name: "Codex",
            avatar: ">_",
            authentication_hint: "Codex authentication failed. Check the server's Codex login. A new sandbox reloads that login.",
        }
    }
    fn observation_rules(&self) -> ObservationRules {
        ObservationRules {
            runtime_executables: vec![
                format!("{DISTRIBUTION}/bin/codex"),
                format!("{DISTRIBUTION}/bin/codex-code-mode-host"),
            ],
            private_paths: vec![
                HOME.into(),
                "/root/.agenttomo-codex".into(),
                "/root/.agenttomo-config".into(),
            ],
        }
    }

    fn discover(
        &self,
        config: Option<&adapter::AgentConfig>,
    ) -> io::Result<Arc<dyn adapter::Installation>> {
        if config.is_some_and(|entry| entry.apikey.is_some() || entry.endpoint.is_some()) {
            return Err(io::Error::other(
                "Codex uses the host login; only path is configurable",
            ));
        }
        Installation::discover(config.and_then(|entry| entry.path.as_deref()))
            .map(|installation| Arc::new(installation) as Arc<dyn adapter::Installation>)
            .ok_or_else(|| io::Error::other("cannot locate Codex; check the executable path"))
    }
    fn home(&self) -> &'static Path {
        Path::new(HOME)
    }
    fn configuration_file(&self, name: &str) -> bool {
        sandbox_configuration_file(name)
    }
    fn configure(&self, command: &mut tokio::process::Command) -> io::Result<()> {
        configure(command)
    }
    fn launch(&self, request: &LaunchRequest<'_>) -> io::Result<Launch> {
        Ok(Launch {
            command: command(request.conversation)?,
            input: request.prompt.as_bytes().to_vec(),
        })
    }
    fn read_output(
        &self,
        input: RuntimeReader,
        diagnostics: tokio::sync::mpsc::Sender<String>,
    ) -> BoxFuture<'_, io::Result<RuntimeOutput>> {
        Box::pin(read_runtime_output(input, diagnostics))
    }
}

#[derive(Clone)]
pub struct Installation {
    pub distribution: PathBuf,
    pub home: PathBuf,
}

impl adapter::Installation for Installation {
    fn adapter(&self) -> Adapter {
        Adapter::new(&Codex)
    }
    fn mounts(&self) -> io::Result<Vec<(PathBuf, String)>> {
        // A native installation can live in ~/bin. Share only runtime components,
        // at their original relative locations so Codex can find its helpers.
        let root = self.distribution.canonicalize()?;
        let mut private_paths = Vec::new();
        for path in [self.home.clone(), self.home.join("auth.json")] {
            match path.canonicalize() {
                Ok(path) => private_paths.push(path),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        let mut mounts = Vec::new();
        for (relative, directory) in [
            ("bin/codex", false),
            ("bin/codex-code-mode-host", false),
            ("codex-package.json", false),
            ("codex-path/rg", false),
            ("codex-resources/bwrap", false),
            ("codex-resources/zsh", true),
            ("codex-resources/voice", true),
        ] {
            let path = root.join(relative);
            let metadata = match path.symlink_metadata() {
                Err(e) if e.kind() == io::ErrorKind::NotFound && relative != "bin/codex" => {
                    continue;
                }
                other => other?,
            };
            if path.canonicalize()? != path
                || !(directory && metadata.is_dir() || !directory && metadata.is_file())
                || private_paths
                    .iter()
                    .any(|private| private.starts_with(&path))
            {
                return Err(io::Error::other(format!(
                    "invalid Codex runtime component: {relative}"
                )));
            }
            mounts.push((path, format!("{DISTRIBUTION}/{relative}")));
        }
        Ok(mounts)
    }
    fn snapshot(&self) -> BoxFuture<'_, io::Result<Prepared>> {
        Box::pin(self.snapshot())
    }
}

impl Installation {
    fn discover(path: Option<&Path>) -> Option<Self> {
        let home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))?;
        let candidates = if let Some(path) = path {
            vec![path.to_owned()]
        } else {
            let mut found = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .map(|directory| directory.join("codex"))
                .collect::<Vec<_>>();
            if let Ok(versions) = std::fs::read_dir("/opt/nvm/versions/node") {
                found.extend(
                    versions
                        .flatten()
                        .map(|entry| entry.path().join("bin/codex")),
                );
            }
            found
        };
        for candidate in candidates {
            let Ok(candidate) = candidate.canonicalize() else {
                continue;
            };
            if !candidate.is_file() {
                continue;
            }
            if let Some(root) = candidate.parent().and_then(Path::parent) {
                if candidate == root.join("bin/codex")
                    && root.join("bin/codex-code-mode-host").is_file()
                {
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
    pub async fn snapshot(&self) -> io::Result<Prepared> {
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
        let mut prepared = credentials::prepare(&self.home, files, std::env::vars().collect())?;
        // Pin session-local defaults, including named profiles, independently of host choices.
        for (name, contents) in &mut prepared.files {
            if !name.ends_with(".toml") {
                continue;
            }
            let mut config: toml::Table = toml::from_str(contents)
                .map_err(|_| io::Error::other("invalid sanitized Codex configuration"))?;
            config.insert("model".into(), MODEL.into());
            config.insert("model_reasoning_effort".into(), REASONING_EFFORT.into());
            *contents = toml::to_string(&config)
                .map_err(|_| io::Error::other("cannot serialize sandbox Codex configuration"))?;
        }
        Ok(prepared)
    }
}

pub const HOME: &str = "/records/codex";
pub const DISTRIBUTION: &str = "/opt/agenttomo-codex";

pub fn configuration_file(name: &str) -> bool {
    !name.contains('/') && matches!(name, "auth.json" | "config.toml" | "models_cache.json")
        || (!name.contains('/') && name.ends_with(".config.toml"))
}

pub fn sandbox_configuration_file(name: &str) -> bool {
    configuration_file(name) || name == "agenttomo-environment.json"
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

fn command(thread: Option<&str>) -> io::Result<tokio::process::Command> {
    let mut command = tokio::process::Command::new("/opt/agenttomo-codex/bin/codex");
    configure(&mut command)?;
    command.arg("exec");
    if let Some(thread) = thread {
        command.arg("resume").arg(thread);
    }
    command
        .args(["--model", MODEL, "--config"])
        .arg(format!("model_reasoning_effort=\"{REASONING_EFFORT}\""))
        .args([
            "--skip-git-repo-check",
            "--dangerously-bypass-approvals-and-sandbox",
            "--json",
            "-",
        ]);
    Ok(command)
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
            output.conversation_id = value["thread_id"].as_str().map(str::to_owned);
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
        assert_eq!(output.conversation_id.as_deref(), Some("thread-1"));
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
        assert_eq!(config["model"].as_str(), Some("gpt-6-sol"));
        assert_eq!(config["model_reasoning_effort"].as_str(), Some("high"));
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

    #[tokio::test]
    async fn snapshot_pins_sandbox_model_across_host_changes_and_named_profiles() {
        let home = std::env::temp_dir().join(format!("agenttomo-model-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir(&home).await.unwrap();
        let installation = Installation {
            distribution: PathBuf::new(),
            home: home.clone(),
        };
        let snapshot = installation.snapshot().await.unwrap().files;
        let config: toml::Value = toml::from_str(&snapshot["config.toml"]).unwrap();
        assert_eq!(
            config.get("model").and_then(toml::Value::as_str),
            Some("gpt-6-sol")
        );
        assert_eq!(config["model_reasoning_effort"].as_str(), Some("high"));
        assert!(!home.join("config.toml").exists());

        for (model, effort) in [("host-first", "low"), ("host-second", "xhigh")] {
            let base = format!(
                "model = '{model}'\nmodel_reasoning_effort = '{effort}'\nsandbox_mode = 'read-only'\n"
            );
            let profile = format!(
                "model = '{model}-profile'\nmodel_reasoning_effort = 'medium'\nsandbox_mode = 'workspace-write'\n"
            );
            for (name, source) in [("config.toml", &base), ("alternate.config.toml", &profile)] {
                tokio::fs::write(home.join(name), source).await.unwrap();
            }
            let snapshot = installation.snapshot().await.unwrap().files;
            for (name, source, sandbox_mode) in [
                ("config.toml", &base, "read-only"),
                ("alternate.config.toml", &profile, "workspace-write"),
            ] {
                let config: toml::Value = toml::from_str(&snapshot[name]).unwrap();
                assert_eq!(config["model"].as_str(), Some("gpt-6-sol"), "{name}");
                assert_eq!(
                    config["model_reasoning_effort"].as_str(),
                    Some("high"),
                    "{name}"
                );
                assert_eq!(
                    config["sandbox_mode"].as_str(),
                    Some(sandbox_mode),
                    "{name}"
                );
                assert_eq!(
                    tokio::fs::read_to_string(home.join(name)).await.unwrap(),
                    *source
                );
            }
        }
        tokio::fs::remove_dir_all(home).await.unwrap();
    }

    #[test]
    fn runtime_mounts_exclude_neighboring_files_from_a_home_bin_installation() {
        use super::adapter::Installation as _;
        let root = std::env::temp_dir().join(format!("agenttomo-install-{}", uuid::Uuid::new_v4()));
        let home = root.join(".codex");
        for directory in ["bin", ".codex", "codex-path", "codex-resources/voice"] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
        }
        for name in [
            "bin/codex",
            "bin/codex-code-mode-host",
            "codex-package.json",
            "codex-path/rg",
            "bin/unrelated",
            ".codex/private-fixture",
        ] {
            std::fs::write(root.join(name), b"fixture").unwrap();
        }
        let install = Installation {
            distribution: root.clone(),
            home,
        };
        let mounts = install.mounts().unwrap();
        for (source, destination) in &mounts {
            let relative = source.strip_prefix(&root).unwrap().to_str().unwrap();
            assert!(
                [
                    "bin/codex",
                    "bin/codex-code-mode-host",
                    "codex-package.json",
                    "codex-path/rg",
                    "codex-resources/voice"
                ]
                .contains(&relative)
            );
            assert_eq!(destination, &format!("{DISTRIBUTION}/{relative}"));
        }
        assert_eq!(mounts.len(), 5);
        std::os::unix::fs::symlink(&install.home, root.join("codex-resources/zsh")).unwrap();
        assert!(install.mounts().is_err());
        std::fs::remove_file(root.join("codex-resources/zsh")).unwrap();
        let target = root.join("codex-resources/voice/fixture");
        std::fs::write(&target, b"fixture").unwrap();
        std::os::unix::fs::symlink(target, install.home.join("auth.json")).unwrap();
        assert!(install.mounts().is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
