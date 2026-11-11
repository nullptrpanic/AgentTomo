mod agent;
mod cli;
mod collectors;
mod credentials;
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
        root_session,
        root_overlay_directory,
        rootfs,
    } = &cli.command
    {
        return ExitCode::from(execution::child::exec(
            command,
            trust_overlay.as_deref(),
            *root_session,
            root_overlay_directory.as_deref(),
            &rootfs.options(),
        ));
    }
    if let cli::Command::Collector {
        record_uid: Some(uid),
        record_gid: Some(gid),
        ..
    } = &cli.command
    {
        if let Err(error) = credentials::filesystem_identity(*uid, *gid) {
            eprintln!("agenttomo-box: {error}");
            return ExitCode::from(125);
        }
    }
    let result = Builder::new_current_thread()
        .enable_all()
        .build()
        .and_then(|runtime| {
            let result = runtime.block_on(async {
                match cli.command {
                    cli::Command::Run(options) => {
                        let proxy = (!options.no_proxy).then(|| options.proxy.unwrap_or_default());
                        execution::run(
                            &options.command,
                            options.output_dir.as_deref(),
                            options.file_preview.limits(),
                            options.http.options(),
                            options.rootfs.options(),
                            proxy,
                        )
                        .await
                    }
                    cli::Command::Playground(options) => playground::run(options).await.map(|()| 0),
                    cli::Command::Workspace { browser } => agent::workspace::run(browser).await,
                    cli::Command::Records { session } => {
                        playground::bridge::serve(&session).await.map(|()| 0)
                    }
                    cli::Command::Collector {
                        command,
                        output_dir,
                        terminal,
                        file_preview,
                        http,
                        rootfs,
                        playground_session,
                        proxy,
                        ..
                    } => {
                        let result = async {
                            let proxy = if let Some(id) = &playground_session {
                                playground::sandbox::initialize(id).await?
                            } else {
                                proxy
                            };
                            execution::collector::run(
                                &command,
                                &output_dir,
                                terminal,
                                file_preview.limits(),
                                http.options(),
                                rootfs.options(),
                                proxy,
                            )
                            .await?;
                            if let Some(id) = &playground_session {
                                playground::sandbox::completed(id).await?;
                            }
                            Ok::<_, std::io::Error>(())
                        }
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
