use crate::{
    agent::{self, AgentConfig, Installation},
    cli::{DEFAULT_PLAYGROUND_IMAGE, PlaygroundArgs},
};
use serde::Deserialize;
use std::{fs, io, net::SocketAddr, path::Path, sync::Arc};

#[cfg(test)]
mod tests;

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    listen: String,
    image: String,
    capture_http: Option<bool>,
    agents: Option<Vec<AgentConfig>>,
}

impl FileConfig {
    fn read(path: &Path) -> io::Result<Self> {
        let bytes = fs::read(path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("cannot read {}: {error}", path.display()),
            )
        })?;
        // A deserialization error may quote an invalid value, including a credential.
        let mut config: Self = serde_json::from_slice(&bytes).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "invalid Playground config {} at line {}, column {}",
                    path.display(),
                    error.line(),
                    error.column()
                ),
            )
        })?;
        let directory = path.parent().unwrap_or(Path::new("."));
        for entry in config.agents.iter_mut().flatten() {
            entry.path = entry
                .path
                .take()
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| directory.join(p));
        }
        Ok(config)
    }
}

pub(super) struct Config {
    pub listen: SocketAddr,
    pub image: String,
    pub capture_http: bool,
    pub agents: Vec<Arc<dyn Installation>>,
}

impl Config {
    pub fn load(options: &PlaygroundArgs) -> io::Result<Self> {
        let file = options
            .config
            .as_deref()
            .map(FileConfig::read)
            .transpose()?
            .unwrap_or_default();
        let listen = match (options.listen, file.listen.trim()) {
            (Some(address), _) => address,
            (None, "") => ([127, 0, 0, 1], 8080).into(),
            (None, value) => value
                .parse()
                .map_err(|_| io::Error::other("listen must be an IP address and port"))?,
        };
        let image = options.image.clone().unwrap_or_else(|| {
            if file.image.is_empty() {
                DEFAULT_PLAYGROUND_IMAGE.to_owned()
            } else {
                file.image
            }
        });
        let agents = agent::discover(file.agents.as_deref())?;
        let capture_http = options.http.enabled || file.capture_http.unwrap_or(!agents.is_empty());
        if !capture_http && !agents.is_empty() {
            return Err(io::Error::other(
                "agent chat requires capture_http=true; set agents to [] for a terminal without HTTP capture",
            ));
        }
        Ok(Self {
            listen,
            image,
            capture_http,
            agents,
        })
    }
}
