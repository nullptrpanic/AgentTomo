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
    #[command(name = "__workspace", hide = true)]
    Workspace {
        #[arg(long)]
        browser: bool,
    },
    #[command(name = "__records", hide = true)]
    Records {
        #[arg(long)]
        session: String,
    },
    #[command(name = "__collector", hide = true)]
    Collector {
        #[arg(short = 'c', allow_hyphen_values = true)]
        command: OsString,
        #[arg(long, allow_hyphen_values = true)]
        output_dir: PathBuf,
        #[arg(long)]
        terminal: bool,
        #[arg(long, requires = "record_gid")]
        record_uid: Option<u32>,
        #[arg(long, requires = "record_uid")]
        record_gid: Option<u32>,
        #[arg(long)]
        playground_session: Option<String>,
        #[arg(long)]
        proxy: Option<crate::proxy::outbound::Proxy>,
        #[command(flatten)]
        file_preview: FilePreviewArgs,
        #[command(flatten)]
        http: HttpArgs,
        #[command(flatten)]
        rootfs: RootfsArgs,
    },
    #[command(name = "__tracee", hide = true)]
    Tracee {
        #[arg(short = 'c', allow_hyphen_values = true)]
        command: OsString,
        #[arg(long)]
        trust_overlay: Option<PathBuf>,
        #[arg(long)]
        root_session: bool,
        #[arg(long)]
        root_overlay_directory: Option<PathBuf>,
        #[command(flatten)]
        rootfs: RootfsArgs,
    },
}

#[derive(Args)]
#[command(
    after_help = "Logs default to .agenttomo/runs/<timestamp>-<pid>/. Uses pipes, not a PTY. The foreground returns the root command's exit code; surviving descendants remain traced."
)]
pub struct RunArgs {
    /// HTTP CONNECT upstream for box's TCP forwarding (default: http://127.0.0.1:8118)
    #[arg(long, value_name = "URL", overrides_with = "no_proxy")]
    pub proxy: Option<crate::proxy::outbound::Proxy>,
    /// Disable box's upstream proxy and connect directly
    #[arg(long, overrides_with = "proxy")]
    pub no_proxy: bool,
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
    #[command(flatten)]
    pub rootfs: RootfsArgs,
}

#[derive(Args)]
pub struct PlaygroundArgs {
    /// HTTP CONNECT upstream (fallback: http://127.0.0.1:8118; host loopback is bridged)
    #[arg(long, value_name = "URL", overrides_with = "no_proxy")]
    pub proxy: Option<crate::proxy::outbound::Proxy>,
    /// Disable box's upstream proxy, including inherited and local settings
    #[arg(long, overrides_with = "proxy")]
    pub no_proxy: bool,
    /// HTTP address; use 0.0.0.0:8080 to accept connections from other hosts
    #[arg(long, default_value = "127.0.0.1:8080")]
    pub listen: SocketAddr,
    /// Local Docker image; compatible images open Chrome automatically
    #[arg(long, default_value = "agenttomo-playground-chrome:24.04")]
    pub image: String,
    /// Legacy compatibility option; Playground data now stays in container tmpfs
    #[arg(long, default_value = ".agenttomo/playground")]
    pub output_dir: PathBuf,
    /// Maximum tmpfs size for diagnostics, Codex state and IPC, in MiB
    #[arg(long, default_value_t = 512, value_parser = clap::value_parser!(u32).range(1..))]
    pub session_tmpfs_size_mib: u32,
    #[command(flatten)]
    pub file_preview: FilePreviewArgs,
    #[command(flatten)]
    pub http: HttpArgs,
    #[command(flatten)]
    pub rootfs: RootfsArgs,
}

#[derive(Args)]
pub struct RootfsArgs {
    /// Overlay eligible whole top-level directories and /etc/ssl; other paths stay shared
    #[arg(id = "overlay_root", long = "overlay-root")]
    pub enabled: bool,
    /// Shared tmpfs limit for the selected directories' overlay data, in MiB
    #[arg(long = "overlay-root-size-mib", default_value_t = 512, value_parser = clap::value_parser!(u32).range(1..))]
    pub size_mib: u32,
    /// Exempt an existing absolute path (its containing overlay is skipped); repeatable
    #[arg(
        long = "overlay-exempt",
        requires = "overlay_root",
        value_name = "PATH"
    )]
    pub exemptions: Vec<PathBuf>,
}

impl RootfsArgs {
    pub fn options(&self) -> crate::execution::rootfs::Options {
        crate::execution::rootfs::Options {
            enabled: self.enabled,
            size_mib: self.size_mib,
            exemptions: self.exemptions.clone(),
        }
    }
}

#[derive(Args)]
pub struct FilePreviewArgs {
    /// Maximum captured bytes per opened file (0 disables previews)
    #[arg(long = "file-preview-max-bytes", default_value_t = 65536, value_parser = clap::value_parser!(u32).range(0..=i64::from(crate::recording::file_preview::MAX_PREVIEW_BYTES)))]
    pub max_bytes: u32,
    /// Total file-preview payload budget per run (0 disables previews)
    #[arg(long = "file-preview-total-bytes", default_value_t = crate::recording::file_preview::Limits::default().total_bytes)]
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
