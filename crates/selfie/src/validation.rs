/// Collection of validation issues (errors and warnings)
///
/// Provides methods to query and filter validation results, allowing
/// callers to handle errors and warnings differently based on their needs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ValidationIssues(Vec<ValidationIssue>);

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

    /// Returns true if there are any issues at all, at any level
    ///
    /// Includes informational notices, which are neither defects nor risks, so
    /// this being true does not imply anything is wrong. Use
    /// [`has_errors`](Self::has_errors) or [`has_warnings`](Self::has_warnings)
    /// to ask that.
    #[must_use]
    pub fn has_issues(&self) -> bool {
        !self.0.is_empty()
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

    /// Get issues by category
    ///
    /// Filters all issues to return only those matching the specified category.
    /// Useful for handling specific types of validation problems.
    #[must_use]
    pub fn issues_by_category(&self, category: &ValidationErrorCategory) -> Vec<&ValidationIssue> {
        self.0
            .iter()
            .filter(|issue| issue.category == *category)
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
#[derive(Debug, Clone, PartialEq)]
pub struct ValidationIssue {
    /// The category of the issue
    ///
    pub(crate) category: ValidationErrorCategory,

    /// The field or context where the issue was found
    ///
    pub(crate) field: String,

    /// Detailed description of the issue
    ///
    pub(crate) message: String,

    /// Is this a warning (false = error)
    ///
    pub(crate) level: ValidationLevel,

    /// Suggested fix for the issue
    ///
    pub(crate) suggestion: Option<String>,

    /// Source location in the YAML file (e.g., `"line 5, column 3"`)
    pub(crate) location: Option<String>,
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
            message: message.to_string(),
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
            message: message.to_string(),
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
        location: Option<String>,
    ) -> Self {
        Self {
            category,
            field: field.to_string(),
            message: message.to_string(),
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
            message: message.to_string(),
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
        location: Option<String>,
    ) -> Self {
        Self {
            category,
            field: field.to_string(),
            message: message.to_string(),
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

    #[must_use]
    pub fn message(&self) -> &str {
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
    pub fn location(&self) -> Option<&str> {
        self.location.as_deref()
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ValidationLevel {
    Error,
    Warning,
    /// Neither a defect nor a risk: something about the package the user should
    /// know. Does not affect whether validation passes.
    Info,
}

/// Categories of package validation errors
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::Display)]
#[strum(serialize_all = "snake_case")]
pub enum ValidationErrorCategory {
    /// Missing required fields
    ///
    RequiredField,

    /// Invalid field values
    ///
    InvalidValue,

    /// Environment-specific errors
    ///
    Environment,

    /// Shell command syntax errors
    ///
    CommandSyntax,

    /// URL format errors
    ///
    UrlFormat,

    /// Path format errors
    ///
    PathFormat,

    /// Something the user should know that is not a defect
    ///
    /// Pairs with [`ValidationLevel::Info`]. The other categories all name a
    /// kind of mistake, and filing a notice under one of them (`InvalidValue`,
    /// say) mislabels it in every table and JSON payload that shows the
    /// category.
    Advisory,
}
