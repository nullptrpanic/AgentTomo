//! Run files and the separate completion states of the root and collection.

mod events;
pub mod file_preview;
pub mod http;
pub mod streams;

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tokio::fs::{DirBuilder, File, OpenOptions};
use tokio::io::AsyncWriteExt;

pub struct Observation {
    pub source: &'static str,
    pub kind: &'static str,
    pub data: Value,
}

pub fn default_output_dir() -> io::Result<PathBuf> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    Ok(Path::new(".agenttomo/runs").join(format!("{timestamp}-{}", std::process::id())))
}

pub async fn create_log(directory: &Path, name: &str) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(directory.join(name))
        .await
}

pub struct Recording {
    pub directory: PathBuf,
    pub stdout: File,
    pub stderr: File,
    exit: File,
    events: events::Writer,
}

impl Recording {
    pub fn directory(&self) -> &Path {
        &self.directory
    }
    pub async fn create(
        command: &OsStr,
        directory: &Path,
        preview_limits: file_preview::Limits,
        exclude_task_trust: bool,
    ) -> io::Result<(Self, File)> {
        if let Some(parent) = directory
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        {
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)
                .await?;
        }
        DirBuilder::new()
            .mode(0o700)
            .create(directory)
            .await
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "cannot create run directory {}: {error}",
                        directory.display()
                    ),
                )
            })?;
        let mut command_log = create_log(directory, "command.txt").await?;
        command_log.write_all(command.as_encoded_bytes()).await?;
        command_log.flush().await?;
        let input = create_log(directory, "stdin.log").await?;
        let result = Self {
            directory: directory.to_path_buf(),
            stdout: create_log(directory, "stdout.log").await?,
            stderr: create_log(directory, "stderr.log").await?,
            exit: create_log(directory, "exit-code.txt").await?,
            events: events::Writer::start(
                create_log(directory, "events.jsonl").await?,
                file_preview::Store::create(directory, preview_limits, exclude_task_trust).await?,
            ),
        };
        result.status("running", None).await?;
        Ok((result, input))
    }

    pub async fn event(
        &mut self,
        source: &'static str,
        kind: &'static str,
        data: Value,
    ) -> io::Result<()> {
        self.events
            .send(events::Content::Event(Observation { source, kind, data }))
            .await
    }

    pub async fn file_open(&mut self, opened: crate::collectors::file::Opened) -> io::Result<()> {
        self.events.send(events::Content::File(opened)).await
    }

    pub async fn writer_failure(&mut self) -> io::Result<()> {
        self.events.failure().await
    }

    pub async fn finish(&mut self) -> io::Result<()> {
        self.flush().await?;
        self.events.finish().await
    }

    pub async fn root_exit(&mut self, code: u8) -> io::Result<()> {
        self.exit.write_all(format!("{code}\n").as_bytes()).await?;
        self.exit.flush().await?;
        self.flush().await
    }

    pub async fn flush(&mut self) -> io::Result<()> {
        tokio::try_join!(self.stdout.flush(), self.stderr.flush())?;
        Ok(())
    }

    pub async fn status(&self, state: &str, error: Option<&str>) -> io::Result<()> {
        let temp = self.directory.join("collection-status.tmp");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)
            .await?;
        let status = json!({"state": state, "collector_pid": std::process::id(), "error": error});
        file.write_all(&serde_json::to_vec(&status)?).await?;
        file.flush().await?;
        tokio::fs::rename(temp, self.directory.join("collection-status.json")).await
    }
}
