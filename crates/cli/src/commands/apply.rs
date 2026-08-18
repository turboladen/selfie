//! Apply command handler for deploying dotfiles
//!
//! This module handles the `selfie apply` CLI command, which deploys
//! dotfiles defined in package YAML files to their target
//! locations on the system.

use std::sync::Arc;

use selfie::dotfile_service::diff::unified_diff;
use selfie::dotfile_service::port::{
    ApplyOptions, ConflictDetail, ConflictResolution, ConflictResolver, DotfileService,
};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::{
    cli::ApplyArgs,
    commands::common::create_dotfile_service,
    config::CliConfig,
    display_manager::{DisplayManager, shorten_path},
    event_processor::EventProcessor,
};

/// What [`InteractiveConflictResolver::reveal`] displays.
enum RevealBody {
    /// A unified diff between the current target and the resolved output.
    Diff(String),
    /// Neither value can be rendered as text; their sizes, and nothing else.
    Binary { incoming: usize, current: usize },
}

/// Build what `reveal` shows for a secret-bearing conflict: a unified diff for
/// UTF-8 content, byte counts for anything else.
// A diff rather than both values in full. At context radius 3 only the changed
// hunks print, so a credentials file with one rotated token among several fields
// puts one line into scrollback instead of the whole file twice. The reveal
// warning already names scrollback and session capture as the residual risk of
// revealing at all, so showing less is a direct reduction of it — and a diff
// answers the question the prompt poses, "rotated or hand-edited?", which a
// line-count summary could only approximate.
//
// This is a property of the reveal path, not the event path. Events have no diff
// to carry, which is why the non-revealing summary exists; here both values are
// already in hand.
//
// Non-UTF-8 content takes neither path. Diffing the `from_utf8_lossy` form of a
// binary secret produces garbage, and writing raw bytes to a terminal can emit
// control sequences and mangle the session. Byte counts are honest,
// terminal-safe, and still answer "did it change".
//
// This makes revealing smaller, not safe: a single-value secret — an SSH key
// blob, a bare token — is one line, so its diff shows both values in full.
fn reveal_body(incoming: &[u8], current: &[u8], target: &str) -> RevealBody {
    // Old is the target and new is the resolved output, matching the direction
    // the repository-file conflict path already uses.
    match (std::str::from_utf8(current), std::str::from_utf8(incoming)) {
        (Ok(current_text), Ok(incoming_text)) => RevealBody::Diff(unified_diff(
            current_text,
            incoming_text,
            target,
            "resolved output",
        )),
        _ => RevealBody::Binary {
            incoming: incoming.len(),
            current: current.len(),
        },
    }
}

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

    /// Show the difference between the two values, after an explicit confirmation.
    ///
    /// The warning names scrollback and session capture specifically: those
    /// persist beyond selfie's control and are the actual residual risk of
    /// showing a credential at all.
    ///
    /// Output goes through `DisplayManager`, which writes straight to the
    /// terminal. It must not be routed through `tracing`, which would put the
    /// values in the log file.
    fn reveal(&self, incoming: &[u8], current: &[u8], target: &str) {
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

        match reveal_body(incoming, current, target) {
            RevealBody::Diff(diff) => self.display.print_diff(&diff),
            RevealBody::Binary { incoming, current } => self.display.println(format!(
                "Content is not valid UTF-8, so it cannot be shown as a diff or printed \
                 safely.\n  resolved output : {incoming} bytes\n  current target  : {current} bytes"
            )),
        }
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
                        self.reveal(incoming, current, target);
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

/// The completion summary apply renders itself, if it renders this one.
///
/// `Some` only for a clean success, which apply prints as info (blue) rather
/// than success (green) so it does not blend with diff additions.
///
/// `None` sends the event to `EventProcessor`'s default handler, and that is
/// load-bearing rather than cosmetic: `handle_event` is the only thing that
/// writes `exit_code`, and `process_events` skips it entirely for any event a
/// custom handler claims. A success carrying a refusal handled *here* would be
/// printed prettily and exit 0. So the exit code for a
/// refusal cannot be fixed in `event_processor.rs` alone; this function is the
/// other half.
fn summary_to_render(
    result: &selfie::package::event::OperationResult,
) -> Option<&selfie::package::event::OperationSuccess> {
    match result {
        selfie::package::event::OperationResult::Success(success) if !success.had_refusals() => {
            Some(success)
        }
        // Failures, and successes carrying a refusal.
        _ => None,
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
    cancellation_token: CancellationToken,
) -> i32 {
    let options = ApplyOptions {
        dry_run: args.dry_run,
        auto_accept: args.yes,
        conflict_resolver: Some(Arc::new(InteractiveConflictResolver {
            display: display.clone(),
        })),
    };

    // The shared constructor, so apply runs provider commands under the same
    // shell as install and check.
    let service = create_dotfile_service(config, display, cancellation_token);

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

            selfie::package::event::PackageEvent::Completed { result, .. } => {
                match summary_to_render(result) {
                    Some(success) => {
                        display_for_handler.print_info(success.to_string());
                        true
                    }
                    None => false,
                }
            }
            _ => false,
        })
        .await;

    result.exit_code
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "s3cr3t-rotated-token";

    // A credentials file with one changed field among many — the driving case.
    fn multi_field(token: &str) -> String {
        format!(
            "---\n\
             :rubygems_api_key: {token}\n\
             filler_two: unchanged-two\n\
             filler_three: unchanged-three\n\
             filler_four: unchanged-four\n\
             filler_five: unchanged-five\n\
             far_away_marker: UNCHANGED-AND-DISTANT\n"
        )
    }

    use selfie::package::event::{OperationFailure, OperationResult, OperationSuccess, StepCount};

    fn applied(refused_count: usize) -> OperationResult {
        OperationResult::Success(OperationSuccess::DotfilesApplied {
            deployed_count: 0,
            skipped_count: 0,
            conflict_count: 0,
            refused_count,
            environment: "test".to_string(),
            steps_completed: StepCount::new(1, 1),
        })
    }

    // A clean success is rendered here and never reaches the default handler.
    #[test]
    fn a_clean_success_is_rendered_by_applies_own_handler() {
        assert!(summary_to_render(&applied(0)).is_some());
    }

    // A success carrying a refusal is handed on, so the exit code gets set.
    //
    // Claiming it here would print the summary and swallow the event, and
    // `EventProcessor::handle_event` — the only writer of `exit_code` — would
    // never see it.
    #[test]
    fn a_success_carrying_a_refusal_falls_through_to_the_default_handler() {
        assert!(summary_to_render(&applied(1)).is_none());
    }

    // Control: failures were always handed on, and still are.
    #[test]
    fn a_failure_falls_through_to_the_default_handler() {
        let failure = OperationResult::Failure(OperationFailure::Generic("boom".to_string()));
        assert!(summary_to_render(&failure).is_none());
    }

    #[test]
    fn revealing_a_rotated_field_shows_a_diff_not_both_files() {
        let current = multi_field("old-token");
        let incoming = multi_field(SECRET);

        let RevealBody::Diff(diff) = reveal_body(
            incoming.as_bytes(),
            current.as_bytes(),
            "~/.gem/credentials",
        ) else {
            panic!("expected a diff for UTF-8 content");
        };

        // The change itself is shown — that is the point of revealing.
        assert!(diff.contains(SECRET), "got: {diff}");
        assert!(diff.contains("old-token"), "got: {diff}");

        // But the untouched remainder is not. This is the whole saving: with two
        // full dumps, every line below would appear twice in scrollback.
        assert!(
            !diff.contains("UNCHANGED-AND-DISTANT"),
            "a line far from the change must not reach scrollback: {diff}"
        );
    }

    #[test]
    fn a_reveal_diff_is_labeled_with_the_target_and_the_resolved_output() {
        let current = multi_field("old-token");
        let incoming = multi_field(SECRET);

        let RevealBody::Diff(diff) = reveal_body(
            incoming.as_bytes(),
            current.as_bytes(),
            "~/.gem/credentials",
        ) else {
            panic!("expected a diff");
        };

        assert!(diff.contains("--- ~/.gem/credentials"), "got: {diff}");
        assert!(diff.contains("+++ resolved output"), "got: {diff}");
    }

    #[test]
    fn non_utf8_content_is_reported_by_size_rather_than_diffed_or_dumped() {
        // An SSH key blob or similar. Diffing the lossy form produces garbage and
        // dumping the bytes can emit terminal control sequences.
        let incoming = [0x00u8, 0xff, 0xfe, 0x1b, b'[', b'2', b'J'];
        let current = [0x00u8, 0xff];

        let body = reveal_body(&incoming, &current, "~/.ssh/id_ed25519");

        match body {
            RevealBody::Binary {
                incoming: i,
                current: c,
            } => {
                assert_eq!(i, 7);
                assert_eq!(c, 2);
            }
            RevealBody::Diff(diff) => {
                panic!("binary content must not be diffed, got: {diff}")
            }
        }
    }

    #[test]
    fn one_non_utf8_side_is_enough_to_refuse_the_diff() {
        // Rotating a text credential to a binary one, or the reverse.
        let incoming = [0xffu8, 0xfe];
        let current = "readable\n";

        assert!(matches!(
            reveal_body(&incoming, current.as_bytes(), "~/.x"),
            RevealBody::Binary { .. }
        ));
        assert!(matches!(
            reveal_body(current.as_bytes(), &incoming, "~/.x"),
            RevealBody::Binary { .. }
        ));
    }

    #[test]
    fn a_single_value_secret_still_reveals_both_values() {
        // Documented as no saving and no harm: a one-line secret's diff shows
        // both values in full. Asserted so the limitation stays visible.
        let RevealBody::Diff(diff) = reveal_body(b"new-token\n", b"old-token\n", "~/.token") else {
            panic!("expected a diff");
        };

        assert!(diff.contains("new-token"), "got: {diff}");
        assert!(diff.contains("old-token"), "got: {diff}");
    }
}
