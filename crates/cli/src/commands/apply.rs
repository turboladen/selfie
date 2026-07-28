//! Apply command handler for deploying dotfiles
//!
//! This module handles the `selfie apply` CLI command, which deploys
//! dotfiles defined in package YAML files to their target
//! locations on the system.

use std::sync::Arc;

use selfie::{
    commands::ShellCommandRunner,
    dotfile_service::{
        port::{
            ApplyOptions, ConflictDetail, ConflictResolution, ConflictResolver, DotfileService,
        },
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
/// For an ordinary file this shows the diff and asks whether to overwrite the
/// target. For a secret-bearing file there is no diff to show — only a summary of
/// each side's shape — so it additionally offers to reveal the two values.
struct InteractiveConflictResolver {
    display: DisplayManager,
}

impl InteractiveConflictResolver {
    /// Ask how to resolve, offering reveal only when `reveal` is set.
    ///
    /// Accept is never the default for a secret-bearing conflict, and reveal is
    /// never reachable by accepting a default: both require a deliberate
    /// selection.
    fn prompt(&self, reveal: bool) -> Option<usize> {
        let mut items = vec![
            "Skip (keep target as-is)",
            "Accept (overwrite target with the new content)",
        ];
        if reveal {
            items.push("Reveal the two values, then choose");
        }

        dialoguer::Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
            .with_prompt("How should this conflict be resolved?")
            .items(&items)
            .default(0)
            .interact()
            .ok()
    }

    /// Print both values after an explicit confirmation.
    ///
    /// The warning names scrollback and session capture specifically: those
    /// persist beyond selfie's control and are the actual residual risk of
    /// showing a credential at all.
    ///
    /// Output goes through `DisplayManager`, which writes straight to the
    /// terminal. It must not be routed through `tracing`, which would put the
    /// values in the log file.
    fn reveal(&self, incoming: &[u8], current: &[u8]) {
        self.display.print_warning(
            "This prints secret values to your terminal. They will remain in scrollback and \
             in any session recording or shared screen.",
        );

        let confirmed = dialoguer::Confirm::new()
            .with_prompt("Show values?")
            .default(false)
            .interact()
            .unwrap_or(false);

        if !confirmed {
            return;
        }

        self.display.println(format!(
            "--- incoming ---\n{}",
            String::from_utf8_lossy(incoming)
        ));
        self.display.println(format!(
            "--- current ---\n{}",
            String::from_utf8_lossy(current)
        ));
    }
}

impl ConflictResolver for InteractiveConflictResolver {
    fn resolve(&self, target: &str, detail: ConflictDetail<'_>) -> ConflictResolution {
        let short_target = shorten_path(target);

        // Blank line for breathing room before the conflict block
        self.display.println("");
        self.display
            .print_warning(format!("  Conflict: {short_target}"));

        match detail {
            ConflictDetail::Diff { source, diff } => {
                self.display
                    .print_progress(format!("{} → {short_target}", shorten_path(source)));
                self.display.print_diff(diff);

                match self.prompt(false) {
                    Some(1) => ConflictResolution::Accept,
                    _ => ConflictResolution::Skip,
                }
            }

            ConflictDetail::Secret {
                summary,
                incoming,
                current,
            } => {
                self.display.println(summary);

                // Reveal is offered only on a terminal. Without one there is
                // nobody to read it and no way to confirm the second prompt.
                // Uses the display's own check, which covers stdout and stderr,
                // rather than a fresh stdout-only probe.
                let can_reveal = self.display.is_tty();

                match self.prompt(can_reveal) {
                    Some(1) => ConflictResolution::Accept,
                    Some(2) => {
                        self.reveal(incoming, current);
                        // Ask again, without offering reveal a second time.
                        match self.prompt(false) {
                            Some(1) => ConflictResolution::Accept,
                            _ => ConflictResolution::Skip,
                        }
                    }
                    _ => ConflictResolution::Skip,
                }
            }
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
    let runner = ShellCommandRunner::new(
        ShellCommandRunner::default_shell(),
        config.selfie_config().command_timeout(),
    );
    let mut service = DotfileServiceImpl::new(repo, fs, runner, config.selfie_config().clone());

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

    let display_for_handler = display.clone();
    let processor = EventProcessor::new(display.clone());
    let result = processor
        .process_events(event_stream, |event| match event {
            // Suppress per-file progress lines — the summary is sufficient
            selfie::package::event::PackageEvent::DotfileDeploying { .. }
            | selfie::package::event::PackageEvent::DotfileDeployed { .. } => true,

            // Render the completion summary as info (blue) rather than
            // success (green) so it doesn't blend with diff additions.
            selfie::package::event::PackageEvent::Completed { result, .. } => {
                match result {
                    selfie::package::event::OperationResult::Success(success) => {
                        display_for_handler.print_info(success.to_string());
                    }
                    _ => return false, // let default handler deal with failures
                }
                true
            }
            _ => false,
        })
        .await;

    result.exit_code
}
