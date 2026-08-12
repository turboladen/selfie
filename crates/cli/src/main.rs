//! Entry point for the selfie command-line application.
//!
//! Parses arguments, loads configuration, and hands off to a command handler,
//! which renders the library's event stream to the terminal.

mod cli;
mod commands;
mod completers;
mod config;
mod display_manager;
mod event_processor;
mod formatters;
mod git_style;
mod status_style;
mod tables;

use std::process;

use clap::{CommandFactory, Parser};
use clap_complete::CompleteEnv;
use display_manager::DisplayManager;
use selfie::{
    config::{YamlLoader, loader::ConfigLoader},
    fs::real::RealFileSystem,
};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::{cli::ClapCli, commands::dispatch_command};

/// Install the tracing subscriber: DEBUG when `verbose`, WARN otherwise.
fn init_tracing(verbose: bool) {
    if verbose {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .init();
    } else {
        // Use a no-op subscriber to disable output while keeping trace calls
        let subscriber = tracing_subscriber::registry();
        tracing::subscriber::set_global_default(subscriber)
            .expect("Failed to set tracing subscriber");
    }
}

/// Wait for a shutdown signal (SIGINT or SIGTERM on Unix, SIGINT on Windows).
///
/// Returns when the first signal is received. Call again to wait for a second signal.
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = sigterm.recv() => {}
                }
            }
            Err(_) => {
                // SIGTERM registration can fail in sandboxed environments;
                // fall back to SIGINT-only.
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Parse arguments, load configuration, dispatch, and exit with the handler's
/// status code.
///
/// # Errors
///
/// [`anyhow::Error`] if configuration cannot be found, read or parsed, or if
/// initialization fails before dispatch. A command's own failures become exit
/// codes rather than errors, and clap handles a bad command line by printing
/// usage and exiting.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    human_panic::setup_panic!();

    // Handle dynamic shell completion requests.
    // When COMPLETE=<shell> is set, generates completions and exits.
    CompleteEnv::with_factory(ClapCli::command).complete();

    let args = ClapCli::parse();

    // Initialize tracing based on verbose flag
    init_tracing(args.verbose);
    debug!("CLI arguments: {:#?}", &args);

    let fs = RealFileSystem;

    // Load and process configuration
    let config = {
        let selfie_config = YamlLoader::new(&fs).load_config()?;
        let cli_section = crate::config::load_cli_section(&fs);
        args.build_cli_config(selfie_config, cli_section)
    };

    debug!("Final config: {:#?}", &config);

    let display = DisplayManager::new(config.use_colors());

    // Set up graceful shutdown: first SIGINT/SIGTERM cancels in-flight operations,
    // second signal forces immediate exit.
    let cancellation_token = CancellationToken::new();
    let token_clone = cancellation_token.clone();
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        eprintln!("\nCanceling... (signal again to force quit)");
        token_clone.cancel();

        // Second signal: force exit
        wait_for_shutdown_signal().await;
        process::exit(130);
    });

    // Dispatch and execute the requested command
    let exit_code =
        dispatch_command(&args.command, &config, display, cancellation_token, &fs).await;

    process::exit(exit_code)
}
