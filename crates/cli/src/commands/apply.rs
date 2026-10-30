//! Apply command handler for deploying dotfiles
//!
//! This module handles the `selfie apply` CLI command, which deploys
//! dotfiles defined in package YAML files to their target
//! locations on the system.

use crate::terminal_text::{Prose, Shown, lit, shown};
use std::sync::{Arc, Mutex};

use selfie::package::event::{DotfileSource, PlannedEntry};

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
    display_manager::{Channel, Diff, DisplayManager, PromptFailure, shorten_path},
    entry_rows::{EntryOutcome, EntryRow},
    event_processor::{EventProcessor, Exit},
    source_paths::{force_heading, relative_text},
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
// hunks print, so a credentials file with one rotated token puts one line into
// scrollback instead of the whole file twice, and it answers the question the
// prompt poses -- "rotated or hand-edited?".
//
// A property of the reveal path, not the event path: events have no diff to
// carry, which is why the non-revealing summary exists.
//
// Non-UTF-8 content takes neither path -- raw bytes can emit control sequences
// and mangle the session. This makes revealing smaller, not safe.
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
    // The conflicts skipped because there was no terminal to ask on.
    unasked: Arc<Mutex<Unasked>>,
}

/// Conflicts left as they are because there was no terminal to ask on, by what
/// would settle them.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct Unasked {
    /// Repository-file conflicts, which `--yes` overwrites.
    files: usize,
    /// Secret-bearing conflicts, which only a terminal can accept.
    secrets: usize,
}

impl Unasked {
    /// Tell the user what settles each kind left unasked.
    fn report(&self, display: &DisplayManager) {
        let Self { files, secrets } = *self;
        if files > 0 {
            display.print_suggestion(shown!(
                "{} {} left as {} with no terminal to ask on. Pass --yes to overwrite {}.",
                files,
                crate::formatters::plural(files, lit!("conflict was"), lit!("conflicts were")),
                crate::formatters::plural(files, lit!("it is"), lit!("they are")),
                crate::formatters::plural(files, lit!("it"), lit!("them"))
            ));
        }
        if secrets > 0 {
            display.print_suggestion(shown!(
                "{} secret-bearing {} left as {} with no terminal to ask on. Only a \
                 terminal can accept one; --yes does not.",
                secrets,
                crate::formatters::plural(secrets, lit!("conflict was"), lit!("conflicts were")),
                crate::formatters::plural(secrets, lit!("it is"), lit!("they are"))
            ));
        }
    }
}

/// Which conflict is being asked about, and so what accepting costs.
// A type rather than a flag: the two kinds make opposite promises about the
// target's current content, and one shared label cannot carry both.
#[derive(Clone, Copy)]
enum Prompt {
    /// A repository file. Content the write would destroy is copied aside first.
    RepositoryFile,
    /// A secret-bearing entry. Nothing is copied aside, ever.
    Secret {
        /// Whether to offer showing the two values.
        reveal: bool,
    },
}

impl Prompt {
    /// What accepting does, including what becomes of the current content.
    fn accept_label(self) -> &'static str {
        match self {
            // "anything it would destroy" rather than "the current content":
            // `BothChanged` is a conflict even when both sides hold the same
            // content, and then there is nothing to copy.
            Self::RepositoryFile => {
                "Accept (overwrite target; anything it would destroy is copied aside first)"
            }
            Self::Secret { .. } => "Accept (overwrite target; no copy of the current one is kept)",
        }
    }
}

impl InteractiveConflictResolver {
    /// Ask how to resolve, offering reveal only for a secret conflict that asked
    /// for it.
    ///
    /// Accept is never the default, and reveal is never reachable by accepting a
    /// default: both require a deliberate selection.
    fn prompt(&self, kind: Prompt) -> Option<usize> {
        let mut items = vec!["Skip (keep target as-is)", kind.accept_label()];
        if matches!(kind, Prompt::Secret { reveal: true }) {
            items.push("Reveal the two values, then choose");
        }

        self.answered(
            self.display.prompt(
                dialoguer::Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
                    .with_prompt("How should this conflict be resolved?")
                    .items(&items)
                    .default(0),
            ),
            |unasked| match kind {
                Prompt::RepositoryFile => unasked.files += 1,
                Prompt::Secret { .. } => unasked.secrets += 1,
            },
        )
    }

    /// The answer a prompt got, or `None` when it got none. No terminal counts
    /// the conflict with `unasked`.
    fn answered<T>(
        &self,
        answer: Result<T, PromptFailure>,
        unasked: impl FnOnce(&mut Unasked),
    ) -> Option<T> {
        match answer {
            Ok(answer) => Some(answer),
            // The prompt has already canceled the run.
            Err(PromptFailure::Interrupted) => None,
            Err(PromptFailure::NoTerminal) => {
                if let Ok(mut count) = self.unasked.lock() {
                    unasked(&mut count);
                }
                None
            }
            Err(PromptFailure::Unreadable(error)) => {
                self.display.print_error(shown!(
                    "Could not read the answer, so the conflict is left as it is: {}",
                    crate::received::sentence(error)
                ));
                None
            }
        }
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
        self.display.print_warning(lit!(
            "This prints secret values to your terminal. They will remain in scrollback and \
             in any session recording or shared screen."
        ));

        // Reveal is offered only on a terminal, so this prompt is never unasked.
        let confirmed = self
            .answered(
                self.display.prompt(
                    dialoguer::Confirm::new()
                        .with_prompt("Show values?")
                        .default(false),
                ),
                |_| {},
            )
            .unwrap_or(false);

        if !confirmed {
            return;
        }

        match reveal_body(incoming, current, target) {
            RevealBody::Diff(diff) => self.display.print_diff(Diff::new(&diff)),
            RevealBody::Binary { incoming, current } => self.display.println(shown!(
                "Content is not valid UTF-8, so it cannot be shown as a diff or printed \
                 safely.\n  resolved output : {} bytes\n  current target  : {} bytes",
                incoming,
                current
            )),
        }
    }
}

impl InteractiveConflictResolver {
    /// Print the conflict's row in the entry table, with its base directory
    /// named inside the block.
    // Inside the block, not through the event processor's headings: this runs on
    // the service's thread while the processor may still be printing lines queued
    // before it, each under its own heading.
    fn print_conflict_row(&self, source: &DotfileSource, target: &str) {
        let _block = self.display.hold_block();
        // A blank line first, so the block's diff and prompt do not read as part
        // of the row above.
        self.display.println(lit!(""));
        if let Some(base) = source.base() {
            force_heading(&self.display, Channel::Stdout, base.kind, &base.directory);
        }
        self.display.print_entry_row(&EntryRow {
            outcome: EntryOutcome::Conflict,
            source: relative_text(source),
            target: shorten_path(target),
            detail: None,
        });
        // The prompt asks on stderr. With stdout redirected, the row and the
        // diff land in the file, so the prompt's own stream names the target.
        if self.display.can_prompt() && !self.display.is_tty() {
            self.display
                .print_warning(shown!("Conflict: {}", shorten_path(target)));
        }
    }
}

impl ConflictResolver for InteractiveConflictResolver {
    fn planned(&self, entries: &[PlannedEntry]) {
        self.display.plan_entry_columns(entries);
    }

    fn resolve(&self, target: &str, detail: ConflictDetail<'_>) -> ConflictResolution {
        match detail {
            ConflictDetail::Diff { source, diff } => {
                self.print_conflict_row(source, target);
                self.display.print_diff(Diff::new(diff));

                match self.prompt(Prompt::RepositoryFile) {
                    Some(1) => ConflictResolution::Accept,
                    _ => ConflictResolution::Skip,
                }
            }

            ConflictDetail::Secret {
                source,
                summary,
                incoming,
                current,
            } => {
                self.print_conflict_row(source, target);
                self.display.println(Shown::from(Prose::new(summary)));

                // Reveal is offered only when stdout is a terminal as well as
                // stderr: the values print on stdout, and a redirected stdout
                // would put them in a file.
                let can_reveal = self.display.is_tty();

                match self.prompt(Prompt::Secret { reveal: can_reveal }) {
                    Some(1) => ConflictResolution::Accept,
                    Some(2) => {
                        self.reveal(incoming, current, target);
                        if self.display.is_canceled() {
                            return ConflictResolution::Skip;
                        }
                        // Ask again, without offering reveal a second time.
                        match self.prompt(Prompt::Secret { reveal: false }) {
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
/// than success (green) so it does not blend with diff additions. Anything else
/// takes the default rendering, which prints it at the level of its verdict.
fn summary_to_render(
    result: &selfie::package::event::OperationResult,
) -> Option<&selfie::package::event::OperationSuccess> {
    match result {
        selfie::package::event::OperationResult::Success(success)
            if success.outcome() == selfie::package::event::Outcome::Clean =>
        {
            Some(success)
        }
        _ => None,
    }
}

/// Handle the apply command
///
/// Creates a `DotfileServiceImpl` over the package and standalone dotfiles
/// repositories and delegates to `apply_all` or `apply` based on whether a
/// package name was given. Events are processed through the standard
/// `EventProcessor`.
pub(crate) async fn handle_apply(
    args: &ApplyArgs,
    config: &CliConfig,
    display: &DisplayManager,
    cancellation_token: CancellationToken,
) -> Exit {
    let unasked = Arc::new(Mutex::new(Unasked::default()));
    let options = ApplyOptions {
        dry_run: args.dry_run,
        auto_accept: args.yes,
        conflict_resolver: Some(Arc::new(InteractiveConflictResolver {
            display: display.clone(),
            unasked: Arc::clone(&unasked),
        })),
    };

    // The shared constructor, so apply runs provider commands under the same
    // shell as install and check.
    let service = create_dotfile_service(config, cancellation_token);

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
            // The prompt already showed a conflict the resolver declined.
            selfie::package::event::PackageEvent::DotfileConflict { declined, .. } => *declined,

            selfie::package::event::PackageEvent::Completed { result, .. } => {
                match summary_to_render(result) {
                    Some(success) => {
                        display_for_handler.print_info(crate::received::success(success));
                        true
                    }
                    None => false,
                }
            }
            _ => false,
        })
        .await;

    // The summary counts every conflict; this says what settles the ones
    // nobody was asked about.
    if let Ok(unasked) = unasked.lock() {
        unasked.report(display);
    }

    result.exit
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each label must say its own half and not the other's: accepting a
    // repository-file conflict is recoverable and accepting a secret one is not,
    // and a reader deciding has to be told which. Asserting what a label must
    // *not* say is what catches a copy-paste between the two arms, which a test
    // that only checks the prompt appeared cannot see.
    #[test]
    fn each_accept_label_says_what_happens_to_the_current_content() {
        let repository = Prompt::RepositoryFile.accept_label();
        let secret = Prompt::Secret { reveal: false }.accept_label();

        assert!(
            repository.contains("copied aside") && !repository.contains("no copy"),
            "{repository}"
        );
        assert!(
            !repository.contains("the current content"),
            "a conflict whose two sides already hold the same content keeps no copy, \
             so the label must not promise one: {repository}"
        );
        assert!(
            secret.contains("no copy") && !secret.contains("copied aside"),
            "{secret}"
        );
        assert_eq!(
            Prompt::Secret { reveal: true }.accept_label(),
            secret,
            "offering to reveal must not change what accepting costs"
        );
    }

    // The prompt names the conflicting file's base directory inside its own
    // block, since it cannot rely on a heading the event processor printed.
    #[test]
    fn the_prompt_names_the_base_directory_inside_its_block() {
        use selfie::package::event::{BaseKind, RepoPath, SourceBase};

        let display = DisplayManager::new(false);
        let resolver = InteractiveConflictResolver {
            display: display.clone(),
            unasked: Arc::default(),
        };
        let source = DotfileSource::File(RepoPath {
            base: Some(SourceBase {
                kind: BaseKind::PackageDirectory,
                directory: "/r/packages".into(),
            }),
            path: "bat/config".into(),
        });

        resolver.print_conflict_row(&source, "~/.config/bat/config");

        let lines: Vec<String> = display
            .printed()
            .into_iter()
            .map(|(_, line)| line)
            .collect();
        assert_eq!(
            lines,
            vec![
                String::new(),
                "Packages: /r/packages".to_string(),
                "conflict    bat/config → ~/.config/bat/config".to_string()
            ]
        );
    }

    // The prompt's heading and source line wait for a block another printer
    // holds, so neither can land inside it.
    #[test]
    fn the_prompt_prints_its_source_as_one_block() {
        use selfie::package::event::{BaseKind, RepoPath, SourceBase};

        let display = DisplayManager::new(false);
        let resolver = InteractiveConflictResolver {
            display: display.clone(),
            unasked: Arc::default(),
        };
        let source = DotfileSource::File(RepoPath {
            base: Some(SourceBase {
                kind: BaseKind::DotfilesDirectory,
                directory: "/r/dotfiles".into(),
            }),
            path: "zshrc".into(),
        });

        let held = display.hold_block();
        let printer = std::thread::spawn(move || resolver.print_conflict_row(&source, "~/.zshrc"));
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(display.printed().is_empty(), "{:?}", display.printed());
        drop(held);
        printer.join().unwrap();

        assert_eq!(display.printed().len(), 3, "{:?}", display.printed());
    }

    fn source() -> DotfileSource {
        DotfileSource::File(selfie::package::event::RepoPath {
            base: None,
            path: "creds.tpl".into(),
        })
    }

    fn resolver(display: &DisplayManager) -> InteractiveConflictResolver {
        InteractiveConflictResolver {
            display: display.clone(),
            unasked: Arc::default(),
        }
    }

    fn resolve_secret(resolver: &InteractiveConflictResolver) -> ConflictResolution {
        let source = source();
        resolver.resolve(
            "~/.creds",
            ConflictDetail::Secret {
                source: &source,
                summary: "1 field differs",
                incoming: b"new",
                current: b"old",
            },
        )
    }

    // The resolver lays out its conflict row from the plan the library hands
    // it, before any event reaches the processor, so a conflict on the first
    // entry lines up with the rows after it.
    #[test]
    fn planned_sets_the_columns_before_any_event() {
        use selfie::package::event::PlannedEntry;

        let display = DisplayManager::new(false);
        let resolver = resolver(&display);
        resolver.planned(&[
            PlannedEntry {
                source: source(),
                target: "/h/.creds".to_string(),
            },
            PlannedEntry {
                source: DotfileSource::Command("echo a-longer-command".to_string()),
                target: "/h/.a-much-longer-target".to_string(),
            },
        ]);

        resolver.print_conflict_row(&source(), "/h/.creds");

        let lines: Vec<String> = display.printed().into_iter().map(|(_, l)| l).collect();
        // Padded to the planned command's width, 30 columns.
        assert_eq!(
            lines,
            vec![
                String::new(),
                format!("conflict    {:<30} → /h/.creds", "creds.tpl")
            ]
        );
    }

    // The prompt asks on stderr, so when stdout is not a terminal the target
    // is named there too; on a terminal the row already shows it.
    #[test]
    fn a_prompt_with_stdout_redirected_names_its_target_on_stderr() {
        use crate::display_manager::answer;

        let redirected = DisplayManager::new(false).answering(vec![answer(0_usize)]);
        resolver(&redirected).print_conflict_row(&source(), "/h/.creds");
        let on_stderr: Vec<String> = redirected
            .printed()
            .into_iter()
            .filter(|(channel, _)| *channel == Channel::Stderr)
            .map(|(_, line)| line)
            .collect();
        assert_eq!(on_stderr, vec!["Conflict: /h/.creds".to_string()]);

        let terminal = DisplayManager::new(false)
            .drawing_as_a_terminal()
            .answering(vec![answer(0_usize)]);
        resolver(&terminal).print_conflict_row(&source(), "/h/.creds");
        assert!(
            terminal
                .printed()
                .iter()
                .all(|(channel, _)| *channel == Channel::Stdout),
            "{:?}",
            terminal.printed()
        );
    }

    // Ctrl+C at the conflict prompt cancels the run and leaves the target.
    #[test]
    fn ctrl_c_at_the_conflict_prompt_cancels_the_run() {
        use crate::display_manager::ctrl_c;

        let token = CancellationToken::new();
        let display = DisplayManager::new(false)
            .with_cancellation(token.clone())
            .answering(vec![ctrl_c()]);
        let resolver = resolver(&display);
        let source = source();

        let resolution = resolver.resolve(
            "~/.creds",
            ConflictDetail::Diff {
                source: &source,
                diff: "",
            },
        );

        assert!(matches!(resolution, ConflictResolution::Skip));
        assert!(token.is_cancelled());
    }

    // A terminal that fails mid-answer leaves the conflict, and says so.
    #[test]
    fn an_unreadable_answer_skips_the_conflict_and_says_why() {
        let failed = Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "gone").into());
        let token = CancellationToken::new();
        let display = DisplayManager::new(false)
            .with_cancellation(token.clone())
            .answering(vec![failed]);
        let resolver = resolver(&display);
        let source = source();

        let resolution = resolver.resolve(
            "~/.creds",
            ConflictDetail::Diff {
                source: &source,
                diff: "",
            },
        );

        assert!(matches!(resolution, ConflictResolution::Skip));
        assert!(!token.is_cancelled());
        assert!(
            display
                .printed()
                .iter()
                .any(|(_, line)| line.starts_with("Could not read the answer")),
            "{:?}",
            display.printed()
        );
    }

    // Ctrl+C at "Show values?" cancels the run; it is not a "No".
    #[test]
    fn ctrl_c_at_the_reveal_confirmation_cancels_the_run() {
        use crate::display_manager::{answer, ctrl_c};

        let token = CancellationToken::new();
        let display = DisplayManager::new(false)
            .with_cancellation(token.clone())
            .drawing_as_a_terminal()
            .answering(vec![answer(2_usize), ctrl_c()]);
        let resolver = resolver(&display);

        assert!(matches!(
            resolve_secret(&resolver),
            ConflictResolution::Skip
        ));
        assert!(token.is_cancelled());
    }

    // Ctrl+C at the conflict prompt asked again after a reveal cancels the run.
    #[test]
    fn ctrl_c_at_the_prompt_after_a_reveal_cancels_the_run() {
        use crate::display_manager::{answer, ctrl_c};

        let token = CancellationToken::new();
        let display = DisplayManager::new(false)
            .with_cancellation(token.clone())
            .drawing_as_a_terminal()
            .answering(vec![answer(2_usize), answer(false), ctrl_c()]);
        let resolver = resolver(&display);

        assert!(matches!(
            resolve_secret(&resolver),
            ConflictResolution::Skip
        ));
        assert!(token.is_cancelled());
    }

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
            failed_count: 0,
            orphan_count: 0,
            environment: "test".to_string(),
            steps_completed: StepCount::new(1, 1),
        })
    }

    // A clean success is rendered here and never reaches the default handler.
    #[test]
    fn a_clean_success_is_rendered_by_applies_own_handler() {
        assert!(summary_to_render(&applied(0)).is_some());
    }

    // A success carrying a refusal is handed on, so it prints as an error rather
    // than as apply's informational summary.
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
