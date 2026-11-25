//! Installation and model credentials for the external AgentKit HTTP application.
mod credentials;
pub(super) mod http;
#[cfg(test)]
mod tests;
use super::adapter::{
    self, Adapter, AgentAdapter, Launch, LaunchRequest, Metadata, ObservationRules, Prepared,
};
use crate::proxy::credentials::{Grant, Policy, Scope, marker, secret_header};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use std::{
    io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::process::Command;

const PROGRAM: &str = "/opt/agenttomo-agentkit";
const CONFIG: &str = "model.json";
const ARK_BEIJING_BASE: &str = "https://ark.cn-beijing.volces.com/api/v3";
pub struct AgentKit;

#[derive(Serialize, Deserialize)]
struct Settings {
    model: String,
    base: String,
    placeholder: String,
}

struct Installed {
    binary: PathBuf,
    model: String,
    base: String,
    credential: credentials::Credential,
}

fn executable(path: &Path) -> Option<PathBuf> {
    let path = path.canonicalize().ok()?;
    let meta = path.metadata().ok()?;
    (meta.is_file() && meta.permissions().mode() & 0o111 != 0).then_some(path)
}

fn locate(variable: &str, name: &str) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(variable) {
        return executable(Path::new(&path));
    }
    std::env::split_paths(&std::env::var_os("PATH")?).find_map(|dir| executable(&dir.join(name)))
}

fn setting(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

impl AgentAdapter for AgentKit {
    fn metadata(&self) -> Metadata {
        Metadata {
            id: "agentkit",
            name: "agentkit",
            avatar: "ak",
            authentication_hint: "Check the host Ark credential and model endpoint.",
            supports_security_tests: true,
        }
    }
    fn conversation_server(&self) -> bool {
        true
    }
    fn discover(
        &self,
        config: Option<&adapter::AgentConfig>,
    ) -> io::Result<Arc<dyn adapter::Installation>> {
        let binary = match config.and_then(|entry| entry.path.as_deref()) {
            Some(path) => executable(path),
            None => locate("AGENTTOMO_AGENTKIT_BIN", "agenttomo-agentkit"),
        }
        .ok_or_else(|| io::Error::other("cannot locate the AgentKit executable; check path"))?;
        let (model, base) = match config {
            Some(entry) => {
                let model = entry
                    .endpoint
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        io::Error::other("endpoint must contain the Ark model endpoint ID")
                    })?;
                (model.to_owned(), ARK_BEIJING_BASE.to_owned())
            }
            None => (
                setting("MODEL_AGENT_NAME")
                    .ok_or_else(|| io::Error::other("missing MODEL_AGENT_NAME"))?,
                setting("MODEL_AGENT_API_BASE")
                    .ok_or_else(|| io::Error::other("missing MODEL_AGENT_API_BASE"))?,
            ),
        };
        Scope::new(&base, &[("POST", "chat/completions")])?;
        let credential = match config.and_then(|entry| entry.apikey.as_deref()) {
            Some(key) if key.trim().is_empty() => return Err(io::Error::other("apikey is empty")),
            Some(key) => credentials::Credential::Key(secret_header(key, true)?),
            None => credentials::Credential::discover(&model).ok_or_else(|| {
                io::Error::other("supply apikey or configure host Ark credentials")
            })?,
        };
        Ok(Arc::new(Installed {
            binary,
            model,
            base,
            credential,
        }))
    }
    fn observation_rules(&self) -> ObservationRules {
        ObservationRules {
            runtime_executables: vec![PROGRAM.into()],
            private_paths: vec![self.home().into()],
        }
    }
    fn home(&self) -> &'static Path {
        Path::new("/records/agentkit")
    }
    fn configuration_file(&self, name: &str) -> bool {
        name == CONFIG
    }
    fn configure(&self, command: &mut Command) -> io::Result<()> {
        command.env("SSL_CERT_FILE", "/etc/ssl/certs/ca-certificates.crt");
        Ok(())
    }
    fn launch(&self, _request: &LaunchRequest<'_>) -> io::Result<Launch> {
        let settings: Settings = serde_json::from_slice(&std::fs::read(self.home().join(CONFIG))?)?;
        let mut command = Command::new(PROGRAM);
        self.configure(&mut command)?;
        command
            .env("MODEL_AGENT_PROVIDER", "ark")
            .env("MODEL_AGENT_NAME", settings.model)
            .env("MODEL_AGENT_API_BASE", settings.base)
            .env("MODEL_AGENT_API_KEY", settings.placeholder);
        Ok(Launch {
            command,
            input: Vec::new(),
        })
    }
}

impl adapter::Installation for Installed {
    fn adapter(&self) -> Adapter {
        Adapter::new(&AgentKit)
    }
    fn mounts(&self) -> io::Result<Vec<(PathBuf, String)>> {
        Ok(vec![(self.binary.clone(), PROGRAM.into())])
    }
    fn snapshot(&self) -> BoxFuture<'_, io::Result<Prepared>> {
        Box::pin(async move {
            let settings = Settings {
                model: self.model.clone(),
                base: self.base.clone(),
                placeholder: marker(),
            };
            let scope = Scope::new(&self.base, &[("POST", "chat/completions")])?;
            let grant = Grant::new(
                scope,
                axum::http::header::AUTHORIZATION,
                secret_header(&settings.placeholder, true)?,
                self.credential.source().await?,
            )?;
            Ok(Prepared {
                files: [(CONFIG.into(), serde_json::to_string(&settings)?)].into(),
                policy: Policy::new(vec![grant]),
            })
        })
    }
}
