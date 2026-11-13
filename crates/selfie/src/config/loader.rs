use std::path::{Path, PathBuf};

use crate::{
    config::diagnostics::LoadedConfig,
    fs::{FileSystem, filesystem::FileSystemError, target::repository_path},
    parts::{Composed, Parts},
};

/// Port for loading configuration from disk
///
/// This trait abstracts configuration loading to allow for different implementations
/// (e.g., YAML files, TOML files, environment variables) and to enable mocking in tests.
#[cfg_attr(any(test, feature = "with_mocks"), mockall::automock)]
pub trait ConfigLoader: Send + Sync {
    /// Load configuration from standard locations
    ///
    /// Searches for configuration files in standard locations and loads the first one found.
    /// The specific search locations depend on the implementation.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigLoadError`] if:
    /// - No configuration file is found in standard locations
    /// - Multiple configuration files are found (ambiguous)
    /// - File system access fails
    /// - The configuration file is not a regular file
    /// - Configuration file content is invalid
    ///
    /// A successful load may still carry diagnostics: see
    /// [`LoadedConfig::ignored_keys`].
    fn load_config(&self) -> Result<LoadedConfig, ConfigLoadError>;

    /// Find possible configuration file paths
    ///
    /// Returns a list of paths where configuration files might be located,
    /// typically including user config directory, current directory, etc.
    ///
    /// # Errors
    ///
    /// [`ConfigLoadError::NotFound`] naming the directory that was searched, or
    /// [`ConfigLoadError::FileSystemError`] when there is no such directory to
    /// search.
    fn find_config_file_paths(&self) -> Result<Vec<std::path::PathBuf>, ConfigLoadError>;
}

/// Errors that can occur during configuration loading
#[derive(Debug)]
pub enum ConfigLoadError {
    /// File system operation failed
    FileSystemError(FileSystemError),

    /// No configuration file found in any of the searched locations
    NotFound { searched: PathBuf },

    /// Multiple configuration files found, creating ambiguity
    MultipleFound(Vec<PathBuf>),

    /// The configuration file is not valid YAML, or does not describe a configuration.
    ///
    /// The message names where and what kind of problem, and never quotes the file.
    Parse {
        path: PathBuf,
        failure: crate::yaml::ParseFailure,
    },

    /// The configuration file is a fifo, socket or device node, not a regular file
    ///
    /// Reading one blocks until a writer arrives, so it is refused before the
    /// read rather than reported after it.
    // Its own wording frame rather than `IrregularTarget`'s own `Display`, which
    // is worded for a deploy target. This one names the file itself, because it
    // is rendered alone -- every command loads the configuration before doing
    // anything, so there is no surrounding context to borrow a path from.
    IrregularFile {
        path: PathBuf,
        kind: crate::fs::IrregularKind,
    },

    /// The configuration file is a symbolic link whose target does not exist
    ///
    /// Distinct from [`NotFound`](Self::NotFound): the file is *there*, selfie
    /// simply cannot follow it. Reporting it as absent would be a lie, and on the
    /// command line it would silently hand the run to the flags instead.
    UnresolvableFile { path: PathBuf },
}

impl Composed for ConfigLoadError {
    fn parts(&self) -> Parts<'_> {
        match self {
            // Its sentence is the filesystem error's own.
            Self::FileSystemError(error) => error.parts(),
            Self::NotFound { searched } => Parts::new()
                .text("No configuration file found in locations: ")
                .path(searched.as_path()),
            Self::MultipleFound(paths) => Parts::new()
                .text("Multiple configuration files found: ")
                .append(Parts::joined(paths, ", ", |path| {
                    Parts::new().path(path.as_path())
                })),
            Self::Parse { path, failure } => Parts::new()
                .text("Cannot parse the configuration file ")
                .path(path.as_path())
                .text(": ")
                .cause(failure),
            Self::IrregularFile { path, kind } => Parts::new().path(path.as_path()).text(format!(
                ": the configuration file is a {}, not a regular file. Replace it with a \
                     regular file or remove it.",
                kind.label()
            )),
            Self::UnresolvableFile { path } => Parts::new().path(path.as_path()).text(
                ": the configuration file is a symbolic link that does not resolve. Restore or \
                 check out its target, or remove the link.",
            ),
        }
    }
}

crate::parts::display_from_parts!(error ConfigLoadError);

impl From<FileSystemError> for ConfigLoadError {
    fn from(error: FileSystemError) -> Self {
        Self::FileSystemError(error)
    }
}

/// [`ConfigLoadError::UnresolvableFile`] if a configuration file is present as a
/// link that goes nowhere.
///
/// Ask this before concluding no configuration file exists. `path_exists`
/// **follows**, so a dangling link answers `false` and reads as absent; a
/// symlink into a dotfiles repository that has not been checked out yet is the
/// ordinary way to get one. Selfie would then run as though the user had no
/// configuration at all — from flags, or with an error naming a directory the
/// file is actually in.
///
/// `None` when the directory genuinely holds no configuration file, and for a
/// link that resolves — pointing the config at one is supported and common.
// A symlink that resolves is `Some` from `symlink_refusal` and `true` from
// `path_exists`; a dangling one is `Some` and `false`; an absent file is `None`
// and `false`. That pair is what separates the three, and it needs no new port
// method. `irregular_target_refusal` cannot help: its `fs::metadata` follows too,
// so a dangling link is `None` there -- deliberately, because for a deploy target
// `symlink_refusal` already covers it.
pub(crate) fn unresolvable_config_refusal<F: FileSystem>(
    fs: &F,
    config_dir: &Path,
) -> Option<ConfigLoadError> {
    ["config.yaml", "config.yml"]
        .iter()
        .map(|name| config_dir.join(name))
        .find(|candidate| {
            fs.symlink_refusal(&repository_path(candidate)).is_some() && !fs.path_exists(candidate)
        })
        .map(|path| ConfigLoadError::UnresolvableFile { path })
}

/// [`ConfigLoadError::IrregularFile`] if `path` is something reading would hang on
///
/// Call this immediately before every read of the configuration file. Opening a
/// fifo blocks until a writer arrives, and a single `mkfifo` at the configuration
/// path would otherwise wedge every command, since they all load configuration
/// first. The classification is a *following* `stat`, which never blocks, so it
/// is safe to ask about the very path that would hang.
///
/// `None` for a regular file and for a path that does not exist.
// `pub(crate)`: the config file is opened in exactly one place.
pub(crate) fn irregular_config_refusal<F: FileSystem>(
    fs: &F,
    path: &Path,
) -> Option<ConfigLoadError> {
    // The second arm fails **closed** and is deliberately not a wildcard that
    // returns `None`. `irregular_target_refusal` returns only `IrregularTarget`
    // today, so nothing reaches it; letting a future variant through would
    // un-guard the read and restore the hang this exists to prevent.
    match fs.irregular_target_refusal(&repository_path(path))? {
        FileSystemError::IrregularTarget { kind, .. } => Some(ConfigLoadError::IrregularFile {
            path: path.to_path_buf(),
            kind,
        }),
        other => Some(ConfigLoadError::FileSystemError(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parts::{Message, values_of};

    // Each path a config load error names is a part of its own, never words of
    // its sentence: every fixture path holds U+200B, which `values_of` refuses
    // in a text part.
    #[test]
    fn a_config_load_error_names_each_path_as_a_part() {
        let path = PathBuf::from("/h/.config/sel\u{200b}fie/config.yml");
        let shown = path.display().to_string();
        let failure =
            crate::yaml::parse_reporting_ignored::<serde_json::Value>("[", |_| {}).unwrap_err();
        let cases = [
            ConfigLoadError::NotFound {
                searched: path.clone(),
            },
            ConfigLoadError::Parse {
                path: path.clone(),
                failure,
            },
            ConfigLoadError::IrregularFile {
                path: path.clone(),
                kind: crate::fs::IrregularKind::Fifo,
            },
            ConfigLoadError::UnresolvableFile { path: path.clone() },
        ];
        for error in &cases {
            assert_eq!(
                values_of(&Message::of(error)),
                std::slice::from_ref(&shown),
                "{error}"
            );
        }
        let both = ConfigLoadError::MultipleFound(vec![
            "/a/\u{200b}config.yml".into(),
            "/a/\u{200b}config.yaml".into(),
        ]);
        assert_eq!(
            values_of(&Message::of(&both)),
            ["/a/\u{200b}config.yml", "/a/\u{200b}config.yaml"]
        );
    }
}
