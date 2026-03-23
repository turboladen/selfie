//! Apply command handler for deploying dotfiles
//!
//! This module handles the `selfie apply` CLI command, which deploys
//! dotfiles defined in package YAML files to their target
//! locations on the system.

use std::sync::Arc;

use selfie::{
    dotfile_service::{
        port::{ApplyOptions, ConflictResolution, ConflictResolver, DotfileService},
        service::DotfileServiceImpl,
    },
    fs::real::RealFileSystem,
    package::repository::yaml::YamlPackageRepository,
};
use tracing::info;

use crate::{
    cli::ApplyArgs,
    commands::common::create_package_repository,
    config::CliConfig,
    display_manager::{DisplayManager, shorten_path},
    event_processor::EventProcessor,
};

/// Interactive conflict resolver that prompts the user via the terminal.
///
/// Shows the diff and asks whether to overwrite the target with the repo
/// version or keep the target as-is.
struct InteractiveConflictResolver {
    display: DisplayManager,
}

impl ConflictResolver for InteractiveConflictResolver {
    fn resolve(&self, source: &str, target: &str, diff: &str) -> ConflictResolution {
        let short_source = shorten_path(source);
        let short_target = shorten_path(target);

        self.display
            .print_warning(format!("  Conflict: {short_source} → {short_target}"));
        self.display.print_diff(diff);

        let items = &[
            "Accept (overwrite target with repo version)",
            "Skip (keep target as-is)",
        ];
        let selection = dialoguer::Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
            .with_prompt("  How should this conflict be resolved?")
            .items(items)
            .default(0)
            .interact();

        match selection {
            Ok(0) => ConflictResolution::Accept,
            _ => ConflictResolution::Skip,
        }
    }
}

/// Handle the apply command
///
/// Creates a `DotfileServiceImpl` and delegates to `apply_all` or `apply`
/// based on whether a package name was given. When the configured
/// `dotfiles_directory` exists, it's added as a second source so
/// standalone dotfiles are included in the apply. Events are processed
/// through the standard `EventProcessor`.
pub(crate) async fn handle_apply(
    args: &ApplyArgs,
    config: &CliConfig,
    display: &DisplayManager,
) -> i32 {
    let options = ApplyOptions {
        dry_run: args.dry_run,
        auto_accept: args.yes,
        conflict_resolver: Some(Arc::new(InteractiveConflictResolver {
            display: display.clone(),
        })),
    };

    let repo = create_package_repository(config);
    let fs = RealFileSystem;
    let mut service = DotfileServiceImpl::new(repo, fs, config.selfie_config().clone());

    // Add standalone dotfiles repository if the directory exists
    let dotfiles_dir = config.selfie_config().dotfiles_directory();
    if dotfiles_dir.is_dir() {
        let dotfiles_repo = YamlPackageRepository::new(RealFileSystem, dotfiles_dir);
        service = service.with_dotfiles_repository(dotfiles_repo);
    }

    let event_stream = if let Some(name) = &args.name {
        info!("Applying dotfiles for package: {}", name);
        service.apply(name, options).await
    } else {
        info!("Applying all dotfiles");
        service.apply_all(options).await
    };

    let processor = EventProcessor::new(display.clone());
    let result = processor.process_events(event_stream, |_event| false).await;

    result.exit_code
}
