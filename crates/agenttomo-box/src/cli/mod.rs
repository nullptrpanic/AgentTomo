use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(about = "Run and inspect programs with AgentTomo")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run a command, recording its streams and process events
    Run(RunArgs),
    /// Open a Docker terminal and live traces in a web browser
    Playground(PlaygroundArgs),
    #[command(name = "__collector", hide = true)]
    Collector {
        #[arg(short = 'c', allow_hyphen_values = true)]
        command: OsString,
        #[arg(long, allow_hyphen_values = true)]
        output_dir: PathBuf,
        #[arg(long)]
        terminal: bool,
        #[command(flatten)]
        file_preview: FilePreviewArgs,
        #[command(flatten)]
        http: HttpArgs,
    },
    #[command(name = "__tracee", hide = true)]
    Tracee {
        #[arg(short = 'c', allow_hyphen_values = true)]
        command: OsString,
        #[arg(long)]
        trust_overlay: Option<PathBuf>,
    },
}

#[derive(Args)]
#[command(
    after_help = "Logs default to .agenttomo/runs/<timestamp>-<pid>/. Uses pipes, not a PTY. The foreground returns the root command's exit code; surviving descendants remain traced."
)]
pub struct RunArgs {
    /// Shell command to execute with /bin/sh -c
    #[arg(short = 'c', value_name = "COMMAND", allow_hyphen_values = true)]
    pub command: OsString,
    /// New directory for this run's logs; must not already exist
    #[arg(long, value_name = "DIRECTORY", allow_hyphen_values = true)]
    pub output_dir: Option<PathBuf>,
    #[command(flatten)]
    pub file_preview: FilePreviewArgs,
    #[command(flatten)]
    pub http: HttpArgs,
}

#[derive(Args)]
pub struct PlaygroundArgs {
    /// Show a traced Chrome window (requires the Playground Chrome image)
    #[arg(long)]
    pub browser: bool,
    /// HTTP address; use 0.0.0.0:8080 to accept connections from other hosts
    #[arg(long, default_value = "127.0.0.1:8080")]
    pub listen: SocketAddr,
    /// Local Docker image containing /bin/bash; images are never pulled automatically
    #[arg(long, default_value = "agenttomo-playground:24.04")]
    pub image: String,
    /// Directory for retained session recordings
    #[arg(long, default_value = ".agenttomo/playground")]
    pub output_dir: PathBuf,
    #[command(flatten)]
    pub file_preview: FilePreviewArgs,
    #[command(flatten)]
    pub http: HttpArgs,
}

#[derive(Args)]
pub struct FilePreviewArgs {
    /// Maximum captured bytes per opened file (0 disables previews)
    #[arg(long = "file-preview-max-bytes", default_value_t = 65536, value_parser = clap::value_parser!(u32).range(0..=i64::from(crate::recording::file_preview::MAX_PREVIEW_BYTES)))]
    pub max_bytes: u32,
    /// Total file-preview payload budget per run (0 disables previews)
    #[arg(long = "file-preview-total-bytes", default_value_t = 16777216)]
    pub total_bytes: u64,
}

impl FilePreviewArgs {
    pub fn limits(&self) -> crate::recording::file_preview::Limits {
        crate::recording::file_preview::Limits {
            max_bytes: self.max_bytes,
            total_bytes: self.total_bytes,
        }
    }
}

#[derive(Args)]
pub struct HttpArgs {
    /// Capture HTTP/1.1 bodies and decrypt TLS using an OverlayFS-injected task CA
    #[arg(long = "capture-http")]
    pub enabled: bool,
    /// Maximum retained bytes per request/response body (forwarding is unlimited)
    #[arg(id = "http_body_max_bytes", long = "http-body-max-bytes", default_value_t = 65536, value_parser = clap::value_parser!(u32).range(0..=i64::from(crate::recording::http::MAX_BODY_BYTES)))]
    pub max_bytes: u32,
    /// Total HTTP body payload budget per run
    #[arg(
        id = "http_body_total_bytes",
        long = "http-body-total-bytes",
        default_value_t = 67108864
    )]
    pub total_bytes: u64,
    /// Maximum retained bytes per SSE/WebSocket message (0 disables retention)
    #[arg(long, default_value_t = 65536, value_parser = clap::value_parser!(u32).range(0..=i64::from(crate::recording::streams::MAX_MESSAGE_BYTES)))]
    pub stream_message_max_bytes: u32,
    /// Run-wide sliding window of stream payload and metadata bytes
    #[arg(long, default_value_t = 4194304, value_parser = clap::value_parser!(u64).range(0..=i64::MAX as u64))]
    pub stream_window_bytes: u64,
    /// Maximum messages retained across all SSE/WebSocket connections
    #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u32).range(1..=i64::from(crate::recording::streams::MAX_MESSAGES)))]
    pub stream_window_messages: u32,
}
impl HttpArgs {
    pub fn options(&self) -> crate::proxy::http::Options {
        crate::proxy::http::Options {
            enabled: self.enabled,
            limits: crate::recording::http::Limits {
                max_bytes: self.max_bytes,
                total_bytes: self.total_bytes,
            },
            streams: crate::recording::streams::Limits {
                message_bytes: self.stream_message_max_bytes,
                window_bytes: self.stream_window_bytes,
                window_messages: self.stream_window_messages,
            },
        }
    }
}
