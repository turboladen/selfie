//! What selfie says about the standalone dotfiles directory.

use std::path::{Path, PathBuf};

use crate::{
    fs::{AbsentReason, DirectoryState, FileSystem},
    package::port::PackageListError,
};

/// What a failed listing of the standalone dotfiles directory means.
pub(crate) enum UnlistedDotfilesDirectory {
    /// No directory is at the default path, and `dotfiles_directory` is not set.
    UnsetAndAbsent,
    /// No directory is at the path the user set, and why not.
    ConfiguredAndAbsent {
        /// The path the user set.
        path: PathBuf,
        /// Why nothing is there, which decides the remedy.
        reason: AbsentReason,
    },
    /// A directory that could not be listed, so it may hold standalone dotfiles.
    Unlistable(PackageListError),
    /// A path selfie could not classify. Nothing is known about it, so it may
    /// hold standalone dotfiles just as an unlistable directory may.
    Unknown(PackageListError),
}

impl UnlistedDotfilesDirectory {
    /// Classify the dotfiles directory at `path` after listing it returned
    /// `error`, where `configured` says whether the user set
    /// `dotfiles_directory`.
    pub(crate) fn classify<F: FileSystem>(
        filesystem: &F,
        path: &Path,
        error: PackageListError,
        configured: bool,
    ) -> Self {
        // The shared classification decides what is there. "Not found" from the
        // listing is not taken at face value: a dangling symlink and an empty path
        // both produce it, and they take different remedies.
        let state = match &error {
            PackageListError::IoError(io) => DirectoryState::from_listing(filesystem, path, io),
            PackageListError::PackageDirectoryNotFound(_) => filesystem.directory_state(path),
        };

        match state {
            DirectoryState::Absent(reason) if configured => Self::ConfiguredAndAbsent {
                path: path.to_path_buf(),
                reason,
            },
            DirectoryState::Absent(_) => Self::UnsetAndAbsent,
            DirectoryState::Unlistable(_) => Self::Unlistable(error),
            DirectoryState::Unknown(_) => Self::Unknown(error),
            // A directory that classified cleanly and would not list. The listing
            // is the more recent answer and it failed, so it may be hiding entries.
            DirectoryState::Directory => Self::Unlistable(error),
        }
    }
}

/// The warning for a command that carries on without the standalone dotfiles in
/// the configured directory at `path`, where `reason` says what is there instead.
pub(crate) fn absent_warning(path: &Path, reason: &AbsentReason) -> String {
    format!(
        "Dotfiles directory {}: {} — standalone dotfiles will not be read.{}",
        absent_clause(reason),
        path.display(),
        remedy(path, reason)
    )
}

/// The refusal for tracking a standalone dotfile into the dotfiles directory at
/// `path`, where `reason` says what is there instead.
pub(crate) fn absent_track_refusal(path: &Path, reason: &AbsentReason) -> String {
    format!(
        "Cannot track a standalone dotfile: the dotfiles directory {}: {}{}",
        absent_clause(reason),
        path.display(),
        remedy(path, reason)
    )
}

/// The refusal for `track_standalone`, given `state` for the dotfiles directory
/// at `path` and the `error` a listing reported.
///
/// Every state refuses. A directory selfie could not read may hold the name
/// about to be tracked, and one it could not classify is no better known, so
/// neither can answer whether the name is free.
pub(crate) fn track_listing_refusal(
    path: &Path,
    state: &DirectoryState,
    error: &PackageListError,
) -> String {
    match state {
        DirectoryState::Absent(reason) => absent_track_refusal(path, reason),
        DirectoryState::Unlistable(_) => format!(
            "Cannot track a standalone dotfile: the dotfiles directory could not be listed, so it may already hold this name: {} — {error}",
            path.display()
        ),
        DirectoryState::Unknown(_) | DirectoryState::Directory => format!(
            "Cannot track a standalone dotfile: the dotfiles directory could not be checked: {} — {error}",
            path.display()
        ),
    }
}

/// The clause naming what is at the path instead of a directory.
fn absent_clause(reason: &AbsentReason) -> String {
    match reason {
        AbsentReason::Empty => "does not exist".to_string(),
        AbsentReason::Occupied { kind } => format!("is not a directory, it is a {kind}"),
        AbsentReason::DanglingSymlink { points_to } => match points_to {
            Some(destination) => {
                format!(
                    "is a symlink to nothing: it points at {}",
                    destination.display()
                )
            }
            // The link was read once and would not read again, so the sentence
            // names what is known rather than guessing a destination.
            None => "is a symlink to nothing".to_string(),
        },
        AbsentReason::ParentNotADirectory { parent } => {
            format!("is below {}, which is not a directory", parent.display())
        }
    }
}

/// The remedy for `reason`, or nothing when no single command is the remedy.
///
/// `mkdir -p` answers only an empty path. Against a plain file it fails with
/// "File exists" and against a dangling symlink with "No such file or
/// directory", so offering it for those sends the user to a command that cannot
/// work.
fn remedy(path: &Path, reason: &AbsentReason) -> String {
    match reason {
        AbsentReason::Empty => format!(" Create it with: mkdir -p {}", shell_quote(path)),
        AbsentReason::Occupied { .. }
        | AbsentReason::DanglingSymlink { .. }
        | AbsentReason::ParentNotADirectory { .. } => String::new(),
    }
}

/// `path` as a single shell word.
///
/// The sentence offers a command to paste, so a path holding a space or a quote
/// has to survive the paste. Single quotes with the shell's own escape for an
/// embedded single quote, which is the only character single quotes do not cover.
fn shell_quote(path: &Path) -> String {
    let rendered = path.display().to_string();
    if rendered
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'))
    {
        return rendered;
    }
    format!("'{}'", rendered.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tempfile::tempdir;

    use super::*;
    use crate::fs::RealFileSystem;

    fn not_found(path: &Path) -> PackageListError {
        PackageListError::PackageDirectoryNotFound(path.to_path_buf())
    }

    fn unlistable_error() -> PackageListError {
        PackageListError::IoError(Arc::new(std::io::Error::from(
            std::io::ErrorKind::PermissionDenied,
        )))
    }

    #[test]
    fn a_configured_directory_that_is_not_there_is_absent_at_its_path() {
        let dir = tempdir().unwrap();
        let absent = dir.path().join("dotfiles");

        match UnlistedDotfilesDirectory::classify(
            &RealFileSystem,
            &absent,
            not_found(&absent),
            true,
        ) {
            UnlistedDotfilesDirectory::ConfiguredAndAbsent { path, reason } => {
                assert_eq!(path, absent);
                assert!(matches!(reason, AbsentReason::Empty));
            }
            _ => panic!("expected a configured, absent directory"),
        }
    }

    // The ordinary state of anyone who keeps no standalone dotfiles.
    #[test]
    fn an_unset_default_that_is_not_there_is_unset_and_absent() {
        let dir = tempdir().unwrap();
        let absent = dir.path().join("dotfiles");

        assert!(matches!(
            UnlistedDotfilesDirectory::classify(
                &RealFileSystem,
                &absent,
                not_found(&absent),
                false
            ),
            UnlistedDotfilesDirectory::UnsetAndAbsent
        ));
    }

    // The case that reached the user as "does not exist" with a `mkdir -p` that
    // fails: the path is there, the link is there, its destination is not.
    #[test]
    fn a_dangling_symlink_is_absent_as_a_link_and_offers_no_mkdir() {
        let dir = tempdir().unwrap();
        let link = dir.path().join("dotfiles");
        std::os::unix::fs::symlink(dir.path().join("elsewhere"), &link).unwrap();

        let UnlistedDotfilesDirectory::ConfiguredAndAbsent { reason, .. } =
            UnlistedDotfilesDirectory::classify(&RealFileSystem, &link, not_found(&link), true)
        else {
            panic!("expected a configured, absent directory");
        };
        assert!(matches!(reason, AbsentReason::DanglingSymlink { .. }));

        for sentence in [
            absent_warning(&link, &reason),
            absent_track_refusal(&link, &reason),
        ] {
            assert!(sentence.contains("symlink to nothing"), "{sentence}");
            assert!(sentence.contains("elsewhere"), "{sentence}");
            assert!(
                !sentence.contains("mkdir"),
                "mkdir -p fails with \"No such file or directory\" here: {sentence}"
            );
            assert!(!sentence.contains("does not exist"), "{sentence}");
        }
    }

    #[test]
    fn a_plain_file_at_the_path_names_what_is_there_and_offers_no_mkdir() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("dotfiles");
        std::fs::write(&file, "x").unwrap();

        let UnlistedDotfilesDirectory::ConfiguredAndAbsent { reason, .. } =
            UnlistedDotfilesDirectory::classify(&RealFileSystem, &file, not_found(&file), true)
        else {
            panic!("expected a configured, absent directory");
        };

        let sentence = absent_warning(&file, &reason);
        assert!(
            sentence.contains("is not a directory, it is a regular file"),
            "{sentence}"
        );
        assert!(
            !sentence.contains("mkdir"),
            "mkdir -p fails with \"File exists\" here: {sentence}"
        );
    }

    // Only the empty path takes the `mkdir -p` remedy, and it is the one case
    // where the command works.
    #[test]
    fn only_an_empty_path_is_offered_mkdir() {
        let dir = tempdir().unwrap();
        let absent = dir.path().join("dotfiles");

        let sentence = absent_warning(&absent, &AbsentReason::Empty);
        assert!(sentence.contains("does not exist"), "{sentence}");
        assert!(
            sentence.contains(&format!("mkdir -p {}", absent.display())),
            "{sentence}"
        );
    }

    // A path the user can paste. An unquoted one breaks the command it offers.
    #[test]
    fn a_path_holding_a_space_is_quoted_in_the_remedy() {
        let sentence = absent_warning(Path::new("/home/me/my dotfiles"), &AbsentReason::Empty);

        assert!(
            sentence.contains("mkdir -p '/home/me/my dotfiles'"),
            "{sentence}"
        );
    }

    #[test]
    fn a_path_below_a_plain_file_names_the_file() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("plain");
        std::fs::write(&file, "x").unwrap();
        let below = file.join("under").join("dotfiles");

        let UnlistedDotfilesDirectory::ConfiguredAndAbsent { reason, .. } =
            UnlistedDotfilesDirectory::classify(&RealFileSystem, &below, not_found(&below), true)
        else {
            panic!("expected a configured, absent directory");
        };

        let sentence = absent_warning(&below, &reason);
        assert!(
            sentence.contains(&format!(
                "is below {}, which is not a directory",
                file.display()
            )),
            "{sentence}"
        );
        assert!(!sentence.contains("mkdir"), "{sentence}");
    }

    // Something is at the path whether or not the user named it, and it may hold
    // standalone dotfiles.
    #[test]
    fn a_directory_that_will_not_list_is_unlistable_whether_or_not_configured() {
        let dir = tempdir().unwrap();

        for configured in [true, false] {
            assert!(
                matches!(
                    UnlistedDotfilesDirectory::classify(
                        &RealFileSystem,
                        dir.path(),
                        unlistable_error(),
                        configured
                    ),
                    UnlistedDotfilesDirectory::Unlistable(_)
                ),
                "configured: {configured}"
            );
        }
    }

    // A loop is a path nothing is known about, not a directory hiding entries.
    // Both refuse, and the sentences differ because the remedies differ.
    #[test]
    fn a_symlink_loop_is_unknown_and_says_it_could_not_be_checked() {
        let dir = tempdir().unwrap();
        let link = dir.path().join("dotfiles");
        std::os::unix::fs::symlink(&link, &link).unwrap();

        // The error a real listing reports for this shape, not a hand-picked one.
        // `read_dir` answers `ELOOP` here, and a fixture using `PermissionDenied`
        // would classify as unlistable and prove nothing about a loop.
        let error = PackageListError::IoError(Arc::new(std::fs::read_dir(&link).unwrap_err()));
        assert!(matches!(
            UnlistedDotfilesDirectory::classify(&RealFileSystem, &link, error.clone(), true),
            UnlistedDotfilesDirectory::Unknown(_)
        ));

        let state = RealFileSystem.directory_state(&link);
        let refusal = track_listing_refusal(&link, &state, &error);
        assert!(refusal.contains("could not be checked"), "{refusal}");
        assert!(!refusal.contains("mkdir"), "{refusal}");
    }

    // Every state refuses a track, because none of them can answer whether the
    // name is already taken.
    #[test]
    fn every_state_refuses_a_track() {
        let dir = tempdir().unwrap();
        let error = unlistable_error();

        for state in [
            DirectoryState::Absent(AbsentReason::Empty),
            DirectoryState::Absent(AbsentReason::Occupied {
                kind: "regular file",
            }),
            DirectoryState::Absent(AbsentReason::DanglingSymlink { points_to: None }),
            DirectoryState::Unlistable(Arc::new(std::io::Error::from(
                std::io::ErrorKind::PermissionDenied,
            ))),
            DirectoryState::Unknown(Arc::new(std::io::Error::from(
                std::io::ErrorKind::PermissionDenied,
            ))),
            DirectoryState::Directory,
        ] {
            let refusal = track_listing_refusal(dir.path(), &state, &error);
            assert!(
                refusal.starts_with("Cannot track a standalone dotfile:"),
                "{state:?}: {refusal}"
            );
        }
    }

    // A directory that cannot be listed may already hold the name, which is the
    // reason the refusal exists, so the sentence has to say it.
    #[test]
    fn an_unlistable_directory_says_it_may_already_hold_the_name() {
        let dir = tempdir().unwrap();
        let state = DirectoryState::Unlistable(Arc::new(std::io::Error::from(
            std::io::ErrorKind::PermissionDenied,
        )));

        let refusal = track_listing_refusal(dir.path(), &state, &unlistable_error());

        assert!(refusal.contains("may already hold this name"), "{refusal}");
        assert!(!refusal.contains("mkdir"), "{refusal}");
    }

    // The agreement decision 1 actually needs, one layer above the port's own.
    //
    // Two routes reach a directory's state. Track asks the port directly and hands
    // the answer to its refusal; apply, drift and list go through the classification
    // here. Nothing made them agree, and a mutation that mapped a loop to unlistable
    // in this function left the port's own agreement test green, because that test
    // compares the port's two constructors and never calls this one.
    //
    // So this asserts across the layer: for every shape, what this function decides
    // and what the port says must be the same fact.
    #[test]
    fn the_service_classification_agrees_with_the_port() {
        let dir = tempdir().unwrap();
        let plain = dir.path().join("plain");
        std::fs::write(&plain, "x").unwrap();
        let dangling = dir.path().join("dangling");
        std::os::unix::fs::symlink(dir.path().join("nowhere"), &dangling).unwrap();
        let loop_link = dir.path().join("loop");
        std::os::unix::fs::symlink(&loop_link, &loop_link).unwrap();

        for path in [plain, dangling, loop_link, dir.path().join("absent")] {
            // The error a real listing gives this shape, so the fixture cannot drift
            // from what production passes in.
            let error = PackageListError::IoError(Arc::new(std::fs::read_dir(&path).unwrap_err()));
            let from_port = RealFileSystem.directory_state(&path);
            let classified =
                UnlistedDotfilesDirectory::classify(&RealFileSystem, &path, error, true);

            let port_name = match from_port {
                DirectoryState::Directory => "directory",
                DirectoryState::Absent(_) => "absent",
                DirectoryState::Unlistable(_) => "unlistable",
                DirectoryState::Unknown(_) => "unknown",
            };
            let service_name = match classified {
                UnlistedDotfilesDirectory::ConfiguredAndAbsent { .. }
                | UnlistedDotfilesDirectory::UnsetAndAbsent => "absent",
                UnlistedDotfilesDirectory::Unlistable(_) => "unlistable",
                UnlistedDotfilesDirectory::Unknown(_) => "unknown",
            };
            assert_eq!(
                port_name,
                service_name,
                "{} is {port_name} to the port and {service_name} to the service",
                path.display()
            );
        }
    }

    // Apply warns and track refuses about the same directory in one session, so
    // the two must say different things while naming the same fix.
    #[test]
    fn the_warning_and_the_refusal_do_not_say_the_same_thing() {
        let dir = Path::new("/nonexistent/dotfiles");

        let warning = absent_warning(dir, &AbsentReason::Empty);
        let refusal = absent_track_refusal(dir, &AbsentReason::Empty);

        assert_ne!(warning, refusal);
        assert!(warning.contains("will not be read"), "{warning}");
        assert!(refusal.contains("Cannot track"), "{refusal}");
        for sentence in [&warning, &refusal] {
            assert!(sentence.contains("/nonexistent/dotfiles"), "{sentence}");
            assert!(
                sentence.contains("mkdir -p /nonexistent/dotfiles"),
                "{sentence}"
            );
        }
    }
}
