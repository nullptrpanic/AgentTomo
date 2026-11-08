mod cli;
mod collectors;
mod execution;
mod network;
mod playground;
mod process;
mod proxy;
mod recording;
mod syscall;
mod terminal;
mod timing;
mod tracing;

use clap::Parser;
use std::process::ExitCode;
use tokio::runtime::Builder;

fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    if let cli::Command::Tracee {
        command,
        trust_overlay,
    } = &cli.command
    {
        return ExitCode::from(execution::child::exec(command, trust_overlay.as_deref()));
    }
    let result = Builder::new_current_thread()
        .enable_all()
        .build()
        .and_then(|runtime| {
            let result = runtime.block_on(async {
                match cli.command {
                    cli::Command::Run(options) => {
                        execution::run(
                            &options.command,
                            options.output_dir.as_deref(),
                            options.file_preview.limits(),
                            options.http.options(),
                        )
                        .await
                    }
                    cli::Command::Playground(options) => playground::run(options).await.map(|()| 0),
                    cli::Command::Collector {
                        command,
                        output_dir,
                        terminal,
                        file_preview,
                        http,
                    } => {
                        let result = execution::collector::run(
                            &command,
                            &output_dir,
                            terminal,
                            file_preview.limits(),
                            http.options(),
                        )
                        .await;
                        if let Err(error) = &result {
                            let _ = execution::protocol::send(
                                &mut tokio::io::stdout(),
                                &execution::protocol::Message::Error(error.to_string()),
                            )
                            .await;
                        }
                        result.map(|()| 0)
                    }
                    cli::Command::Tracee { .. } => unreachable!("handled before runtime creation"),
                }
            });
            // CLI stdin can leave an uncancellable blocking read. Logs are already
            // flushed; do not hold the foreground open waiting for that read.
            runtime.shutdown_background();
            result
        });
    match result {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("agenttomo-box: {error}");
            ExitCode::from(125)
        }
    }
}
