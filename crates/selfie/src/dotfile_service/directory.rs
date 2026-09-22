//! What selfie says about the standalone dotfiles directory.

use std::path::{Path, PathBuf};

use crate::{
    fs::{AbsentReason, DirectoryState, FileSystem},
    package::port::PackageListError,
};

/// What a failed listing of the standalone dotfiles directory means.
pub(crate) enum UnlistedDotfilesDirectory {
    /// Nothing at all is at the unset default path, which is the ordinary state of
    /// a setup that keeps no standalone dotfiles and is worth no word.
    OrdinarilyAbsent,
    /// No directory is at the path, and why not. Worth saying whatever the user
    /// configured, unless it is the unset default with nothing at it.
    Absent {
        /// The path selfie looked at.
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
        match error.directory_state(filesystem, path) {
            // Being configured governs an **empty** path and nothing else. An absent
            // default is the ordinary condition of anyone who keeps no standalone
            // dotfiles, and a word about it on every run is noise. Anything else at
            // the path is a mistake: a plain file, or a link whose destination is
            // gone, did not get there by the setting being left out, and staying
            // silent about it hides the reason the directory is not being read.
            DirectoryState::Absent(AbsentReason::Empty) if !configured => Self::OrdinarilyAbsent,
            DirectoryState::Absent(reason) => Self::Absent {
                path: path.to_path_buf(),
                reason,
            },
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
        "Dotfiles directory {} {} — standalone dotfiles will not be read.{}",
        path.display(),
        reason.clause(),
        match reason.remedy(path) {
            Some(command) => format!(" {command}"),
            None => String::new(),
        }
    )
}

/// The refusal for tracking a standalone dotfile into the dotfiles directory at
/// `path`, where `reason` says what is there instead.
pub(crate) fn absent_track_refusal(path: &Path, reason: &AbsentReason) -> String {
    format!(
        "Cannot track a standalone dotfile: the dotfiles directory {} {}{}",
        path.display(),
        reason.clause(),
        match reason.remedy(path) {
            Some(command) => format!(" {command}"),
            None => String::new(),
        }
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
        // A directory that classified cleanly and still would not list could not be
        // listed. Only an unclassifiable path is unchecked.
        DirectoryState::Unlistable(_) | DirectoryState::Directory => format!(
            "Cannot track a standalone dotfile: the dotfiles directory at {} could not be listed, so it may already hold this name — {error}",
            path.display()
        ),
        DirectoryState::Unknown(_) => format!(
            "Cannot track a standalone dotfile: the dotfiles directory at {} could not be checked — {error}",
            path.display()
        ),
    }
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
            UnlistedDotfilesDirectory::Absent { path, reason } => {
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
            UnlistedDotfilesDirectory::OrdinarilyAbsent
        ));
    }

    // Being unset buys silence for an empty path and for nothing else. A plain file
    // at the default path is a mistake whoever made it wants to hear about, and it
    // cannot have been made by leaving the setting out.
    #[test]
    fn a_plain_file_at_the_unset_default_is_still_worth_saying() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("dotfiles");
        std::fs::write(&file, "x").unwrap();

        let UnlistedDotfilesDirectory::Absent { reason, .. } =
            UnlistedDotfilesDirectory::classify(&RealFileSystem, &file, not_found(&file), false)
        else {
            panic!("a file in the way is not the ordinary absent default");
        };

        assert!(
            absent_warning(&file, &reason).contains("is not a directory, it is a regular file"),
            "the sentence must name what is there"
        );
    }

    // The same for a link whose destination is gone, which is the other shape a user
    // reaches by accident rather than by not configuring anything.
    #[test]
    fn a_dangling_symlink_at_the_unset_default_is_still_worth_saying() {
        let dir = tempdir().unwrap();
        let link = dir.path().join("dotfiles");
        std::os::unix::fs::symlink(dir.path().join("elsewhere"), &link).unwrap();

        let UnlistedDotfilesDirectory::Absent { reason, .. } =
            UnlistedDotfilesDirectory::classify(&RealFileSystem, &link, not_found(&link), false)
        else {
            panic!("a dangling link is not the ordinary absent default");
        };

        let sentence = absent_warning(&link, &reason);
        assert!(sentence.contains("is a symlink to nothing"), "{sentence}");
        assert!(
            !sentence.contains("mkdir"),
            "mkdir -p cannot create a path a link occupies: {sentence}"
        );
    }

    // The case that reached the user as "does not exist" with a `mkdir -p` that
    // fails: the path is there, the link is there, its destination is not.
    #[test]
    fn a_dangling_symlink_is_absent_as_a_link_and_offers_no_mkdir() {
        let dir = tempdir().unwrap();
        let link = dir.path().join("dotfiles");
        std::os::unix::fs::symlink(dir.path().join("elsewhere"), &link).unwrap();

        let UnlistedDotfilesDirectory::Absent { reason, .. } =
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

        let UnlistedDotfilesDirectory::Absent { reason, .. } =
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
            sentence.contains(&format!("mkdir -p -- {}", absent.display())),
            "{sentence}"
        );
    }

    // A path the user can paste. An unquoted one breaks the command it offers.
    #[test]
    fn a_path_holding_a_space_is_quoted_in_the_remedy() {
        let sentence = absent_warning(Path::new("/home/me/my dotfiles"), &AbsentReason::Empty);

        assert!(
            sentence.contains("mkdir -p -- '/home/me/my dotfiles'"),
            "{sentence}"
        );
    }

    // Quoting alone does not save a path that begins with a dash: mkdir reads it as
    // options. The second assertion is the one that fails without `--`, since the
    // first still matches a command that has the path but not the separator.
    #[test]
    fn a_path_beginning_with_a_dash_is_not_read_as_options() {
        let sentence = absent_warning(Path::new("-foo"), &AbsentReason::Empty);

        assert!(sentence.contains("-foo"), "{sentence}");
        assert!(
            sentence.contains("mkdir -p -- -foo"),
            "the option list must be ended before the path: {sentence}"
        );
    }

    // A tilde survives only outside the quotes. Quoted, the pasted command creates a
    // directory named `~` in the working directory rather than one under the home
    // directory. The path reaches here unexpanded only when selfie could not find a
    // home directory, which is when the user has to run the command themselves.
    #[test]
    fn a_leading_tilde_stays_outside_the_quotes() {
        let sentence = absent_warning(Path::new("~/my dotfiles"), &AbsentReason::Empty);

        assert!(
            sentence.contains("mkdir -p -- ~/'my dotfiles'"),
            "{sentence}"
        );
        assert!(
            !sentence.contains("'~/"),
            "a quoted tilde is not expanded: {sentence}"
        );
    }

    #[test]
    fn a_path_below_a_plain_file_names_the_file() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("plain");
        std::fs::write(&file, "x").unwrap();
        let below = file.join("under").join("dotfiles");

        let UnlistedDotfilesDirectory::Absent { reason, .. } =
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
                UnlistedDotfilesDirectory::Absent { .. }
                | UnlistedDotfilesDirectory::OrdinarilyAbsent => "absent",
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
                sentence.contains("mkdir -p -- /nonexistent/dotfiles"),
                "{sentence}"
            );
        }
    }
}
