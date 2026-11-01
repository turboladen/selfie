// Each sentence these values word, as its Display writes it, whole. The MCP
// server writes these Displays into its JSON, so a change of wording or of a
// path's place in a sentence fails here, where the sentence a user reads is
// stated in full.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use selfie::fs::target::StatePathError;
use selfie::fs::{AbsentReason, IoFailure};

// Asserts that `shown` are `expected`, one for one, in order.
fn assert_worded(shown: &[String], expected: &[&str]) {
    assert_eq!(shown.len(), expected.len(), "{shown:#?}");
    for (index, (shown, expected)) in shown.iter().zip(expected).enumerate() {
        assert_eq!(shown, expected, "sentence {index}");
    }
}

const SUCCESSES: &[&str] = &[
    "Package 'tool' check completed successfully in environment 'env'",
    "Package 'tool' audit completed clean in environment 'env'",
    "Audit completed for 3 package(s) in environment 'env': 1 with conflicts, 1 not installed, 0 could not be audited, 1 spec(s) left out",
    "Package 'tool' was already installed in environment 'env', at /p/tool.yml",
    "Package 'tool' installed in environment 'env'; could not answer for: tool (refused); selfie failed for: tool (an unexpected lookup error)",
    "Package 'tool' validation failed with 2 error(s) and 1 warning(s) in environment 'env'",
    "Package 'tool' spec info retrieved successfully",
    "Package 'tool' status checked in environment 'env'; could not answer for: tool (refused); selfie failed for: tool (an unexpected lookup error)",
    "Package listing completed with 2 valid package(s), 1 invalid package(s) and 1 refused package(s) in environment 'env'; could not answer for: tool (refused); selfie failed for: tool (an unexpected lookup error)",
    "Listed dotfiles across 2 package(s); could not answer for: tool (refused); selfie failed for: tool (an unexpected lookup error)",
    "Package 'tool' created at /p/tool.yml",
    "Package 'tool' updated successfully",
    "Package 'tool' removed from /p/tool.yml (had 1 dependent package(s))",
    "Spec listing completed with 2 valid spec(s) and 1 refused spec(s) in environment 'env'",
    "Spec validation completed in environment 'env': 3 package(s) validated, 1 with errors, 1 with warnings, 0 unparsable, 0 ambiguous or unlistable, 1 other warning(s)",
    "Dotfiles applied in environment 'env': 1 deployed, 1 skipped, 0 conflict(s), 1 refused, 1 failed, 1 orphaned",
    "Dotfile drift check in environment 'env': 1 drifted out of 2, 0 refused, 1 not verifiable, 1 orphaned, 1 not checked for orphans",
    "Now tracking '/p/tool.yml' in spec 'tool'",
    "Pushed 2 commits to remote",
    "Pulled 1 commit from remote (updated: tool, tool; removed: tool)",
    "Already up to date with remote",
    "Nothing to push — working tree is clean",
    "done",
];

const STATE_VALUES: &[&str] = &[
    "Cannot locate the deploy state file: state_directory '/s/state' is not an absolute path. Use an absolute path; a leading `~` or `~/` is expanded, but only when the home directory can be determined, and `~user` is not",
    "Cannot locate the deploy state file: no state_directory is configured and the home directory cannot be determined. Set state_directory in the config, or fix HOME",
    "Cannot use the deploy state: '/s/state' does not exist. Remove it, or point state_directory elsewhere",
    "Cannot use the deploy state: '/s/state' is not a directory, it is a regular file. Remove it, or point state_directory elsewhere",
    "Cannot use the deploy state: '/s/state' is a symlink to nothing: it points at /s/gone. Remove it, or point state_directory elsewhere",
    "Cannot use the deploy state: '/s/state' is a symlink to nothing. Remove it, or point state_directory elsewhere",
    "Cannot use the deploy state: '/s/state' is below /s/file, which is not a directory. Remove it, or point state_directory elsewhere",
    "Cannot use the deploy state: '/s/state' could not be checked: permission denied. Point state_directory at a directory selfie can read",
    "The deploy state cannot be written: the state directory '/s/state' does not accept a new file (permission denied). Make it writable, or point state_directory elsewhere",
    "The deploy state cannot be written: the state directory '/s/state' is not there, and selfie cannot create it under '/s/' (permission denied). Correct state_directory if that path is a typo, or make '/s/' writable",
    "The deploy state cannot be written: the state directory '/s/state' is not there, and selfie cannot create it under '/s/' (permission denied). Make '/s/' writable",
    "The deploy state cannot be written: the state directory '/s/state' does not accept a new file (IO error: permission denied). Make it writable, or point state_directory elsewhere",
    "The deploy state cannot be written: the state directory '/s/state' does not accept a new file (IO error: /s/deploy-state.yml: permission denied). Make it writable, or point state_directory elsewhere",
    "The deploy state cannot be written: the state directory '/s/state' does not accept a new file (Home directory not found). Make it writable, or point state_directory elsewhere",
    "The deploy state cannot be written: the state directory '/s/state' does not accept a new file (/s/deploy-state.yml: target is a symlink to '/s/elsewhere' and selfie will not write through it). Make it writable, or point state_directory elsewhere",
    "The deploy state cannot be written: the state directory '/s/state' does not accept a new file (/s/deploy-state.yml: target is a symlink and selfie will not write through it). Make it writable, or point state_directory elsewhere",
    "The deploy state cannot be written: the state directory '/s/state' does not accept a new file (/s/deploy-state.yml: target resolves to a fifo and selfie will not write to it). Make it writable, or point state_directory elsewhere",
    "The deploy state cannot be written: the state directory '/s/state' does not accept a new file (IO error: /s: permission denied). Make it writable, or point state_directory elsewhere",
    "The deploy state cannot be written: the state directory '/s/state' does not accept a new file (IO error: /s/deploy-state.yml: permission denied). Make it writable, or point state_directory elsewhere",
    "The deploy state cannot be written: Cannot serialize the deploy state: a value in it has a shape YAML cannot hold. Free the space or lift the lock that stops it, or point state_directory elsewhere",
    "The deploy state cannot be written: Cannot write deploy state '/s/deploy-state.yml': IO error: permission denied. Free the space or lift the lock that stops it, or point state_directory elsewhere",
    "The deploy state cannot be written: Cannot write deploy state '/s/deploy-state.yml': IO error: /s/deploy-state.yml: permission denied. Free the space or lift the lock that stops it, or point state_directory elsewhere",
    "The deploy state cannot be written: Cannot write deploy state '/s/deploy-state.yml': Home directory not found. Free the space or lift the lock that stops it, or point state_directory elsewhere",
    "The deploy state cannot be written: Cannot write deploy state '/s/deploy-state.yml': /s/deploy-state.yml: target is a symlink to '/s/elsewhere' and selfie will not write through it. Free the space or lift the lock that stops it, or point state_directory elsewhere",
    "The deploy state cannot be written: Cannot write deploy state '/s/deploy-state.yml': /s/deploy-state.yml: target is a symlink and selfie will not write through it. Free the space or lift the lock that stops it, or point state_directory elsewhere",
    "The deploy state cannot be written: Cannot write deploy state '/s/deploy-state.yml': /s/deploy-state.yml: target resolves to a fifo and selfie will not write to it. Free the space or lift the lock that stops it, or point state_directory elsewhere",
    "The deploy state cannot be written: Cannot write deploy state '/s/deploy-state.yml': IO error: /s: permission denied. Free the space or lift the lock that stops it, or point state_directory elsewhere",
    "The deploy state cannot be written: Cannot write deploy state '/s/deploy-state.yml': IO error: /s/deploy-state.yml: permission denied. Free the space or lift the lock that stops it, or point state_directory elsewhere",
    "The deploy state cannot be written: Cannot write deploy state '/s/deploy-state.yml': /s/: permission denied. Free the space or lift the lock that stops it, or point state_directory elsewhere",
    "Cannot read the deploy state: '/s/deploy-state.yml' is a fifo. Remove it, or point state_directory elsewhere",
    "Cannot read deploy state '/s/deploy-state.yml': IO error: /s/deploy-state.yml: permission denied. Fix the file's permissions, or move it aside",
    "Deploy state '/s/deploy-state.yml' is empty. Delete it to start over, or restore it from a backup",
    "Cannot parse deploy state '/s/deploy-state.yml': the file has the wrong shape, expected string scalar at line 1, column 7. Repair the file, or move it aside to start over",
    "state_directory '/s/state' is not there yet, so nothing shows as deployed; selfie creates it on the first write. Correct the setting if that path is a typo",
    "state_directory '/s/state' is not there yet, so nothing shows as deployed. The deploy state cannot be written: the state directory '/s/state' is not there, and selfie cannot create it under '/s/' (permission denied). Correct state_directory if that path is a typo, or make '/s/' writable",
    "Deploy state '/s/deploy-state.yml' is empty. Delete it to start over, or restore it from a backup; continuing as though nothing had been deployed",
    "The deploy state cannot be written: the state directory '/s/state' does not accept a new file (permission denied). Make it writable, or point state_directory elsewhere. A real run stops before its first write, including recording a file already in place",
    "Deployed '/s/target' but cannot record it: Cannot write deploy state '/s/deploy-state.yml': IO error: /s/deploy-state.yml: permission denied",
    "Found in sync '/s/target' but cannot record it: Cannot write deploy state '/s/deploy-state.yml': IO error: /s/deploy-state.yml: permission denied",
    "Could not tidy the deploy state after the run, so the next apply tries again: Cannot write deploy state '/s/deploy-state.yml': IO error: /s/deploy-state.yml: permission denied",
    "Deploy state '/s/deploy-state.yml' is empty. Delete it to start over, or restore it from a backup",
    "Stopped with nothing written. The deploy state cannot be written: the state directory '/s/state' does not accept a new file (permission denied). Make it writable, or point state_directory elsewhere",
    "Stopped after failing to record '/s/target' in the deploy state; the next run re-evaluates it once the state can be written",
    "Nothing was tracked. The deploy state cannot be written: the state directory '/s/state' does not accept a new file (permission denied). Make it writable, or point state_directory elsewhere",
    "Tracked '/s/target': the copy at '/s/copy' and the entry in '/s/tool.yml' are written. The deployment was not recorded: Cannot write deploy state '/s/deploy-state.yml': IO error: /s/deploy-state.yml: permission denied. Run `selfie apply tool` once the state file can be written, and it records the deployment.",
];

const CAUSES: &[&str] = &[
    "IO error: permission denied",
    "IO error: /s/deploy-state.yml: permission denied",
    "Home directory not found",
    "/s/deploy-state.yml: target is a symlink to '/s/elsewhere' and selfie will not write through it",
    "/s/deploy-state.yml: target is a symlink and selfie will not write through it",
    "/s/deploy-state.yml: target resolves to a fifo and selfie will not write to it",
    "IO error: /s: permission denied",
    "IO error: /s/deploy-state.yml: permission denied",
    "/s/: permission denied",
];

const OTHERS: &[&str] = &[
    "state_directory 's/state' is not an absolute path. Use an absolute path; a leading `~` or `~/` is expanded, but only when the home directory can be determined, and `~user` is not",
    "no state_directory is configured and the home directory cannot be determined. Set state_directory in the config, or fix HOME",
    "does not exist",
    "is not a directory, it is a regular file",
    "is a symlink to nothing: it points at /s/gone",
    "is a symlink to nothing",
    "is below /s/file, which is not a directory",
    "/s/deploy-state.yml: permission denied",
    "permission denied",
];

#[test]
fn every_success_summary_is_worded_as_pinned() {
    let shown: Vec<String> = test_common::every_success("env", "tool", "/p/tool.yml")
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_worded(&shown, SUCCESSES);
}

#[test]
fn every_deploy_state_sentence_is_worded_as_pinned() {
    let shown: Vec<String> = test_common::every_state_value("/s")
        .iter()
        .map(test_common::StateValue::library)
        .collect();
    assert_worded(&shown, STATE_VALUES);
}

#[test]
fn every_filesystem_error_is_worded_as_pinned() {
    let shown: Vec<String> = test_common::every_cause(&|leaf| PathBuf::from(format!("/s/{leaf}")))
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_worded(&shown, CAUSES);
}

#[test]
fn every_state_path_absence_and_io_sentence_is_worded_as_pinned() {
    let io = test_common::permission_denied;
    let shown = [
        StatePathError::NotAbsolute("s/state".into()).to_string(),
        StatePathError::NoHome.to_string(),
        AbsentReason::Empty.to_string(),
        AbsentReason::Occupied {
            kind: "regular file",
        }
        .to_string(),
        AbsentReason::DanglingSymlink {
            points_to: Some("/s/gone".into()),
        }
        .to_string(),
        AbsentReason::DanglingSymlink { points_to: None }.to_string(),
        AbsentReason::ParentNotADirectory {
            parent: "/s/file".into(),
        }
        .to_string(),
        IoFailure::at(Path::new("/s/deploy-state.yml"), io()).to_string(),
        IoFailure::from(io()).to_string(),
    ];
    assert_worded(&shown, OTHERS);
}

// A cause whose own text ends in a line break is embedded without it, so the
// sentence holding it goes on on the same line, for the MCP server and the
// CLI alike.
#[test]
fn a_cause_ending_in_a_line_break_is_embedded_without_it() {
    use selfie::dotfile_service::{StateLoadFailure, StateSaveError};
    use selfie::fs::FileSystemError;

    let io = || Arc::new(std::io::Error::other("disk full\n"));
    let shown = [
        StateSaveError::Write {
            path: "/s/deploy-state.yml".into(),
            cause: FileSystemError::IoError(IoFailure::at(Path::new("/s/deploy-state.yml"), io())),
        }
        .to_string(),
        StateLoadFailure::StateDirectoryNotWritable {
            directory: "/s/state".into(),
            blocked: "/s/state".into(),
            cause: Box::new(FileSystemError::CannotCreateIn {
                path: "/s/state".into(),
                cause: io(),
            }),
            configured: true,
        }
        .to_string(),
        FileSystemError::CannotCreateIn {
            path: "/s".into(),
            cause: io(),
        }
        .to_string(),
    ];
    assert_worded(
        &shown,
        &[
            "Cannot write deploy state '/s/deploy-state.yml': IO error: /s/deploy-state.yml: disk full",
            "The deploy state cannot be written: the state directory '/s/state' does not accept a new file (disk full). Make it writable, or point state_directory elsewhere",
            "/s: disk full",
        ],
    );
}

const LISTINGS: &[&str] = &[
    "/d/dots could not be listed",
    "/d/dots does not exist",
    "/d/dots is not a directory, it is a regular file",
    "/d/dots is a symlink to nothing: it points at /d/gone",
    "/d/dots is a symlink to nothing",
    "/d/dots is below /d/file, which is not a directory",
    "/d/dots could not be listed: permission denied",
    "/d/dots could not be checked: permission denied",
];

#[test]
fn every_listing_error_is_worded_as_pinned() {
    let shown: Vec<String> = test_common::every_listing_error("/d")
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_worded(&shown, LISTINGS);
}

// The two refusals of a dotfiles directory that would not list, as a warning
// carries them, and the name check's two refusals over a directory it could
// not read.
#[test]
fn every_directory_refusal_is_worded_as_pinned() {
    use selfie::namespace::NamespaceValidationError;
    use selfie::package::event::CollectionRefusal;

    let errors = test_common::every_listing_error("/d");
    let (absent, unlistable, unknown) = (&errors[1], &errors[6], &errors[7]);
    let shown = [
        CollectionRefusal::UnlistableDotfilesDirectory(unlistable.clone()).sentence(),
        CollectionRefusal::UncheckableDotfilesDirectory(unknown.clone()).sentence(),
        NamespaceValidationError::PackageDirectoryUnreadable(absent.clone()).to_string(),
        NamespaceValidationError::PackageDirectoryUnreadable(unlistable.clone()).to_string(),
        NamespaceValidationError::DotfilesDirectoryUnreadable(unknown.clone()).to_string(),
    ];
    assert_worded(
        &shown,
        &[
            "Failed to load standalone dotfiles: /d/dots could not be listed: permission denied",
            "Could not check the standalone dotfiles directory: /d/dots could not be checked: permission denied",
            "/d/dots does not exist",
            "cannot tell whether the name is already taken: /d/dots could not be listed: permission denied",
            "cannot tell whether the name is already taken: /d/dots could not be checked: permission denied",
        ],
    );
}
