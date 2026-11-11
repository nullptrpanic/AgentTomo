//! Built-in agent adapters. Only IDs and sanitized files cross the sandbox boundary.
use super::codex;
use futures_util::future::BoxFuture;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    ops::Deref,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{io::AsyncRead, process::Command, sync::mpsc};

#[cfg(test)]
mod tests;

const ADAPTERS: &[Adapter] = &[Adapter::new(&codex::Codex)];

#[derive(Serialize)]
pub struct Metadata {
    pub id: &'static str,
    pub name: &'static str,
    pub avatar: &'static str,
    pub authentication_hint: &'static str,
}

// Intentionally not serializable: the policy contains host-only credential sources.
pub struct Prepared {
    pub files: BTreeMap<String, String>,
    pub policy: crate::proxy::credentials::Policy,
}

#[derive(Default)]
pub struct ObservationRules {
    pub runtime_executables: Vec<String>,
    pub private_paths: Vec<PathBuf>,
}

#[derive(Default)]
pub struct RuntimeOutput {
    pub conversation_id: Option<String>,
    pub error: Option<String>,
}

pub type RuntimeReader = Box<dyn AsyncRead + Unpin + Send>;

pub struct LaunchRequest<'a> {
    pub conversation: Option<&'a str>,
    pub prompt: &'a str,
}

/// One CLI invocation per turn. Arguments and stdin encoding belong to the adapter;
/// spawning, correlation environment, cancellation and pipe draining remain shared.
pub struct Launch {
    pub command: Command,
    pub input: Vec<u8>,
}

pub trait AgentAdapter: Send + Sync {
    fn metadata(&self) -> Metadata;
    fn discover(&self) -> Option<Arc<dyn Installation>>;
    fn observation_rules(&self) -> ObservationRules;
    fn home(&self) -> &'static Path;
    fn configuration_file(&self, name: &str) -> bool;
    fn configure(&self, command: &mut Command) -> io::Result<()>;
    fn launch(&self, request: &LaunchRequest<'_>) -> io::Result<Launch>;
    fn read_output(
        &self,
        input: RuntimeReader,
        diagnostics: mpsc::Sender<String>,
    ) -> BoxFuture<'_, io::Result<RuntimeOutput>>;
}

/// Installation-specific host paths and credential preparation stay with the plugin.
pub trait Installation: Send + Sync {
    fn adapter(&self) -> Adapter;
    fn mount(&self) -> (&Path, &'static str);
    fn snapshot(&self) -> BoxFuture<'_, io::Result<Prepared>>;
}

#[derive(Clone, Copy)]
pub struct Adapter(&'static dyn AgentAdapter);

impl Adapter {
    pub const fn new(plugin: &'static dyn AgentAdapter) -> Self {
        Self(plugin)
    }

    pub fn selected() -> io::Result<Option<Self>> {
        match std::env::var("AGENTTOMO_AGENT_ADAPTER") {
            Ok(id) => select(Some(&id), ADAPTERS),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(_) => Err(io::Error::other("invalid agent adapter ID")),
        }
    }
}

impl Deref for Adapter {
    type Target = dyn AgentAdapter;
    fn deref(&self) -> &Self::Target {
        self.0
    }
}

fn select(id: Option<&str>, registry: &[Adapter]) -> io::Result<Option<Adapter>> {
    id.map(|id| {
        registry
            .iter()
            .copied()
            .find(|adapter| adapter.metadata().id == id)
            .ok_or_else(|| io::Error::other("unsupported agent adapter"))
    })
    .transpose()
}

impl Serialize for Adapter {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.metadata().id)
    }
}

impl<'de> Deserialize<'de> for Adapter {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let id = String::deserialize(deserializer)?;
        select(Some(&id), ADAPTERS)
            .map_err(serde::de::Error::custom)?
            .ok_or_else(|| serde::de::Error::custom("missing agent adapter"))
    }
}

pub fn discover() -> Option<Arc<dyn Installation>> {
    ADAPTERS.iter().find_map(|adapter| adapter.discover())
}

pub fn observation_rules(selected: Option<Adapter>) -> ObservationRules {
    let mut rules = selected
        .map(|adapter| adapter.observation_rules())
        .unwrap_or_default();
    // Preserve private-file protection even without an active agent. These are
    // exclusions only: they must not implicitly classify a process as a runtime.
    rules.private_paths = ADAPTERS
        .iter()
        .flat_map(|adapter| adapter.observation_rules().private_paths)
        .chain(rules.private_paths)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    rules
}
