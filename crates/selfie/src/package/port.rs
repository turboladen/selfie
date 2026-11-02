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

use crate::{
    fs::{DirectoryState, FileSystem, filesystem::FileSystemError},
    package::{GetPackage, Package},
    parts::{Composed, Message, Parts},
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

    /// Whether anything already occupies `path`, including a symlink whether or
    /// not it resolves.
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

    /// Serialize a package and write it back to `path`, the file it was loaded
    /// from, if that file still holds what was loaded.
    ///
    /// For rewriting a loaded spec. A change made to the file since the load is
    /// left as it is, and the package is not written. Where the storage cannot
    /// compare and replace atomically, a change made at the same moment as the
    /// write may still be replaced. A new spec goes through
    /// [`Self::create_package`], which leaves what it finds, within the guarantee
    /// it states.
    ///
    /// # Errors
    ///
    /// [`PackageRepoError::ChangedSinceLoad`] if the file at `path` no longer
    /// holds what the package was loaded from, including when it is gone or a
    /// fifo, socket or device node has taken its place.
    /// [`PackageRepoError::UnwritablePath`] if a symlink is at `path`, as for any
    /// spec write. [`PackageRepoError::NotLoaded`]
    /// if the package was not loaded from `path`. Otherwise [`PackageRepoError`]
    /// if the package cannot be serialized to YAML, or if the file cannot be read
    /// back or written.
    fn save_package(&self, package: &Package, path: &Path) -> Result<(), PackageRepoError>;

    /// Serialize a new package and write it to `path`, where nothing is.
    ///
    /// Whatever is at `path` when the write lands is left as it is, and the
    /// package is not written. Where the storage cannot refuse an existing name
    /// atomically, something appearing in a short window just before the write
    /// may still be replaced.
    ///
    /// # Errors
    ///
    /// [`PackageRepoError::Occupied`] if a file or directory is at `path`. A
    /// symlink, fifo, socket or device node there is refused as
    /// [`PackageRepoError::UnwritablePath`]. A package carrying keys selfie does
    /// not model is refused as [`Self::save_package`] refuses it, in the same
    /// words, which speak of a rewrite. Otherwise as [`Self::save_package`].
    fn create_package(&self, package: &Package, path: &Path) -> Result<(), PackageRepoError>;

    /// Delete the definition file `package` was loaded from, if it still holds
    /// what was loaded. Irreversible, so confirm with the user first, and pass
    /// the package the confirmation described.
    ///
    /// A change made to the file since the load is left as it is, and nothing is
    /// removed. A change made at the same instant as the removal may still be
    /// deleted. A symlink is judged by the file it names, and only the link is
    /// removed.
    ///
    /// # Errors
    ///
    /// Returns [`PackageRepoError::ChangedSinceLoad`] if the file no longer holds
    /// what the package was loaded from, including when it is gone or something
    /// other than a regular file has taken its place. Returns
    /// [`PackageRepoError::NotLoaded`] if this repository did not load the
    /// package from a file in its own directory. Returns another
    /// [`PackageRepoError`], naming the file, if it cannot be read back or the
    /// deletion fails.
    fn remove_package(&self, package: &Package) -> Result<(), PackageRepoError>;

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

// A repository listing no spec, for a run that compares against the others and has
// none to compare.
#[cfg(test)]
impl MockPackageRepository {
    pub(crate) fn listing_nothing() -> Self {
        let mut repo = Self::new();
        repo.expect_list_packages()
            .returning(|| Ok(ListPackagesOutput(Vec::new())));
        repo.expect_directory_identity().return_const(None);
        repo
    }
}

/// Errors that can occur during package repository operations
///
/// This enum represents all possible failures when interacting with the
/// package repository, providing detailed context for debugging and
/// error handling.
#[derive(Debug, Clone)]
pub enum PackageRepoError {
    /// Package-specific error (not found, parse error, etc.)
    PackageError(Box<PackageError>),

    /// Package listing operation failed
    PackageListError(PackageListError),

    /// IO error during repository operation
    IoError(Arc<std::io::Error>),

    /// File system error during repository operation
    FileSystemError(FileSystemError),

    /// The package could not be written as YAML, so nothing was saved.
    Unserializable(crate::yaml::SerializeFailure),

    /// Refused to rewrite a package file because it carries keys the struct does
    /// not model, or because its top level could not be read back to tell.
    ///
    /// Saving re-serializes from the struct, so every such key would be dropped,
    /// with whatever it holds. The message names every level in `levels`, so the
    /// file is fixed in one pass.
    // The remedy names editing the file rather than a selfie command on purpose:
    // every command that could rewrite the file is refused by this same guard, so
    // pointing at one would send the user back to a tool that just refused them.
    UntrustedKeys {
        /// The file that would have been rewritten.
        path: PathBuf,
        /// What makes the file unsafe to rewrite, in report order.
        levels: UntrustedLevels,
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
    UnwritablePath {
        /// The package file selfie declined to write.
        path: PathBuf,
        /// What was found there.
        reason: UnwritableReason,
    },

    /// Refused to create a package file because something is already at its
    /// path. What is there is left as it is.
    // Never says "target", for the reason `UnwritablePath` gives. Worded for what
    // reaches it: every caller finds the path free first, so the occupant arrived
    // during the write, and whether it is a spec for the same name is unknown. So
    // it gives no remedy beyond looking.
    Occupied {
        /// The package file selfie declined to create.
        path: PathBuf,
    },

    /// Refused to rewrite or remove a spec because the file at its path no
    /// longer holds what it was loaded from. Nothing was written or removed.
    // Covers a removed file, and anything other than a regular file that takes
    // its place: none is what was loaded. The remedy is worded for all of them,
    // since a run that reads the file again either acts on what is there or
    // reports why it cannot. A read failure that says nothing about the file is
    // passed through as itself rather than called a change.
    ChangedSinceLoad {
        /// The spec selfie declined to change.
        path: PathBuf,
        /// What selfie declined to do to it.
        refused: RefusedChange,
    },

    /// Refused to rewrite or remove a spec this repository did not load from the
    /// path it would change, so there is nothing to tell a change by. Nothing was
    /// written or removed.
    NotLoaded {
        /// The path selfie declined to change.
        path: PathBuf,
    },

    /// Refused to remove a spec because another file now answers to its name, so
    /// removing the loaded one would not remove the package. Nothing was removed.
    AnotherFileAnswers {
        /// The spec selfie loaded and declined to remove.
        path: PathBuf,
        /// The file the name resolves to now.
        answering: PathBuf,
    },
}

crate::parts::display_from_parts!(PackageRepoError);

// A package or listing error is this error, so its own `source()` is this one's.
// No other variant chains its cause: each writes the cause into its sentence, and
// a consumer walking the chain would print it twice.
impl std::error::Error for PackageRepoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::PackageError(error) => error.source(),
            Self::PackageListError(error) => error.source(),
            Self::IoError(_)
            | Self::FileSystemError(_)
            | Self::Unserializable(_)
            | Self::UntrustedKeys { .. }
            | Self::UnwritablePath { .. }
            | Self::Occupied { .. }
            | Self::ChangedSinceLoad { .. }
            | Self::NotLoaded { .. }
            | Self::AnotherFileAnswers { .. } => None,
        }
    }
}

impl Composed for PackageRepoError {
    fn parts(&self) -> Parts<'_> {
        match self {
            Self::PackageError(error) => error.parts(),
            Self::PackageListError(error) => error.parts(),
            Self::IoError(error) => Parts::new().text("IO error: ").cause(&**error),
            Self::FileSystemError(error) => Parts::new()
                .text("File system error: ")
                .append(error.parts()),
            Self::Unserializable(failure) => Parts::new()
                .text("The package cannot be written as YAML: ")
                .cause(failure),
            Self::UntrustedKeys { path, levels } => untrusted_keys_parts(path, levels.levels()),
            Self::UnwritablePath { path, reason } => Parts::new()
                .text("refusing to write ")
                .path(path)
                .text(": ")
                .append(reason.parts())
                .text(". Remove what is at that path, or choose another name."),
            Self::Occupied { path } => Parts::new().text("something appeared at ").path(path).text(
                " while selfie was writing the spec there; nothing was written over it. Run \
                     the command again to see what is there.",
            ),
            Self::ChangedSinceLoad { path, refused } => Parts::new().path(path).text(format!(
                " changed after selfie read it, so {refused}. Run the command again, which \
                     reads the file as it is now."
            )),
            Self::NotLoaded { path } => Parts::new()
                .text("refusing to change ")
                .path(path)
                .text(": this repository did not load the spec from it"),
            Self::AnotherFileAnswers { path, answering } => Parts::new()
                .path(answering)
                .text(" now answers to the name of ")
                .path(path)
                .text(
                    ", which selfie read, so nothing was removed. Run the command again, which \
                     reads the spec as it is now.",
                ),
        }
    }
}

/// What [`PackageRepoError::UnwritablePath`] found at a package file's path.
#[derive(Debug, Clone)]
pub enum UnwritableReason {
    /// A symlink.
    Symlink(SymlinkReason),
    /// A fifo, socket or device node.
    Irregular(crate::fs::IrregularKind),
}

crate::parts::display_from_parts!(UnwritableReason);

impl Composed for UnwritableReason {
    fn parts(&self) -> Parts<'_> {
        match self {
            Self::Symlink(reason) => reason.parts(),
            Self::Irregular(kind) => Parts::new().text(format!("it is a {}", kind.label())),
        }
    }
}

/// What [`PackageRepoError::ChangedSinceLoad`] declined to do to a spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusedChange {
    /// Rewrite it.
    Write,
    /// Remove it.
    Removal,
}

impl std::fmt::Display for RefusedChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Write => "nothing was written to it",
            Self::Removal => "it was not removed",
        })
    }
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

impl From<PackageListError> for PackageRepoError {
    fn from(error: PackageListError) -> Self {
        Self::PackageListError(error)
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
            // Raised by `save_package`, `create_package` or `remove_package`, never by
            // a lookup.
            Self::IoError(_)
            | Self::Unserializable(_)
            | Self::UntrustedKeys { .. }
            | Self::UnwritablePath { .. }
            | Self::Occupied { .. }
            | Self::ChangedSinceLoad { .. }
            | Self::AnotherFileAnswers { .. }
            | Self::NotLoaded { .. }
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

crate::parts::display_from_parts!(LookupFailure);

// A failed lookup is the error it holds.
impl Composed for LookupFailure {
    fn parts(&self) -> Parts<'_> {
        match self {
            Self::NotFound(NotFound(error))
            | Self::AmbiguousName(AmbiguousName { error, .. })
            | Self::Unloadable(UnloadableSpec { error, .. })
            | Self::PackageDirectoryUnreadable(UnreadableDirectory(error)) => error.parts(),
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
#[derive(Debug, Clone)]
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
}

// The path, then why the listing produced nothing, in the words the directory's
// state gives it, so a listing failure and a directory warning about one path
// describe it in the same words.
crate::parts::display_from_parts!(error PackageListError);

impl Composed for PackageListError {
    fn parts(&self) -> Parts<'_> {
        let parts = Parts::new().path(&self.path).text(" ");
        match &self.state {
            // A path the port calls a directory whose listing still failed. The
            // listing is the more recent answer, so it may be hiding entries.
            DirectoryState::Directory => parts.text("could not be listed"),
            state => parts.append(state.parts()),
        }
    }
}

/// Why several spec files claiming one name are refused, and what to do about
/// it, as parts in which the name and each file's name are names and the
/// directory is a path.
///
/// The one wording, for install and for apply and drift alike. `directory` is
/// where the files are, and `paths` the files.
pub(crate) fn ambiguous_name_parts<'a>(
    name: &'a str,
    directory: &'a Path,
    paths: &'a [PathBuf],
) -> Parts<'a> {
    // File names alone: the directory is already named, so repeating the full
    // path for every conflict buries the part that differs.
    let files = Parts::joined(paths, ", ", |path| {
        Parts::new().name(
            path.file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy(),
        )
    });
    Parts::new()
        .text("Multiple packages found with name `")
        .name(name)
        .text("` in path ")
        .path(directory)
        .text(": ")
        .append(files)
        .text(
            ". Names are compared ignoring case, and `.yml` and `.yaml` name the same package, \
             so these files all claim one name. Rename or remove all but one.",
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
#[must_use]
pub fn name_field_hint(declared: &[NameDeclaration]) -> Message {
    name_field_hint_parts(declared).into()
}

// This builds `name_field_hint`'s parts, borrowing each value from `declared`.
pub(crate) fn name_field_hint_parts(declared: &[NameDeclaration]) -> Parts<'_> {
    if declared.is_empty() {
        return Parts::new();
    }
    // The file by its directory and name, as `packages/nvim.yml`, so a reader
    // with both directories can tell which one holds it.
    let file = |d: &NameDeclaration| {
        let name = d.path.file_name().map(Path::new).unwrap_or(&d.path);
        d.path
            .parent()
            .and_then(Path::file_name)
            .map_or_else(|| name.to_path_buf(), |dir| Path::new(dir).join(name))
    };
    let claims = Parts::joined(declared, " and ", |d| {
        Parts::new()
            .path(file(d))
            .text(" declares `name: ")
            .name(&d.name)
            .text("`")
    });
    // Each name appears once without deduplicating: two files claiming one name
    // are ambiguous, and an ambiguous name is never collected or looked up.
    let names: Vec<String> = declared
        .iter()
        .filter_map(|d| crate::package::spec_name_of(&d.path))
        .collect();
    let names = Parts::joined(names, " or ", |name| {
        Parts::new().text("'").name(name).text("'")
    });
    Parts::new()
        .text(". ")
        .append(claims)
        .text("; a package is named by its file, so use ")
        .append(names)
}

/// One reason a package file cannot be rewritten, as
/// [`PackageRepoError::UntrustedKeys`] reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UntrustedLevel {
    /// Dotfile entries carry these keys, e.g. `dotfiles[0].var`. Such a key is
    /// what makes apply refuse the entry, so a rewrite dropping it would turn an
    /// entry apply skips into one it deploys.
    DotfileFields {
        /// The offending field paths.
        fields: Vec<String>,
        /// How many distinct entries carry them.
        entries: usize,
    },
    /// The top level carries these keys, e.g. `_dotfiles` or `configs`. A
    /// rewrite would take every entry under the key with it.
    TopLevelFields(Vec<String>),
    /// Environments carry these keys, e.g. `environments.work.audt`. A rewrite
    /// would delete what the user set the key to, such as a command.
    EnvironmentFields(Vec<String>),
    /// The top level could not be read back, carrying why, so whether it holds
    /// a key the struct would drop is unknown.
    UncheckedTopLevel(crate::yaml::ParseFailure),
}

/// The levels a [`PackageRepoError::UntrustedKeys`] refusal names: never
/// empty, in report order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UntrustedLevels(Vec<UntrustedLevel>);

impl UntrustedLevels {
    // `None` for no levels, so a refusal always has one to name.
    pub(crate) fn new(levels: Vec<UntrustedLevel>) -> Option<Self> {
        (!levels.is_empty()).then_some(Self(levels))
    }

    /// Every level, in report order.
    #[must_use]
    pub fn levels(&self) -> &[UntrustedLevel] {
        &self.0
    }
}

impl UntrustedLevel {
    // The level a rewrite refuses for `reason`, or `None` for a reason that is
    // not about keys a rewrite would drop.
    pub(crate) fn of(reason: &crate::package::RefusalReason) -> Option<Self> {
        use crate::package::RefusalReason;
        match reason {
            RefusalReason::UnknownTopLevelKeys(_) => Some(Self::TopLevelFields(reason.fields())),
            RefusalReason::UnknownEnvironmentKeys(_) => {
                Some(Self::EnvironmentFields(reason.fields()))
            }
            RefusalReason::UncheckedTopLevel(error) => Some(Self::UncheckedTopLevel(error.clone())),
            // A missing environment is a deployment rule, not a writing one: a
            // spec declaring none is still the user's text. Named, with no
            // catch-all, so a new `RefusalReason` variant fails to compile here.
            // `untrusted_keys_refusal` never returns this one, which
            // `untrusted_keys_refusal_never_names_a_missing_environment` holds;
            // the listings' rule that does is `every_environment_refusal`.
            RefusalReason::NoEnvironments(_) => None,
        }
    }

    // The field paths this level names; none for an unread top level.
    fn fields(&self) -> &[String] {
        match self {
            Self::DotfileFields { fields, .. }
            | Self::TopLevelFields(fields)
            | Self::EnvironmentFields(fields) => fields,
            Self::UncheckedTopLevel(_) => &[],
        }
    }

    // What this level holds and what a rewrite would cost, as a clause, each noun
    // agreeing with its own count, and each key's field path, which the file
    // spells, as a name. `after_keys` says a level naming keys comes before this
    // one in the message.
    fn clause(&self, after_keys: bool) -> Parts<'_> {
        let fields = self.fields();
        let several = fields.len() > 1;
        let harm = match self {
            Self::DotfileFields { entries, .. } if *entries > 1 => {
                "silently make the entries deployable"
            }
            Self::DotfileFields { .. } => "silently make the entry deployable",
            Self::TopLevelFields(_) if several => "everything under them",
            Self::TopLevelFields(_) => "everything under it",
            Self::EnvironmentFields(_) if several => "whatever they were set to",
            Self::EnvironmentFields(_) => "whatever it was set to",
            Self::UncheckedTopLevel(_) => {
                let other = if after_keys { "other " } else { "" };
                return Parts::new().text(format!(
                    "its top level could not be read back, so any {other}key selfie does not \
                     model would be dropped without warning"
                ));
            }
        };
        let key = if several { "keys" } else { "key" };
        Parts::new()
            .text("unrecognized ")
            .append(Parts::joined(fields, ", ", |field| {
                Parts::new().name(field)
            }))
            .text(format!(". Saving would delete the {key} and {harm}"))
    }
}

// The refusal names the file once and gives one remedy, after a clause per
// level. An unread top level comes last, as `untrusted_keys_refusal` orders
// it, and its parse failure ends the message: it is the parser's own words and
// where it stopped, the part a reader can act on last, and anything after it
// would read as part of it. Nothing is known about an unread top level, and a
// rewrite serializes from the struct regardless, so declining costs the user a
// retry where writing would delete silently.
fn untrusted_keys_parts<'a>(path: &'a Path, levels: &'a [UntrustedLevel]) -> Parts<'a> {
    let fields: usize = levels.iter().map(|level| level.fields().len()).sum();
    let keys = if fields > 1 { "keys" } else { "key" };
    let edit = Parts::new().text("Edit ").path(path);
    let remedy = match levels {
        [UntrustedLevel::UncheckedTopLevel(_)] => {
            edit.text(" directly, or simplify it until selfie can read it.")
        }
        [_, .., UntrustedLevel::UncheckedTopLevel(_)] => edit.text(format!(
            " directly to correct or remove the {keys}, and simplify it until selfie can read \
             it."
        )),
        _ => edit.text(format!(" directly to correct or remove the {keys}.")),
    };
    let failure = match levels.last() {
        Some(UntrustedLevel::UncheckedTopLevel(error)) => {
            Parts::new().text(" The read failed with: ").cause(error)
        }
        _ => Parts::new(),
    };
    let clauses = levels
        .iter()
        .enumerate()
        .map(|(i, level)| level.clause(i > 0));
    Parts::new()
        .text("refusing to rewrite ")
        .path(path)
        .text(": ")
        .append(super::join_sentence_parts(clauses))
        .text(". ")
        .append(remedy)
        .append(failure)
}

/// The ambiguity sentence for files that all sit in one directory, named
/// by the first file's.
pub(crate) fn ambiguous_files_parts<'a>(name: &'a str, paths: &'a [PathBuf]) -> Parts<'a> {
    let directory = paths
        .first()
        .and_then(|path| path.parent())
        .unwrap_or_else(|| Path::new("."));
    ambiguous_name_parts(name, directory, paths)
}

/// Errors that can occur with specific package operations
///
/// Represents detailed failures when working with individual packages,
/// providing rich context for debugging and user-friendly error messages.
#[derive(Debug, Clone)]
pub enum PackageError {
    /// No package with the specified name could be found
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
    UnusableName {
        name: String,
        packages_path: PathBuf,
        /// The file whose name is refused
        failed_file: PathBuf,
        cause: PackageParseError,
    },

    /// The requested environment is not configured for this package
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
    NoCheckCommand {
        package_name: String,
        environment: String,
        package_file: PathBuf,
        /// Whether other environments have check commands (for suggestions)
        other_envs_with_check: Vec<String>,
    },

    /// A package with the specified name already exists
    PackageAlreadyExists { name: String, file_path: PathBuf },

    /// The write path for a new package is occupied, though the name is free
    PackagePathOccupied { name: String, file_path: PathBuf },

    /// Package environment exists but has no install command configured
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

crate::parts::display_from_parts!(error PackageError);

impl Composed for PackageError {
    fn parts(&self) -> Parts<'_> {
        match self {
            Self::PackageNotFound {
                name,
                packages_path,
                declared_by,
                ..
            } => Parts::new()
                .text("Package `")
                .name(name)
                .text("` not found in path ")
                .path(packages_path)
                .append(name_field_hint_parts(declared_by)),
            Self::MultiplePackagesFound {
                name,
                packages_path,
                conflicting_paths,
                ..
            } => ambiguous_name_parts(name, packages_path, conflicting_paths),
            Self::ParseError {
                name,
                failed_file,
                cause,
                ..
            } => unloadable_parts("Parse error in ", name, failed_file, cause),
            Self::UnreadableFile {
                name,
                failed_file,
                cause,
                ..
            } => unloadable_parts("Cannot read ", name, failed_file, cause),
            Self::UnusableName {
                name,
                failed_file,
                cause,
                ..
            } => unloadable_parts("Cannot use ", name, failed_file, cause),
            Self::EnvironmentNotFound {
                package_name,
                environment,
                required_by,
                ..
            } => Parts::new()
                .text("Environment `")
                .name(environment)
                .text("` not found in ")
                .append(package_named(package_name))
                .append(required_by_parts(required_by.as_deref())),
            Self::NoCheckCommand {
                package_name,
                environment,
                ..
            } => Parts::new()
                .text("No check command defined for ")
                .append(package_named(package_name))
                .text(" in environment `")
                .name(environment)
                .text("`"),
            Self::PackageAlreadyExists { name, file_path } => Parts::new()
                .text("Package `")
                .name(name)
                .text("` already exists at ")
                .path(file_path),
            Self::PackagePathOccupied { name, file_path } => Parts::new()
                .text("Cannot create `")
                .name(name)
                .text("`: ")
                .path(file_path)
                .text(
                    " is already taken by something selfie did not find under that name; selfie \
                     will not write over it.",
                ),
            Self::NoInstallCommand {
                package_name,
                environment,
                required_by,
                ..
            } => Parts::new()
                .text("No install command defined for ")
                .append(package_named(package_name))
                .text(" in environment `")
                .name(environment)
                .text("`")
                .append(required_by_parts(required_by.as_deref())),
        }
    }
}

// These word the pieces a package error's sentence shares. `unloadable_parts`
// words a spec that could not be loaded; `PackageError::unloadable`, below,
// chooses which variant that is.

// This words the spec a sentence is about as `package `<name>``.
fn package_named(name: &str) -> Parts<'_> {
    Parts::new().text("package `").name(name).text("`")
}

// This words a spec file that answers to `name` and could not be loaded, as
// `<prefix>package `<name>` from <file>: <cause>`.
fn unloadable_parts<'a>(
    prefix: &'a str,
    name: &'a str,
    failed_file: &'a Path,
    cause: &'a PackageParseError,
) -> Parts<'a> {
    Parts::new()
        .text(prefix)
        .append(package_named(name))
        .text(" from ")
        .path(failed_file)
        .text(": ")
        .append(cause.parts())
}

// This words the package that needs this one, when one does, with it as a name.
fn required_by_parts(required_by: Option<&str>) -> Parts<'_> {
    required_by.map_or_else(Parts::new, |by| {
        Parts::new().text(", which `").name(by).text("` requires")
    })
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
    required_by_parts(required_by).to_string()
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
/// library's wording of it in prose.
// Split into a path and a path-free reason so that "the message names the file
// exactly once" holds for every kind's parts at once rather than for each of them
// separately. `Io`, `Unreadable` and `Refused` can still name a path of their own
// (a cause's path, a directory, a symlink's destination), so nothing building one
// may name the file it was reading in it.
#[derive(Debug, Clone)]
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

    /// Where in the file the failure is, for the kinds that know.
    #[must_use]
    pub fn location(&self) -> Option<crate::yaml::SourceLocation> {
        self.reason_and_location().1
    }

    /// What went wrong, without the file or the [`location`](Self::location).
    ///
    /// Use this where the location has a place of its own; render the error
    /// itself where it does not.
    #[must_use]
    pub fn reason(&self) -> String {
        self.reason_and_location().0
    }

    fn reason_and_location(&self) -> (String, Option<crate::yaml::SourceLocation>) {
        // One match, and no catch-all: a new kind has to state here whether it
        // reports a location, rather than inheriting `None` from an arm that never
        // considered it.
        match &self.kind {
            PackageParseKind::Yaml { cause } => (cause.reason(), cause.location()),
            other @ (PackageParseKind::Io { .. }
            | PackageParseKind::Unreadable { .. }
            | PackageParseKind::IrregularFile { .. }
            | PackageParseKind::Refused { .. }
            | PackageParseKind::InvalidName { .. }) => (other.to_string(), None),
        }
    }
}

crate::parts::display_from_parts!(error PackageParseError);

impl Composed for PackageParseError {
    fn parts(&self) -> Parts<'_> {
        self.kind.parts()
    }
}

/// Why a package file could not be turned into a package.
///
/// No variant names the file: the path lives on
/// [`PackageParseError`](PackageParseError::package_path), and a renderer that
/// wants it in the sentence asks for it there.
#[derive(Debug, Clone)]
pub enum PackageParseKind {
    /// YAML syntax or structure error in the package file
    Yaml { cause: crate::yaml::ParseFailure },

    /// IO error occurred while reading the package file
    Io { cause: crate::fs::IoFailure },

    /// The package file could not be read, for a reason that is nobody's mistake
    ///
    /// Distinct from [`Refused`](Self::Refused), which is selfie declining a file
    /// it could have read or did read. Nothing here was declined; the read could
    /// not be attempted.
    Unreadable(UnreadableSpecFile),

    /// The package file is a fifo, socket or device node rather than a regular file
    ///
    /// Reading one blocks until a writer arrives, so it is refused before the
    /// read rather than reported after it.
    IrregularFile { kind: crate::fs::IrregularKind },

    /// selfie declined the package file: the filesystem port refused to read
    /// it, or selfie read it and the spec guard will not use what it says
    ///
    /// Carries its own reason rather than the file system's refusal.
    // Every filesystem refusal variant's own `Display` names a *target* and says
    // selfie will not **write** through it, having been written for the deploy
    // side. Rendering one here would report a write refusal on a read path, so a
    // read refusal is worded by `RefusedSpecFile` instead.
    //
    // The filesystem half is reached only if `irregular_target_refusal` ever
    // returns something other than `IrregularTarget`, so that growth fails closed
    // with sensible wording. The spec guard's half carries a refusal such as a
    // key shadowing `environments:`, for a file that parsed.
    Refused(RefusedSpecFile),

    /// The file's name, without its extension, is not a valid spec name
    ///
    /// Refused before the file is read, since no name the file could be asked
    /// for would be one selfie accepts everywhere else.
    InvalidName { name: String },
}

crate::parts::display_from_parts!(error PackageParseKind);

impl Composed for PackageParseKind {
    fn parts(&self) -> Parts<'_> {
        match self {
            Self::Yaml { cause } => Parts::new()
                .text("YAML parsing error: ")
                .text(cause.to_string()),
            Self::Io { cause } => Parts::new()
                .text("I/O error reading the package file: ")
                .append(cause.parts()),
            Self::Unreadable(reason) => Parts::new()
                .text("the package file could not be read: ")
                .append(reason.parts()),
            Self::IrregularFile { kind } => Parts::new().text(format!(
                "the package file is a {}, not a regular file. Replace it with a regular \
                 file or remove it from the package directory.",
                kind.label()
            )),
            Self::Refused(reason) => Parts::new()
                .text("selfie will not use the package file: ")
                .append(reason.parts()),
            Self::InvalidName { name } => Parts::new()
                .text("its name '")
                .name(name)
                .text("' is not a valid spec name. Rename the file: ")
                .text(crate::package::SPEC_NAME_RULE)
                .text("."),
        }
    }
}

/// Why a package file could not be read, when the read could not be attempted.
#[derive(Debug, Clone)]
pub enum UnreadableSpecFile {
    /// The home directory could not be determined.
    HomeDirectory,
    /// No new file can be created in `directory`.
    CannotCreateIn {
        /// The directory that refused one.
        directory: PathBuf,
        /// Why it refused.
        cause: Arc<std::io::Error>,
    },
}

crate::parts::display_from_parts!(UnreadableSpecFile);

impl Composed for UnreadableSpecFile {
    fn parts(&self) -> Parts<'_> {
        match self {
            Self::HomeDirectory => Parts::new().text("the home directory could not be determined"),
            // `FileSystemError::CannotCreateIn`'s own words, which name no target.
            Self::CannotCreateIn { directory, cause } => {
                Parts::new().path(directory).text(": ").cause(&**cause)
            }
        }
    }
}

/// Why selfie declined a package file it could have read, or did read.
#[derive(Debug, Clone)]
pub enum RefusedSpecFile {
    /// The file is a symlink.
    Symlink(SymlinkReason),
    /// Several files claim the spec's name, so none of them is used.
    AmbiguousName {
        /// The name they claim.
        name: String,
        /// Every file claiming it.
        paths: Vec<PathBuf>,
    },
    /// The file carries keys selfie does not model, or could not be checked for
    /// them, as the spec guard words it.
    UntrustedKeys(Message),
}

crate::parts::display_from_parts!(RefusedSpecFile);

impl Composed for RefusedSpecFile {
    fn parts(&self) -> Parts<'_> {
        match self {
            Self::Symlink(reason) => reason.parts(),
            Self::AmbiguousName { name, paths } => ambiguous_files_parts(name, paths),
            Self::UntrustedKeys(reason) => reason.parts(),
        }
    }
}

/// That a file selfie refused to read or write is a symlink, naming where it
/// points when that could be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymlinkReason {
    /// Where the link points, or `None` when the link could not be read.
    pub to: Option<PathBuf>,
}

crate::parts::display_from_parts!(SymlinkReason);

impl Composed for SymlinkReason {
    fn parts(&self) -> Parts<'_> {
        let to = self.to.as_deref().map_or_else(Parts::new, |destination| {
            Parts::new().text(" to '").path(destination).text("'")
        });
        Parts::new().text("it is a symlink").append(to)
    }
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
            PackageParseKind::Unreadable(crate::package::port::UnreadableSpecFile::HomeDirectory),
        );
        let (name_file, bad_name) = unreadable(
            "Dep",
            PackageParseKind::Unreadable(
                crate::package::port::UnreadableSpecFile::CannotCreateIn {
                    directory: "/packages".into(),
                    cause: std::sync::Arc::new(std::io::Error::from(
                        std::io::ErrorKind::PermissionDenied,
                    )),
                },
            ),
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
                PackageParseKind::Unreadable(UnreadableSpecFile::CannotCreateIn {
                    directory: "/p".into(),
                    cause: Arc::new(std::io::Error::other("the cause")),
                }),
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
                std::io::Error::other("the cause").into(),
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

    // A not-found failure's hint, whole: each file by its directory and name, the
    // `name:` value as the file writes it, and the name to use.
    #[test]
    fn a_name_field_hint_is_worded_whole() {
        let declared = [NameDeclaration {
            path: PathBuf::from("/home/me/p\u{200B}k/bar.yml"),
            name: "f\u{1b}oo".to_string(),
        }];

        assert_eq!(
            name_field_hint(&declared).to_string(),
            ". p\u{200B}k/bar.yml declares `name: f\u{1b}oo`; a package is named by its file, so \
             use 'bar'"
        );
        assert_eq!(name_field_hint(&[]).to_string(), "");
    }

    // The `name:` value as the hint's words carry it, one row per class of
    // character an escape could treat differently: an invisible one, the three
    // ASCII characters a Rust escape backslashes, and a blank that is not a
    // space. The value is a name part, written as it is; escaping it is the
    // CLI's business.
    #[test]
    fn a_name_field_hints_value_keeps_its_spelling() {
        for name in ["ze\u{200B}o", "ze\"o", "ze'o", "ze\\o", "ze\u{A0}o"] {
            let declared = [NameDeclaration {
                path: PathBuf::from("/home/me/pkgs/bar.yml"),
                name: name.to_string(),
            }];
            assert_eq!(
                name_field_hint(&declared).to_string(),
                format!(
                    ". pkgs/bar.yml declares `name: {name}`; a package is named by its file, \
                     so use 'bar'"
                ),
                "{name:?}"
            );
        }
    }

    // Every path and name a spec file's read failure gives is a part of its own,
    // so a value holding U+200B never sits inside selfie's words.
    #[test]
    fn a_spec_files_read_failure_names_its_values_as_parts() {
        use crate::parts::values_of;

        let directory = PathBuf::from("/p\u{200B}k");
        let paths = [directory.join("m.yml"), directory.join("m.yaml")];
        let untrusted: Message = Parts::new()
            .text("in environment '")
            .name("w\u{200B}k")
            .text("': unknown field 'bogus'")
            .into();
        let cases = [
            (
                PackageParseKind::Refused(RefusedSpecFile::Symlink(SymlinkReason {
                    to: Some(PathBuf::from("/else\u{200B}where")),
                })),
                vec!["/else\u{200B}where"],
            ),
            (
                PackageParseKind::Refused(RefusedSpecFile::AmbiguousName {
                    name: "m".to_string(),
                    paths: paths.to_vec(),
                }),
                vec!["m", "/p\u{200B}k", "m.yml", "m.yaml"],
            ),
            (
                PackageParseKind::Refused(RefusedSpecFile::UntrustedKeys(untrusted)),
                vec!["w\u{200B}k"],
            ),
            (
                PackageParseKind::Unreadable(UnreadableSpecFile::CannotCreateIn {
                    directory: directory.clone(),
                    cause: Arc::new(std::io::Error::other("the cause")),
                }),
                vec!["/p\u{200B}k"],
            ),
            (
                PackageParseKind::Io {
                    cause: crate::fs::IoFailure::at(&directory, std::io::Error::other("the cause")),
                },
                vec!["/p\u{200B}k"],
            ),
            (
                PackageParseKind::InvalidName {
                    name: "g\u{200B}host".to_string(),
                },
                vec!["g\u{200B}host"],
            ),
        ];
        for (kind, values) in cases {
            let error = PackageParseError::new(directory.join("g.yml"), kind);
            assert_eq!(values_of(&error.parts().into()), values);
        }
    }

    // An ambiguity names its spec and each file as names and its directory as a
    // path, which `values_of` alone cannot tell apart.
    #[test]
    fn an_ambiguity_names_its_spec_as_a_name_and_its_directory_as_a_path() {
        use crate::parts::Part;

        let directory = Path::new("/p\u{200B}k");
        let paths = [directory.join("m.yml")];
        let parts = ambiguous_name_parts("m", directory, &paths);
        let parts: Vec<&Part<'_>> = parts.into_iter().collect();

        assert!(parts.contains(&&Part::Name("m".into())), "{parts:?}");
        assert!(parts.contains(&&Part::Path(directory.into())), "{parts:?}");
        assert!(parts.contains(&&Part::Name("m.yml".into())), "{parts:?}");
    }

    // A not-found hint names the declaring file as a path and the spec to use as
    // a name.
    #[test]
    fn a_name_field_hint_names_its_file_as_a_path_and_its_spec_as_a_name() {
        use crate::parts::{Part, values_of};

        let declared = [NameDeclaration {
            path: PathBuf::from("/home/p\u{200B}k/bar.yml"),
            name: "f\u{200B}oo".to_string(),
        }];
        let hint = name_field_hint(&declared);

        assert_eq!(
            values_of(&hint),
            ["p\u{200B}k/bar.yml", "f\u{200B}oo", "bar"]
        );
        let parts = hint.parts();
        let parts: Vec<&Part<'_>> = parts.into_iter().collect();
        assert!(
            parts.contains(&&Part::Path(Path::new("p\u{200B}k/bar.yml").into())),
            "{parts:?}"
        );
        assert!(parts.contains(&&Part::Name("bar".into())), "{parts:?}");
        assert!(
            parts.contains(&&Part::Name("f\u{200B}oo".into())),
            "{parts:?}"
        );
    }

    // Each package error, with values holding U+200B, and its sentence.
    fn package_errors() -> Vec<(PackageError, String)> {
        let (name, environment, required_by) = ("n\u{200B}m", "w\u{200B}k", "r\u{200B}q");
        let file = || PathBuf::from("/p\u{200B}k/a.yml");
        let cause = || {
            PackageParseError::new(
                file(),
                PackageParseKind::InvalidName {
                    name: "a\u{200B}".to_string(),
                },
            )
        };
        let unloadable = |prefix: &str| {
            format!(
                "{prefix} package `{name}` from /p\u{200B}k/a.yml: its name 'a\u{200B}' is not a \
                 valid spec name. Rename the file: {}.",
                crate::package::SPEC_NAME_RULE
            )
        };
        vec![
            (
                PackageError::PackageNotFound {
                    name: name.to_string(),
                    packages_path: "/p\u{200B}k".into(),
                    files_examined: 0,
                    search_patterns: Vec::new(),
                    declared_by: Vec::new(),
                },
                format!("Package `{name}` not found in path /p\u{200B}k"),
            ),
            (
                PackageError::PackageNotFound {
                    name: name.to_string(),
                    packages_path: "/p\u{200B}k".into(),
                    files_examined: 1,
                    search_patterns: Vec::new(),
                    declared_by: vec![NameDeclaration {
                        path: "/p\u{200B}k/bar.yml".into(),
                        name: "foo".to_string(),
                    }],
                },
                format!(
                    "Package `{name}` not found in path /p\u{200B}k. p\u{200B}k/bar.yml declares \
                     `name: foo`; a package is named by its file, so use 'bar'"
                ),
            ),
            (
                PackageError::MultiplePackagesFound {
                    name: name.to_string(),
                    packages_path: "/p\u{200B}k".into(),
                    conflicting_paths: vec![
                        "/p\u{200B}k/m.yml".into(),
                        "/p\u{200B}k/m.yaml".into(),
                    ],
                    files_examined: 2,
                    search_patterns: Vec::new(),
                },
                format!(
                    "Multiple packages found with name `{name}` in path /p\u{200B}k: m.yml, m.yaml. \
                     Names are compared ignoring case, and `.yml` and `.yaml` name the same \
                     package, so these files all claim one name. Rename or remove all but one."
                ),
            ),
            (
                PackageError::ParseError {
                    name: name.to_string(),
                    packages_path: "/p\u{200B}k".into(),
                    failed_file: file(),
                    cause: cause(),
                },
                unloadable("Parse error in"),
            ),
            (
                PackageError::UnreadableFile {
                    name: name.to_string(),
                    packages_path: "/p\u{200B}k".into(),
                    failed_file: file(),
                    cause: cause(),
                },
                unloadable("Cannot read"),
            ),
            (
                PackageError::UnusableName {
                    name: name.to_string(),
                    packages_path: "/p\u{200B}k".into(),
                    failed_file: file(),
                    cause: cause(),
                },
                unloadable("Cannot use"),
            ),
            (
                PackageError::EnvironmentNotFound {
                    package_name: name.to_string(),
                    environment: environment.to_string(),
                    available_environments: Vec::new(),
                    package_file: file(),
                    required_by: None,
                },
                format!("Environment `{environment}` not found in package `{name}`"),
            ),
            (
                PackageError::EnvironmentNotFound {
                    package_name: name.to_string(),
                    environment: environment.to_string(),
                    available_environments: Vec::new(),
                    package_file: file(),
                    required_by: Some(required_by.to_string()),
                },
                format!(
                    "Environment `{environment}` not found in package `{name}`, which \
                     `{required_by}` requires"
                ),
            ),
            (
                PackageError::NoCheckCommand {
                    package_name: name.to_string(),
                    environment: environment.to_string(),
                    package_file: file(),
                    other_envs_with_check: Vec::new(),
                },
                format!(
                    "No check command defined for package `{name}` in environment `{environment}`"
                ),
            ),
            (
                PackageError::PackageAlreadyExists {
                    name: name.to_string(),
                    file_path: file(),
                },
                format!("Package `{name}` already exists at /p\u{200B}k/a.yml"),
            ),
            (
                PackageError::PackagePathOccupied {
                    name: name.to_string(),
                    file_path: file(),
                },
                format!(
                    "Cannot create `{name}`: /p\u{200B}k/a.yml is already taken by something selfie \
                     did not find under that name; selfie will not write over it."
                ),
            ),
            (
                PackageError::NoInstallCommand {
                    package_name: name.to_string(),
                    environment: environment.to_string(),
                    package_file: file(),
                    other_envs_with_install: Vec::new(),
                    required_by: None,
                },
                format!(
                    "No install command defined for package `{name}` in environment `{environment}`"
                ),
            ),
            (
                PackageError::NoInstallCommand {
                    package_name: name.to_string(),
                    environment: environment.to_string(),
                    package_file: file(),
                    other_envs_with_install: Vec::new(),
                    required_by: Some(required_by.to_string()),
                },
                format!(
                    "No install command defined for package `{name}` in environment \
                     `{environment}`, which `{required_by}` requires"
                ),
            ),
        ]
    }

    // Every package error's sentence, whole, so a change to how they are built
    // cannot change their words. Each value in it is a part of the kind the CLI
    // escapes it as: a spec, an environment or a requiring package as a name, and
    // a file or a directory as a path. `values_of` panics on a value written as
    // text, and the kinds tell a name from a path, which `values_of` cannot.
    #[test]
    fn a_package_errors_sentences_are_worded_whole() {
        use crate::parts::{Part, values_of};

        let (name, path) = (
            |value: &str| ("name", value.to_string()),
            |value: &str| ("path", value.to_string()),
        );
        let (n, e, r) = ("n\u{200B}m", "w\u{200B}k", "r\u{200B}q");
        let (file, directory) = ("/p\u{200B}k/a.yml", "/p\u{200B}k");
        let unloadable = vec![name(n), path(file), name("a\u{200B}")];
        let kinds = [
            vec![name(n), path(directory)],
            vec![
                name(n),
                path(directory),
                path("p\u{200B}k/bar.yml"),
                name("foo"),
                name("bar"),
            ],
            vec![name(n), path(directory), name("m.yml"), name("m.yaml")],
            unloadable.clone(),
            unloadable.clone(),
            unloadable,
            vec![name(e), name(n)],
            vec![name(e), name(n), name(r)],
            vec![name(n), name(e)],
            vec![name(n), path(file)],
            vec![name(n), path(file)],
            vec![name(n), name(e)],
            vec![name(n), name(e), name(r)],
        ];
        let errors = package_errors();
        assert_eq!(errors.len(), kinds.len());
        for ((error, sentence), kinds) in errors.into_iter().zip(kinds) {
            assert_eq!(error.to_string(), sentence);
            let values: Vec<String> = kinds.iter().map(|(_, value)| value.clone()).collect();
            assert_eq!(values_of(&error.parts().into()), values, "{error}");
            let shown: Vec<(&str, String)> = error
                .parts()
                .into_iter()
                .filter_map(|part| match part {
                    Part::Name(value) => Some(("name", value.to_string())),
                    Part::Path(value) => Some(("path", value.display().to_string())),
                    Part::Foreign(value) => Some(("foreign", value.to_string())),
                    Part::SpecCommand(value) => Some(("spec command", value.as_str().to_string())),
                    Part::Text(_) | Part::Command(_) => None,
                })
                .collect();
            assert_eq!(shown, kinds, "{error}");
        }
    }

    // Every repository error's sentence, whole, and the failure each becomes, so a
    // change to how they are built cannot change their words.
    #[test]
    fn a_repository_errors_sentences_are_worded_whole() {
        use crate::package::event::OperationFailure;

        let path = || PathBuf::from("/p/a.yml");
        let cause = || Arc::new(std::io::Error::other("the cause"));
        let untrusted = |levels: Vec<UntrustedLevel>| PackageRepoError::UntrustedKeys {
            path: path(),
            levels: UntrustedLevels::new(levels).unwrap(),
        };
        let top = || UntrustedLevel::TopLevelFields(vec!["configs".to_string()]);
        let unchecked = || {
            UntrustedLevel::UncheckedTopLevel(
                crate::yaml::parse::<serde_json::Value>("[").unwrap_err(),
            )
        };
        let cases = [
            (PackageRepoError::IoError(cause()), "IO error: the cause"),
            (
                PackageRepoError::FileSystemError(FileSystemError::CannotCreateIn {
                    path: "/p".into(),
                    cause: cause(),
                }),
                "File system error: /p: the cause",
            ),
            (
                PackageRepoError::Unserializable(crate::yaml::SerializeFailure::Value),
                "The package cannot be written as YAML: a value in it has a shape YAML cannot hold",
            ),
            (
                untrusted(vec![top()]),
                "refusing to rewrite /p/a.yml: unrecognized configs. Saving would delete the key \
                 and everything under it. Edit /p/a.yml directly to correct or remove the key.",
            ),
            (
                untrusted(vec![
                    UntrustedLevel::DotfileFields {
                        fields: vec!["dotfiles[0].var".to_string(), "dotfiles[1].x".to_string()],
                        entries: 2,
                    },
                    UntrustedLevel::EnvironmentFields(vec!["environments.work.audt".to_string()]),
                ]),
                "refusing to rewrite /p/a.yml: unrecognized dotfiles[0].var, dotfiles[1].x. Saving \
                 would delete the keys and silently make the entries deployable. Unrecognized \
                 environments.work.audt. Saving would delete the key and whatever it was set to. \
                 Edit /p/a.yml directly to correct or remove the keys.",
            ),
            (
                PackageRepoError::UnwritablePath {
                    path: path(),
                    reason: UnwritableReason::Symlink(SymlinkReason {
                        to: Some("/elsewhere/a.yml".into()),
                    }),
                },
                "refusing to write /p/a.yml: it is a symlink to '/elsewhere/a.yml'. Remove what is \
                 at that path, or choose another name.",
            ),
            (
                PackageRepoError::UnwritablePath {
                    path: path(),
                    reason: UnwritableReason::Irregular(crate::fs::IrregularKind::Fifo),
                },
                "refusing to write /p/a.yml: it is a named pipe (fifo). Remove what is at that \
                 path, or choose another name.",
            ),
            (
                PackageRepoError::Occupied { path: path() },
                "something appeared at /p/a.yml while selfie was writing the spec there; nothing \
                 was written over it. Run the command again to see what is there.",
            ),
            (
                PackageRepoError::ChangedSinceLoad {
                    path: path(),
                    refused: RefusedChange::Write,
                },
                "/p/a.yml changed after selfie read it, so nothing was written to it. Run the \
                 command again, which reads the file as it is now.",
            ),
            (
                PackageRepoError::ChangedSinceLoad {
                    path: path(),
                    refused: RefusedChange::Removal,
                },
                "/p/a.yml changed after selfie read it, so it was not removed. Run the command \
                 again, which reads the file as it is now.",
            ),
            (
                PackageRepoError::NotLoaded { path: path() },
                "refusing to change /p/a.yml: this repository did not load the spec from it",
            ),
            (
                PackageRepoError::AnotherFileAnswers {
                    path: path(),
                    answering: "/p/A.yaml".into(),
                },
                "/p/A.yaml now answers to the name of /p/a.yml, which selfie read, so nothing was \
                 removed. Run the command again, which reads the spec as it is now.",
            ),
        ];
        for (error, sentence) in cases {
            assert_eq!(error.to_string(), sentence);
            assert_eq!(OperationFailure::from(error).to_string(), sentence);
        }
        // The parse failure ends these two in the parser's own words, which only
        // selfie's frame around them pins.
        let read = crate::yaml::parse::<serde_json::Value>("[")
            .unwrap_err()
            .to_string();
        let unread = [
            (
                untrusted(vec![unchecked()]),
                format!(
                    "refusing to rewrite /p/a.yml: its top level could not be read back, so any \
                     key selfie does not model would be dropped without warning. Edit /p/a.yml \
                     directly, or simplify it until selfie can read it. The read failed with: \
                     {read}"
                ),
            ),
            (
                untrusted(vec![top(), unchecked()]),
                format!(
                    "refusing to rewrite /p/a.yml: unrecognized configs. Saving would delete the \
                     key and everything under it. Its top level could not be read back, so any \
                     other key selfie does not model would be dropped without warning. Edit \
                     /p/a.yml directly to correct or remove the key, and simplify it until selfie \
                     can read it. The read failed with: {read}"
                ),
            ),
        ];
        for (error, sentence) in unread {
            assert_eq!(error.to_string(), sentence);
            assert_eq!(OperationFailure::from(error).to_string(), sentence);
        }
        // An I/O cause's trailing line break is not part of the sentence.
        let trailing = PackageRepoError::IoError(Arc::new(std::io::Error::other("the cause\n")));
        assert_eq!(trailing.to_string(), "IO error: the cause");
    }

    // Every path and key a repository error gives is a part of its own, so a value
    // holding U+200B never sits inside selfie's words, and each failure but a
    // package's or a listing's own becomes `OperationFailure::Repository`.
    #[test]
    fn a_repository_error_names_its_values_as_parts() {
        use crate::package::event::OperationFailure;
        use crate::parts::values_of;

        let path = || PathBuf::from("/p\u{200B}k/a.yml");
        let cause = || Arc::new(std::io::Error::other("the cause"));
        let cases = [
            (PackageRepoError::IoError(cause()), vec![]),
            (
                PackageRepoError::FileSystemError(FileSystemError::CannotCreateIn {
                    path: "/p\u{200B}k".into(),
                    cause: cause(),
                }),
                vec!["/p\u{200B}k"],
            ),
            (
                PackageRepoError::UntrustedKeys {
                    path: path(),
                    levels: UntrustedLevels::new(vec![
                        UntrustedLevel::EnvironmentFields(vec![
                            "environments.w\u{200B}k.a\u{200B}udt".to_string(),
                        ]),
                        UntrustedLevel::TopLevelFields(vec![
                            "co\u{200B}nfigs".to_string(),
                            "x".to_string(),
                        ]),
                    ])
                    .unwrap(),
                },
                vec![
                    "/p\u{200B}k/a.yml",
                    "environments.w\u{200B}k.a\u{200B}udt",
                    "co\u{200B}nfigs",
                    "x",
                    "/p\u{200B}k/a.yml",
                ],
            ),
            (
                PackageRepoError::UntrustedKeys {
                    path: path(),
                    levels: UntrustedLevels::new(vec![UntrustedLevel::UncheckedTopLevel(
                        crate::yaml::parse::<serde_json::Value>("[").unwrap_err(),
                    )])
                    .unwrap(),
                },
                vec!["/p\u{200B}k/a.yml", "/p\u{200B}k/a.yml"],
            ),
            (
                PackageRepoError::UnwritablePath {
                    path: path(),
                    reason: UnwritableReason::Symlink(SymlinkReason {
                        to: Some("/else\u{200B}where".into()),
                    }),
                },
                vec!["/p\u{200B}k/a.yml", "/else\u{200B}where"],
            ),
            (
                PackageRepoError::UnwritablePath {
                    path: path(),
                    reason: UnwritableReason::Irregular(crate::fs::IrregularKind::Fifo),
                },
                vec!["/p\u{200B}k/a.yml"],
            ),
            (
                PackageRepoError::Occupied { path: path() },
                vec!["/p\u{200B}k/a.yml"],
            ),
            (
                PackageRepoError::ChangedSinceLoad {
                    path: path(),
                    refused: RefusedChange::Write,
                },
                vec!["/p\u{200B}k/a.yml"],
            ),
            (
                PackageRepoError::NotLoaded { path: path() },
                vec!["/p\u{200B}k/a.yml"],
            ),
            (
                PackageRepoError::AnotherFileAnswers {
                    path: path(),
                    answering: "/p\u{200B}k/A.yaml".into(),
                },
                vec!["/p\u{200B}k/A.yaml", "/p\u{200B}k/a.yml"],
            ),
        ];
        for (error, values) in cases {
            assert_eq!(values_of(&error.parts().into()), values, "{error}");
            assert!(
                matches!(
                    OperationFailure::from(error),
                    OperationFailure::Repository(_)
                ),
                "a repository error became another failure"
            );
        }
    }

    // A failed lookup names what the error it holds names, as the same parts.
    #[test]
    fn a_failed_lookup_names_its_values_as_parts() {
        use crate::parts::values_of;

        let error = PackageRepoError::from(PackageError::MultiplePackagesFound {
            name: "m".to_string(),
            packages_path: "/p\u{200B}k".into(),
            conflicting_paths: vec!["/p\u{200B}k/m.yml".into(), "/p\u{200B}k/m.yaml".into()],
            files_examined: 2,
            search_patterns: Vec::new(),
        });
        let expected = values_of(&error.parts().into());
        let failure = error.into_lookup_failure().expect("a lookup returns it");

        assert_eq!(values_of(&failure.parts().into()), expected);
        assert_eq!(expected, ["m", "/p\u{200B}k", "m.yml", "m.yaml"]);
    }

    // A repository error names a refused key as a name and its file as a path,
    // which `values_of` alone cannot tell apart.
    #[test]
    fn a_repository_error_names_a_key_as_a_name_and_a_file_as_a_path() {
        use crate::parts::Part;

        let error = PackageRepoError::UntrustedKeys {
            path: "/p/a.yml".into(),
            levels: UntrustedLevels::new(vec![UntrustedLevel::TopLevelFields(vec![
                "configs".to_string(),
            ])])
            .unwrap(),
        };
        let answers = PackageRepoError::AnotherFileAnswers {
            path: "/p/a.yml".into(),
            answering: "/p/A.yaml".into(),
        };

        let parts = error.parts();
        let parts: Vec<&Part<'_>> = parts.into_iter().collect();
        assert!(parts.contains(&&Part::Name("configs".into())), "{parts:?}");
        assert!(
            parts.contains(&&Part::Path(Path::new("/p/a.yml").into())),
            "{parts:?}"
        );
        let parts = answers.parts();
        let parts: Vec<&Part<'_>> = parts.into_iter().collect();
        assert!(
            parts.contains(&&Part::Path(Path::new("/p/A.yaml").into())),
            "{parts:?}"
        );
    }
}
