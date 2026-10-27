use crate::terminal_text::DisplayPath;
use selfie::package::{
    event::{
        CollectionRefusal, OperationResult, OperationSuccess, Outcome, PackageEvent,
        RefusedPackage, SpecStatus, ValidationResultData,
    },
    port::PackageParseError,
    service::{SpecService, skipped_spec_warning},
};
use selfie::validation::ValidationLevel;

use crate::display_manager::Channel;
use crate::{
    commands::presentation::{RowResult, paint_row_text, row_marker},
    commands::validation_display::{
        ValidationGroup, ValidationRow, display_validation_groups, level_counts,
    },
    config::CliConfig,
    display_manager::DisplayManager,
    event_processor::{EventProcessor, Exit, refused_packages_lines},
    status_style,
    tables::{Cell, SafeTable},
};

pub(crate) async fn handle_validate(
    service: &impl SpecService,
    package_name: &str,
    config: &CliConfig,
    display: &DisplayManager,
) -> Exit {
    tracing::debug!("Running validate command for package: {}", package_name);

    display.print_status(format!("Validating {package_name}..."));

    let event_stream = service.validate(package_name, None).await;

    let processor = EventProcessor::new(display.clone());
    let result = processor
        .process_events(event_stream, |event| {
            handle_validate_event(event, config, display)
        })
        .await;
    result.exit
}

pub(crate) async fn handle_validate_all(
    service: &impl SpecService,
    config: &CliConfig,
    display: &DisplayManager,
) -> Exit {
    tracing::debug!("Running validate --all command");

    display.print_status("Validating all packages...");

    let event_stream = service.validate_all().await;

    let processor = EventProcessor::new(display.clone());
    let mut held = Held::default();
    let result = processor
        .process_events(event_stream, |event| {
            handle_validate_all_event(event, &mut held, config.use_colors(), display)
        })
        .await;
    // A stream that ended without a result, as one does when the task running it
    // panics, still shows what the run found before it stopped.
    held.flush(&[], config.use_colors(), display);
    result.exit
}

/// What `validate --all` holds back until its result arrives.
#[derive(Default)]
struct Held {
    results: Vec<ValidationResultData>,
    /// Each warning event, in the order it arrived.
    warnings: Vec<PackageEvent>,
}

impl Held {
    /// Print and drop everything held: the warnings for whatever `unvalidated`
    /// does not list, then the results.
    fn flush(
        &mut self,
        unvalidated: &[CollectionRefusal],
        use_colors: bool,
        display: &DisplayManager,
    ) {
        print_unlisted_warnings(&std::mem::take(&mut self.warnings), unvalidated, display);
        display_results(
            &std::mem::take(&mut self.results),
            unvalidated,
            use_colors,
            display,
        );
    }
}

// Everything is held until the result says what the run could not validate. A
// skipped spec, a refused name or an unreadable directory the result lists is
// named there, so printing its warning as well would name it twice, the first
// time far above the results. Anything else, such as a dotfiles spec a package
// spec shadows, keeps its warning, since that is its only mention.
fn handle_validate_all_event(
    event: &PackageEvent,
    held: &mut Held,
    use_colors: bool,
    display: &DisplayManager,
) -> bool {
    match event {
        PackageEvent::ValidationResultCompleted {
            validation_result, ..
        } => {
            held.results.push(validation_result.clone());
            true
        }
        PackageEvent::SpecSkipped { .. }
        | PackageEvent::PackagesRefused { .. }
        | PackageEvent::Warning { .. } => {
            held.warnings.push(event.clone());
            true
        }
        PackageEvent::Completed { result, .. } => {
            let unvalidated = match result {
                OperationResult::Success(OperationSuccess::SpecsValidated {
                    unvalidated, ..
                }) => unvalidated.as_slice(),
                _ => &[],
            };
            held.flush(unvalidated, use_colors, display);
            // The summary is the shared handler's, and comes last.
            false
        }
        // A canceled run sends no result, so nothing it found is listed there, and
        // every warning prints above whatever was validated before the cancel.
        PackageEvent::Canceled { .. } => {
            held.flush(&[], use_colors, display);
            false
        }
        _ => false,
    }
}

/// Print, in order, the warnings for whatever `warnings` reports that
/// `unvalidated` does not.
fn print_unlisted_warnings(
    warnings: &[PackageEvent],
    unvalidated: &[CollectionRefusal],
    display: &DisplayManager,
) {
    let listed = |named: &dyn Fn(&CollectionRefusal) -> bool| unvalidated.iter().any(named);
    for warning in warnings {
        match warning {
            PackageEvent::SpecSkipped { error, .. } if !listed(&|r| names_file(r, error)) => {
                display.print_warning(skipped_spec_warning(error));
            }
            PackageEvent::PackagesRefused {
                reason, packages, ..
            } => {
                let unlisted: Vec<RefusedPackage> = packages
                    .iter()
                    .filter(|package| !listed(&|r| names_name(r, package)))
                    .cloned()
                    .collect();
                for line in refused_packages_lines(reason, &unlisted, display.is_verbose()) {
                    display.print_warning(line);
                }
            }
            // A directory refusal arrives as a plain warning carrying its
            // sentence, so that sentence is what identifies it.
            PackageEvent::Warning { message, .. } if !listed(&|r| names_directory(r, message)) => {
                display.print_warning(message);
            }
            _ => {}
        }
    }
}

/// Whether `refusal` is the spec file `error` reports.
fn names_file(refusal: &CollectionRefusal, error: &PackageParseError) -> bool {
    matches!(refusal, CollectionRefusal::UnloadableSpec { error: refused, .. }
        if refused.package_path() == error.package_path())
}

/// Whether `refusal` is the unreadable directory the warning `message` reports.
fn names_directory(refusal: &CollectionRefusal, message: &str) -> bool {
    matches!(
        refusal,
        CollectionRefusal::UnlistableDotfilesDirectory(_)
            | CollectionRefusal::UncheckableDotfilesDirectory(_)
    ) && refusal.sentence() == message
}

/// Whether `refusal` is the ambiguous name `package` reports.
fn names_name(refusal: &CollectionRefusal, package: &RefusedPackage) -> bool {
    matches!(refusal, CollectionRefusal::AmbiguousName { name, .. } if *name == package.name)
}

/// What a table cell shows when it has nothing to say.
const NO_VALUE: &str = "-";

/// A validated spec's Issues cell: its count at each level present, or `None`
/// when it has no issue.
fn issue_counts(result: &ValidationResultData) -> Option<String> {
    let count = |level| {
        result
            .issues
            .iter()
            .filter(|issue| issue.level == level)
            .count()
    };
    let counted = level_counts(
        count(ValidationLevel::Error),
        count(ValidationLevel::Warning),
        count(ValidationLevel::Info),
    );
    (!counted.is_empty()).then(|| counted.join(", "))
}

/// Something the run could not validate, as its table row and its entry in the
/// issues section show it.
struct Unvalidated {
    /// The Spec cell: the spec's name, or the directory.
    spec: String,
    /// The Status cell, also the entry's category.
    status: SpecStatus,
    /// The Issues cell, if it has anything to say.
    issues: Option<String>,
    /// The entry's label: the file, the name or the directory.
    label: String,
    /// The entry's Field cell, if it has one.
    field: Option<&'static str>,
    message: String,
    location: Option<selfie::yaml::SourceLocation>,
}

impl Unvalidated {
    fn of(refusal: &CollectionRefusal) -> Self {
        match refusal {
            CollectionRefusal::UnloadableSpec { error, .. } => Self {
                spec: error.display_name(),
                status: refusal.status(),
                issues: error.location().map(|at| at.to_string()),
                label: DisplayPath::of(error.package_path()).to_string(),
                field: Some(error.kind().label()),
                message: error.reason(),
                location: error.location(),
            },
            CollectionRefusal::AmbiguousName { name, paths, .. } => Self {
                spec: name.clone(),
                status: refusal.status(),
                issues: Some(format!("{} files", paths.len())),
                label: name.clone(),
                field: None,
                message: refusal.sentence(),
                location: None,
            },
            // One class for both, as the summary counts them; the message says
            // which.
            CollectionRefusal::UnlistableDotfilesDirectory(error)
            | CollectionRefusal::UncheckableDotfilesDirectory(error) => Self {
                spec: DisplayPath::of(error.path()).to_string(),
                status: refusal.status(),
                issues: None,
                label: DisplayPath::of(error.path()).to_string(),
                field: None,
                message: refusal.sentence(),
                location: None,
            },
        }
    }
}

/// Print the results: one table row per spec, validated or not, then the issues
/// of every spec with a problem.
fn display_results(
    results: &[ValidationResultData],
    unvalidated: &[CollectionRefusal],
    use_colors: bool,
    display: &DisplayManager,
) {
    let mut results: Vec<&ValidationResultData> = results.iter().collect();
    results.sort_by(|a, b| a.package_name.cmp(&b.package_name));
    let unvalidated: Vec<Unvalidated> = unvalidated.iter().map(Unvalidated::of).collect();
    if results.is_empty() && unvalidated.is_empty() {
        return;
    }

    let mut table = SafeTable::new();
    table.set_header(["", "Spec", "Status", "Issues"]);
    let mut add = |result: RowResult, spec: &str, status: SpecStatus, issues: Option<&str>| {
        table.add_row([
            Cell::from(row_marker(&result, use_colors)),
            spec.into(),
            paint_row_text(&status.to_string(), &result, use_colors).into(),
            issues.unwrap_or(NO_VALUE).into(),
        ]);
    };
    for result in &results {
        add(
            result.outcome.into(),
            &result.package_name,
            SpecStatus::from(result.outcome),
            issue_counts(result).as_deref(),
        );
    }
    for entry in &unvalidated {
        add(
            RowResult::Failure,
            &entry.spec,
            entry.status,
            entry.issues.as_deref(),
        );
    }
    display.println(crate::terminal_text::shown!("{}", table));

    let mut groups: Vec<ValidationGroup<'_>> = results
        .iter()
        .filter(|result| result.outcome != Outcome::Clean)
        .map(|result| ValidationGroup {
            label: &result.package_name,
            rows: issue_rows(result),
        })
        .collect();
    groups.extend(unvalidated.iter().map(|entry| ValidationGroup {
        label: &entry.label,
        rows: vec![ValidationRow {
            level: ValidationLevel::Error,
            category: entry.status.to_string(),
            field: entry.field.unwrap_or(NO_VALUE),
            message: &entry.message,
            location: entry.location,
            suggestion: None,
        }],
    }));
    display_validation_groups(&groups, use_colors, display, Channel::Stdout);
}

fn handle_validate_event(
    event: &PackageEvent,
    config: &CliConfig,
    display: &DisplayManager,
) -> bool {
    match event {
        PackageEvent::ValidationResultCompleted {
            validation_result, ..
        } => {
            display_validation_result(validation_result, config, display);
            true // Handled
        }
        _ => false, // Use default handling for other events
    }
}

fn display_validation_result(
    validation_result: &ValidationResultData,
    config: &CliConfig,
    display: &DisplayManager,
) {
    match validation_result.outcome {
        Outcome::Clean => {
            // Show success card for valid packages
            display_validation_success_card(validation_result, config, display);

            // A valid package can still carry informational notices — the
            // apply-time command count is one, and it exists precisely to be
            // seen. Gating the table on the outcome would hide it from every
            // package that has nothing else wrong, which is most of them.
            display_validation_issues_table(validation_result, config, display);
        }
        Outcome::Found | Outcome::Failed => {
            // Show table for packages with issues
            display_validation_issues_table(validation_result, config, display);
        }
    }
}

fn display_validation_success_card(
    validation_result: &ValidationResultData,
    config: &CliConfig,
    display: &DisplayManager,
) {
    display
        .result_card("Validation Results")
        .field("Package", &validation_result.package_name)
        .field("Environment", &validation_result.environment)
        .styled_field("Status", status_style::format_valid(config.use_colors()))
        .print();
}

fn display_validation_issues_table(
    validation_result: &ValidationResultData,
    config: &CliConfig,
    display: &DisplayManager,
) {
    if validation_result.issues.is_empty() {
        return;
    }

    let groups = vec![ValidationGroup {
        label: &validation_result.package_name,
        rows: issue_rows(validation_result),
    }];

    display_validation_groups(&groups, config.use_colors(), display, Channel::Stdout);
}

/// One table row per issue `validation_result` found.
fn issue_rows(validation_result: &ValidationResultData) -> Vec<ValidationRow<'_>> {
    validation_result
        .issues
        .iter()
        .map(|issue| ValidationRow {
            level: issue.level,
            category: issue.category.to_string(),
            field: &issue.field,
            message: &issue.message,
            location: issue.location,
            suggestion: issue.suggestion.as_deref(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use selfie::package::event::ValidationIssueData;
    use selfie::package::port::PackageListError;
    use selfie::validation::ValidationErrorCategory;

    fn printed(display: &DisplayManager) -> String {
        display
            .printed()
            .into_iter()
            .map(|(_, line)| line + "\n")
            .collect()
    }

    // The cells of the results table's row for `spec`: the table with four
    // columns, which no issues table has.
    fn row<'a>(out: &'a str, spec: &str) -> Vec<&'a str> {
        let rows: Vec<Vec<&str>> = out
            .lines()
            .map(|line| {
                line.trim_matches('│')
                    .split('┆')
                    .map(str::trim)
                    .collect::<Vec<_>>()
            })
            .filter(|cells| cells.len() == 4 && cells[1] == spec)
            .collect();
        assert_eq!(rows.len(), 1, "one row for {spec}: {out}");
        rows.into_iter().next().unwrap()
    }

    fn directory(path: &str, state: selfie::fs::DirectoryState) -> PackageListError {
        PackageListError::new(path.into(), state)
    }

    // Either directory selfie could not read through is a row, under the one word
    // the summary counts it by, and an entry whose message says which it was.
    #[test]
    fn an_unreadable_dotfiles_directory_is_a_row_and_an_entry() {
        let denied =
            || std::sync::Arc::new(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        let unvalidated = [
            CollectionRefusal::UnlistableDotfilesDirectory(directory(
                "/listed",
                selfie::fs::DirectoryState::Unlistable(denied()),
            )),
            CollectionRefusal::UncheckableDotfilesDirectory(directory(
                "/checked",
                selfie::fs::DirectoryState::Unknown(denied()),
            )),
        ];
        let display = DisplayManager::new(false);

        display_results(&[], &unvalidated, false, &display);

        let out = printed(&display);
        assert_eq!(row(&out, "/listed")[2], "unlistable", "{out}");
        assert_eq!(row(&out, "/checked")[2], "unlistable", "{out}");
        let label = |path: &str| {
            out.lines()
                .position(|line| line == format!("  ✗ {path}"))
                .unwrap_or_else(|| panic!("no entry for {path}: {out}"))
        };
        let entry = |path: &str| out.lines().skip(label(path)).take(6).collect::<String>();
        assert!(entry("/listed").contains("Failed to load"), "{out}");
        assert!(entry("/checked").contains("Could not check"), "{out}");
    }

    fn issue(level: ValidationLevel) -> ValidationIssueData {
        ValidationIssueData {
            category: ValidationErrorCategory::CommandSyntax,
            field: "environments.test.install".to_string(),
            message: "m".to_string(),
            level,
            suggestion: None,
            location: None,
        }
    }

    fn result(issues: Vec<ValidationIssueData>) -> ValidationResultData {
        ValidationResultData {
            package_name: "p".to_string(),
            environment: "test".to_string(),
            outcome: Outcome::Clean,
            issues,
        }
    }

    #[test]
    fn the_issues_cell_counts_each_level_present() {
        assert_eq!(issue_counts(&result(Vec::new())), None);
        assert_eq!(
            issue_counts(&result(vec![issue(ValidationLevel::Info)])).as_deref(),
            Some("1 notice(s)")
        );
        assert_eq!(
            issue_counts(&result(vec![
                issue(ValidationLevel::Error),
                issue(ValidationLevel::Warning),
                issue(ValidationLevel::Error),
            ]))
            .as_deref(),
            Some("2 error(s), 1 warning(s)")
        );
    }

    // `validate --all` over a stream holding a skipped spec, a validated spec,
    // and then `last`, if any: its exit and everything it printed.
    async fn run_ending_with(last: Option<PackageEvent>) -> (Exit, String) {
        use selfie::package::{
            event::EventStream, port::PackageParseKind, service::MockSpecService,
        };

        let mut events = vec![
            PackageEvent::SpecSkipped {
                operation_info: info(),
                error: PackageParseError::new(
                    "/p/creds.yml",
                    PackageParseKind::Unreadable {
                        reason: "a fifo".to_string(),
                    },
                ),
            },
            PackageEvent::ValidationResultCompleted {
                operation_info: info(),
                validation_result: result(Vec::new()),
            },
        ];
        events.extend(last);
        let mut service = MockSpecService::new();
        service.expect_validate_all().return_once(move || {
            Box::pin(async move { Box::pin(futures::stream::iter(events)) as EventStream })
        });
        let config = CliConfig::wrap_for_test(
            selfie::config::SelfieConfigBuilder::default()
                .environment("test")
                .build(),
        );
        let display = DisplayManager::new(false);

        let exit = handle_validate_all(&service, &config, &display).await;
        (exit, printed(&display))
    }

    fn info() -> selfie::package::event::OperationInfo {
        selfie::package::event::OperationInfo {
            id: uuid::Uuid::new_v4(),
            operation_type: selfie::package::event::metadata::OperationType::SpecValidateAll,
            package_name: "all".to_string(),
            environment: "test".to_string(),
            context: selfie::package::event::OperationContext::default(),
            timestamp: std::time::Instant::now(),
        }
    }

    // A run canceled after it validated a spec and skipped another still shows
    // both, above the cancel line: a canceled run sends no result, so nothing it
    // held may wait for one.
    #[tokio::test]
    async fn a_canceled_run_still_prints_what_it_found() {
        let (exit, out) = run_ending_with(Some(PackageEvent::Canceled {
            operation_info: info(),
            reason: "Ctrl+C".to_string(),
        }))
        .await;

        assert_eq!(exit, Exit::Canceled, "{out}");
        assert_eq!(row(&out, "p")[2], "valid", "{out}");
        let at = |text: &str| {
            out.find(text)
                .unwrap_or_else(|| panic!("no {text:?} in: {out}"))
        };
        assert!(
            at("Skipping package file /p/creds.yml") < at("Operation canceled"),
            "{out}"
        );
        assert!(at("│ ✓ ┆ p ") < at("Operation canceled"), "{out}");
    }

    // A stream that ends with neither a result nor a cancel, as one does when the
    // task running it panics, still shows what it found.
    #[tokio::test]
    async fn a_run_that_never_reports_a_result_still_prints_what_it_found() {
        let (exit, out) = run_ending_with(None).await;

        assert_eq!(exit, Exit::Failed, "{out}");
        assert_eq!(row(&out, "p")[2], "valid", "{out}");
        assert!(out.contains("Skipping package file /p/creds.yml"), "{out}");
    }
}
