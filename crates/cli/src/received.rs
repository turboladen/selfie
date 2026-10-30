//! What the CLI shows of the values it receives from the library, each field
//! as the kind it is.
//!
//! The library hands the CLI plain strings. Each view here maps the fields a
//! command shows to their kinds, so a handler reads a [`Name`], a [`Prose`] or
//! a [`DisplayPath`] and never formats the raw field. A library value's own
//! sentence, its `Display`, is [`sentence`]. An event's fields follow the same
//! rules where the event is handled: a name is a `Name`, a path a
//! `DisplayPath`, and a sentence [`sentence`].

use std::fmt::Display;

use selfie::package::event::{
    AuditResult, AuditResultData, CheckResult, CheckResultData, DependencyStatus,
    EnvironmentStatus, EnvironmentStatusData, OperationInfo, PackageInfoData, RefusedSpec,
    SkipReason, SpecListItem, ValidationIssueData, ValidationResultData,
};
use selfie::package::port::PackageParseError;
use selfie::sync_service::port::PackageValidationIssue;
use selfie::validation::{ValidationIssue, ValidationLevel};
use selfie::yaml::SourceLocation;

use crate::terminal_text::{Name, Output, Prose, Shown, interim_one_line};

/// `value`'s sentence, as the library words it, without a trailing line
/// break.
pub(crate) fn sentence(value: impl Display) -> Prose {
    Prose::new(value.to_string().trim_end_matches(['\r', '\n']))
}

/// `first`, or `second` when `first` is blank, as a tool's output; `None` when
/// both are blank.
fn first_output(first: &str, second: &str) -> Option<Output> {
    [first, second]
        .into_iter()
        .find(|text| !text.trim().is_empty())
        .map(Output::new)
}

/// Where in a file an issue is, as a table cell shows it.
pub(crate) fn location(at: Option<SourceLocation>) -> Option<Name> {
    at.map(|at| Name::new(&at.to_string()))
}

// A command a spec runs, shown on one line until the Command kind exists.
fn command(text: Option<&str>) -> Option<Shown> {
    text.map(interim_one_line)
}

/// An operation, as a run's header and its error summary name it.
pub(crate) struct Operation {
    pub(crate) package: Name,
    pub(crate) environment: Name,
    /// What it is, such as "package install".
    pub(crate) kind: Prose,
}

impl From<&OperationInfo> for Operation {
    fn from(data: &OperationInfo) -> Self {
        Self {
            package: Name::new(&data.package_name),
            environment: Name::new(&data.environment),
            kind: sentence(data.operation_type),
        }
    }
}

/// A check's result, as a command shows it.
pub(crate) struct Check {
    pub(crate) package: Name,
    pub(crate) environment: Name,
    pub(crate) command: Option<Shown>,
    pub(crate) answer: CheckAnswer,
}

/// What a check answered.
pub(crate) enum CheckAnswer {
    /// Installed, with what the check printed: stdout, or stderr when stdout
    /// is blank.
    Installed { output: Option<Output> },
    /// Not installed, with what the check printed: stderr, or stdout when
    /// stderr is blank; and its exit code.
    NotInstalled {
        output: Option<Output>,
        exit_code: Option<i32>,
    },
    /// No answer: no check command, or the check could not run.
    None,
}

impl From<&CheckResultData> for Check {
    fn from(data: &CheckResultData) -> Self {
        let answer = match &data.result {
            CheckResult::Success { stdout, stderr } => CheckAnswer::Installed {
                output: first_output(stdout, stderr),
            },
            CheckResult::Failed {
                stdout,
                stderr,
                exit_code,
            } => CheckAnswer::NotInstalled {
                output: first_output(stderr, stdout),
                exit_code: *exit_code,
            },
            CheckResult::NoCheckCommand | CheckResult::CouldNotRun(_) => CheckAnswer::None,
        };
        Self {
            package: Name::new(&data.package_name),
            environment: Name::new(&data.environment),
            command: command(data.check_command.as_deref()),
            answer,
        }
    }
}

/// An audit's result, as a command shows it.
pub(crate) struct Audit {
    pub(crate) package: Name,
    pub(crate) environment: Name,
    pub(crate) command: Option<Shown>,
    pub(crate) finding: AuditFinding,
}

/// What an audit found.
pub(crate) enum AuditFinding {
    Clean {
        sources: Vec<Name>,
    },
    Conflicts {
        sources: Vec<Name>,
        expected: Vec<Name>,
        /// The sources the spec does not expect.
        unexpected: Vec<Name>,
    },
    NotInstalled,
    NoAuditCommand,
    /// The audit failed, with what it printed, or could not run, with why.
    Error(Shown),
}

fn names(texts: &[String]) -> Vec<Name> {
    texts.iter().map(|text| Name::new(text)).collect()
}

impl From<&AuditResultData> for Audit {
    fn from(data: &AuditResultData) -> Self {
        let finding = match &data.result {
            AuditResult::Clean { sources } => AuditFinding::Clean {
                sources: names(sources),
            },
            AuditResult::Conflicts { sources, expected } => AuditFinding::Conflicts {
                sources: names(sources),
                expected: names(expected),
                unexpected: sources
                    .iter()
                    .filter(|source| !AuditResult::is_expected_source(source, expected))
                    .map(|source| Name::new(source))
                    .collect(),
            },
            AuditResult::NotInstalled => AuditFinding::NotInstalled,
            AuditResult::NoAuditCommand => AuditFinding::NoAuditCommand,
            AuditResult::Error(error) => AuditFinding::Error(Output::new(error).into()),
            // selfie's sentence naming the command, so prose: a carriage return
            // or an escape sequence in the command shows as an escape, where a
            // tool's output would keep only what follows the last return.
            AuditResult::CouldNotRun(failure) => AuditFinding::Error(sentence(failure).into()),
        };
        Self {
            package: Name::new(&data.package_name),
            environment: Name::new(&data.environment),
            command: command(data.audit_command.as_deref()),
            finding,
        }
    }
}

/// A spec's validation result, as a command shows it.
pub(crate) struct Validation {
    pub(crate) package: Name,
    pub(crate) environment: Name,
}

impl From<&ValidationResultData> for Validation {
    fn from(data: &ValidationResultData) -> Self {
        Self {
            package: Name::new(&data.package_name),
            environment: Name::new(&data.environment),
        }
    }
}

/// One validation issue, as a command shows it.
pub(crate) struct Issue {
    pub(crate) level: ValidationLevel,
    /// The issue's category, worded for a person.
    pub(crate) category: Shown,
    pub(crate) field: Shown,
    pub(crate) message: Shown,
    /// How to fix it, when the issue says.
    pub(crate) suggestion: Option<Shown>,
    pub(crate) location: Option<Name>,
}

impl Issue {
    // The one mapping for the library's three issue types, which carry the
    // same fields.
    fn of(
        level: ValidationLevel,
        category: impl Display,
        field: &str,
        message: &str,
        suggestion: Option<&str>,
        at: Option<SourceLocation>,
    ) -> Self {
        Self {
            level,
            category: Name::new(&category.to_string()).into(),
            field: Name::new(field).into(),
            message: sentence(message).into(),
            suggestion: suggestion.map(|suggestion| sentence(suggestion).into()),
            location: location(at),
        }
    }
}

impl From<&ValidationIssueData> for Issue {
    fn from(data: &ValidationIssueData) -> Self {
        Self::of(
            data.level,
            data.category,
            &data.field,
            &data.message,
            data.suggestion.as_deref(),
            data.location,
        )
    }
}

impl From<&PackageValidationIssue> for Issue {
    fn from(data: &PackageValidationIssue) -> Self {
        Self::of(
            data.level,
            data.category,
            &data.field,
            &data.message,
            data.suggestion.as_deref(),
            data.location,
        )
    }
}

impl From<&ValidationIssue> for Issue {
    fn from(data: &ValidationIssue) -> Self {
        Self::of(
            data.level(),
            data.category(),
            data.field(),
            data.message(),
            data.suggestion().map(String::as_str),
            data.location(),
        )
    }
}

/// Why a spec file did not load, as the library words it.
pub(crate) fn parse_reason(error: &PackageParseError) -> Shown {
    sentence(error.reason()).into()
}

/// The warning for a spec file a run skips because it does not parse.
pub(crate) fn skipped_spec(error: &PackageParseError) -> Shown {
    sentence(selfie::package::service::skipped_spec_warning(error)).into()
}

/// The warning for a spec file a listing leaves out because several files
/// claim its name.
pub(crate) fn ambiguous_spec(row: &RefusedSpec) -> Shown {
    sentence(selfie::package::service::ambiguous_spec_warning(row)).into()
}

/// Why an entry was skipped, as an entry row's detail shows it.
pub(crate) fn skip_reason(reason: &SkipReason) -> Shown {
    sentence(reason).into()
}

/// A spec's information, as `spec info` shows it.
pub(crate) struct Info {
    pub(crate) name: Name,
    pub(crate) description: Option<Prose>,
    pub(crate) homepage: Option<Name>,
    /// Why apply would refuse the spec here.
    pub(crate) refusal: Option<Prose>,
    /// Why apply would refuse the spec in another environment.
    pub(crate) refusal_elsewhere: Option<Prose>,
    pub(crate) current_environment: Name,
}

impl From<&PackageInfoData> for Info {
    fn from(data: &PackageInfoData) -> Self {
        Self {
            name: Name::new(&data.name),
            description: data.description.as_deref().map(Prose::new),
            homepage: data.homepage.as_deref().map(Name::new),
            refusal: data.refusal.as_deref().map(sentence),
            refusal_elsewhere: data.refusal_elsewhere.as_deref().map(sentence),
            current_environment: Name::new(&data.current_environment),
        }
    }
}

/// One environment of a spec, as `spec info` and `package status` show it.
pub(crate) struct Environment {
    pub(crate) name: Name,
    /// The install and check commands, each keeping a block-scalar
    /// command's line breaks.
    // Prose until the Command kind exists.
    pub(crate) install_command: Option<Prose>,
    pub(crate) check_command: Option<Prose>,
    pub(crate) dependencies: Vec<Name>,
    pub(crate) recommends: Vec<Name>,
    /// Each checked dependency's name, with its status.
    pub(crate) dependency_statuses: Vec<(Name, EnvironmentStatus)>,
    pub(crate) recommend_statuses: Vec<(Name, EnvironmentStatus)>,
}

fn statuses(statuses: &[DependencyStatus]) -> Vec<(Name, EnvironmentStatus)> {
    statuses
        .iter()
        .map(|dependency| (Name::new(&dependency.name), dependency.status.clone()))
        .collect()
}

impl From<&EnvironmentStatusData> for Environment {
    fn from(data: &EnvironmentStatusData) -> Self {
        Self {
            name: Name::new(&data.environment_name),
            install_command: data.install_command.as_deref().map(Prose::new),
            check_command: data.check_command.as_deref().map(Prose::new),
            dependencies: names(&data.dependencies),
            recommends: names(&data.recommends),
            dependency_statuses: statuses(&data.dependency_statuses),
            recommend_statuses: statuses(&data.recommend_statuses),
        }
    }
}

/// A spec file that did not load, as the library words its failure.
pub(crate) fn parse_error(error: &PackageParseError) -> Shown {
    sentence(error).into()
}

/// One spec in `spec list`.
pub(crate) struct SpecItem {
    pub(crate) name: Name,
    pub(crate) description: Prose,
}

impl From<&SpecListItem> for SpecItem {
    fn from(data: &SpecListItem) -> Self {
        Self {
            name: Name::new(&data.name),
            description: Prose::new(data.description.as_deref().unwrap_or_default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use selfie::package::event::UnknownStatus;

    fn environment(dependencies: &[&str]) -> EnvironmentStatusData {
        EnvironmentStatusData {
            environment_name: "env".to_string(),
            is_current: true,
            install_command: None,
            check_command: None,
            dependencies: dependencies.iter().map(ToString::to_string).collect(),
            dependency_statuses: Vec::new(),
            recommends: Vec::new(),
            recommend_statuses: Vec::new(),
            status: EnvironmentStatus::Unknown(UnknownStatus::NoCheckCommand),
        }
    }

    fn info(description: &str) -> PackageInfoData {
        PackageInfoData {
            name: "tool".to_string(),
            description: Some(description.to_string()),
            homepage: None,
            environments: Vec::new(),
            current_environment: "env".to_string(),
            git_status: None,
            refusal: None,
            refusal_elsewhere: None,
            dotfiles: Vec::new(),
            apply_commands: 0,
        }
    }

    // A description is prose: its line breaks are kept and a joiner renders.
    #[test]
    fn a_description_keeps_its_line_breaks() {
        let shown = Info::from(&info("one\ntwo\u{200D}x\u{1b}[2J"))
            .description
            .expect("a description");

        assert_eq!(shown.to_string(), "one\ntwo\u{200D}x\\u{1b}[2J");
    }

    // A dependency is a name: a line break in it is escaped, so the list stays
    // on one line, and so is an invisible character.
    #[test]
    fn a_dependency_name_escapes_a_line_break() {
        let names: Vec<String> = Environment::from(&environment(&["a\nb", "z\u{200B}w"]))
            .dependencies
            .iter()
            .map(ToString::to_string)
            .collect();

        assert_eq!(names, [r"a\nb", r"z\u{200b}w"]);
    }
}
