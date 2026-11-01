/// Collection of validation issues (errors and warnings)
///
/// Provides methods to query and filter validation results, allowing
/// callers to handle errors and warnings differently based on their needs.
#[derive(Debug, Clone, Default)]
pub struct ValidationIssues(Vec<ValidationIssue>);

impl Extend<ValidationIssue> for ValidationIssues {
    fn extend<I: IntoIterator<Item = ValidationIssue>>(&mut self, issues: I) {
        self.0.extend(issues);
    }
}

impl ValidationIssues {
    /// Get all validation issues regardless of level
    #[must_use]
    pub fn all_issues(&self) -> &[ValidationIssue] {
        &self.0
    }

    /// Returns true if the validation passed (no errors)
    ///
    /// Note: This returns `true` even if there are warnings, as warnings
    /// do not prevent successful validation.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        !self.has_errors()
    }

    /// Get all informational notices (neither errors nor warnings)
    #[must_use]
    pub fn infos(&self) -> Vec<&ValidationIssue> {
        self.0
            .iter()
            .filter(|issue| issue.level == ValidationLevel::Info)
            .collect()
    }

    /// Returns true if there is anything wrong: an error or a warning
    ///
    /// Informational notices are excluded. The name reads as "is anything
    /// wrong", and a package carrying only a notice — which every package using
    /// a provider-sourced dotfile does — is not. Counting those here would make
    /// the first caller to trust the name treat correct packages as defective.
    /// Use [`all_issues`](Self::all_issues) to ask whether there is anything at
    /// all to display, and [`infos`](Self::infos) for the notices alone.
    #[must_use]
    pub fn has_issues(&self) -> bool {
        self.has_errors() || self.has_warnings()
    }

    /// How a validation scores: [`Failed`](crate::package::event::Outcome::Failed)
    /// on an error, [`Found`](crate::package::event::Outcome::Found) on a
    /// warning, and clean otherwise. Informational notices do not count.
    #[must_use]
    pub fn outcome(&self) -> crate::package::event::Outcome {
        use crate::package::event::Outcome;

        if self.has_errors() {
            Outcome::Failed
        } else if self.has_warnings() {
            Outcome::Found
        } else {
            Outcome::Clean
        }
    }

    /// Returns true if the validation has errors
    ///
    /// Errors indicate validation failures that should prevent further processing.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.0
            .iter()
            .any(|issue| matches!(issue.level, ValidationLevel::Error))
    }

    /// Get all errors (not warnings)
    ///
    /// Returns only the validation issues that are marked as errors,
    /// filtering out any warnings.
    #[must_use]
    pub fn errors(&self) -> Vec<&ValidationIssue> {
        self.0
            .iter()
            .filter(|issue| issue.level == ValidationLevel::Error)
            .collect()
    }

    /// Returns true if the validation has warnings
    ///
    /// Warnings indicate potential issues that don't prevent validation
    /// but should be brought to the user's attention.
    #[must_use]
    pub fn has_warnings(&self) -> bool {
        self.0
            .iter()
            .any(|issue| matches!(issue.level, ValidationLevel::Warning))
    }

    /// Get all warnings (not errors)
    ///
    /// Returns only the validation issues that are marked as warnings,
    /// filtering out any errors.
    #[must_use]
    pub fn warnings(&self) -> Vec<&ValidationIssue> {
        self.0
            .iter()
            .filter(|issue| issue.level == ValidationLevel::Warning)
            .collect()
    }
}

impl From<Vec<ValidationIssue>> for ValidationIssues {
    fn from(value: Vec<ValidationIssue>) -> Self {
        Self(value)
    }
}

/// A single validation issue (error or warning)
///
#[derive(Debug, Clone)]
pub struct ValidationIssue {
    /// The category of the issue
    ///
    pub(crate) category: ValidationErrorCategory,

    /// The field or context where the issue was found
    ///
    pub(crate) field: String,

    /// Detailed description of the issue
    ///
    pub(crate) message: IssueMessage,

    /// Is this a warning (false = error)
    ///
    pub(crate) level: ValidationLevel,

    /// Suggested fix for the issue
    ///
    pub(crate) suggestion: Option<String>,

    /// Where in the YAML file the issue is, when the file says.
    pub(crate) location: Option<crate::yaml::SourceLocation>,
}

/// What a validation issue says: its own words, or the deploy state's verdict
/// in the words a run gives it.
#[derive(Debug, Clone)]
pub enum IssueMessage {
    /// The issue's own sentence.
    Text(String),
    /// The deploy state's verdict.
    // Boxed: the notice is large, and most issues have none.
    DeployState(Box<DeployStateNotice>),
}

crate::parts::display_from_parts!(IssueMessage);

impl crate::parts::Composed for IssueMessage {
    fn parts(&self) -> crate::parts::Parts<'_> {
        match self {
            Self::Text(text) => crate::parts::Parts::new().text(text.as_str()),
            Self::DeployState(notice) => notice.parts(),
        }
    }
}

/// The deploy state's verdict a configuration issue reports, with the paths
/// its message names.
#[derive(Debug, Clone)]
pub enum DeployStateNotice {
    /// The state could not be used, or its directory would refuse a write.
    Failure(crate::dotfile_service::StateLoadFailure),
    /// Something about the state worth saying that stops nothing.
    Warning(crate::dotfile_service::DeployStateWarning),
}

crate::parts::display_from_parts!(DeployStateNotice);

impl crate::parts::Composed for DeployStateNotice {
    fn parts(&self) -> crate::parts::Parts<'_> {
        match self {
            Self::Failure(failure) => failure.parts(),
            Self::Warning(warning) => warning.parts(),
        }
    }
}

impl ValidationIssue {
    /// Create a new validation error
    ///
    pub(super) fn error(
        category: ValidationErrorCategory,
        field: &str,
        message: &str,
        suggestion: Option<&str>,
    ) -> Self {
        Self {
            category,
            field: field.to_string(),
            message: IssueMessage::Text(message.to_string()),
            level: ValidationLevel::Error,
            suggestion: suggestion.map(std::string::ToString::to_string),
            location: None,
        }
    }

    /// Create an informational notice.
    ///
    /// Reported alongside errors and warnings but never affects validity. Used
    /// where the user needs to know something about a package that is not a
    /// defect — a warning here would fire on every correct package and train
    /// people to ignore warnings.
    pub(super) fn info(
        category: ValidationErrorCategory,
        field: &str,
        message: &str,
        suggestion: Option<&str>,
    ) -> Self {
        Self {
            category,
            field: field.to_string(),
            message: IssueMessage::Text(message.to_string()),
            level: ValidationLevel::Info,
            suggestion: suggestion.map(std::string::ToString::to_string),
            location: None,
        }
    }

    /// Create a new validation error with source location
    pub(super) fn error_at(
        category: ValidationErrorCategory,
        field: &str,
        message: &str,
        suggestion: Option<&str>,
        location: Option<crate::yaml::SourceLocation>,
    ) -> Self {
        Self {
            category,
            field: field.to_string(),
            message: IssueMessage::Text(message.to_string()),
            level: ValidationLevel::Error,
            suggestion: suggestion.map(std::string::ToString::to_string),
            location,
        }
    }

    /// Create a new validation warning
    pub(super) fn warning(
        category: ValidationErrorCategory,
        field: &str,
        message: &str,
        suggestion: Option<&str>,
    ) -> Self {
        Self {
            category,
            field: field.to_string(),
            message: IssueMessage::Text(message.to_string()),
            level: ValidationLevel::Warning,
            suggestion: suggestion.map(std::string::ToString::to_string),
            location: None,
        }
    }

    /// Create a new validation warning with source location
    pub(super) fn warning_at(
        category: ValidationErrorCategory,
        field: &str,
        message: &str,
        suggestion: Option<&str>,
        location: Option<crate::yaml::SourceLocation>,
    ) -> Self {
        Self {
            category,
            field: field.to_string(),
            message: IssueMessage::Text(message.to_string()),
            level: ValidationLevel::Warning,
            suggestion: suggestion.map(std::string::ToString::to_string),
            location,
        }
    }

    #[must_use]
    pub fn category(&self) -> ValidationErrorCategory {
        self.category
    }

    #[must_use]
    pub fn field(&self) -> &str {
        &self.field
    }

    /// The issue's sentence, as its [`wording`](Self::wording) words it.
    #[must_use]
    pub fn message(&self) -> std::borrow::Cow<'_, str> {
        match &self.message {
            IssueMessage::Text(text) => text.as_str().into(),
            IssueMessage::DeployState(notice) => notice.to_string().into(),
        }
    }

    /// What the issue says, as parts a caller can show by kind.
    #[must_use]
    pub fn wording(&self) -> &IssueMessage {
        &self.message
    }

    #[must_use]
    pub fn level(&self) -> ValidationLevel {
        self.level
    }

    #[must_use]
    pub fn suggestion(&self) -> Option<&String> {
        self.suggestion.as_ref()
    }

    #[must_use]
    pub fn location(&self) -> Option<crate::yaml::SourceLocation> {
        self.location
    }

    /// An issue at `level` reporting `notice`, worded as the notice is.
    pub(crate) fn of_deploy_state(
        level: ValidationLevel,
        category: ValidationErrorCategory,
        notice: DeployStateNotice,
    ) -> Self {
        Self {
            category,
            field: "state_directory".to_string(),
            message: IssueMessage::DeployState(Box::new(notice)),
            level,
            suggestion: None,
            location: None,
        }
    }
}

/// How serious a validation issue is.
///
/// Converts to a label for an adapter: `error`, `warning` or `info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum ValidationLevel {
    Error,
    Warning,
    /// Neither a defect nor a risk: something about the package the user should
    /// know. Does not affect whether validation passes.
    Info,
}

/// Categories of package validation errors
///
/// [`Display`](std::fmt::Display) words a category for a person: "required
/// field", "command syntax".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::Display)]
pub enum ValidationErrorCategory {
    /// Missing required fields
    #[strum(to_string = "required field")]
    RequiredField,

    /// Invalid field values
    #[strum(to_string = "invalid value")]
    InvalidValue,

    /// Environment-specific errors
    #[strum(to_string = "environment")]
    Environment,

    /// Shell command syntax errors
    #[strum(to_string = "command syntax")]
    CommandSyntax,

    /// URL format errors
    #[strum(to_string = "URL format")]
    UrlFormat,

    /// Path format errors
    #[strum(to_string = "path format")]
    PathFormat,

    /// Something the user should know that is not a defect
    ///
    /// Pairs with [`ValidationLevel::Info`]. The other categories all name a
    /// kind of mistake, and filing a notice under one of them (`InvalidValue`,
    /// say) mislabels it in every table and JSON payload that shows the
    /// category.
    #[strum(to_string = "advisory")]
    Advisory,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::event::Outcome;

    fn issue(level: ValidationLevel) -> ValidationIssue {
        ValidationIssue {
            category: ValidationErrorCategory::Advisory,
            field: "f".to_string(),
            message: IssueMessage::Text("m".to_string()),
            level,
            suggestion: None,
            location: None,
        }
    }

    // A notice alone must stay clean: every package with a command-sourced
    // dotfile carries one, and scoring it would fail every such package.
    #[test]
    fn a_validation_scores_by_its_worst_issue_and_ignores_notices() {
        let cases = [
            ("nothing", vec![], Outcome::Clean),
            (
                "notice only",
                vec![issue(ValidationLevel::Info)],
                Outcome::Clean,
            ),
            (
                "warning",
                vec![
                    issue(ValidationLevel::Info),
                    issue(ValidationLevel::Warning),
                ],
                Outcome::Found,
            ),
            (
                "error",
                vec![
                    issue(ValidationLevel::Warning),
                    issue(ValidationLevel::Error),
                ],
                Outcome::Failed,
            ),
        ];
        for (name, issues, expected) in cases {
            assert_eq!(ValidationIssues::from(issues).outcome(), expected, "{name}");
        }
    }
}
