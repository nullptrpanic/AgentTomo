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
    },
    #[command(name = "__tracee", hide = true)]
    Tracee {
        #[arg(short = 'c', allow_hyphen_values = true)]
        command: OsString,
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
}

#[derive(Args)]
pub struct PlaygroundArgs {
    /// HTTP address; use 0.0.0.0:8080 to accept connections from other hosts
    #[arg(long, default_value = "127.0.0.1:8080")]
    pub listen: SocketAddr,
    /// Local Docker image containing /bin/bash; images are never pulled automatically
    #[arg(long, default_value = "ubuntu:24.04")]
    pub image: String,
    /// Directory for retained session recordings
    #[arg(long, default_value = ".agenttomo/playground")]
    pub output_dir: PathBuf,
}
