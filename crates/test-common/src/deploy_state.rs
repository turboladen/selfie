//! A value of every shape the deploy state's sentences take, for tests of how
//! those sentences are worded and shown.

use std::path::PathBuf;
use std::sync::Arc;

use selfie::dotfile_service::{
    DeployStateFailure, DeployStateWarning, Recorded, StateLoadFailure, StateSaveError,
};
use selfie::fs::target::StatePathError;
use selfie::fs::{AbsentReason, FileSystemError, IoFailure};

/// An IO error that says permission was denied, as the deploy state's
/// fixtures carry it.
#[must_use]
pub fn permission_denied() -> Arc<std::io::Error> {
    Arc::new(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
}

/// One deploy-state value: a failure to load, a warning, or a failure a
/// recording command stopped on.
#[derive(Debug)]
pub enum StateValue {
    Load(StateLoadFailure),
    Warning(DeployStateWarning),
    Failure(DeployStateFailure),
}

impl StateValue {
    /// The value's sentence, as the library's `Display` words it.
    #[must_use]
    pub fn library(&self) -> String {
        match self {
            Self::Load(failure) => failure.to_string(),
            Self::Warning(warning) => warning.to_string(),
            Self::Failure(failure) => failure.to_string(),
        }
    }

    // Which shape this is: one per branch a sentence's wording takes, not one
    // per variant. No match here has a wildcard, so a variant the library adds
    // to any of these types, a cause included, fails to build here, and
    // `STATE_SHAPES` counts the arms, so a shape with no fixture fails
    // `every_state_value`.
    fn shape(&self) -> usize {
        use DeployStateFailure as F;
        use DeployStateWarning as W;
        use StateLoadFailure as L;
        let save = |error: &StateSaveError| match error {
            StateSaveError::Serialize(_) => 0,
            StateSaveError::Write { cause, .. } => 1 + cause_shape(cause),
        };
        let load = |k| shape_in("load", LOAD, LOAD_SHAPES, k);
        let file = |k| shape_in("file", FILE, FILE_SHAPES, k);
        let warning = |k| shape_in("warning", WARNING, WARNING_SHAPES, k);
        let failure = |k| shape_in("failure", FAILURE, FAILURE_SHAPES, k);
        match self {
            Self::Load(L::Locate(StatePathError::NotAbsolute(_))) => load(0),
            Self::Load(L::Locate(StatePathError::NoHome)) => load(1),
            Self::Load(L::StateDirectoryOccupied { reason, .. }) => match reason {
                AbsentReason::Empty => load(2),
                AbsentReason::Occupied { .. } => load(3),
                AbsentReason::DanglingSymlink { points_to: Some(_) } => load(4),
                AbsentReason::DanglingSymlink { points_to: None } => load(5),
                AbsentReason::ParentNotADirectory { .. } => load(6),
            },
            Self::Load(L::StateDirectoryUnreadable { .. }) => load(7),
            Self::Load(L::StateDirectoryNotWritable {
                directory,
                blocked,
                cause,
                configured,
            }) => match (&**cause, directory == blocked, configured) {
                (FileSystemError::CannotCreateIn { .. }, true, _) => load(8),
                (FileSystemError::CannotCreateIn { .. }, false, true) => load(9),
                (FileSystemError::CannotCreateIn { .. }, false, false) => load(10),
                // Every other refusal, each its own shape.
                (refusal, _, _) => shape_in("refusal", REFUSAL, CAUSE_SHAPES, cause_shape(refusal)),
            },
            Self::Load(L::StateNotWritable(error)) => {
                shape_in("save", SAVE, SAVE_SHAPES, save(error))
            }
            Self::Load(L::Irregular { .. }) => file(0),
            Self::Load(L::Read { .. }) => file(1),
            Self::Load(L::Empty { .. }) => file(2),
            Self::Load(L::Parse { .. }) => file(3),
            Self::Warning(W::NotThereYet {
                cannot_create: None,
                ..
            }) => warning(0),
            Self::Warning(W::NotThereYet {
                cannot_create: Some(_),
                ..
            }) => warning(1),
            Self::Warning(W::ReadingPast(_)) => warning(2),
            Self::Warning(W::DryRunWouldStop(_)) => warning(3),
            Self::Warning(W::NotRecorded {
                recorded: Recorded::Deployed,
                ..
            }) => warning(4),
            Self::Warning(W::NotRecorded {
                recorded: Recorded::InSync,
                ..
            }) => warning(5),
            Self::Warning(W::Untidied(_)) => warning(6),
            Self::Failure(F::Unusable(_)) => failure(0),
            Self::Failure(F::StoppedBeforeWriting(_)) => failure(1),
            Self::Failure(F::StoppedAfterRecording { .. }) => failure(2),
            Self::Failure(F::NothingTracked(_)) => failure(3),
            Self::Failure(F::TrackUnrecorded { .. }) => failure(4),
        }
    }
}

// A cause's shape, one per FileSystemError variant and per whether an IO
// error names a path or a symlink refusal names its destination.
fn cause_shape(cause: &FileSystemError) -> usize {
    match cause {
        FileSystemError::IoError(IoFailure { path: None, .. }) => 0,
        FileSystemError::IoError(IoFailure { path: Some(_), .. }) => 1,
        FileSystemError::HomeDirNotFound => 2,
        FileSystemError::SymlinkedTarget {
            points_to: Some(_), ..
        } => 3,
        FileSystemError::SymlinkedTarget {
            points_to: None, ..
        } => 4,
        FileSystemError::IrregularTarget { .. } => 5,
        FileSystemError::BelowNonDirectory { .. } => 6,
        FileSystemError::DirectoryTarget { .. } => 7,
        FileSystemError::CannotCreateIn { .. } => 8,
    }
}

// The shape `k` of the block named `block`, which starts at `start` and holds
// `size` shapes. A shape past its block's size is a block that has grown
// without its size, and is named here rather than overlapping the next.
fn shape_in(block: &str, start: usize, size: usize, k: usize) -> usize {
    assert!(
        k < size,
        "shape {k} of the {block} block is past its size, {size}"
    );
    start + k
}

// Each block's size, and where it starts: right after the block before it.
const CAUSE_SHAPES: usize = 9;
const SAVE_SHAPES: usize = 1 + CAUSE_SHAPES;
const LOAD_SHAPES: usize = 11;
const FILE_SHAPES: usize = 4;
const WARNING_SHAPES: usize = 7;
const FAILURE_SHAPES: usize = 5;
const LOAD: usize = 0;
const SAVE: usize = LOAD + LOAD_SHAPES;
const FILE: usize = SAVE + SAVE_SHAPES;
const WARNING: usize = FILE + FILE_SHAPES;
const FAILURE: usize = WARNING + WARNING_SHAPES;
const REFUSAL: usize = FAILURE + FAILURE_SHAPES;
// A directory's refusal is never CannotCreateIn, whose shapes are the load
// block's, so the refusal block has one shape fewer than a cause.
const STATE_SHAPES: usize = REFUSAL + CAUSE_SHAPES - 1;

/// One `FileSystemError` of each shape: each variant, and an IO error with a
/// path and without one, and a symlink refusal with a destination and without
/// one. Each path in it is under `at`.
#[must_use]
pub fn every_cause(at: &dyn Fn(&str) -> PathBuf) -> Vec<FileSystemError> {
    let io = permission_denied;
    vec![
        FileSystemError::IoError(io().into()),
        FileSystemError::IoError(IoFailure::at(&at("deploy-state.yml"), io())),
        FileSystemError::HomeDirNotFound,
        FileSystemError::SymlinkedTarget {
            path: at("deploy-state.yml"),
            points_to: Some(at("elsewhere")),
        },
        FileSystemError::SymlinkedTarget {
            path: at("deploy-state.yml"),
            points_to: None,
        },
        FileSystemError::IrregularTarget {
            path: at("deploy-state.yml"),
            kind: "fifo",
        },
        FileSystemError::BelowNonDirectory {
            path: at("deploy-state.yml"),
            parent: Some(at("file")),
            cause: io(),
        },
        FileSystemError::DirectoryTarget {
            path: at("deploy-state.yml"),
            cause: io(),
        },
        FileSystemError::CannotCreateIn {
            path: at(""),
            cause: io(),
        },
    ]
}

/// One deploy-state value of every shape its sentence takes, each path in it
/// under `root`.
///
/// # Panics
///
/// Panics when a shape has no value here, so a shape the library adds fails
/// every test that asks for these until it has one.
#[must_use]
pub fn every_state_value(root: &str) -> Vec<StateValue> {
    let at = |leaf: &str| PathBuf::from(format!("{root}/{leaf}"));
    let io = permission_denied;
    // This is a failed write's error, naming the state file as a real one
    // does.
    let failed_write = || FileSystemError::IoError(IoFailure::at(&at("deploy-state.yml"), io()));
    let parse = selfie::yaml::parse::<selfie::package::Package>("name: [\n")
        .expect_err("the fixture must not parse");
    let write = |cause| StateSaveError::Write {
        path: at("deploy-state.yml"),
        cause,
    };
    let saves = || {
        std::iter::once(StateSaveError::Serialize(
            selfie::yaml::SerializeFailure::Value,
        ))
        .chain(every_cause(&at).into_iter().map(write))
    };
    let unusable = || StateLoadFailure::Empty {
        path: at("deploy-state.yml"),
    };
    let not_writable = |blocked: &str, configured| StateLoadFailure::StateDirectoryNotWritable {
        directory: at("state"),
        blocked: at(blocked),
        cause: Box::new(FileSystemError::CannotCreateIn {
            path: at(blocked),
            cause: io(),
        }),
        configured,
    };
    let mut values = vec![
        StateLoadFailure::Locate(StatePathError::NotAbsolute(at("state"))),
        StateLoadFailure::Locate(StatePathError::NoHome),
    ];
    values.extend(
        [
            AbsentReason::Empty,
            AbsentReason::Occupied {
                kind: "regular file",
            },
            AbsentReason::DanglingSymlink {
                points_to: Some(at("gone")),
            },
            AbsentReason::DanglingSymlink { points_to: None },
            AbsentReason::ParentNotADirectory { parent: at("file") },
        ]
        .into_iter()
        .map(|reason| StateLoadFailure::StateDirectoryOccupied {
            path: at("state"),
            reason,
        }),
    );
    values.extend([
        StateLoadFailure::StateDirectoryUnreadable {
            path: at("state"),
            why: "could not be checked: permission denied".to_string(),
        },
        not_writable("state", true),
        not_writable("", true),
        not_writable("", false),
    ]);
    values.extend(
        every_cause(&at)
            .into_iter()
            .filter(|cause| !matches!(cause, FileSystemError::CannotCreateIn { .. }))
            .map(|cause| StateLoadFailure::StateDirectoryNotWritable {
                directory: at("state"),
                blocked: at("state"),
                cause: Box::new(cause),
                configured: true,
            }),
    );
    values.extend(saves().map(StateLoadFailure::StateNotWritable));
    values.extend([
        StateLoadFailure::Irregular {
            path: at("deploy-state.yml"),
            kind: "fifo",
        },
        StateLoadFailure::Read {
            path: at("deploy-state.yml"),
            cause: failed_write(),
        },
        unusable(),
        StateLoadFailure::Parse {
            path: at("deploy-state.yml"),
            cause: parse,
        },
    ]);
    let mut all: Vec<StateValue> = values.into_iter().map(StateValue::Load).collect();
    all.extend(
        [
            DeployStateWarning::NotThereYet {
                directory: at("state"),
                cannot_create: None,
            },
            DeployStateWarning::NotThereYet {
                directory: at("state"),
                cannot_create: Some(not_writable("", true)),
            },
            DeployStateWarning::ReadingPast(unusable()),
            DeployStateWarning::DryRunWouldStop(not_writable("state", false)),
            DeployStateWarning::NotRecorded {
                recorded: Recorded::Deployed,
                target: at("target"),
                error: write(failed_write()),
            },
            DeployStateWarning::NotRecorded {
                recorded: Recorded::InSync,
                target: at("target"),
                error: write(failed_write()),
            },
            DeployStateWarning::Untidied(write(failed_write())),
        ]
        .into_iter()
        .map(StateValue::Warning),
    );
    all.extend(
        [
            DeployStateFailure::Unusable(unusable()),
            DeployStateFailure::StoppedBeforeWriting(not_writable("state", true)),
            DeployStateFailure::StoppedAfterRecording {
                target: at("target"),
            },
            DeployStateFailure::NothingTracked(not_writable("state", true)),
            DeployStateFailure::TrackUnrecorded {
                error: write(failed_write()),
                name: "tool".to_string(),
                target: format!("{root}/target"),
                spec_path: at("tool.yml"),
                copy: at("copy"),
            },
        ]
        .into_iter()
        .map(StateValue::Failure),
    );
    let mut shapes: Vec<usize> = all.iter().map(StateValue::shape).collect();
    shapes.sort_unstable();
    shapes.dedup();
    assert_eq!(
        shapes,
        (0..STATE_SHAPES).collect::<Vec<_>>(),
        "every deploy-state shape needs a value in every_state_value"
    );
    all
}
