//! Container-only initialization. Configuration crosses stdin, never a host mount.
use std::{collections::BTreeMap, io, path::Path};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const ROOT: &str = "/records";
const MAX_BOOTSTRAP: usize = 32 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
pub struct Bootstrap {
    pub metadata: Value,
    pub configuration: BTreeMap<String, String>,
    pub proxy: Option<crate::proxy::outbound::Proxy>,
}

pub async fn send(output: &mut (impl AsyncWrite + Unpin), bootstrap: &Bootstrap) -> io::Result<()> {
    let bytes = serde_json::to_vec(bootstrap)?;
    if bytes.len() > MAX_BOOTSTRAP {
        return Err(io::Error::other("session bootstrap exceeds 32 MiB"));
    }
    output.write_u32(bytes.len() as u32).await?;
    output.write_all(&bytes).await?;
    output.flush().await
}

pub fn stdin() -> io::Result<tokio::net::unix::pipe::Receiver> {
    // Dup fd 0 so bootstrap cannot close the collector's subsequent IPC input.
    tokio::net::unix::pipe::Receiver::from_owned_fd(nix::unistd::dup(std::io::stdin())?)
}

pub async fn initialize(id: &str) -> io::Result<Option<crate::proxy::outbound::Proxy>> {
    if !super::valid_session(id) {
        return Err(io::Error::other("invalid session id"));
    }
    let mut input = stdin()?;
    let length = input.read_u32().await? as usize;
    if length > MAX_BOOTSTRAP {
        return Err(io::Error::other("session bootstrap exceeds 32 MiB"));
    }
    let mut bytes = vec![0; length];
    input.read_exact(&mut bytes).await?;
    let bootstrap: Bootstrap = serde_json::from_slice(&bytes)?;
    let directory = Path::new(ROOT).join(id);
    tokio::fs::DirBuilder::new()
        .mode(0o700)
        .create(&directory)
        .await?;
    let mut metadata = crate::recording::create_log(&directory, "session.json").await?;
    metadata
        .write_all(&serde_json::to_vec(&bootstrap.metadata)?)
        .await?;
    metadata.flush().await?;
    tokio::fs::DirBuilder::new()
        .mode(0o700)
        .create(crate::agent::codex::HOME)
        .await?;
    tokio::fs::DirBuilder::new()
        .mode(0o777)
        .create(Path::new(ROOT).join("control"))
        .await?;
    for (name, contents) in bootstrap.configuration {
        if !crate::agent::codex::configuration_file(&name) && name != "agenttomo-environment.json" {
            return Err(io::Error::other("invalid configuration filename"));
        }
        let mut file =
            crate::recording::create_log(Path::new(crate::agent::codex::HOME), &name).await?;
        file.write_all(contents.as_bytes()).await?;
        // Tokio write_all may only enqueue the last write. Configuration must
        // be fully visible (or fail) before any Codex process is launched.
        file.flush().await?;
    }
    Ok(bootstrap.proxy)
}

pub async fn completed(id: &str) -> io::Result<()> {
    let directory = Path::new(ROOT).join(id);
    let status: Value = serde_json::from_slice(
        &tokio::fs::read(directory.join("run/collection-status.json")).await?,
    )?;
    let temp = directory.join("session-result.tmp");
    tokio::fs::write(
        &temp,
        serde_json::to_vec(&json!({"reason":"completed", "status":status}))?,
    )
    .await?;
    tokio::fs::rename(temp, directory.join("session-result.json")).await?;
    // Preserve tmpfs and its final tail until the owner consumes completion and
    // removes Docker. Merely exiting here would unmount tmpfs before that read.
    std::future::pending::<io::Result<()>>().await
}
