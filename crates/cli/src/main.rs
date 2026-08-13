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
    config::{ConfigLoadError, YamlLoader, loader::ConfigLoader},
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
    let (config, notices) = {
        // A missing file is not a failure when the flags carry what it would
        // have. Every *other* load error still is — in particular a config file
        // that is not a regular one, which must not be mistaken for an absent
        // one and silently replaced by the flags.
        let (selfie_config, mut notices, cli_load) = match YamlLoader::new(&fs).load_config() {
            Ok(loaded) => {
                let notices = crate::config::library_config_notices(loaded.ignored_keys());
                // From the same parse, not a second read of the same file.
                let cli_load = crate::config::cli_section(&loaded);
                (loaded.into_config(), notices, cli_load)
            }
            Err(ConfigLoadError::NotFound { searched }) => {
                debug!(
                    "No configuration file in {}; building from flags",
                    searched.display()
                );
                // No file, so no `cli:` section to read and nothing to report.
                (
                    args.config_from_flags(searched)?,
                    Vec::new(),
                    crate::config::CliSectionLoad::default(),
                )
            }
            Err(other) => return Err(other.into()),
        };

        // Both halves, rendered together below. The library reports top-level
        // keys and the CLI reports what is inside `cli:`; neither can see the
        // other's, which is why they are collected here rather than at either
        // source.
        notices.extend(cli_load.notices);

        (
            args.build_cli_config(selfie_config, cli_load.section),
            notices,
        )
    };

    debug!("Final config: {:#?}", &config);

    let display = DisplayManager::new(config.use_colors());

    // After `display` exists, so the warnings honor `--no-color`, and before
    // dispatch, so they are not buried under a command's own output.
    //
    // Skipped for `config validate`, which reports the same keys itself as rows
    // in its table. Printing here as well showed every ignored key twice, in two
    // formats, in the one command whose entire job is reporting them.
    if !matches!(
        args.command,
        cli::ClapCommands::Config(cli::ConfigCommands {
            command: cli::ConfigSubcommands::Validate
        })
    ) {
        crate::config::report_config_notices(&notices, &display);
    }

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
