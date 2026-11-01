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

use selfie::config::ConfigLoadError;
use selfie::dotfile_service::{
    DeployStateFailure, DeployStateWarning, Recorded, StateLoadFailure, StateSaveError,
};
use selfie::fs::target::StatePathError;
use selfie::fs::{AbsentReason, FileSystemError};
use selfie::package::event::{
    AuditResult, AuditResultData, CheckResult, CheckResultData, DependencyStatus,
    EnvironmentStatus, EnvironmentStatusData, LinkAtTarget, OperationInfo, PackageInfoData,
    RefusedSpec, SkipReason, SpecListItem, Uncreatable, ValidationIssueData, ValidationResultData,
};
use selfie::package::event::{
    OperationFailure, OperationSuccess, RefusalKind, UncheckedSource, UnknownStatus,
};
use selfie::package::port::{PackageError, PackageParseError, PackageParseKind, PackageRepoError};
use selfie::sync_service::port::PackageValidationIssue;
use selfie::validation::{DeployStateNotice, ValidationIssue, ValidationLevel};
use selfie::yaml::SourceLocation;

use crate::display_manager::Suggestion;
use crate::sentence::composed;
use crate::terminal_text::{Command, DisplayPath, Name, Output, Prose, Shown, lit, shown};

pub(crate) use crate::sentence::{message, sentence};

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

// Shows a command a spec runs as its author wrote it.
fn command(text: Option<&str>) -> Option<Command> {
    text.map(Command::written)
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
    pub(crate) command: Option<Command>,
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
    pub(crate) command: Option<Command>,
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
            // `message` is also a parameter here, so the function is named
            // by its path.
            message: crate::sentence::message(message).into(),
            suggestion: suggestion.map(|suggestion| crate::sentence::message(suggestion).into()),
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
        let mut issue = Self::of(
            data.level(),
            data.category(),
            data.field(),
            data.message(),
            data.suggestion().map(String::as_str),
            data.location(),
        );
        // The deploy state's issue names its paths as paths, as a run does.
        if let Some(notice) = data.deploy_state() {
            issue.message = match notice {
                DeployStateNotice::Failure(failure) => state_load_failure(failure),
                DeployStateNotice::Warning(warning) => deploy_state_warning(warning),
            };
        }
        issue
    }
}

// This is the rule a spec's name follows, worded as the remedy a refusal ends
// with.
fn spec_name_rule() -> Prose {
    Prose::new(selfie::package::SPEC_NAME_RULE)
}

// Why a spec file did not load, without its path. A file whose name breaks the
// rule names it as a name, as the listing beside it does.
fn parse_kind(kind: &PackageParseKind) -> Shown {
    match kind {
        PackageParseKind::InvalidName { name } => shown!(
            "its name '{}' is not a valid spec name. Rename the file: {}.",
            Name::new(name),
            spec_name_rule()
        ),
        other => sentence(other).into(),
    }
}

/// Why a spec file did not load, without where in it.
pub(crate) fn parse_reason(error: &PackageParseError) -> Shown {
    match error.kind() {
        PackageParseKind::Yaml { .. } => message(error.reason()).into(),
        other => parse_kind(other),
    }
}

// This words "Multiple packages found ..." as the library does, with the name
// a Name, the directory a DisplayPath and each file's name a Name.
fn ambiguous_name(name: &str, directory: &std::path::Path, paths: &[std::path::PathBuf]) -> Shown {
    let files = crate::formatters::comma_joined(paths.iter().map(|path| {
        Name::new(
            &path
                .file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy(),
        )
    }));
    shown!(
        "Multiple packages found with name `{}` in path {}: {}. Names are compared ignoring \
         case, and `.yml` and `.yaml` name the same package, so these files all claim one name. \
         Rename or remove all but one.",
        Name::new(name),
        DisplayPath::of(directory),
        files
    )
}

/// Why a listing could not trust a spec. An ambiguity names the spec and the
/// files that claim it.
pub(crate) fn refusal_reason(row: &RefusedSpec) -> Shown {
    match (row.kind, row.claimants.first()) {
        (RefusalKind::AmbiguousName, Some(first)) => ambiguous_name(
            &row.package_name,
            first.parent().unwrap_or_else(|| std::path::Path::new(".")),
            &row.claimants,
        ),
        _ => message(&row.reason).into(),
    }
}

/// A package error, with the spec it names as a Name and its file as a path.
pub(crate) fn package_error(error: &PackageError) -> Shown {
    match error {
        PackageError::ParseError {
            name,
            failed_file,
            cause,
            ..
        } => shown!(
            "Parse error in package `{}` from {}: {}",
            Name::new(name),
            DisplayPath::of(failed_file),
            parse_error(cause)
        ),
        PackageError::UnreadableFile {
            name,
            failed_file,
            cause,
            ..
        } => shown!(
            "Cannot read package `{}` from {}: {}",
            Name::new(name),
            DisplayPath::of(failed_file),
            parse_error(cause)
        ),
        PackageError::UnusableName {
            name,
            failed_file,
            cause,
            ..
        } => shown!(
            "Cannot use package `{}` from {}: {}",
            Name::new(name),
            DisplayPath::of(failed_file),
            parse_error(cause)
        ),
        PackageError::MultiplePackagesFound {
            name,
            packages_path,
            conflicting_paths,
            ..
        } => ambiguous_name(name, packages_path, conflicting_paths),
        PackageError::PackageNotFound {
            name,
            packages_path,
            declared_by,
            ..
        } => {
            let hint = selfie::package::port::name_field_hint(declared_by);
            shown!(
                "Package `{}` not found in path {}{}",
                Name::new(name),
                DisplayPath::of(packages_path),
                message(&hint)
            )
        }
        other => sentence(other).into(),
    }
}

/// A repository's error, a package error composed as [`package_error`]
/// composes it.
pub(crate) fn repo_error(error: &PackageRepoError) -> Shown {
    match error {
        PackageRepoError::PackageError(error) => package_error(error),
        other => sentence(other).into(),
    }
}

/// Why a dependency's status is unknown, with a lookup's error composed as
/// [`repo_error`] composes it.
pub(crate) fn unknown_status(status: &UnknownStatus) -> Shown {
    match status {
        UnknownStatus::LookupFailed(failure) => repo_error(&failure.clone().into_error()),
        UnknownStatus::Error(error) => repo_error(error),
        other => sentence(other).into(),
    }
}

/// An operation's failure, as the error line and the error summary both show
/// it.
pub(crate) fn failure(failure: &OperationFailure) -> Shown {
    match failure {
        OperationFailure::Uncreatable {
            package_name,
            reason,
        } => uncreatable(package_name, reason),
        OperationFailure::Package(error) => package_error(error),
        OperationFailure::DeployState(failure) => deploy_state_failure(failure),
        OperationFailure::UnreadableSpec {
            package_name,
            reason,
        } => shown!(
            "Cannot use package `{}`: {}",
            Name::new(package_name),
            message(reason)
        ),
        other => sentence(other).into(),
    }
}

/// An operation's success, as its summary line shows it, with each name and
/// path in it escaped as the header and the table escape them.
pub(crate) fn success(success: &OperationSuccess) -> Shown {
    composed(success)
}

/// The warning for a run that could not check its targets against `sources`,
/// with each spec file shown as a path.
pub(crate) fn collisions_unchecked(sources: &[UncheckedSource]) -> Shown {
    let named = crate::formatters::comma_joined(sources.iter().map(|source| -> Shown {
        match source {
            UncheckedSource::SpecFile { .. } => DisplayPath::new(&source.to_string()).into(),
            UncheckedSource::DotfilesDirectory => sentence(source).into(),
        }
    }));
    shown!(
        "Collisions with these were not checked, since selfie could not use them: {}",
        named
    )
}

// This is the warning for a spec file a run leaves out, for `reason`.
fn skipping(path: &std::path::Path, reason: impl Into<Shown>) -> Shown {
    shown!(
        "Skipping package file {}: {}",
        DisplayPath::of(path),
        reason.into()
    )
}

/// The warning for a spec file a run skips because it does not parse.
pub(crate) fn skipped_spec(error: &PackageParseError) -> Shown {
    skipping(error.package_path(), parse_error(error))
}

/// The warning for a spec file a listing leaves out because several files
/// claim its name.
pub(crate) fn ambiguous_spec(row: &RefusedSpec) -> Shown {
    skipping(&row.path, refusal_reason(row))
}

/// Why `spec create` will not create `package_name`.
pub(crate) fn uncreatable(package_name: &str, reason: &Uncreatable) -> Shown {
    let name = Name::new(package_name);
    match reason {
        Uncreatable::InvalidName => shown!(
            "Refusing to create '{}': it is not a valid spec name. Rename it: {}.",
            name,
            spec_name_rule()
        ),
        Uncreatable::OutsidePackageDirectory { path, directory } => shown!(
            "Refusing to create '{}': {} is not directly in the package directory {}, where \
             every spec has to be.",
            name,
            DisplayPath::of(path),
            DisplayPath::of(directory)
        ),
        Uncreatable::UnloadableFileName { path } => shown!(
            "Refusing to create '{}': selfie would not load {} as a spec. Its name before the \
             .yml or .yaml extension has to follow the rule for a package name.",
            name,
            DisplayPath::of(path)
        ),
    }
}

/// The refusal for `name` when it breaks the spec-name rule, or `None` when it
/// follows it.
pub(crate) fn invalid_name_refusal(name: &str) -> Option<Shown> {
    selfie::package::spec_name_refusal(name).map(|reason| uncreatable(name, &reason))
}

/// `name` as a prompt for a spec name suggests it: unkeepable, or nothing at
/// all when it shows as written and the spec-name rule refuses it.
// Unkeepable: the rule refuses every character a name's escape changes, so a
// name the prompt can only show escaped is never a valid answer. A plain name
// the rule refuses is not suggested: pre-filled, Enter would only send it back
// to be refused.
pub(crate) fn spec_name_suggestion(name: &str) -> Suggestion {
    if Name::is_plain(name) && selfie::package::spec_name_refusal(name).is_some() {
        Suggestion::Nothing
    } else {
        Suggestion::Unkeepable(name.to_string())
    }
}

/// `error`'s sentence, each path in it a [`DisplayPath`]. `None` for a
/// file-system error, which carries causes of its own for the caller to show.
pub(crate) fn config_load_error(error: &ConfigLoadError) -> Option<Shown> {
    Some(match error {
        ConfigLoadError::Parse { path, failure } => shown!(
            "Cannot parse the configuration file {}: {}",
            DisplayPath::of(path),
            sentence(failure)
        ),
        ConfigLoadError::IrregularFile { path, kind } => shown!(
            "{}: the configuration file is a {}, not a regular file. Replace it with a regular \
             file or remove it.",
            DisplayPath::of(path),
            Prose::new(kind)
        ),
        ConfigLoadError::UnresolvableFile { path } => shown!(
            "{}: the configuration file is a symbolic link that does not resolve. Restore or \
             check out its target, or remove the link.",
            DisplayPath::of(path)
        ),
        ConfigLoadError::NotFound { searched } => shown!(
            "No configuration file found in locations: {}",
            DisplayPath::of(searched)
        ),
        ConfigLoadError::MultipleFound(paths) => shown!(
            "Multiple configuration files found: {}",
            Shown::join(paths.iter().map(|path| DisplayPath::new(path)), lit!(", "))
        ),
        ConfigLoadError::FileSystemError(_) => return None,
    })
}

/// What is at a path instead of a directory, as a clause after the path, with
/// any path it names a [`DisplayPath`].
fn absent_clause(reason: &AbsentReason) -> Shown {
    match reason {
        AbsentReason::DanglingSymlink {
            points_to: Some(destination),
        } => shown!(
            "is a symlink to nothing: it points at {}",
            DisplayPath::of(destination)
        ),
        AbsentReason::ParentNotADirectory { parent } => shown!(
            "is below {}, which is not a directory",
            DisplayPath::of(parent)
        ),
        // Named rather than caught, so a variant the library adds must choose
        // here how a path in it is shown.
        AbsentReason::Empty
        | AbsentReason::Occupied { .. }
        | AbsentReason::DanglingSymlink { points_to: None } => message(reason.clause()).into(),
    }
}

/// `error`'s sentence, with the path it names a [`DisplayPath`].
// A cause from tempfile can still name its temporary file as text
// (selfie-c2lb).
fn file_system_error(error: &FileSystemError) -> Shown {
    match error {
        FileSystemError::IrregularTarget { path, kind } => shown!(
            "{}: target resolves to a {} and selfie will not write to it",
            DisplayPath::of(path),
            Prose::new(kind)
        ),
        FileSystemError::SymlinkedTarget { path, points_to } => {
            let to = points_to
                .as_deref()
                .map_or_else(Shown::default, |destination| {
                    shown!(" to '{}'", DisplayPath::of(destination))
                });
            shown!(
                "{}: target is a symlink{} and selfie will not write through it",
                DisplayPath::of(path),
                to
            )
        }
        FileSystemError::CannotCreateIn { path, cause } => {
            shown!("{}: {}", DisplayPath::of(path), sentence(&**cause))
        }
        // Named rather than caught, so a variant the library adds must choose
        // here how a path in it is shown.
        FileSystemError::IoError(failure) => io_error(failure.path.as_deref(), &failure.error),
        FileSystemError::BelowNonDirectory { path, cause, .. } => {
            io_error(Some(FileSystemError::uncreatable_directory(path)), cause)
        }
        FileSystemError::DirectoryTarget { path, cause } => io_error(Some(path), cause),
        FileSystemError::HomeDirNotFound => sentence(error).into(),
    }
}

/// An IO error's sentence, with the path it names a [`DisplayPath`].
fn io_error(path: Option<&std::path::Path>, cause: &std::io::Error) -> Shown {
    match path {
        Some(path) => shown!("IO error: {}: {}", DisplayPath::of(path), sentence(cause)),
        None => shown!("IO error: {}", sentence(cause)),
    }
}

fn state_path_error(error: &StatePathError) -> Shown {
    match error {
        StatePathError::NotAbsolute(path) => shown!(
            "state_directory '{}' is not an absolute path. Use an absolute path; a leading `~` or \
             `~/` is expanded, but only when the home directory can be determined, and `~user` is \
             not",
            DisplayPath::of(path)
        ),
        StatePathError::NoHome => sentence(error).into(),
    }
}

/// Why the deploy state could not be used, worded as the library words it,
/// with each path in it a [`DisplayPath`].
pub(crate) fn state_load_failure(failure: &StateLoadFailure) -> Shown {
    match failure {
        StateLoadFailure::Locate(error) => shown!(
            "Cannot locate the deploy state file: {}",
            state_path_error(error)
        ),
        StateLoadFailure::StateDirectoryOccupied { path, reason } => shown!(
            "Cannot use the deploy state: '{}' {}. Remove it, or point state_directory elsewhere",
            DisplayPath::of(path),
            absent_clause(reason)
        ),
        StateLoadFailure::StateDirectoryUnreadable { path, why } => shown!(
            "Cannot use the deploy state: '{}' {}. Point state_directory at a directory selfie \
             can read",
            DisplayPath::of(path),
            Prose::new(why)
        ),
        StateLoadFailure::StateDirectoryNotWritable {
            directory,
            blocked,
            cause,
            configured,
        } => {
            // `blocked` names the directory, so a refusal naming it gives only its cause.
            let why = match &**cause {
                FileSystemError::CannotCreateIn { cause, .. } => sentence(&**cause).into(),
                other => file_system_error(other),
            };
            if directory == blocked {
                return shown!(
                    "The deploy state cannot be written: the state directory '{}' does not accept \
                     a new file ({}). Make it writable, or point state_directory elsewhere",
                    DisplayPath::of(directory),
                    why
                );
            }
            let typo = if *configured {
                lit!("Correct state_directory if that path is a typo, or make")
            } else {
                lit!("Make")
            };
            shown!(
                "The deploy state cannot be written: the state directory '{}' is not there, and \
                 selfie cannot create it under '{}' ({}). {} '{}' writable",
                DisplayPath::of(directory),
                DisplayPath::of(blocked),
                why,
                typo,
                DisplayPath::of(blocked)
            )
        }
        StateLoadFailure::StateNotWritable(error) => shown!(
            "The deploy state cannot be written: {}. Free the space or lift the lock that stops \
             it, or point state_directory elsewhere",
            state_save_error(error)
        ),
        StateLoadFailure::Irregular { path, kind } => shown!(
            "Cannot read the deploy state: '{}' is a {}. Remove it, or point state_directory \
             elsewhere",
            DisplayPath::of(path),
            Prose::new(kind)
        ),
        StateLoadFailure::Read { path, cause } => shown!(
            "Cannot read deploy state '{}': {}. Fix the file's permissions, or move it aside",
            DisplayPath::of(path),
            file_system_error(cause)
        ),
        StateLoadFailure::Empty { path } => shown!(
            "Deploy state '{}' is empty. Delete it to start over, or restore it from a backup",
            DisplayPath::of(path)
        ),
        StateLoadFailure::Parse { path, cause } => shown!(
            "Cannot parse deploy state '{}': {}. Repair the file, or move it aside to start over",
            DisplayPath::of(path),
            sentence(cause)
        ),
    }
}

/// Why the deploy state could not be written, with its path a [`DisplayPath`].
fn state_save_error(error: &StateSaveError) -> Shown {
    match error {
        StateSaveError::Serialize(failure) => {
            shown!("Cannot serialize the deploy state: {}", sentence(failure))
        }
        StateSaveError::Write { path, cause } => shown!(
            "Cannot write deploy state '{}': {}",
            DisplayPath::of(path),
            file_system_error(cause)
        ),
    }
}

/// A warning about the deploy state, worded as the library words it, with
/// each path in it a [`DisplayPath`].
pub(crate) fn deploy_state_warning(warning: &DeployStateWarning) -> Shown {
    match warning {
        DeployStateWarning::NotThereYet {
            directory,
            cannot_create: None,
        } => shown!(
            "state_directory '{}' is not there yet, so nothing shows as deployed; selfie creates \
             it on the first write. Correct the setting if that path is a typo",
            DisplayPath::of(directory)
        ),
        DeployStateWarning::NotThereYet {
            directory,
            cannot_create: Some(failure),
        } => shown!(
            "state_directory '{}' is not there yet, so nothing shows as deployed. {}",
            DisplayPath::of(directory),
            state_load_failure(failure)
        ),
        DeployStateWarning::ReadingPast(failure) => shown!(
            "{}; continuing as though nothing had been deployed",
            state_load_failure(failure)
        ),
        DeployStateWarning::DryRunWouldStop(failure) => shown!(
            "{}. A real run stops before its first write, including recording a file already in \
             place",
            state_load_failure(failure)
        ),
        DeployStateWarning::NotRecorded {
            recorded,
            target,
            error,
        } => {
            let recorded = match recorded {
                Recorded::Deployed => lit!("Deployed"),
                Recorded::InSync => lit!("Found in sync"),
            };
            shown!(
                "{} '{}' but cannot record it: {}",
                recorded,
                DisplayPath::of(target),
                state_save_error(error)
            )
        }
        DeployStateWarning::Untidied(error) => shown!(
            "Could not tidy the deploy state after the run, so the next apply tries again: {}",
            state_save_error(error)
        ),
    }
}

/// Why a recording command stopped over the deploy state, worded as the
/// library words it, with each path in it a [`DisplayPath`] and the command it
/// names quoted as a [`Command`].
pub(crate) fn deploy_state_failure(failure: &DeployStateFailure) -> Shown {
    match failure {
        DeployStateFailure::Unusable(failure) => state_load_failure(failure),
        DeployStateFailure::StoppedBeforeWriting(failure) => shown!(
            "Stopped with nothing written. {}",
            state_load_failure(failure)
        ),
        DeployStateFailure::StoppedAfterRecording { target } => shown!(
            "Stopped after failing to record '{}' in the deploy state; the next run re-evaluates \
             it once the state can be written",
            DisplayPath::of(target)
        ),
        DeployStateFailure::NothingTracked(failure) => {
            shown!("Nothing was tracked. {}", state_load_failure(failure))
        }
        DeployStateFailure::TrackUnrecorded {
            error,
            name,
            target,
            spec_path,
            copy,
        } => {
            // A name holding NUL has no shell word, so no command is offered.
            let remedy = crate::formatters::selfie_command(&[lit!("apply")], name, &[])
                .map_or_else(Shown::default, |command| {
                    shown!(
                        " Run `{}` once the state file can be written, and it records the \
                         deployment.",
                        command
                    )
                });
            shown!(
                "Tracked '{}': the copy at '{}' and the entry in '{}' are written. The deployment \
                 was not recorded: {}.{}",
                DisplayPath::new(target),
                DisplayPath::of(copy),
                DisplayPath::of(spec_path),
                state_save_error(error),
                remedy
            )
        }
    }
}

/// What a step of a run is doing, on one line, as a spinner, a progress line
/// or a status line shows it.
// The library sends the step's sentence whole, with any name or path already
// inside, so the sentence is escaped as a name. Its sources are selfie's ASCII
// words around names, paths, counts and a remote's name, and one around a
// commit message the user wrote, which `sync push` previews as a name too. None
// holds a command, whose blanks a name's escape would not keep. A step that
// quotes a command needs the subject sent as its parts first (selfie-hkjy).
pub(crate) fn step(message: &str) -> Name {
    Name::new(message.trim_end_matches(['\r', '\n']))
}

/// Why an entry was skipped, as an entry row's detail shows it. A secret dry
/// run names the link it would replace as a path.
pub(crate) fn skip_reason(reason: &SkipReason) -> Shown {
    // No wildcard, so a reason or a link that carries a path chooses here how
    // it is shown.
    match reason {
        SkipReason::SecretDryRun { commands, link } => {
            let destination = match link {
                LinkAtTarget::NoLink => return sentence(reason).into(),
                LinkAtTarget::To(path) => shown!(" to '{}'", DisplayPath::of(path)),
                LinkAtTarget::DestinationUnknown => Shown::default(),
            };
            shown!(
                "dry run: would run {} command(s), then replace the symlink{} with a regular \
                 file readable only by you",
                *commands,
                destination
            )
        }
        SkipReason::UpToDate
        | SkipReason::InSync
        | SkipReason::PermissionsTightened
        | SkipReason::DryRun
        | SkipReason::Unverifiable => sentence(reason).into(),
    }
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
            refusal: data.refusal.as_deref().map(message),
            refusal_elsewhere: data.refusal_elsewhere.as_deref().map(message),
            current_environment: Name::new(&data.current_environment),
        }
    }
}

/// One environment of a spec, as `spec info` and `package status` show it.
pub(crate) struct Environment {
    pub(crate) name: Name,
    /// The install and check commands, each keeping a block-scalar
    /// command's line breaks.
    pub(crate) install_command: Option<Command>,
    pub(crate) check_command: Option<Command>,
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
            install_command: command(data.install_command.as_deref()),
            check_command: command(data.check_command.as_deref()),
            dependencies: names(&data.dependencies),
            recommends: names(&data.recommends),
            dependency_statuses: statuses(&data.dependency_statuses),
            recommend_statuses: statuses(&data.recommend_statuses),
        }
    }
}

/// A spec file that did not load, as the library words its failure.
pub(crate) fn parse_error(error: &PackageParseError) -> Shown {
    parse_kind(error.kind())
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

    // A name that shows as written is pre-filled; one the prompt can only show
    // escaped is unkeepable, so it is noted but never returned; a plain name
    // the spec-name rule refuses is not suggested.
    #[test]
    fn a_spec_name_is_suggested_as_the_rule_and_the_escape_allow() {
        let unkeepable = |name: &str| Suggestion::Unkeepable(name.to_string());

        for (name, suggested) in [
            ("batrc", unkeepable("batrc")),
            ("bat\u{1b}[2Jrc", unkeepable("bat\u{1b}[2Jrc")),
            ("bat\u{202E}rc", unkeepable("bat\u{202E}rc")),
            ("bat\u{A0}rc", unkeepable("bat\u{A0}rc")),
            ("bat\u{2800}rc", unkeepable("bat\u{2800}rc")),
            ("bat rc", Suggestion::Nothing),
            ("bat#rc", Suggestion::Nothing),
            ("", Suggestion::Nothing),
        ] {
            assert_eq!(spec_name_suggestion(name), suggested, "{name:?}");
        }
    }

    // Every name the spec-name rule accepts shows as it is written, so a prompt
    // can pre-fill any valid name, and a suggestion that would show escaped can
    // never be a valid one. Every character is tried, not a sample, at the
    // start, in the middle and at the end of a name. One buffer is reused, and
    // the escape check, which allocates nothing for a plain name, runs first.
    #[test]
    fn every_name_the_rule_accepts_shows_as_written() {
        let mut name = String::new();
        let mut shown_escaped = Vec::new();
        for c in '\0'..=char::MAX {
            for (before, after) in [("", "ab"), ("a", "b"), ("ab", "")] {
                name.clear();
                name.push_str(before);
                name.push(c);
                name.push_str(after);
                if !crate::terminal_text::Name::is_plain(&name)
                    && selfie::package::spec_name_refusal(&name).is_none()
                {
                    shown_escaped.push(name.clone());
                }
            }
        }

        assert_eq!(shown_escaped, Vec::<String>::new());
    }

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

    // A secret dry run names the link it would replace as a path, so an
    // invisible character or a line break in the destination shows as an
    // escape and the row stays one line.
    #[test]
    fn a_secret_dry_run_names_the_link_destination_as_a_path() {
        let reason = SkipReason::SecretDryRun {
            commands: 1,
            link: LinkAtTarget::To("/x/de\u{200B}st\nz".into()),
        };

        assert_eq!(
            skip_reason(&reason).as_str(),
            r"dry run: would run 1 command(s), then replace the symlink to '/x/de\u{200b}st\nz' with a regular file readable only by you"
        );
    }

    // Each configuration load error reads as the library words it for a plain
    // path, so composing it changes only how the path is escaped.
    #[test]
    fn a_config_load_error_matches_the_library_wording_for_plain_paths() {
        let failure = selfie::yaml::parse::<selfie::package::Package>("name: [\n")
            .expect_err("the fixture must not parse");
        for error in [
            ConfigLoadError::Parse {
                path: "/c/config.yaml".into(),
                failure,
            },
            ConfigLoadError::IrregularFile {
                path: "/c/config.yaml".into(),
                kind: "fifo",
            },
            ConfigLoadError::UnresolvableFile {
                path: "/c/config.yaml".into(),
            },
            ConfigLoadError::NotFound {
                searched: "/c".into(),
            },
            ConfigLoadError::MultipleFound(vec!["/c/a.yaml".to_string(), "/c/b.yaml".to_string()]),
        ] {
            let composed = config_load_error(&error).expect("a configuration file's error");
            assert_eq!(composed.as_str(), error.to_string());
        }
    }

    // Each package error and refusal the CLI composes reads as
    // the library words it for plain values, so composing it changes only how
    // a name or a path in it is escaped.
    #[test]
    fn a_composed_package_sentence_matches_the_library_wording_for_plain_values() {
        let invalid = || {
            PackageParseError::new(
                "/p/a b.yml",
                PackageParseKind::InvalidName {
                    name: "a b".to_string(),
                },
            )
        };
        let source = selfie::yaml::parse::<selfie::package::Package>("name: [\n")
            .expect_err("the fixture must not parse");
        let errors = [
            PackageError::UnusableName {
                name: "a b".to_string(),
                packages_path: "/p".into(),
                failed_file: "/p/a b.yml".into(),
                cause: invalid(),
            },
            PackageError::UnreadableFile {
                name: "a b".to_string(),
                packages_path: "/p".into(),
                failed_file: "/p/a b.yml".into(),
                cause: invalid(),
            },
            PackageError::ParseError {
                name: "tool".to_string(),
                packages_path: "/p".into(),
                failed_file: "/p/tool.yml".into(),
                cause: PackageParseError::new(
                    "/p/tool.yml",
                    PackageParseKind::Yaml { cause: source },
                ),
            },
            PackageError::MultiplePackagesFound {
                name: "tool".to_string(),
                packages_path: "/p".into(),
                conflicting_paths: vec!["/p/tool.yml".into(), "/p/Tool.yaml".into()],
                files_examined: 2,
                search_patterns: Vec::new(),
            },
            not_found(Vec::new()),
            not_found(vec![selfie::package::port::NameDeclaration {
                path: "/p/other.yml".into(),
                name: "Tool".to_string(),
            }]),
        ];
        for error in &errors {
            assert_eq!(package_error(error).as_str(), error.to_string());
        }

        let ambiguous = RefusedSpec {
            package_name: "tool".to_string(),
            path: "/p/tool.yml".into(),
            kind: RefusalKind::AmbiguousName,
            reason: errors[3].to_string(),
            claimants: vec!["/p/tool.yml".into(), "/p/Tool.yaml".into()],
        };
        assert_eq!(refusal_reason(&ambiguous).as_str(), ambiguous.reason);

        let sources = [
            UncheckedSource::SpecFile {
                base: selfie::package::event::BaseKind::PackageDirectory,
                file: "a b.yaml".to_string(),
            },
            UncheckedSource::DotfilesDirectory,
        ];
        assert_eq!(
            collisions_unchecked(&sources).as_str(),
            selfie::package::service::collisions_unchecked_warning(&sources)
        );
    }

    // Each of `values`, a library value built from plain parts, renders as
    // its own Display less the line breaks at its end, so the CLI and the MCP
    // server say the same thing. Every Composed type's fixtures go through
    // here.
    fn assert_renders_as_its_display<T: selfie::parts::Composed + std::fmt::Debug>(values: &[T]) {
        for value in values {
            assert_eq!(
                composed(value).as_str(),
                value.to_string().trim_end_matches(['\r', '\n']),
                "{value:?}"
            );
        }
    }

    // A success summary of plain values reads as the library words it. The
    // Generic lines hold a line break inside, which the CLI keeps, and an LF
    // and a CRLF at the end, which it drops.
    #[test]
    fn a_success_summary_of_plain_values_reads_as_the_library_words_it() {
        let mut successes = test_common::every_success("env", "tool", "/p/tool.yml");
        successes.push(OperationSuccess::Generic("two\nlines".to_string()));
        successes.push(OperationSuccess::Generic("ends\n".to_string()));
        successes.push(OperationSuccess::Generic("ends\r\n".to_string()));

        assert_renders_as_its_display(&successes);
    }

    // Every name and path in a success summary shows escaped, as the header
    // and the table show it: no U+00A0, U+200B or line break reaches the
    // line raw, and each value shows escaped once for each time the library
    // names it.
    #[test]
    fn a_success_summary_names_each_value_escaped() {
        let (environment, package, path) = ("a\u{A0}b", "p\u{200B}q", "/d/x\ny");
        let escaped = [
            (environment, r"a\u{a0}b"),
            (package, r"p\u{200b}q"),
            (path, r"/d/x\ny"),
        ];
        let mut named = [0; 3];
        for line in test_common::every_success(environment, package, path) {
            let shown = success(&line).into_string();
            let library = line.to_string();

            assert!(!shown.contains(['\u{A0}', '\u{200B}', '\n']), "{shown:?}");
            for ((raw, spelled), count) in escaped.into_iter().zip(&mut named) {
                assert_eq!(
                    shown.matches(spelled).count(),
                    library.matches(raw).count(),
                    "{spelled} in {shown:?} from {library:?}"
                );
                *count += library.matches(raw).count();
            }
        }
        // Each value is named somewhere, so the counts above compare
        // something.
        assert!(named.iter().all(|count| *count > 0), "{named:?}");
    }

    fn not_found(declared_by: Vec<selfie::package::port::NameDeclaration>) -> PackageError {
        PackageError::PackageNotFound {
            name: "tool".to_string(),
            packages_path: "/p".into(),
            files_examined: 0,
            search_patterns: Vec::new(),
            declared_by,
        }
    }

    // A spec file a run could not check for collisions is named as a path, so
    // a line break in its name does not split the warning.
    #[test]
    fn an_unchecked_spec_file_is_named_escaped() {
        let sources = [UncheckedSource::SpecFile {
            base: selfie::package::event::BaseKind::PackageDirectory,
            file: "l\nf\u{200B}.yaml".to_string(),
        }];

        assert_eq!(
            collisions_unchecked(&sources).as_str(),
            r"Collisions with these were not checked, since selfie could not use them: packages/l\nf\u{200b}.yaml"
        );
    }

    // A dependency no spec answers to is named as a name in its status.
    #[test]
    fn a_missing_dependency_is_named_escaped() {
        let status = UnknownStatus::LookupFailed(
            PackageRepoError::from(PackageError::PackageNotFound {
                name: "dep\u{200B}x".to_string(),
                packages_path: "/p".into(),
                files_examined: 0,
                search_patterns: Vec::new(),
                declared_by: Vec::new(),
            })
            .into_lookup_failure()
            .expect("a lookup returns this error"),
        );

        assert_eq!(
            unknown_status(&status).as_str(),
            r"Package `dep\u{200b}x` not found in path /p"
        );
    }

    // An ambiguity names the spec and each file that claims it as names, so an
    // invisible character in either shows as an escape.
    #[test]
    fn an_ambiguity_names_the_spec_and_its_files_escaped() {
        let row = RefusedSpec {
            package_name: "t\u{200B}ool".to_string(),
            path: "/p/t\u{200B}ool.yml".into(),
            kind: RefusalKind::AmbiguousName,
            reason: String::new(),
            claimants: vec!["/p/t\u{200B}ool.yml".into(), "/p/T\u{200B}ool.yaml".into()],
        };

        let shown = refusal_reason(&row);

        assert!(
            shown.as_str().starts_with(r"Multiple packages found with name `t\u{200b}ool` in path /p: t\u{200b}ool.yml, T\u{200b}ool.yaml."),
            "{shown}"
        );
    }

    // A dry run with no link, or one whose destination is unknown, reads as
    // the library words it.
    #[test]
    fn a_secret_dry_run_without_a_destination_reads_as_the_library_words_it() {
        for link in [LinkAtTarget::NoLink, LinkAtTarget::DestinationUnknown] {
            let reason = SkipReason::SecretDryRun { commands: 2, link };
            assert_eq!(skip_reason(&reason).as_str(), reason.to_string());
        }
    }

    // A refusal to create a spec names the spec as a name and its paths as
    // paths.
    #[test]
    fn an_uncreatable_path_names_it_as_a_path() {
        let shown = uncreatable(
            "a\u{200B}b",
            &Uncreatable::OutsidePackageDirectory {
                path: "/p/x\u{200B}y.yml".into(),
                directory: "/p\nq".into(),
            },
        );

        assert_eq!(
            shown.as_str(),
            r"Refusing to create 'a\u{200b}b': /p/x\u{200b}y.yml is not directly in the package directory /p\nq, where every spec has to be."
        );
    }

    // Each composed sentence reads as the library words it for a plain value,
    // so composing it in the CLI changes only how a value in it is escaped.
    #[test]
    fn a_composed_sentence_matches_the_library_wording_for_plain_values() {
        for reason in [
            Uncreatable::InvalidName,
            Uncreatable::OutsidePackageDirectory {
                path: "/p/x.yml".into(),
                directory: "/p".into(),
            },
            Uncreatable::UnloadableFileName {
                path: "/p/x.yml".into(),
            },
        ] {
            assert_eq!(
                uncreatable("tool", &reason).as_str(),
                reason.refusal("tool")
            );
        }
        let invalid = PackageParseError::new(
            "/p/a b.yml",
            PackageParseKind::InvalidName {
                name: "a b".to_string(),
            },
        );
        assert_eq!(parse_error(&invalid).as_str(), invalid.to_string());
        assert_eq!(
            skipped_spec(&invalid).as_str(),
            selfie::package::service::skipped_spec_warning(&invalid)
        );
        let ambiguous = RefusedSpec {
            package_name: "tool".to_string(),
            path: "/p/tool.yml".into(),
            kind: selfie::package::event::RefusalKind::UnknownTopLevelKeys,
            reason: "several files claim it".to_string(),
            claimants: Vec::new(),
        };
        assert_eq!(
            ambiguous_spec(&ambiguous).as_str(),
            selfie::package::service::ambiguous_spec_warning(&ambiguous)
        );
        let link = SkipReason::SecretDryRun {
            commands: 3,
            link: LinkAtTarget::To("/etc/real".into()),
        };
        assert_eq!(skip_reason(&link).as_str(), link.to_string());
    }

    use test_common::{StateValue, every_state_value};

    // A deploy-state value, as the CLI composes it.
    fn composed_state(value: &StateValue) -> Shown {
        match value {
            StateValue::Load(failure) => state_load_failure(failure),
            StateValue::Warning(warning) => deploy_state_warning(warning),
            StateValue::Failure(failure) => deploy_state_failure(failure),
        }
    }

    // Each deploy-state sentence reads as the library words it for a plain
    // path, over a value of every shape.
    #[test]
    fn a_deploy_state_sentence_matches_the_library_wording_for_plain_paths() {
        for value in &every_state_value("/s") {
            assert_eq!(composed_state(value).as_str(), value.library());
        }
    }

    // Every path in a deploy-state sentence is a path: a line break and U+200B
    // in it show as escapes, each path the library names is named once escaped,
    // and the sentence stays one line.
    #[test]
    fn a_deploy_state_sentence_names_each_path_escaped() {
        let root = "/s/st\u{200B}a\nte";
        for value in every_state_value(root) {
            let composed = composed_state(&value).into_string();
            let library = value.library();

            assert!(
                !composed.contains(['\u{200B}', '\n']),
                "{composed:?} from {library:?}"
            );
            assert_eq!(
                composed.matches(r"/s/st\u{200b}a\nte").count(),
                library.matches(root).count(),
                "{composed:?} from {library:?}"
            );
        }
    }

    // A filesystem error a real write returns names the path it failed on as a
    // path, right after "IO error: ", on every route that reports a failed
    // state write. The write into an unwritable directory also carries
    // tempfile's own copy of its temporary path, which this does not count.
    #[test]
    fn a_real_write_failure_names_its_path_escaped() {
        use selfie::fs::FileSystem as _;
        use selfie::fs::{IfPresent, RealFileSystem, expand_target_path};

        let temp = tempfile::TempDir::new().unwrap();
        let dir = temp.path().join("st\u{200B}a\nte");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("file"), "x").unwrap();
        std::fs::create_dir(dir.join("occupied")).unwrap();
        let write = |path: &std::path::Path| {
            RealFileSystem
                .write_file_private(
                    &expand_target_path(&RealFileSystem, path.to_str().unwrap()),
                    b"x",
                    IfPresent::Replace,
                )
                .unwrap_err()
        };
        // Each entry holds a failed write and the path its error names.
        let mut failures = vec![
            (write(&dir.join("file/sub/state.yml")), dir.join("file/sub")),
            (write(&dir.join("occupied")), dir.join("occupied")),
        ];
        let locked = test_common::LockedDir::create(&dir.join("locked"), 0o500);
        if locked.refuses_new_files() {
            failures.push((
                write(&dir.join("locked/state.yml")),
                dir.join("locked/state.yml"),
            ));
            failures.push((
                write(&dir.join("locked/sub/state.yml")),
                dir.join("locked/sub"),
            ));
        } else {
            eprintln!("SKIP the 0o500 cases: this user can write into a 0o500 directory");
        }
        drop(locked);

        for (cause, named) in failures {
            let write = || StateSaveError::Write {
                path: dir.join("state.yml"),
                cause: cause.clone(),
            };
            let routes = [
                state_load_failure(&StateLoadFailure::StateNotWritable(write())),
                deploy_state_warning(&DeployStateWarning::NotRecorded {
                    recorded: Recorded::Deployed,
                    target: dir.join("target"),
                    error: write(),
                }),
                deploy_state_warning(&DeployStateWarning::Untidied(write())),
                deploy_state_failure(&DeployStateFailure::TrackUnrecorded {
                    error: write(),
                    name: "tool".to_string(),
                    target: "~/.toolrc".to_string(),
                    spec_path: dir.join("tool.yml"),
                    copy: dir.join("copy"),
                }),
            ];
            let at = format!("IO error: {}: ", DisplayPath::of(&named));
            for composed in routes {
                let composed = composed.into_string();
                assert!(!composed.contains(['\u{200B}', '\n']), "{composed:?}");
                assert_eq!(composed.matches(&at).count(), 1, "{at:?} in {composed:?}");
            }
        }
    }

    // Track's unrecorded failure names `selfie apply` as a command, its name
    // quoted as one shell word, and reads as the library words it whatever the
    // name, so the CLI and MCP offer the same command or none.
    #[test]
    fn a_track_that_could_not_record_quotes_the_name_it_names() {
        let failure = |name: &str| DeployStateFailure::TrackUnrecorded {
            error: StateSaveError::Serialize(selfie::yaml::SerializeFailure::Value),
            name: name.to_string(),
            target: "~/.apprc".to_string(),
            spec_path: "/p/my app.yml".into(),
            copy: "/d/apprc".into(),
        };

        assert!(
            deploy_state_failure(&failure("my app"))
                .as_str()
                .contains("Run `selfie apply 'my app'` once the state file can be written"),
            "{}",
            deploy_state_failure(&failure("my app")).as_str()
        );
        for name in ["my app", "-x", "tool", "a\0b"] {
            assert_eq!(
                deploy_state_failure(&failure(name)).as_str(),
                failure(name).to_string(),
                "{name:?}"
            );
        }
    }

    // A name in a composed sentence escapes a blank other than a space, as it
    // does in a listing.
    #[test]
    fn a_composed_sentence_escapes_a_visible_blank() {
        assert_eq!(
            uncreatable("a\u{A0}b", &Uncreatable::InvalidName)
                .as_str()
                .split(':')
                .next(),
            Some(r"Refusing to create 'a\u{a0}b'")
        );
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
