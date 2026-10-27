use selfie::package::{
    event::{
        CollectionRefusal, OperationResult, OperationSuccess, Outcome, PackageEvent,
        ValidationResultData,
    },
    port::PackageParseError,
    service::{SpecService, skipped_spec_warning},
};

use crate::display_manager::Channel;
use crate::{
    commands::validation_display::{ValidationGroup, ValidationRow, display_validation_groups},
    config::CliConfig,
    display_manager::DisplayManager,
    event_processor::{EventProcessor, Exit},
    status_style,
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
    let mut skipped = Vec::new();
    let result = processor
        .process_events(event_stream, |event| {
            handle_validate_all_event(event, &mut skipped, config, display)
        })
        .await;
    // A stream that ended without a result, as one does when the task running it
    // panics, still names what the run skipped.
    print_skipped(&mut skipped, &[], display);
    result.exit
}

// A skipped spec is held until the result says whether it is one the run could
// not validate. If it is, the result names it, and printing the skip line as well
// would name it twice, the first time far above the results. If it is not, as for
// a dotfiles spec a package spec shadows, the skip line is the only mention.
fn handle_validate_all_event(
    event: &PackageEvent,
    skipped: &mut Vec<PackageParseError>,
    config: &CliConfig,
    display: &DisplayManager,
) -> bool {
    match event {
        PackageEvent::SpecSkipped { error, .. } => {
            skipped.push(error.clone());
            true
        }
        PackageEvent::Completed { result, .. } => {
            let unvalidated = match result {
                OperationResult::Success(OperationSuccess::SpecsValidated {
                    unvalidated, ..
                }) => unvalidated.as_slice(),
                _ => &[],
            };
            print_skipped(skipped, unvalidated, display);
            display_unvalidated(unvalidated, config, display);
            // The summary is the shared handler's, and comes last.
            false
        }
        // A canceled run sends no result, so nothing it skipped is listed there,
        // and every skip line prints above the cancel line.
        PackageEvent::Canceled { .. } => {
            print_skipped(skipped, &[], display);
            false
        }
        _ => handle_validate_event(event, config, display),
    }
}

/// Print and drop the skip line for each of `skipped` that `unvalidated` does not
/// list.
fn print_skipped(
    skipped: &mut Vec<PackageParseError>,
    unvalidated: &[CollectionRefusal],
    display: &DisplayManager,
) {
    for error in skipped.drain(..) {
        if !unvalidated
            .iter()
            .any(|refusal| names_file(refusal, &error))
        {
            display.print_warning(skipped_spec_warning(&error));
        }
    }
}

/// Whether `refusal` is the spec file `error` reports.
fn names_file(refusal: &CollectionRefusal, error: &PackageParseError) -> bool {
    matches!(refusal, CollectionRefusal::UnloadableSpec(refused)
        if refused.package_path() == error.package_path())
}

/// One entry in the issues section for something the run could not validate.
struct UnvalidatedIssue {
    /// The group's label: the file, the name or the directory.
    label: String,
    /// The word the results use for it.
    category: &'static str,
    field: &'static str,
    message: String,
    location: Option<selfie::yaml::SourceLocation>,
}

impl UnvalidatedIssue {
    fn of(refusal: &CollectionRefusal) -> Option<Self> {
        match refusal {
            CollectionRefusal::UnloadableSpec(error) => Some(Self {
                label: error.package_path().display().to_string(),
                category: "unparsable",
                field: error.kind().label(),
                message: error.reason(),
                location: error.location(),
            }),
            _ => None,
        }
    }
}

/// Print the issues section's groups for what the run could not validate.
fn display_unvalidated(
    unvalidated: &[CollectionRefusal],
    config: &CliConfig,
    display: &DisplayManager,
) {
    let issues: Vec<UnvalidatedIssue> = unvalidated
        .iter()
        .filter_map(UnvalidatedIssue::of)
        .collect();
    let groups: Vec<ValidationGroup<'_>> = issues
        .iter()
        .map(|issue| ValidationGroup {
            label: &issue.label,
            rows: vec![ValidationRow {
                level: selfie::validation::ValidationLevel::Error,
                category: issue.category.to_string(),
                field: issue.field,
                message: &issue.message,
                location: issue.location,
                suggestion: None,
            }],
        })
        .collect();
    display_validation_groups(&groups, config.use_colors(), display, Channel::Stdout);
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
        .field("Status", status_style::format_valid(config.use_colors()))
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

    let rows: Vec<ValidationRow<'_>> = validation_result
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
        .collect();

    let groups = vec![ValidationGroup {
        label: &validation_result.package_name,
        rows,
    }];

    display_validation_groups(&groups, config.use_colors(), display, Channel::Stdout);
}

#[cfg(test)]
mod tests {
    use super::*;
    use selfie::package::{
        event::{EventStream, OperationContext, OperationInfo, metadata::OperationType},
        port::PackageParseKind,
        service::MockSpecService,
    };

    fn info() -> OperationInfo {
        OperationInfo {
            id: uuid::Uuid::new_v4(),
            operation_type: OperationType::SpecValidateAll,
            package_name: "all".to_string(),
            environment: "test".to_string(),
            context: OperationContext::default(),
            timestamp: std::time::Instant::now(),
        }
    }

    // A run canceled after it skipped a spec still names it, above the cancel
    // line: a canceled run sends no result, so a held skip line may not wait for
    // one.
    #[tokio::test]
    async fn a_canceled_run_still_names_the_spec_it_skipped() {
        let events = vec![
            PackageEvent::SpecSkipped {
                operation_info: info(),
                error: PackageParseError::new(
                    "/p/creds.yml",
                    PackageParseKind::Unreadable {
                        reason: "a fifo".to_string(),
                    },
                ),
            },
            PackageEvent::Canceled {
                operation_info: info(),
                reason: "Ctrl+C".to_string(),
            },
        ];
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

        let out: String = display
            .printed()
            .into_iter()
            .map(|(_, line)| line + "\n")
            .collect();
        assert_eq!(exit, Exit::Canceled, "{out}");
        let at = |text: &str| {
            out.find(text)
                .unwrap_or_else(|| panic!("no {text:?} in: {out}"))
        };
        assert!(
            at("Skipping package file /p/creds.yml") < at("Operation canceled"),
            "{out}"
        );
    }
}
