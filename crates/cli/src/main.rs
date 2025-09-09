//! Selfie CLI - Command-line interface for the selfie package manager
//!
//! This is the main entry point for the selfie command-line application.
//! It provides a user-friendly interface for managing packages across
//! different environments and package managers.
//!
//! # Architecture
//!
//! The CLI follows a modular design with separate concerns:
//! - Command parsing and routing
//! - Configuration loading and management
//! - Event processing and user feedback
//! - Terminal output formatting and progress reporting
//!
//! # Usage
//!
//! The CLI supports various package management operations:
//! ```bash
//! selfie install <package>     # Install a package
//! selfie check <package>       # Check if a package is installed
//! selfie list                  # List available packages
//! selfie info <package>        # Get package information
//! selfie validate <package>    # Validate package definition
//! ```

mod cli;
mod commands;
mod config;
mod event_processor;
mod formatters;
mod tables;
mod terminal_progress_reporter;

use std::process;

use clap::Parser;
use selfie::{
    config::{
        YamlLoader,
        loader::{ApplyToConfg, ConfigLoader},
    },
    fs::real::RealFileSystem,
};
use terminal_progress_reporter::TerminalProgressReporter;
use tracing::debug;

use crate::{cli::ClapCli, commands::dispatch_command};

/// Initialize tracing/logging based on verbosity level
///
/// Sets up the tracing subscriber with appropriate log levels:
/// - Verbose mode: DEBUG level for detailed troubleshooting
/// - Normal mode: WARN level for important messages only
///
/// # Arguments
///
/// * `verbose` - Whether to enable verbose (DEBUG) logging
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

/// Main entry point for the selfie CLI application
///
/// This function handles the complete CLI workflow:
/// 1. Parse command-line arguments using clap
/// 2. Initialize logging/tracing based on verbosity
/// 3. Load configuration from file and apply CLI overrides
/// 4. Set up terminal progress reporting
/// 5. Dispatch to the appropriate command handler
/// 6. Exit with the appropriate status code
///
/// # Errors
///
/// Returns [`anyhow::Error`] if:
/// - Configuration file cannot be found or parsed
/// - Configuration file contains invalid YAML syntax
/// - Required configuration fields are missing or invalid
/// - File system permissions prevent configuration access
/// - Critical initialization steps fail before command dispatch
///
/// # Panics
///
/// This function calls `process::exit()` which terminates the program
/// and does not return normally. Most command-specific errors are handled
/// within the command dispatch system and result in appropriate exit codes
/// rather than propagated errors.
///
/// Note: Command parsing errors are handled by clap and will cause the
/// program to exit with usage information rather than returning an error.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = ClapCli::parse();

    // Initialize tracing based on verbose flag
    init_tracing(args.verbose);
    debug!("CLI arguments: {:#?}", &args);

    let fs = RealFileSystem;

    // Load and process configuration:
    // - `config`: Used for most operations (includes CLI argument overrides)
    // - `original_config`: Used for config commands that need the raw file content
    let (config, original_config) = {
        // 1. Load config.yaml
        let config = YamlLoader::new(&fs).load_config()?;

        // 2. Apply CLI args to config (overriding)
        (args.apply_to_config(config.clone()), config)
    };

    debug!("Final config: {:#?}", &config);

    // TODO: Maybe don't need to build this until it's needed?
    let reporter = TerminalProgressReporter::new(config.use_colors());

    // 3. Dispatch and execute the requested command
    let exit_code = dispatch_command(&args.command, &config, original_config, reporter).await;

    process::exit(exit_code)
}
