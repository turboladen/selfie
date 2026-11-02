//! What selfie says about the standalone dotfiles directory.

use std::path::{Path, PathBuf};

use crate::{
    fs::{AbsentReason, DirectoryState},
    package::port::PackageListError,
    parts::{Composed, Parts, SuggestedCommand},
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
    /// A path that may hold standalone dotfiles selfie could not read.
    Unreadable(UnreadableDotfilesDirectory),
}

impl UnlistedDotfilesDirectory {
    /// Classify the dotfiles directory the failed listing in `error` was reading,
    /// where `configured` says whether the user set `dotfiles_directory`.
    ///
    /// Takes neither a file system nor a path: the error carries the directory it read
    /// and the state the repository found there, and asking again is what let one
    /// command say two things about one directory.
    pub(crate) fn classify(error: PackageListError, configured: bool) -> Self {
        // The state the repository classified, not a second look at the path. Two
        // looks can disagree, and the sentence a user reads then contradicts the
        // refusal that follows it.
        match error.state().clone() {
            // Being configured governs an **empty** path and nothing else. An absent
            // default is the ordinary condition of anyone who keeps no standalone
            // dotfiles, and a word about it on every run is noise. Anything else at
            // the path is a mistake: a plain file, or a link whose destination is
            // gone, did not get there by the setting being left out, and staying
            // silent about it hides the reason the directory is not being read.
            DirectoryState::Absent(AbsentReason::Empty) if !configured => Self::OrdinarilyAbsent,
            DirectoryState::Absent(reason) => Self::Absent {
                path: error.path().to_path_buf(),
                reason,
            },
            DirectoryState::Unlistable(_) => {
                Self::Unreadable(UnreadableDotfilesDirectory::Unlistable(error))
            }
            DirectoryState::Unknown(_) => {
                Self::Unreadable(UnreadableDotfilesDirectory::Uncheckable(error))
            }
            // A directory that classified cleanly and would not list. The listing
            // is the more recent answer and it failed, so it may be hiding entries.
            DirectoryState::Directory => {
                Self::Unreadable(UnreadableDotfilesDirectory::Unlistable(error))
            }
        }
    }

    /// Whether a command reading the standalone dotfiles refuses to go on: it
    /// cannot claim what it read is complete. An absent directory holds nothing,
    /// so a command warns about it and carries on.
    pub(crate) fn refuses_collection(&self) -> bool {
        matches!(self, Self::Unreadable(_))
    }
}

/// A standalone dotfiles directory selfie could not read through, so it may
/// hold standalone dotfiles a command did not see.
#[derive(Debug, Clone)]
pub enum UnreadableDotfilesDirectory {
    /// A directory that could not be listed.
    Unlistable(PackageListError),
    /// A path selfie could not classify, so nothing is known about it. Kept apart
    /// from an unlistable one because that one says a directory is there: this
    /// one cannot say even that.
    Uncheckable(PackageListError),
}

impl UnreadableDotfilesDirectory {
    /// The listing's error, which names the directory and says what is at it.
    #[must_use]
    pub fn error(&self) -> &PackageListError {
        match self {
            Self::Unlistable(error) | Self::Uncheckable(error) => error,
        }
    }
}

crate::parts::display_from_parts!(UnreadableDotfilesDirectory);

impl Composed for UnreadableDotfilesDirectory {
    fn parts(&self) -> Parts<'_> {
        match self {
            Self::Unlistable(error) => Parts::new()
                .text("Failed to load standalone dotfiles: ")
                .append(error.parts()),
            Self::Uncheckable(error) => Parts::new()
                .text("Could not check the standalone dotfiles directory: ")
                .append(error.parts()),
        }
    }
}

/// What a command reading the standalone dotfiles says about a directory it
/// could not use: one it carries on without, or one it could not read through.
#[derive(Debug, Clone)]
pub enum DotfilesDirectoryWarning {
    /// No directory is at the configured `path`, and `reason` says what is
    /// there instead. The command carries on without standalone dotfiles.
    Absent {
        /// The path selfie looked at.
        path: PathBuf,
        /// What is there instead of a directory.
        reason: AbsentReason,
    },
    /// A directory selfie could not read through. The command refuses what
    /// needs every spec, and says so.
    Unreadable(UnreadableDotfilesDirectory),
}

crate::parts::display_from_parts!(DotfilesDirectoryWarning);

impl Composed for DotfilesDirectoryWarning {
    fn parts(&self) -> Parts<'_> {
        match self {
            Self::Absent { path, reason } => Parts::new()
                .text("Dotfiles directory ")
                .path(path)
                .text(" ")
                .append(reason.parts())
                .text(" — standalone dotfiles will not be read.")
                .append(remedy(path, reason)),
            Self::Unreadable(unreadable) => unreadable.parts(),
        }
    }
}

/// The refusal to track a standalone dotfile into a dotfiles directory a
/// listing of it could not read: the listing's error says what is at it.
///
/// Every state refuses. A directory selfie could not read may hold the name
/// about to be tracked, and one it could not classify is no better known, so
/// neither can answer whether the name is free.
#[derive(Debug, Clone)]
pub struct TrackDirectoryRefusal(PackageListError);

impl TrackDirectoryRefusal {
    /// The refusal for the dotfiles directory `error` reports.
    #[must_use]
    pub fn new(error: PackageListError) -> Self {
        Self(error)
    }
}

crate::parts::display_from_parts!(TrackDirectoryRefusal);

impl Composed for TrackDirectoryRefusal {
    fn parts(&self) -> Parts<'_> {
        let error = &self.0;
        let parts = Parts::new().text("Cannot track a standalone dotfile: the dotfiles directory ");
        // The listing's own sentence starts with the path, so each arm that
        // shows it names the path no other way.
        match error.state() {
            DirectoryState::Absent(reason) => parts
                .path(error.path())
                .text(" ")
                .append(reason.parts())
                .text(".")
                .append(remedy(error.path(), reason)),
            // A directory that classified cleanly and still would not list could
            // not be listed. Only an unclassifiable path is unchecked.
            DirectoryState::Unlistable(_) | DirectoryState::Directory => parts
                .text("could not be listed, so it may already hold this name — ")
                .append(error.parts()),
            DirectoryState::Unknown(_) => {
                parts.text("could not be checked — ").append(error.parts())
            }
        }
    }
}

// The command that creates the directory at `path`, as a sentence of its own,
// or nothing when `reason` is not one `mkdir -p` fixes. `mkdir -p` fails with
// "File exists" against a plain file and "No such file or directory" against a
// dangling symlink, so offering it for those sends the user to a command that
// cannot work. A path no shell word can carry offers none either.
fn remedy(path: &Path, reason: &AbsentReason) -> Parts<'static> {
    match SuggestedCommand::make_directory(path).filter(|_| reason.creatable()) {
        Some(mkdir) => Parts::new().text(" Create it with: ").command(mkdir),
        None => Parts::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tempfile::tempdir;

    use super::*;
    use crate::fs::{FileSystem as _, RealFileSystem};

    // The warning a command sends about an absent directory at `path`.
    fn absent_warning(path: &Path, reason: &AbsentReason) -> String {
        DotfilesDirectoryWarning::Absent {
            path: path.to_path_buf(),
            reason: reason.clone(),
        }
        .to_string()
    }

    // The refusal of a track into the directory `error` reports.
    fn track_listing_refusal(error: &PackageListError) -> String {
        TrackDirectoryRefusal::new(error.clone()).to_string()
    }

    // The listing error a real repository builds for `path`: it classifies the path
    // itself, so a fixture cannot claim a state the file system would not produce.
    fn listing_error(path: &Path) -> PackageListError {
        PackageListError::new(path.to_path_buf(), RealFileSystem.directory_state(path))
    }

    fn unlistable_error(path: &Path) -> PackageListError {
        PackageListError::new(
            path.to_path_buf(),
            DirectoryState::Unlistable(Arc::new(std::io::Error::from(
                std::io::ErrorKind::PermissionDenied,
            ))),
        )
    }

    #[test]
    fn a_configured_directory_that_is_not_there_is_absent_at_its_path() {
        let dir = tempdir().unwrap();
        let absent = dir.path().join("dotfiles");

        match UnlistedDotfilesDirectory::classify(listing_error(&absent), true) {
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
            UnlistedDotfilesDirectory::classify(listing_error(&absent), false),
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
            UnlistedDotfilesDirectory::classify(listing_error(&file), false)
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
            UnlistedDotfilesDirectory::classify(listing_error(&link), false)
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
            UnlistedDotfilesDirectory::classify(listing_error(&link), true)
        else {
            panic!("expected a configured, absent directory");
        };
        assert!(matches!(reason, AbsentReason::DanglingSymlink { .. }));

        for sentence in [
            absent_warning(&link, &reason),
            track_listing_refusal(&PackageListError::new(
                link.clone(),
                DirectoryState::Absent(reason.clone()),
            )),
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
            UnlistedDotfilesDirectory::classify(listing_error(&file), true)
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
            UnlistedDotfilesDirectory::classify(listing_error(&below), true)
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
                    UnlistedDotfilesDirectory::classify(unlistable_error(dir.path()), configured),
                    UnlistedDotfilesDirectory::Unreadable(UnreadableDotfilesDirectory::Unlistable(
                        _
                    ))
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
        let error = PackageListError::from_listing(
            &RealFileSystem,
            link.clone(),
            &std::fs::read_dir(&link).unwrap_err(),
        );
        assert!(matches!(
            UnlistedDotfilesDirectory::classify(error.clone(), true),
            UnlistedDotfilesDirectory::Unreadable(UnreadableDotfilesDirectory::Uncheckable(_))
        ));

        let refusal = track_listing_refusal(&error);
        assert!(refusal.contains("could not be checked"), "{refusal}");
        assert!(!refusal.contains("mkdir"), "{refusal}");
    }

    // Every state refuses a track, because none of them can answer whether the
    // name is already taken.
    #[test]
    fn every_state_refuses_a_track() {
        let dir = tempdir().unwrap();

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
            let label = format!("{state:?}");
            let refusal =
                track_listing_refusal(&PackageListError::new(dir.path().to_path_buf(), state));
            assert!(
                refusal.starts_with("Cannot track a standalone dotfile:"),
                "{label}: {refusal}"
            );
        }
    }

    // A directory that cannot be listed may already hold the name, which is the
    // reason the refusal exists, so the sentence has to say it.
    #[test]
    fn an_unlistable_directory_says_it_may_already_hold_the_name() {
        let dir = tempdir().unwrap();

        let refusal = track_listing_refusal(&unlistable_error(dir.path()));

        assert!(refusal.contains("may already hold this name"), "{refusal}");
        assert!(!refusal.contains("mkdir"), "{refusal}");
    }

    // The agreement decision 1 actually needs, one layer above the port's own: for
    // every shape, what this function decides from a listing error and what the port
    // says about the same path must be the same fact. A mutation that mapped a loop
    // to unlistable in this function leaves the port's own agreement test green,
    // because that test compares the port's two constructors and never calls this one.
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
            let error = PackageListError::from_listing(
                &RealFileSystem,
                path.clone(),
                &std::fs::read_dir(&path).unwrap_err(),
            );
            let from_port = RealFileSystem.directory_state(&path);
            let classified = UnlistedDotfilesDirectory::classify(error, true);

            let port_name = match from_port {
                DirectoryState::Directory => "directory",
                DirectoryState::Absent(_) => "absent",
                DirectoryState::Unlistable(_) => "unlistable",
                DirectoryState::Unknown(_) => "unknown",
            };
            let service_name = match classified {
                UnlistedDotfilesDirectory::Absent { .. }
                | UnlistedDotfilesDirectory::OrdinarilyAbsent => "absent",
                UnlistedDotfilesDirectory::Unreadable(UnreadableDotfilesDirectory::Unlistable(
                    _,
                )) => "unlistable",
                UnlistedDotfilesDirectory::Unreadable(
                    UnreadableDotfilesDirectory::Uncheckable(_),
                ) => "unknown",
            };
            assert_eq!(
                port_name,
                service_name,
                "{} is {port_name} to the port and {service_name} to the service",
                path.display()
            );
        }
    }

    // Each sentence about a dotfiles directory a command cannot use, whole, for
    // every state the directory can be in.
    #[test]
    fn every_directory_sentence_is_worded_as_pinned() {
        let denied = || Arc::new(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        let errors: Vec<PackageListError> = [
            DirectoryState::Directory,
            DirectoryState::Absent(AbsentReason::Empty),
            DirectoryState::Absent(AbsentReason::Occupied {
                kind: "regular file",
            }),
            DirectoryState::Absent(AbsentReason::DanglingSymlink {
                points_to: Some("/d/gone".into()),
            }),
            DirectoryState::Absent(AbsentReason::DanglingSymlink { points_to: None }),
            DirectoryState::Absent(AbsentReason::ParentNotADirectory {
                parent: "/d/file".into(),
            }),
            DirectoryState::Unlistable(denied()),
            DirectoryState::Unknown(denied()),
        ]
        .into_iter()
        .map(|state| PackageListError::new("/d/dots".into(), state))
        .collect();
        let mut shown = Vec::new();
        for error in &errors {
            if let DirectoryState::Absent(reason) = error.state() {
                shown.push(absent_warning(error.path(), reason));
                shown.push(track_listing_refusal(error));
            }
        }
        shown.extend(errors.iter().map(track_listing_refusal));

        let expected = [
            "Dotfiles directory /d/dots does not exist — standalone dotfiles will not be read. Create it with: mkdir -p -- /d/dots",
            "Cannot track a standalone dotfile: the dotfiles directory /d/dots does not exist. Create it with: mkdir -p -- /d/dots",
            "Dotfiles directory /d/dots is not a directory, it is a regular file — standalone dotfiles will not be read.",
            "Cannot track a standalone dotfile: the dotfiles directory /d/dots is not a directory, it is a regular file.",
            "Dotfiles directory /d/dots is a symlink to nothing: it points at /d/gone — standalone dotfiles will not be read.",
            "Cannot track a standalone dotfile: the dotfiles directory /d/dots is a symlink to nothing: it points at /d/gone.",
            "Dotfiles directory /d/dots is a symlink to nothing — standalone dotfiles will not be read.",
            "Cannot track a standalone dotfile: the dotfiles directory /d/dots is a symlink to nothing.",
            "Dotfiles directory /d/dots is below /d/file, which is not a directory — standalone dotfiles will not be read.",
            "Cannot track a standalone dotfile: the dotfiles directory /d/dots is below /d/file, which is not a directory.",
            "Cannot track a standalone dotfile: the dotfiles directory could not be listed, so it may already hold this name — /d/dots could not be listed",
            "Cannot track a standalone dotfile: the dotfiles directory /d/dots does not exist. Create it with: mkdir -p -- /d/dots",
            "Cannot track a standalone dotfile: the dotfiles directory /d/dots is not a directory, it is a regular file.",
            "Cannot track a standalone dotfile: the dotfiles directory /d/dots is a symlink to nothing: it points at /d/gone.",
            "Cannot track a standalone dotfile: the dotfiles directory /d/dots is a symlink to nothing.",
            "Cannot track a standalone dotfile: the dotfiles directory /d/dots is below /d/file, which is not a directory.",
            "Cannot track a standalone dotfile: the dotfiles directory could not be listed, so it may already hold this name — /d/dots could not be listed: permission denied",
            "Cannot track a standalone dotfile: the dotfiles directory could not be checked — /d/dots could not be checked: permission denied",
        ];
        assert_eq!(shown.len(), expected.len(), "{shown:#?}");
        for (shown, expected) in shown.iter().zip(expected) {
            assert_eq!(shown, expected);
        }
    }

    // Apply warns and track refuses about the same directory in one session, so
    // the two must say different things while naming the same fix.
    #[test]
    fn the_warning_and_the_refusal_do_not_say_the_same_thing() {
        let dir = Path::new("/nonexistent/dotfiles");

        let warning = absent_warning(dir, &AbsentReason::Empty);
        let refusal = track_listing_refusal(&PackageListError::new(
            dir.to_path_buf(),
            DirectoryState::Absent(AbsentReason::Empty),
        ));

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
