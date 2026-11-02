//! A fixture for every shape of a directory listing's error, for tests of how
//! its sentence is worded and shown.

use std::path::PathBuf;

use selfie::fs::{AbsentReason, DirectoryState};
use selfie::package::port::PackageListError;

use crate::permission_denied;

/// One [`DirectoryState`] of every shape its clause takes, each path in it
/// under `root`.
///
/// # Panics
///
/// Panics when a shape has no state here, so a shape the library adds fails
/// every test that asks for these until it has one.
#[must_use]
pub fn every_directory_state(root: &str) -> Vec<DirectoryState> {
    let at = |leaf: &str| PathBuf::from(format!("{root}/{leaf}"));
    let states = vec![
        DirectoryState::Directory,
        DirectoryState::Absent(AbsentReason::Empty),
        DirectoryState::Absent(AbsentReason::Occupied {
            kind: "regular file",
        }),
        DirectoryState::Absent(AbsentReason::DanglingSymlink {
            points_to: Some(at("gone")),
        }),
        DirectoryState::Absent(AbsentReason::DanglingSymlink { points_to: None }),
        DirectoryState::Absent(AbsentReason::ParentNotADirectory { parent: at("file") }),
        DirectoryState::Unlistable(permission_denied()),
        DirectoryState::Unknown(permission_denied()),
    ];
    // No catch-all, so a state the library adds fails to compile here until
    // it has a fixture.
    for state in &states {
        match state {
            DirectoryState::Directory
            | DirectoryState::Absent(
                AbsentReason::Empty
                | AbsentReason::Occupied { .. }
                | AbsentReason::DanglingSymlink { .. }
                | AbsentReason::ParentNotADirectory { .. },
            )
            | DirectoryState::Unlistable(_)
            | DirectoryState::Unknown(_) => {}
        }
    }
    states
}

/// The listing error for the directory `{root}/dots` in each state
/// [`every_directory_state`] gives, in that order.
#[must_use]
pub fn every_listing_error(root: &str) -> Vec<PackageListError> {
    every_directory_state(root)
        .into_iter()
        .map(|state| PackageListError::new(PathBuf::from(format!("{root}/dots")), state))
        .collect()
}
