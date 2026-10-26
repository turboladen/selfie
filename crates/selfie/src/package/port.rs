//! Package repository port and error types
//!
//! This module defines the core port for package repository operations in the
//! hexagonal architecture. The `PackageRepository` trait abstracts package
//! storage and retrieval, allowing different implementations (YAML files,
//! databases, remote repositories, etc.) while maintaining a consistent interface.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use thiserror::Error;

use crate::{
    fs::{DirectoryState, FileSystem, filesystem::FileSystemError},
    package::{GetPackage, Package},
};

/// Port for package storage: discovering, loading, saving and removing package
/// definitions.
#[cfg_attr(feature = "with_mocks", mockall::automock)]
pub trait PackageRepository: Send + Sync {
    /// Load a package by name, with every environment configuration, dependency
    /// and piece of metadata it declares.
    ///
    /// The name corresponds to the package file's name without its extension.
    ///
    /// # Errors
    ///
    /// [`PackageRepoError`] if no package by that name exists, if several do, if
    /// the definition is malformed, or if file system access fails.
    fn get_package(&self, name: &str) -> Result<GetPackage, PackageRepoError>;

    /// Whether anything already occupies `path`.
    ///
    /// Asked of the path, not of a package name: a caller about to write to
    /// `path` needs to know whether that write would land on something, and
    /// [`Self::get_package`] cannot answer it, because a name and a path do not
    /// have to agree about what is stored.
    fn path_is_occupied(&self, path: &Path) -> bool;

    /// Read a file a package refers to but does not contain, such as a dotfile
    /// template whose contents validation inspects.
    ///
    /// `relative_path` resolves against `package_path`'s parent directory — the
    /// same base `dotfiles` sources resolve against.
    ///
    /// It lives on the repository because the repository owns access to package
    /// storage; validation works on an already-loaded [`Package`] and has no file
    /// system of its own.
    ///
    /// # Errors
    ///
    /// Returns [`FileSystemError`] if the file does not exist, cannot be read, is
    /// not valid UTF-8, or names a path outside the package directory.
    fn read_referenced_file(
        &self,
        package_path: &Path,
        relative_path: &str,
    ) -> Result<String, FileSystemError>;

    /// Load every package definition in the configured directory, returning the
    /// ones that parsed **and** the errors from the ones that did not, so a
    /// caller can carry on past a single bad file.
    ///
    /// # Errors
    ///
    /// [`PackageListError`] if the package directory cannot be accessed or
    /// listed.
    fn list_packages(&self) -> Result<ListPackagesOutput, PackageListError>;

    /// Every package file matching `name`. More than one is how an ambiguous
    /// package name is detected.
    ///
    /// # Errors
    ///
    /// [`PackageListError`] if the package directory cannot be accessed.
    fn find_package_files(&self, name: &str) -> Result<Vec<PathBuf>, PackageListError>;

    /// Serialize a package and write it to `path`.
    ///
    /// # Errors
    ///
    /// [`PackageRepoError`] if the package cannot be serialized to YAML, or if
    /// the target directory does not exist or cannot be written to.
    fn save_package(&self, package: &Package, path: &Path) -> Result<(), PackageRepoError>;

    /// Delete a package's definition file. Irreversible, so confirm with the user
    /// before calling it.
    ///
    /// # Errors
    ///
    /// [`PackageRepoError`] if the package does not exist, permission is denied,
    /// or the deletion otherwise fails.
    fn remove_package(&self, name: &str) -> Result<(), PackageRepoError>;

    /// Every package listing `target_package` as a dependency in any of its
    /// environments, so a caller can say what a removal would break, paired with
    /// the spec files that could not be read while looking, or that selfie
    /// refuses to read.
    ///
    /// A caller that ignores the second list is claiming nothing depends on the
    /// target when the honest answer is that it does not know: an unreadable
    /// spec declares dependencies selfie never saw.
    ///
    /// The failures are returned rather than rendered, so an adapter reporting
    /// structure has the kind and location as values.
    ///
    /// # Errors
    ///
    /// [`PackageRepoError`] if the package listing fails.
    fn find_dependent_packages(
        &self,
        target_package: &str,
    ) -> Result<(Vec<Package>, Vec<PackageParseError>), PackageRepoError>;

    /// The identity of the directory this repository reads, or `None` when it
    /// cannot be found, such as when nothing is there.
    ///
    /// Two repositories answering the same `Some` read one directory, however
    /// each was configured. `None` says nothing about another repository.
    fn directory_identity(&self) -> Option<crate::fs::FileIdentity>;

    /// The home directory dotfile targets expand against, as the repository's
    /// file system reports it.
    ///
    /// # Errors
    ///
    /// [`FileSystemError`] when the home directory cannot be determined, as
    /// [`HomeDir::home`](crate::fs::target::HomeDir::home) reports it.
    fn home_directory(&self) -> Result<PathBuf, FileSystemError>;
}

/// Errors that can occur during package repository operations
///
/// This enum represents all possible failures when interacting with the
/// package repository, providing detailed context for debugging and
/// error handling.
#[derive(Error, Debug, Clone)]
pub enum PackageRepoError {
    /// Package-specific error (not found, parse error, etc.)
    #[error(transparent)]
    PackageError(Box<PackageError>),

    /// Package listing operation failed
    #[error(transparent)]
    PackageListError(#[from] PackageListError),

    /// IO error during repository operation
    #[error("IO error: {0}")]
    IoError(Arc<std::io::Error>),

    /// File system error during repository operation
    #[error("File system error: {0}")]
    FileSystemError(FileSystemError),

    /// The package could not be written as YAML, so nothing was saved.
    #[error("The package cannot be written as YAML: {0}")]
    Unserializable(crate::yaml::SerializeFailure),

    /// Refused to rewrite a package whose dotfile entries carry unrecognized keys.
    ///
    /// Saving re-serializes from the struct, so a key the struct does not model
    /// is dropped. For a dotfile entry that key is exactly what makes
    /// [`content_source`](crate::package::DotfileEntry::content_source) refuse
    /// the entry, so the rewrite would turn an entry `selfie apply` skips into
    /// one it deploys.
    // The remedy names editing the file rather than a selfie command on purpose:
    // every command that could rewrite the file is refused by this same guard, so
    // pointing at one would send the user back to a tool that just refused them.
    #[error(
        "refusing to rewrite {path}: unrecognized {fields}. \
         Saving would delete the key and silently make the entry deployable. \
         Edit {path} directly to correct or remove the key."
    )]
    UnknownDotfileFields {
        /// The file that would have been rewritten.
        path: PathBuf,
        /// The offending field paths, e.g. `dotfiles[0].var`.
        fields: String,
    },

    /// Refused to rewrite a package file because its top level carries a key the
    /// struct does not model.
    ///
    /// `_dotfiles:` read as an anchor, or a plain misspelling like `configs:`,
    /// is dropped by a rewrite -- taking every entry under it, which is the
    /// largest thing any of these refusals protects.
    #[error(
        "refusing to rewrite {path}: unrecognized {fields}. \
         Saving would delete the key and everything under it. \
         Edit {path} directly to correct or remove the key."
    )]
    UnknownTopLevelFields {
        /// The file that would have been rewritten.
        path: PathBuf,
        /// The offending field paths, e.g. `_dotfiles`.
        fields: String,
    },

    /// Refused to rewrite a package file because its top level could not be read
    /// back, so selfie cannot say whether it carries keys the struct would drop.
    ///
    /// Distinct from [`UnknownTopLevelFields`](Self::UnknownTopLevelFields),
    /// which names keys it found. Here nothing is known, and a rewrite serializes
    /// from the struct regardless -- so the safe direction is to decline the
    /// write, which costs the user a retry, rather than to delete silently.
    // The parse failure ends the message, because it is the part a reader can act
    // on last: the remedy is the same whatever the file turned out to be wrong
    // about. The apply-path warning orders the same string the same way.
    #[error(
        "refusing to rewrite {path}: its top level could not be read back, so any \
         key selfie does not model would be dropped without warning. \
         Edit {path} directly, or simplify it until selfie can read it. \
         The read failed with: {error}"
    )]
    UncheckedTopLevel {
        /// The file that would have been rewritten.
        path: PathBuf,
        /// Why the re-read failed.
        error: String,
    },

    /// Refused to rewrite a package file because an environment carries a key
    /// the struct does not model.
    ///
    /// Separate from [`UnknownDotfileFields`](Self::UnknownDotfileFields)
    /// because the harm differs. No entry becomes deployable here: the key is
    /// a setting of the environment, so re-serializing deletes whatever the user
    /// wrote for it -- `audt: "brew audit myapp"` takes the command with it.
    // Same reasoning as the sibling for naming the file rather than a command:
    // every writer is refused by this guard.
    #[error(
        "refusing to rewrite {path}: unrecognized {fields}. \
         Saving would delete the key and whatever it was set to. \
         Edit {path} directly to correct or remove the key."
    )]
    UnknownEnvironmentFields {
        /// The file that would have been rewritten.
        path: PathBuf,
        /// The offending field paths, e.g. `environments.work.audt`.
        fields: String,
    },

    /// Refused to write a package file because of what is at its path.
    ///
    /// Carries its own wording, which **must never say "target"**. Anything that
    /// is not a refusal keeps the plain
    /// [`FileSystemError`](Self::FileSystemError) passthrough.
    // Both `FileSystemError` refusal variants say "target" in their `Display` —
    // "target is a symlink", "target resolves to a …" — because both were written
    // for a dotfile target, the path selfie deploys out to. A package file is the
    // opposite direction: selfie writing into its own repository. Telling a user
    // their "target" is a symlink when they ran `selfie spec update` names a
    // thing that is not in the sentence they typed.
    // `a_refused_package_path_does_not_call_it_a_target` holds this.
    #[error(
        "refusing to write {path}: {reason}. \
         Remove what is at that path, or choose another name."
    )]
    UnwritablePath {
        /// The package file selfie declined to write.
        path: PathBuf,
        /// What was found there, phrased to follow a colon.
        reason: String,
    },
}

impl From<Arc<std::io::Error>> for PackageRepoError {
    fn from(error: Arc<std::io::Error>) -> Self {
        Self::IoError(error)
    }
}

impl From<FileSystemError> for PackageRepoError {
    fn from(error: FileSystemError) -> Self {
        Self::FileSystemError(error)
    }
}

impl PackageRepoError {
    /// Whether this means no package file exists at that name.
    ///
    /// Two answers do: the name matched nothing, and nothing at all is at the
    /// package directory's path. Every other error means selfie found something and
    /// could not use it -- a file that will not parse, one it refused to read,
    /// two files claiming the same name -- and each is a file that creating or
    /// templating over would destroy.
    // Lives on the error rather than beside one caller because two commands ask
    // this and a third will. A rule stated twice is a rule that can drift, which
    // is what selfie-vhw4 was about.
    #[must_use]
    pub fn means_no_such_package(&self) -> bool {
        self.lookup_kind() == Some(LookupKind::NotFound)
    }

    /// Whether this error is about the file or files sitting at that name.
    ///
    /// `true` when selfie found something there and will not use it: a file that
    /// will not parse, one it could not read, or two files claiming the name.
    /// `false` when the failure is not about that package's own file at all,
    /// such as a directory selfie could not list.
    ///
    /// A caller that reports every failure as the package's own spec names the
    /// wrong thing for the second group. [`means_no_such_package`] answers a
    /// different question and the two are not opposites: a name with nothing
    /// behind it is neither.
    ///
    /// [`means_no_such_package`]: Self::means_no_such_package
    #[must_use]
    pub fn names_an_unusable_spec(&self) -> bool {
        matches!(
            self.lookup_kind(),
            Some(LookupKind::AmbiguousName | LookupKind::Unloadable)
        )
    }

    /// Says what a failed lookup that returned this error was about, or `None`
    /// for an error a lookup does not return.
    #[must_use]
    pub fn lookup_kind(&self) -> Option<LookupKind> {
        self.lookup().map(|lookup| match lookup {
            Lookup::NotFound => LookupKind::NotFound,
            Lookup::AmbiguousName(_) => LookupKind::AmbiguousName,
            Lookup::Unloadable(_) => LookupKind::Unloadable,
            Lookup::PackageDirectoryUnreadable => LookupKind::PackageDirectoryUnreadable,
        })
    }

    /// Turns this error from a package lookup into the failure
    /// [`lookup_kind`](Self::lookup_kind) names, carrying the facts that kind
    /// needs.
    ///
    /// # Errors
    ///
    /// Returns the error unchanged when a lookup does not return it, so the
    /// caller reports it as itself.
    pub fn into_lookup_failure(self) -> Result<LookupFailure, Self> {
        // The facts are copied out first, which ends the borrow before `self`
        // moves into the wrapper beside them.
        let facts = match self.lookup() {
            None => return Err(self),
            Some(Lookup::NotFound) => Facts::NotFound,
            Some(Lookup::AmbiguousName(paths)) => Facts::Paths(paths.to_vec()),
            Some(Lookup::Unloadable(failure)) => Facts::Failure(failure.clone()),
            Some(Lookup::PackageDirectoryUnreadable) => Facts::UnreadableDirectory,
        };
        Ok(match facts {
            Facts::NotFound => LookupFailure::NotFound(NotFound(self)),
            Facts::UnreadableDirectory => {
                LookupFailure::PackageDirectoryUnreadable(UnreadableDirectory(self))
            }
            Facts::Paths(conflicting_paths) => LookupFailure::AmbiguousName(AmbiguousName {
                conflicting_paths,
                error: self,
            }),
            Facts::Failure(failure) => LookupFailure::Unloadable(UnloadableSpec {
                failure,
                error: self,
            }),
        })
    }

    /// What a failed lookup that returned this error was about, borrowing the
    /// facts its kind needs. The one classification every lookup question asks.
    fn lookup(&self) -> Option<Lookup<'_>> {
        // Every variant is listed, with no `_` arm, so a new error is a compile
        // error here and not a silent relabeling. `None`, shown as itself, is the
        // safe answer for an error that is not about one spec: a caller losing the
        // package's name is recoverable, a caller blaming the wrong file is not.
        match self {
            Self::PackageError(error) => match &**error {
                PackageError::PackageNotFound { .. } => Some(Lookup::NotFound),
                PackageError::MultiplePackagesFound {
                    conflicting_paths, ..
                } => Some(Lookup::AmbiguousName(conflicting_paths)),
                PackageError::ParseError { cause, .. }
                | PackageError::UnreadableFile { cause, .. }
                | PackageError::UnusableName { cause, .. } => Some(Lookup::Unloadable(cause)),
                // Raised by the service once a spec has loaded, never by a lookup.
                PackageError::EnvironmentNotFound { .. }
                | PackageError::NoCheckCommand { .. }
                | PackageError::NoInstallCommand { .. }
                // Raised by `spec create`, never by a lookup.
                | PackageError::PackageAlreadyExists { .. }
                | PackageError::PackagePathOccupied { .. } => None,
            },
            // A missing package directory holds no file to lose, and the write
            // creates it -- `write_file_no_follow` runs `create_dir_all` first. A
            // file or a dangling link at the path holds none either, but that
            // `create_dir_all` fails against it, so it is not "no such package".
            Self::PackageListError(listing) if listing.may_be_created() => Some(Lookup::NotFound),
            Self::PackageListError(_) => Some(Lookup::PackageDirectoryUnreadable),
            // Raised by `save_package`, never by a lookup.
            Self::IoError(_)
            | Self::Unserializable(_)
            | Self::UnknownDotfileFields { .. }
            | Self::UnknownTopLevelFields { .. }
            | Self::UncheckedTopLevel { .. }
            | Self::UnknownEnvironmentFields { .. }
            | Self::UnwritablePath { .. }
            // Raised by `dotfiles track`, never by a lookup.
            | Self::FileSystemError(_) => None,
        }
    }
}

/// What [`PackageRepoError::lookup`] found, borrowing the facts each kind needs.
enum Lookup<'a> {
    NotFound,
    AmbiguousName(&'a [PathBuf]),
    Unloadable(&'a PackageParseError),
    PackageDirectoryUnreadable,
}

/// The facts copied out of a [`Lookup`] before the error moves.
enum Facts {
    NotFound,
    Paths(Vec<PathBuf>),
    Failure(PackageParseError),
    UnreadableDirectory,
}

/// What a failed lookup of a package was about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookupKind {
    /// No spec answers to the name.
    NotFound,
    /// Several spec files claim the name.
    AmbiguousName,
    /// A spec answers to the name and could not be loaded.
    Unloadable,
    /// The package directory could not be read.
    PackageDirectoryUnreadable,
}

/// A failed lookup of one package, carrying the facts its kind needs.
///
/// [`Display`](std::fmt::Display) is the error's own sentence. Its label is the
/// variant's name in snake_case. Outside selfie, only
/// [`PackageRepoError::into_lookup_failure`] builds one, so a variant always
/// carries the error its kind names.
#[derive(Debug, Clone, strum::IntoStaticStr, strum::VariantNames)]
#[strum(serialize_all = "snake_case")]
pub enum LookupFailure {
    /// No spec answers to the name.
    NotFound(NotFound),
    /// Several spec files claim the name, so none is used.
    AmbiguousName(AmbiguousName),
    /// A spec answers to the name and could not be loaded.
    Unloadable(UnloadableSpec),
    /// The package directory could not be read, so no spec was looked at.
    PackageDirectoryUnreadable(UnreadableDirectory),
}

impl LookupFailure {
    /// The failure for a spec file that answers to `name` and could not be
    /// loaded, carrying the error [`PackageError::unloadable`] chooses.
    #[must_use]
    pub(crate) fn unloadable(
        name: &str,
        packages_path: &Path,
        failed_file: &Path,
        cause: PackageParseError,
    ) -> Self {
        let error = PackageError::unloadable(name, packages_path, failed_file, cause.clone());
        Self::Unloadable(UnloadableSpec {
            failure: cause,
            error: error.into(),
        })
    }

    /// The label for this failure, such as `"not_found"`.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        self.into()
    }

    /// The error the lookup returned.
    #[must_use]
    pub fn into_error(self) -> PackageRepoError {
        match self {
            Self::NotFound(NotFound(error))
            | Self::AmbiguousName(AmbiguousName { error, .. })
            | Self::Unloadable(UnloadableSpec { error, .. })
            | Self::PackageDirectoryUnreadable(UnreadableDirectory(error)) => error,
        }
    }
}

impl std::fmt::Display for LookupFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(NotFound(error))
            | Self::AmbiguousName(AmbiguousName { error, .. })
            | Self::Unloadable(UnloadableSpec { error, .. })
            | Self::PackageDirectoryUnreadable(UnreadableDirectory(error)) => write!(f, "{error}"),
        }
    }
}

/// A lookup no spec answered, holding the error as it came. Its sentence names
/// the package directory when nothing at all is at its path.
#[derive(Debug, Clone)]
pub struct NotFound(PackageRepoError);

/// A name several spec files claim.
#[derive(Debug, Clone)]
pub struct AmbiguousName {
    conflicting_paths: Vec<PathBuf>,
    error: PackageRepoError,
}

impl AmbiguousName {
    /// The files claiming the name.
    #[must_use]
    pub fn conflicting_paths(&self) -> &[PathBuf] {
        &self.conflicting_paths
    }
}

/// A spec that answers to a name and could not be loaded.
#[derive(Debug, Clone)]
pub struct UnloadableSpec {
    failure: PackageParseError,
    error: PackageRepoError,
}

impl UnloadableSpec {
    /// What went wrong, with its place in the file when it does not parse.
    #[must_use]
    pub fn failure(&self) -> &PackageParseError {
        &self.failure
    }
}

/// A package directory that could not be read, holding the error as it came.
#[derive(Debug, Clone)]
pub struct UnreadableDirectory(PackageRepoError);

impl From<PackageError> for PackageRepoError {
    fn from(err: PackageError) -> Self {
        Self::PackageError(Box::new(err))
    }
}

/// A package directory that would not give up its specs, with what is at its path.
///
/// One shape, not a set of variants, because every consumer needs the same two facts
/// and the classification is made once, by the repository that did the listing. A
/// consumer that re-derived it from an error kind reached a different answer for the
/// same path: that is how an unreadable directory came out as "not found" and was
/// offered a `mkdir -p` that cannot work.
#[derive(Error, Debug, Clone)]
#[error("{} {}", self.path.display(), self.clause())]
pub struct PackageListError {
    path: PathBuf,
    state: DirectoryState,
}

impl PackageListError {
    /// The error for `path`, whose state the caller has already determined.
    #[must_use]
    pub fn new(path: PathBuf, state: DirectoryState) -> Self {
        Self { path, state }
    }

    /// The error for `path` after a listing of it failed with `error`.
    ///
    /// Classifies once, here, so the state travels with the error instead of every
    /// consumer stating the path again and risking a different answer.
    #[must_use]
    pub fn from_listing<F: FileSystem + ?Sized>(
        filesystem: &F,
        path: PathBuf,
        error: &std::io::Error,
    ) -> Self {
        let state = DirectoryState::from_listing(filesystem, &path, error);
        Self { path, state }
    }

    /// The directory this error is about.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What is at that path, as the repository classified it.
    #[must_use]
    pub fn state(&self) -> &DirectoryState {
        &self.state
    }

    /// Whether nothing that can hold a spec is at the path.
    ///
    /// The question a caller asks to mean "this directory holds no names". An
    /// unlistable or unclassifiable path is not absent: it may be hiding specs, so a
    /// name check that treated it as empty would answer without looking. Whether the
    /// directory may be created is [`may_be_created`](Self::may_be_created).
    #[must_use]
    pub fn is_absent(&self) -> bool {
        matches!(self.state, DirectoryState::Absent(_))
    }

    /// Whether nothing at all is at the path, so the directory may be created there.
    ///
    /// Narrower than [`is_absent`](Self::is_absent). A file, a dangling symlink or a
    /// path below something that is not a directory also holds no specs, but a write
    /// that creates the directory fails against each of them, so a caller about to
    /// create must refuse those.
    #[must_use]
    pub fn may_be_created(&self) -> bool {
        matches!(
            self.state,
            DirectoryState::Absent(crate::fs::AbsentReason::Empty)
        )
    }

    /// Why the listing produced nothing, as a clause that follows the path.
    ///
    /// The absent reasons come from
    /// [`AbsentReason::clause`](crate::fs::AbsentReason::clause), so a listing failure
    /// and a directory warning about one path describe it in the same words.
    #[must_use]
    pub fn clause(&self) -> String {
        match &self.state {
            // A path the port calls a directory whose listing still failed. The
            // listing is the more recent answer, so it may be hiding entries.
            DirectoryState::Directory => "could not be listed".to_string(),
            state => state.clause(),
        }
    }
}

/// Why several spec files claiming one name are refused, and what to do about it.
///
/// The one wording, for install and for apply and drift alike. `directory` is
/// where the files are, and `paths` the files.
pub(crate) fn ambiguous_name_sentence(name: &str, directory: &Path, paths: &[PathBuf]) -> String {
    format!(
        "Multiple packages found with name `{name}` in path {}: {}. Names are compared ignoring \
         case, and `.yml` and `.yaml` name the same package, so these files all claim one name. \
         Rename or remove all but one.",
        directory.display(),
        format_conflicting_paths(paths)
    )
}

/// A spec whose `name:` field answers to a name its file is not named for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameDeclaration {
    /// The spec file.
    pub path: PathBuf,
    /// Its `name:` field, as written.
    pub name: String,
}

/// Every one of `packages` whose `name:` field folds to `name`, in order of
/// path.
///
/// Asked only once no file answers to `name`, so every match is a spec whose
/// file is named for something else.
pub(crate) fn declaring_name(name: &str, packages: &[Package]) -> Vec<NameDeclaration> {
    let wanted = name.to_lowercase();
    let mut declared: Vec<NameDeclaration> = packages
        .iter()
        .filter(|package| package.name().to_lowercase() == wanted)
        .map(|package| NameDeclaration {
            path: package.path().clone(),
            name: package.name().to_string(),
        })
        .collect();
    declared.sort_by(|a, b| a.path.cmp(&b.path));
    declared
}

/// `error` with the specs of `repo` whose `name:` field answers to the name it
/// could not find, when it is a lookup's not-found failure; any other error as
/// it is.
///
/// Parses every spec `repo` lists, so it is for a failure about to reach the
/// user, never for a lookup whose not-found is an answer.
pub(crate) fn with_name_hint<R: PackageRepository + ?Sized>(
    repo: &R,
    error: PackageRepoError,
) -> PackageRepoError {
    let PackageRepoError::PackageError(mut boxed) = error else {
        return error;
    };
    if let PackageError::PackageNotFound {
        name, declared_by, ..
    } = boxed.as_mut()
    {
        // A listing that fails gives no hint; the not-found answer stands alone.
        let listed: Vec<Package> = repo
            .list_packages()
            .map(|output| output.0.into_iter().filter_map(Result::ok).collect())
            .unwrap_or_default();
        *declared_by = declaring_name(name, &listed);
    }
    PackageRepoError::PackageError(boxed)
}

/// The sentence a not-found failure appends for `declared`, naming each file
/// and the name to use instead, or nothing when `declared` is empty.
pub(crate) fn name_field_hint(declared: &[NameDeclaration]) -> String {
    if declared.is_empty() {
        return String::new();
    }
    // The file by its directory and name, as `packages/nvim.yml`, so a reader
    // with both directories can tell which one holds it.
    let file = |d: &NameDeclaration| {
        let name = d.path.file_name().map(Path::new).unwrap_or(&d.path);
        d.path
            .parent()
            .and_then(Path::file_name)
            .map_or_else(|| name.to_path_buf(), |dir| Path::new(dir).join(name))
            .display()
            .to_string()
    };
    // The `name:` field is text from a file, so control characters are escaped
    // before it reaches a terminal.
    let claims: Vec<String> = declared
        .iter()
        .map(|d| format!("{} declares `name: {}`", file(d), d.name.escape_debug()))
        .collect();
    // Each name appears once without deduplicating: two files claiming one name
    // are ambiguous, and an ambiguous name is never collected or looked up.
    let names: Vec<String> = declared
        .iter()
        .filter_map(|d| crate::package::spec_name_of(&d.path))
        .map(|name| format!("'{name}'"))
        .collect();
    format!(
        ". {}; a package is named by its file, so use {}",
        claims.join(" and "),
        names.join(" or ")
    )
}

/// [`ambiguous_name_sentence`] for files that all sit in one directory, named
/// by the first file's.
pub(crate) fn ambiguous_files_sentence(name: &str, paths: &[PathBuf]) -> String {
    let directory = paths
        .first()
        .and_then(|path| path.parent())
        .unwrap_or_else(|| Path::new("."));
    ambiguous_name_sentence(name, directory, paths)
}

// File names alone -- the directory is already named earlier in the message, so
// repeating the full path for every conflict buries the part that differs.
fn format_conflicting_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| {
            p.file_name()
                .unwrap_or(p.as_os_str())
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Errors that can occur with specific package operations
///
/// Represents detailed failures when working with individual packages,
/// providing rich context for debugging and user-friendly error messages.
#[derive(Error, Debug, Clone)]
pub enum PackageError {
    /// No package with the specified name could be found
    #[error(
        "Package `{name}` not found in path {}{}",
        packages_path.display(),
        name_field_hint(declared_by)
    )]
    #[allow(clippy::doc_link_with_quotes)]
    PackageNotFound {
        name: String,
        packages_path: PathBuf,
        /// Number of files examined during search
        files_examined: usize,
        /// Search patterns used (e.g., ["package.yml", "package.yaml"])
        search_patterns: Vec<String>,
        /// The specs whose `name:` field answers to `name`, though no file is
        /// named for it.
        declared_by: Vec<NameDeclaration>,
    },

    /// Multiple package files found with the same name, creating ambiguity
    #[error("{}", ambiguous_name_sentence(name, packages_path, conflicting_paths))]
    MultiplePackagesFound {
        name: String,
        packages_path: PathBuf,
        /// The conflicting file paths found
        conflicting_paths: Vec<PathBuf>,
        files_examined: usize,
        search_patterns: Vec<String>,
    },

    /// Package definition file exists but could not be parsed
    // The failing FILE, not the directory holding it. Every consumer of this
    // message tells the reader to go open it, and a directory is not something
    // you can open -- the two `spec` commands that say so were both naming a
    // path the user still had to search.
    #[error("Parse error in package `{name}` from {}: {cause}", failed_file.display())]
    ParseError {
        name: String,
        packages_path: PathBuf,
        /// The specific file that failed to parse
        failed_file: PathBuf,
        cause: PackageParseError,
    },

    /// Package definition file exists but its contents never reached the parser
    ///
    /// Separate from [`ParseError`](Self::ParseError) because nothing was parsed:
    /// selfie either declined to open the file or the read itself failed. Saying
    /// "parse error" for a fifo sends the reader to inspect YAML syntax in a file
    /// that has none.
    #[error("Cannot read package `{name}` from {}: {cause}", failed_file.display())]
    UnreadableFile {
        name: String,
        packages_path: PathBuf,
        /// The specific file that could not be read
        failed_file: PathBuf,
        cause: PackageParseError,
    },

    /// Package definition file exists, but its file name is not a valid spec name
    ///
    /// The file is never read, so neither "parse error" nor "cannot read" is
    /// true of it: the remedy is renaming the file.
    #[error("Cannot use package `{name}` from {}: {cause}", failed_file.display())]
    UnusableName {
        name: String,
        packages_path: PathBuf,
        /// The file whose name is refused
        failed_file: PathBuf,
        cause: PackageParseError,
    },

    /// The requested environment is not configured for this package
    #[error(
        "Environment `{environment}` not found in package `{package_name}`{}",
        required_by_clause(required_by.as_deref())
    )]
    EnvironmentNotFound {
        package_name: String,
        environment: String,
        /// Available environments for suggestions
        available_environments: Vec<String>,
        package_file: PathBuf,
        /// The package that needs this one, when it is a dependency or a
        /// recommend of the package being installed.
        required_by: Option<String>,
    },

    /// Package environment exists but has no check command configured
    #[error("No check command defined for package `{package_name}` in environment `{environment}`")]
    NoCheckCommand {
        package_name: String,
        environment: String,
        package_file: PathBuf,
        /// Whether other environments have check commands (for suggestions)
        other_envs_with_check: Vec<String>,
    },

    /// A package with the specified name already exists
    #[error("Package `{name}` already exists at {}", file_path.display())]
    PackageAlreadyExists { name: String, file_path: PathBuf },

    /// The write path for a new package is occupied, though the name is free
    #[error(
        "Cannot create `{name}`: {} is already taken by something selfie did not find under \
         that name; selfie will not write over it.",
        file_path.display()
    )]
    PackagePathOccupied { name: String, file_path: PathBuf },

    /// Package environment exists but has no install command configured
    #[error(
        "No install command defined for package `{package_name}` in environment \
         `{environment}`{}",
        required_by_clause(required_by.as_deref())
    )]
    NoInstallCommand {
        package_name: String,
        environment: String,
        package_file: PathBuf,
        /// Whether other environments have install commands (for suggestions)
        other_envs_with_install: Vec<String>,
        /// The package that needs this one, when it is a dependency or a
        /// recommend of the package being installed.
        required_by: Option<String>,
    },
}

impl PackageError {
    /// The error for a spec file that answers to `name` and could not be
    /// loaded, chosen by why: a file never opened, a name refused, or YAML that
    /// does not parse.
    #[must_use]
    pub(crate) fn unloadable(
        name: &str,
        packages_path: &Path,
        failed_file: &Path,
        cause: PackageParseError,
    ) -> Self {
        let name = name.to_string();
        let packages_path = packages_path.to_path_buf();
        let failed_file = failed_file.to_path_buf();
        // A file that was never parsed gets its own variant. `ParseError` renders
        // as "Parse error in package `ghost`", which would send the user to
        // inspect YAML in a file selfie could not open -- whether it declined to
        // open it or the open failed. A file refused for its name gets another,
        // since it was not opened and the remedy is a rename.
        //
        // Exhaustive, so a new kind has to say which of these it is here.
        match cause.kind() {
            PackageParseKind::IrregularFile { .. }
            | PackageParseKind::Refused { .. }
            | PackageParseKind::Unreadable { .. }
            | PackageParseKind::Io { .. } => PackageError::UnreadableFile {
                name,
                packages_path,
                failed_file,
                cause,
            },
            PackageParseKind::InvalidName { .. } => PackageError::UnusableName {
                name,
                packages_path,
                failed_file,
                cause,
            },
            PackageParseKind::Yaml { .. } => PackageError::ParseError {
                name,
                packages_path,
                failed_file,
                cause,
            },
        }
    }
}

pub(crate) fn required_by_clause(required_by: Option<&str>) -> String {
    required_by.map_or_else(String::new, |by| format!(", which `{by}` requires"))
}

/// Output from listing packages in the repository
///
/// Contains the results of attempting to load all packages from the repository.
/// This includes both successfully loaded packages and any parse errors that
/// occurred, allowing callers to handle partial failures gracefully. Every
/// package that failed to parse is included whatever environment is
/// selected, because a spec that failed to parse has no environment list to
/// filter on.
#[derive(Debug)]
pub struct ListPackagesOutput(pub(crate) Vec<Result<Package, PackageParseError>>);

impl ListPackagesOutput {
    /// Get the total number of packages found (both valid and invalid)
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Check if no packages were found
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Create a `ListPackagesOutput` from a list of valid packages.
    ///
    /// Useful in tests where you want to construct output without going
    /// through a real repository.
    #[cfg(any(test, feature = "with_mocks"))]
    #[must_use]
    pub fn from_packages(packages: Vec<Package>) -> Self {
        Self(packages.into_iter().map(Ok).collect())
    }

    /// Create a `ListPackagesOutput` from a mix of loaded packages and failures.
    // The field is `pub(crate)`, so without this a test outside the library can
    // only build an output in which nothing failed -- which is exactly the case
    // that cannot catch a caller dropping the failures silently.
    #[cfg(any(test, feature = "with_mocks"))]
    #[must_use]
    pub fn from_results(results: Vec<Result<Package, PackageParseError>>) -> Self {
        Self(results)
    }

    /// Get all package loading results (both successes and failures)
    ///
    /// Returns a slice containing the result of attempting to load each
    /// package file found in the repository.
    pub fn all_results(&self) -> &[Result<Package, PackageParseError>] {
        &self.0
    }

    /// Get an iterator over successfully loaded packages
    ///
    /// Filters out any packages that failed to parse and returns only
    /// the valid package definitions.
    pub fn valid_packages(&self) -> impl Iterator<Item = &Package> {
        self.0.iter().filter_map(|maybe_p| maybe_p.as_ref().ok())
    }

    /// Find a package by name among the ones that parsed.
    ///
    /// `None` covers both "no such package" and "it failed to parse".
    #[must_use]
    pub fn get(&self, package_name: &str) -> Option<&Package> {
        self.0.iter().find_map(|maybe_p| match maybe_p {
            Ok(p) => {
                if p.name() == package_name {
                    Some(p)
                } else {
                    None
                }
            }
            Err(_) => None,
        })
    }

    /// Get an iterator over packages that failed to parse
    ///
    /// Returns parse errors for packages that could not be loaded successfully.
    /// This is useful for debugging configuration issues.
    pub fn invalid_packages(&self) -> impl Iterator<Item = &PackageParseError> {
        self.0.iter().filter_map(|maybe_p| maybe_p.as_ref().err())
    }

    /// What the name table says about the listed files, asked once: the packages
    /// a command can use, the names several files claim, one refused row per
    /// loaded file of such a name, and every name a file claims.
    #[must_use]
    pub fn names(&self) -> ListingNames<'_> {
        let path_of = |result: &Result<Package, PackageParseError>| match result {
            Ok(package) => package.path().clone(),
            Err(failure) => failure.package_path().to_path_buf(),
        };
        let mut ambiguous =
            crate::package::group_by_spec_name(self.0.iter().map(path_of), |path: &PathBuf| {
                path.file_name().and_then(|name| name.to_str())
            });
        let claimed = ambiguous.keys().cloned().collect();
        ambiguous.retain(|_, files| files.len() > 1);
        let mut usable = Vec::new();
        let mut rows = Vec::new();
        for package in self.valid_packages() {
            match package
                .spec_name()
                .and_then(|name| ambiguous.get(&name).map(|files| (name, files)))
            {
                Some((name, files)) => {
                    rows.push(crate::package::event::RefusedSpec::ambiguous_file(
                        &name,
                        package.path(),
                        files,
                    ))
                }
                None => usable.push(package),
            }
        }
        ListingNames {
            usable,
            ambiguous,
            rows,
            claimed,
        }
    }
}

/// What [`ListPackagesOutput::names`] found.
#[derive(Debug)]
pub struct ListingNames<'a> {
    /// The packages that parsed and whose name no other listed file claims.
    pub usable: Vec<&'a Package>,
    /// Every name several listed files claim, loaded or not, with those files,
    /// sorted. No command uses any file of such a name.
    pub ambiguous: std::collections::BTreeMap<String, Vec<PathBuf>>,
    /// One refused row for each loaded file of an ambiguous name, naming every
    /// file the name's claim covers. A file that did not parse is left to the
    /// listing's own failure row.
    pub rows: Vec<crate::package::event::RefusedSpec>,
    /// Every name a listed file claims, loaded or not.
    pub claimed: std::collections::BTreeSet<String>,
}

/// A package file that could not be turned into a package.
///
/// Carries the file it was reading and why. No [`PackageParseKind`]'s own wording
/// names the file, so renderers put the path where their own layout wants it -- a
/// column, a label, a JSON field -- and
/// [`skipped_spec_warning`](crate::package::service::skipped_spec_warning) is the
/// one that puts it in prose.
// Split into a path and a path-free reason so that "the message names the file
// exactly once" holds for every `#[error]` attribute at once rather than for each
// of them separately. The kinds carrying free text -- `Io`, `Unreadable`,
// `Refused` -- can still hold a path at runtime, so nothing building one of them
// may re-tag a failure with the path it was reading.
#[derive(Error, Debug, Clone)]
#[error("{kind}")]
pub struct PackageParseError {
    package_path: PathBuf,
    // Interpolated and not also `#[source]`: an error that both renders its cause
    // and chains it prints the cause twice for any consumer that walks the chain.
    // The same holds for every error in this module.
    kind: PackageParseKind,
}

impl PackageParseError {
    /// The name a listing shows for the spec: its file's stem, or the whole
    /// path when the stem is not UTF-8.
    #[must_use]
    pub fn display_name(&self) -> String {
        self.package_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map_or_else(|| self.package_path.display().to_string(), str::to_string)
    }

    /// Report `kind` against the package file at `package_path`.
    #[must_use]
    pub fn new(package_path: impl Into<PathBuf>, kind: PackageParseKind) -> Self {
        Self {
            package_path: package_path.into(),
            kind,
        }
    }

    /// The package file that failed to parse.
    #[must_use]
    pub fn package_path(&self) -> &Path {
        &self.package_path
    }

    /// What went wrong, for a caller that has to tell the cases apart.
    #[must_use]
    pub fn kind(&self) -> &PackageParseKind {
        &self.kind
    }
}

/// Why a package file could not be turned into a package.
///
/// No variant names the file: the path lives on
/// [`PackageParseError`](PackageParseError::package_path), and a renderer that
/// wants it in the sentence asks for it there.
#[derive(Error, Debug, Clone)]
pub enum PackageParseKind {
    /// YAML syntax or structure error in the package file
    #[error("YAML parsing error: {cause}")]
    Yaml { cause: crate::yaml::ParseFailure },

    /// IO error occurred while reading the package file
    #[error("I/O error reading the package file: {cause}")]
    Io { cause: Arc<std::io::Error> },

    /// The package file could not be read, for a reason that is nobody's mistake
    ///
    /// Distinct from [`Refused`](Self::Refused), which is selfie declining a file
    /// it could have read or did read. Nothing here was declined; the read could
    /// not be attempted.
    #[error("the package file could not be read: {reason}")]
    Unreadable { reason: String },

    /// The package file is a fifo, socket or device node rather than a regular file
    ///
    /// Reading one blocks until a writer arrives, so it is refused before the
    /// read rather than reported after it.
    #[error(
        "the package file is a {kind}, not a regular file. Replace it with a regular file or remove it from the package directory."
    )]
    IrregularFile { kind: &'static str },

    /// selfie declined the package file: the filesystem port refused to read
    /// it, or selfie read it and the spec guard will not use what it says
    ///
    /// Carries a `reason` rather than the refusal itself.
    // Every filesystem refusal variant's own `Display` names a *target* and says
    // selfie will not **write** through it, having been written for the deploy
    // side. Rendering one here would report a write refusal on a read path.
    //
    // The filesystem half is reached only if `irregular_target_refusal` ever
    // returns something other than `IrregularTarget`, so that growth fails closed
    // with sensible wording. The spec guard's half carries a refusal such as a
    // key shadowing `environments:`, for a file that parsed.
    #[error("selfie will not use the package file: {reason}")]
    Refused { reason: String },

    /// The file's name, without its extension, is not a valid spec name
    ///
    /// Refused before the file is read, since no name the file could be asked
    /// for would be one selfie accepts everywhere else.
    #[error("its name '{name}' is not a valid spec name. Rename the file: {rule}.", rule = crate::package::SPEC_NAME_RULE)]
    InvalidName { name: String },
}

impl PackageParseKind {
    /// A bare label for this kind, for a caller that has to tell them apart.
    ///
    /// Stable and machine-readable, so a consumer can branch on the kind without
    /// reading the sentence beside it. Explanations belong in that sentence, never
    /// here.
    // `&'static str` and not `String`, so nothing can append prose to a value
    // callers treat as an enum -- which is the defect selfie-c0vk exists to
    // complain about on a neighboring event field.
    //
    // Matched here rather than in the adapter, and with no catch-all, so a new
    // kind is a compile error beside the enum -- where whoever adds one is already
    // looking -- rather than in a crate they may never open.
    #[must_use]
    pub fn label(&self) -> &'static str {
        // Adding a kind also means editing the MCP tool descriptions in
        // `selfie-mcp`, which spell these labels out for callers. Nothing makes
        // that a compile error, so it is named here instead.
        match self {
            Self::Yaml { .. } => "yaml",
            Self::Io { .. } => "io",
            Self::Unreadable { .. } => "unreadable",
            Self::IrregularFile { .. } => "irregular_file",
            Self::Refused { .. } => "refused",
            Self::InvalidName { .. } => "invalid_name",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use mockall::predicate::*;

    fn listing(state: DirectoryState) -> PackageRepoError {
        PackageRepoError::from(PackageListError::new("/packages".into(), state))
    }

    fn package_error(error: PackageError) -> PackageRepoError {
        PackageRepoError::from(error)
    }

    fn unreadable(name: &str, kind: PackageParseKind) -> (PathBuf, PackageParseError) {
        let file = PathBuf::from(format!("/packages/{name}.yml"));
        (file.clone(), PackageParseError::new(file, kind))
    }

    // Every error a lookup returns, each with the kind it is classified as.
    fn lookup_errors() -> Vec<(PackageRepoError, LookupKind)> {
        let source = crate::yaml::parse::<crate::package::Package>("name: dep\nx: {oops\n")
            .expect_err("the fixture must not parse");
        let (file, parse) = unreadable("dep", PackageParseKind::Yaml { cause: source });
        let (unread_file, unread) = unreadable(
            "dep",
            PackageParseKind::Unreadable {
                reason: "a fifo".to_string(),
            },
        );
        let (name_file, bad_name) = unreadable(
            "Dep",
            PackageParseKind::Unreadable {
                reason: "a capital".to_string(),
            },
        );
        vec![
            (
                package_error(PackageError::PackageNotFound {
                    name: "dep".to_string(),
                    packages_path: "/packages".into(),
                    files_examined: 0,
                    search_patterns: Vec::new(),
                    declared_by: Vec::new(),
                }),
                LookupKind::NotFound,
            ),
            (
                listing(DirectoryState::Absent(crate::fs::AbsentReason::Empty)),
                LookupKind::NotFound,
            ),
            (
                package_error(PackageError::MultiplePackagesFound {
                    name: "twice".to_string(),
                    packages_path: "/packages".into(),
                    conflicting_paths: vec![
                        "/packages/twice.yaml".into(),
                        "/packages/twice.yml".into(),
                    ],
                    files_examined: 2,
                    search_patterns: Vec::new(),
                }),
                LookupKind::AmbiguousName,
            ),
            (
                package_error(PackageError::ParseError {
                    name: "dep".to_string(),
                    packages_path: "/packages".into(),
                    failed_file: file,
                    cause: parse,
                }),
                LookupKind::Unloadable,
            ),
            (
                package_error(PackageError::UnreadableFile {
                    name: "dep".to_string(),
                    packages_path: "/packages".into(),
                    failed_file: unread_file,
                    cause: unread,
                }),
                LookupKind::Unloadable,
            ),
            (
                package_error(PackageError::UnusableName {
                    name: "Dep".to_string(),
                    packages_path: "/packages".into(),
                    failed_file: name_file,
                    cause: bad_name,
                }),
                LookupKind::Unloadable,
            ),
            (
                listing(DirectoryState::Unlistable(Arc::new(std::io::Error::from(
                    std::io::ErrorKind::PermissionDenied,
                )))),
                LookupKind::PackageDirectoryUnreadable,
            ),
        ]
    }

    // Each error a lookup returns is classified by what it is about, its failure
    // takes that kind's label, and it reads as the error's own sentence.
    #[test]
    fn a_failed_lookup_is_classified_by_what_it_is_about() {
        for (error, kind) in lookup_errors() {
            assert_eq!(error.lookup_kind(), Some(kind), "{error:?}");
            assert_eq!(
                error.means_no_such_package(),
                kind == LookupKind::NotFound,
                "{error:?}"
            );
            assert_eq!(
                error.names_an_unusable_spec(),
                matches!(kind, LookupKind::AmbiguousName | LookupKind::Unloadable),
                "{error:?}"
            );
            let sentence = error.to_string();
            let failure = error
                .into_lookup_failure()
                .expect("a lookup returns this error");
            assert_eq!(failure.to_string(), sentence);
            let label = match kind {
                LookupKind::NotFound => "not_found",
                LookupKind::AmbiguousName => "ambiguous_name",
                LookupKind::Unloadable => "unloadable",
                LookupKind::PackageDirectoryUnreadable => "package_directory_unreadable",
            };
            assert_eq!(failure.kind(), label);
        }
    }

    // Each lookup failure carries the facts its kind needs: the files claiming an
    // ambiguous name, and an unloadable spec's parse failure.
    #[test]
    fn a_lookup_failure_carries_its_facts() {
        for (error, _) in lookup_errors() {
            match error
                .into_lookup_failure()
                .expect("a lookup returns this error")
            {
                LookupFailure::AmbiguousName(name) => {
                    assert_eq!(name.conflicting_paths().len(), 2);
                }
                LookupFailure::Unloadable(spec) => {
                    assert!(
                        spec.failure().package_path().starts_with("/packages"),
                        "{spec:?}"
                    );
                }
                LookupFailure::NotFound(_) | LookupFailure::PackageDirectoryUnreadable(_) => {}
            }
        }
    }

    // An error a lookup never returns has no lookup kind and comes back
    // unchanged, so a caller reports it as itself.
    #[test]
    fn an_error_no_lookup_returns_comes_back_unchanged() {
        let errors = [
            package_error(PackageError::NoCheckCommand {
                package_name: "dep".to_string(),
                environment: "test".to_string(),
                package_file: "/packages/dep.yml".into(),
                other_envs_with_check: Vec::new(),
            }),
            PackageRepoError::IoError(Arc::new(std::io::Error::other("save"))),
            PackageRepoError::Unserializable(crate::yaml::SerializeFailure::Value),
            package_error(PackageError::PackageAlreadyExists {
                name: "dep".to_string(),
                file_path: "/packages/dep.yml".into(),
            }),
        ];
        for error in errors {
            assert_eq!(error.lookup_kind(), None, "{error:?}");
            assert!(!error.means_no_such_package());
            assert!(!error.names_an_unusable_spec());
            let sentence = error.to_string();
            let returned = error
                .into_lookup_failure()
                .expect_err("a lookup does not return this error");
            assert_eq!(returned.to_string(), sentence);
        }
        assert!(matches!(
            package_error(PackageError::NoCheckCommand {
                package_name: "dep".to_string(),
                environment: "test".to_string(),
                package_file: "/packages/dep.yml".into(),
                other_envs_with_check: Vec::new(),
            })
            .into_lookup_failure(),
            Err(PackageRepoError::PackageError(e)) if matches!(*e, PackageError::NoCheckCommand { .. })
        ));
    }

    // An error that names its cause in its message does not also chain it, so
    // a consumer walking the chain prints each cause once.
    #[test]
    fn every_package_error_names_its_cause_once() {
        let parse = || {
            PackageParseError::new(
                "/p/bat.yml",
                PackageParseKind::Unreadable {
                    reason: "the cause".to_string(),
                },
            )
        };
        let errors: Vec<Box<dyn std::error::Error>> = vec![
            Box::new(PackageError::ParseError {
                name: "bat".to_string(),
                packages_path: "/p".into(),
                failed_file: "/p/bat.yml".into(),
                cause: parse(),
            }),
            Box::new(PackageError::UnreadableFile {
                name: "bat".to_string(),
                packages_path: "/p".into(),
                failed_file: "/p/bat.yml".into(),
                cause: parse(),
            }),
            Box::new(PackageError::UnusableName {
                name: "bat".to_string(),
                packages_path: "/p".into(),
                failed_file: "/p/bat.yml".into(),
                cause: parse(),
            }),
            Box::new(parse()),
            Box::new(PackageRepoError::IoError(Arc::new(std::io::Error::other(
                "the cause",
            )))),
            Box::new(PackageRepoError::FileSystemError(FileSystemError::IoError(
                Arc::new(std::io::Error::other("the cause")),
            ))),
        ];
        for error in errors {
            let message = error.to_string();
            assert_eq!(message.matches("the cause").count(), 1, "{message}");
            assert!(error.source().is_none(), "{message} also chains its cause");
        }
    }

    #[test]
    fn test_mock_find_dependent_packages() {
        // Test that the new find_dependent_packages method can be mocked
        let mut mock_repo = MockPackageRepository::new();

        // Set up expectations
        mock_repo
            .expect_find_dependent_packages()
            .with(eq("target-package"))
            .times(1)
            .returning(|_| {
                use crate::package::Package;

                use std::path::PathBuf;

                let mut env1 = crate::package::Environments::new();
                env1.insert(
                    "test".to_string(),
                    crate::package::EnvironmentConfig::new(
                        "echo install".to_string(),
                        None,
                        None,
                        vec!["target-package".to_string()],
                        Vec::new(),
                    ),
                );

                let mut env2 = crate::package::Environments::new();
                env2.insert(
                    "test".to_string(),
                    crate::package::EnvironmentConfig::new(
                        "echo install".to_string(),
                        None,
                        None,
                        vec!["target-package".to_string()],
                        Vec::new(),
                    ),
                );

                Ok((
                    vec![
                        Package::new(
                            "dependent1".to_string(),
                            None,
                            None,
                            Vec::new(),
                            None,
                            env1,
                            PathBuf::from("/test/dependent1.yml"),
                        ),
                        Package::new(
                            "dependent2".to_string(),
                            None,
                            None,
                            Vec::new(),
                            None,
                            env2,
                            PathBuf::from("/test/dependent2.yml"),
                        ),
                    ],
                    Vec::new(),
                ))
            });

        // Call the mocked method
        let result = mock_repo.find_dependent_packages("target-package");

        // Verify the result
        assert!(result.is_ok());
        let (dependents, unreadable) = result.unwrap();
        assert_eq!(dependents.len(), 2);
        assert!(unreadable.is_empty());

        let names: Vec<String> = dependents.iter().map(|p| p.name().to_string()).collect();
        assert!(names.contains(&"dependent1".to_string()));
        assert!(names.contains(&"dependent2".to_string()));
    }

    #[test]
    fn test_mock_find_dependent_packages_empty() {
        // Test mocking when no dependents are found
        let mut mock_repo = MockPackageRepository::new();

        mock_repo
            .expect_find_dependent_packages()
            .with(eq("standalone-package"))
            .times(1)
            .returning(|_| Ok((vec![], Vec::new())));

        let result = mock_repo.find_dependent_packages("standalone-package");

        assert!(result.is_ok());
        let (dependents, unreadable) = result.unwrap();
        assert!(dependents.is_empty());
        assert!(unreadable.is_empty());
    }

    #[test]
    fn test_mock_find_dependent_packages_error() {
        // Test mocking error conditions
        let mut mock_repo = MockPackageRepository::new();

        mock_repo
            .expect_find_dependent_packages()
            .with(eq("error-package"))
            .times(1)
            .returning(|_| {
                Err(PackageRepoError::PackageListError(PackageListError::new(
                    PathBuf::from("/nonexistent"),
                    DirectoryState::Absent(crate::fs::AbsentReason::Empty),
                )))
            });

        let result = mock_repo.find_dependent_packages("error-package");

        assert!(result.is_err());
        match result.unwrap_err() {
            PackageRepoError::PackageListError(listing) if listing.is_absent() => {
                // Expected error type
            }
            other => panic!("expected an absent package directory, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod may_be_created_tests {
    use super::*;
    use crate::fs::AbsentReason;

    // Only an empty path may be created. The other absent reasons hold no specs,
    // which `is_absent` still answers, but a directory cannot be created through
    // any of them.
    #[test]
    fn only_an_empty_path_may_be_created() {
        let error = |state| PackageListError::new(PathBuf::from("/packages"), state);

        assert!(error(DirectoryState::Absent(AbsentReason::Empty)).may_be_created());
        for reason in [
            AbsentReason::Occupied {
                kind: "regular file",
            },
            AbsentReason::DanglingSymlink { points_to: None },
            AbsentReason::ParentNotADirectory {
                parent: PathBuf::from("/file"),
            },
        ] {
            let listing = error(DirectoryState::Absent(reason));
            assert!(listing.is_absent(), "{listing:?}");
            assert!(!listing.may_be_created(), "{listing:?}");
            assert!(
                !PackageRepoError::PackageListError(listing).means_no_such_package(),
                "a path that cannot be created is not \"no such package\""
            );
        }
        assert!(!error(DirectoryState::Directory).may_be_created());
    }
}

#[cfg(test)]
mod conflicting_path_tests {
    use super::*;

    // The conflicting file names are the only part of this error a user can act
    // on, and they were captured but never rendered. Asserting the rendered
    // string is what keeps them there.
    #[test]
    fn the_ambiguity_error_names_the_files_and_the_remedy() {
        let rendered = PackageError::MultiplePackagesFound {
            name: "neovim".to_string(),
            packages_path: PathBuf::from("/packages"),
            conflicting_paths: vec![
                PathBuf::from("/packages/Neovim.yml"),
                PathBuf::from("/packages/neovim.yml"),
            ],
            files_examined: 2,
            search_patterns: vec![],
        }
        .to_string();

        assert!(rendered.contains("Neovim.yml"), "got: {rendered}");
        assert!(rendered.contains("neovim.yml"), "got: {rendered}");
        assert!(rendered.contains("Rename"), "no remedy: {rendered}");
        // The directory is stated once; repeating it per file hides the
        // difference between them, which is the whole point of listing them.
        assert_eq!(rendered.matches("/packages/").count(), 0, "got: {rendered}");
    }

    // One error covers two different collisions: names that differ only by
    // case, and `.yml` against `.yaml`. Wording that blames only capitalization
    // sends the reader of this pair looking for a difference that is not there.
    #[test]
    fn the_ambiguity_error_covers_an_extension_collision_too() {
        let rendered = PackageError::MultiplePackagesFound {
            name: "neovim".to_string(),
            packages_path: PathBuf::from("/packages"),
            conflicting_paths: vec![
                PathBuf::from("/packages/neovim.yaml"),
                PathBuf::from("/packages/neovim.yml"),
            ],
            files_examined: 2,
            search_patterns: vec![],
        }
        .to_string();

        assert!(rendered.contains("neovim.yaml"), "got: {rendered}");
        assert!(rendered.contains("neovim.yml"), "got: {rendered}");
        assert!(rendered.contains("`.yml` and `.yaml`"), "got: {rendered}");
    }
}
