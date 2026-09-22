//! The deploy state file: where it lives, loading it, and writing it back.
//!
//! [`LoadedState`] is what separates the two halves. Only [`load_deploy_state`]
//! builds one and [`save_deploy_state`] accepts nothing else, so a state file
//! selfie could not read is never written over: a caller that did not get a
//! usable load has no value to hand the writer.

use std::path::PathBuf;

use thiserror::Error;

use crate::{
    config::SelfieConfig,
    dotfile_service::state::DeployState,
    fs::{
        AbsentReason, DirectoryState,
        filesystem::{FileSystem, FileSystemError},
        target::{StatePathError, TargetPath, state_file_path},
    },
    yaml::ParseFailure,
};

const DEPLOY_STATE_FILENAME: &str = "deploy-state.yml";

/// Where copies of overwritten targets live, relative to the state directory.
const BACKUPS_DIRNAME: &str = "backups";

/// A deploy state read from disk, or found absent, which may be written back.
///
/// An absent file and a loaded one are the same case here: both may be saved,
/// and a first run has nothing on disk to protect.
pub(super) struct LoadedState {
    path: TargetPath,
    state: DeployState,
    directory_warning: Option<String>,
}

impl LoadedState {
    pub(super) fn state(&self) -> &DeployState {
        &self.state
    }

    /// What to tell the user about the directory this state came from, or `None`.
    ///
    /// Present when a configured `state_directory` is not there. Every caller sends
    /// it, because the load succeeds either way and an empty state that arrives
    /// silently is indistinguishable from a machine where nothing is deployed.
    pub(super) fn directory_warning(&self) -> Option<&str> {
        self.directory_warning.as_deref()
    }

    /// Where a deploy keeps a copy of a target it is about to overwrite.
    pub(super) fn backups_root(&self) -> PathBuf {
        // Derived from the loaded path, not resolved again: `state_file_path`
        // joins a separator-free filename, so replacing it names the directory
        // that same resolution produced.
        self.path.path().with_file_name(BACKUPS_DIRNAME)
    }

    pub(super) fn state_mut(&mut self) -> &mut DeployState {
        &mut self.state
    }

    /// The state alone, for a caller that only reads it.
    pub(super) fn into_state(self) -> DeployState {
        self.state
    }
}

/// What loading the deploy state produced.
pub(super) enum StateLoad {
    /// A state that may be read and written back.
    Usable(LoadedState),
    /// A file selfie could not use, or no location to look for one.
    Unusable(StateLoadFailure),
}

/// Why the deploy state could not be used. Each message names its remedy.
#[derive(Debug, Error)]
pub(super) enum StateLoadFailure {
    /// The file has no location.
    #[error("Cannot locate the deploy state file: {0}")]
    Locate(#[source] StatePathError),
    /// Something that is not a directory occupies the state directory's path.
    #[error(
        "Cannot use the deploy state: '{}' {what}. Remove it, or point state_directory elsewhere",
        .path.display()
    )]
    StateDirectoryOccupied { path: PathBuf, what: String },
    /// The state directory could not be read or could not be classified, so
    /// selfie cannot tell whether a state it must not overwrite is in there.
    #[error(
        "Cannot use the deploy state: '{}' {why}. Point state_directory at a directory selfie can read",
        .path.display()
    )]
    StateDirectoryUnreadable { path: PathBuf, why: String },
    /// Something that is not a regular file sits at the path.
    #[error(
        "Cannot read the deploy state: '{}' is a {kind}. Remove it, or point state_directory elsewhere",
        .path.display()
    )]
    Irregular { path: PathBuf, kind: &'static str },
    /// The file exists and could not be read.
    #[error(
        "Cannot read deploy state '{}': {source}. Fix the file's permissions, or move it aside",
        .path.display()
    )]
    Read {
        path: PathBuf,
        #[source]
        source: FileSystemError,
    },
    /// The file exists and holds nothing.
    #[error(
        "Deploy state '{}' is empty. Delete it to start over, or restore it from a backup",
        .path.display()
    )]
    Empty { path: PathBuf },
    /// The file exists and is not a deploy state selfie can read.
    #[error(
        "Cannot parse deploy state '{}': {source}. Repair the file, or move it aside to start over",
        .path.display()
    )]
    Parse {
        path: PathBuf,
        #[source]
        source: ParseFailure,
    },
}

/// Why the deploy state could not be written.
#[derive(Debug, Error)]
pub(super) enum StateSaveError {
    /// selfie's own state would not serialize: a bug, not a filesystem condition.
    #[error("Cannot serialize the deploy state: {0}")]
    Serialize(#[source] serde_saphyr::SerializeError),
    /// The write failed.
    #[error("Cannot write deploy state '{}': {source}", .path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: FileSystemError,
    },
}

/// How a command that only reads the state reports an unusable one.
pub(super) fn read_only_state_warning(failure: &StateLoadFailure) -> String {
    format!("{failure}; continuing as though nothing had been deployed")
}

/// Where the deploy state lives, with a warning about its directory when one is
/// worth making.
fn deploy_state_path<F: FileSystem>(
    filesystem: &F,
    config: &SelfieConfig,
) -> Result<(TargetPath, Option<String>), StateLoadFailure> {
    let path = state_file_path(
        filesystem,
        config.state_directory().map(PathBuf::as_path),
        DEPLOY_STATE_FILENAME,
    )
    .map_err(StateLoadFailure::Locate)?;
    // The state directory is selfie's own, so selfie creates it — whether the user
    // configured the path or took the default. `write_file_private` creates the
    // parent on the first write, so an absent directory needs nothing done here
    // and a read before that first write has nothing to read either way.
    //
    // Checked after the path rule above, so a relative value is reported as
    // relative rather than as absent. The directory is the resolved file's parent
    // rather than the configured value, so the default gets the same checks: a file
    // in the way would otherwise surface as a write error after the run had done
    // its work.
    let directory = path.path().parent().unwrap_or_else(|| path.path());
    match filesystem.directory_state(directory) {
        DirectoryState::Directory => Ok((path, None)),
        // Nothing is there, and the first write creates it. Said out loud when the
        // user named the path, because the two ways to reach this are a first run and
        // a typo, and they are indistinguishable from the run's output otherwise: a
        // mistyped directory reports every deployed dotfile as untracked, and an
        // `apply -y` then overwrites edited targets instead of reporting conflicts.
        // An unnamed default needs no warning, since a first run is its ordinary
        // state and nothing was typed to get it wrong.
        DirectoryState::Absent(AbsentReason::Empty) => {
            let warning = config.state_directory().map(|_| {
                format!(
                    "state_directory '{}' is not there yet, so nothing shows as deployed; \
                     selfie creates it on the first write. Correct the setting if that path \
                     is a typo",
                    directory.display()
                )
            });
            Ok((path, warning))
        }
        // Creating cannot help: something else holds the path. `create_dir_all`
        // fails here, and it would fail after the run had already deployed.
        DirectoryState::Absent(reason) => Err(StateLoadFailure::StateDirectoryOccupied {
            path: directory.to_path_buf(),
            what: occupied_clause(&reason),
        }),
        // A path selfie cannot classify may hold a state it must not overwrite, so it
        // refuses rather than starting from an empty one and writing over whatever is
        // there.
        //
        // `Unlistable` is grouped in to keep the match total, not because this call
        // can produce it: only a listing discovers that a directory will not open its
        // entries, and this stats the path. A `0o000` state directory is a
        // `Directory` here and is refused by the read that follows, which names the
        // state file rather than the directory.
        DirectoryState::Unlistable(error) | DirectoryState::Unknown(error) => {
            Err(StateLoadFailure::StateDirectoryUnreadable {
                path: directory.to_path_buf(),
                why: format!("could not be checked: {error}"),
            })
        }
    }
}

/// What is at the state directory's path instead of a directory.
fn occupied_clause(reason: &AbsentReason) -> String {
    match reason {
        AbsentReason::Empty => "is not there".to_string(),
        AbsentReason::Occupied { kind } => format!("is a {kind}"),
        AbsentReason::DanglingSymlink { points_to } => match points_to {
            Some(destination) => format!(
                "is a symlink to nothing: it points at {}",
                destination.display()
            ),
            None => "is a symlink to nothing".to_string(),
        },
        AbsentReason::ParentNotADirectory { parent } => {
            format!("is below {}, which is not a directory", parent.display())
        }
    }
}

/// Load the deploy state.
///
/// An absent file is the ordinary first run and is usable with nothing in it.
/// A file that cannot be located, read or parsed, or that is empty, is
/// unusable, and the failure says which because the fixes differ.
pub(super) fn load_deploy_state<F: FileSystem>(filesystem: &F, config: &SelfieConfig) -> StateLoad {
    let (path, directory_warning) = match deploy_state_path(filesystem, config) {
        Ok(pair) => pair,
        Err(failure) => return StateLoad::Unusable(failure),
    };
    // Ahead of the read: `read_file` opens the path, and opening a fifo blocks
    // until a writer arrives, which hangs every dotfile command before it does
    // any work. A missing path answers `None` here, so an absent file goes on
    // to the read and is told apart from an unreadable one by the error kind.
    //
    // There is no existence probe: the port's `path_exists` answers false for
    // any path it cannot stat, which read a state file beneath an inaccessible
    // parent as absent and let a run deploy what it could never record.
    match filesystem.irregular_target_refusal(&path) {
        Some(FileSystemError::IrregularTarget { path, kind }) => {
            return StateLoad::Unusable(StateLoadFailure::Irregular { path, kind });
        }
        Some(source) => {
            return StateLoad::Unusable(StateLoadFailure::Read {
                path: path.path().to_path_buf(),
                source,
            });
        }
        None => {}
    }
    let content = match filesystem.read_file(path.path()) {
        Ok(content) => content,
        // A file that is not there is the ordinary first run. Only that one
        // condition is absence; a permission failure on the file or any parent
        // is a file selfie cannot read.
        Err(FileSystemError::IoError(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return StateLoad::Usable(LoadedState {
                path,
                state: DeployState::empty(),
                directory_warning,
            });
        }
        Err(source) => {
            return StateLoad::Unusable(StateLoadFailure::Read {
                path: path.path().to_path_buf(),
                source,
            });
        }
    };

    // A zero-length file is what a rename lost to a crash leaves behind. It is
    // checked before the parse because the parser's sentence for it describes a
    // shape problem, while the remedy is to delete or restore the file.
    if content.trim().is_empty() {
        return StateLoad::Unusable(StateLoadFailure::Empty {
            path: path.path().to_path_buf(),
        });
    }

    // The failure's message reaches the MCP server's JSON. No credential can be
    // in it -- secret-bearing entries record nothing (ADR-0003) -- but the file
    // names every repository-file dotfile on the machine, each with a checksum,
    // which is why `save_deploy_state` writes it owner-only.
    //
    // `crate::yaml::parse` is what keeps the file's keys and values out:
    // serde-saphyr's `Display` interpolates parsed content into several messages,
    // and the duplicate-key one quotes the key. A line, a column, and the single
    // character a scanner failure stopped on still get through; the reasoning for
    // accepting those is on `ParseFailure`.
    match crate::yaml::parse(&content) {
        Ok(state) => StateLoad::Usable(LoadedState {
            path,
            state,
            directory_warning,
        }),
        Err(source) => StateLoad::Unusable(StateLoadFailure::Parse {
            path: path.path().to_path_buf(),
            source,
        }),
    }
}

/// Write the deploy state, owner-only.
///
/// Owner-only because the file names every repository-file dotfile selfie
/// manages here, with checksums: no credentials, but a reconnaissance aid on a
/// shared host.
///
/// # Errors
///
/// [`StateSaveError`] if the state cannot be serialized or the write fails.
pub(super) fn save_deploy_state<F: FileSystem>(
    filesystem: &F,
    loaded: &LoadedState,
) -> Result<(), StateSaveError> {
    // Deliberately less durable than `write_file_no_follow`: both sync the file's
    // data before renaming, and `write_file_private` skips the directory fsync,
    // which is the safe direction. Losing this file costs nothing -- the next run
    // re-derives it.
    //
    // Do not "fix" that by syncing harder. A record only lies when it outlives the
    // write it describes, so making the state survive a crash the target write did
    // not would widen that window. The ordering is established at the other end:
    // `write_file_no_follow` is durable before `record_deployment` runs (selfie-aub).
    let yaml = serde_saphyr::to_string(&loaded.state).map_err(StateSaveError::Serialize)?;
    filesystem
        .write_file_private(&loaded.path, yaml.as_bytes())
        .map_err(|source| StateSaveError::Write {
            path: loaded.path.path().to_path_buf(),
            source,
        })
}

// What `load_deploy_state` reports, and what it stays quiet about.
//
// At this layer rather than through a service, because the path-resolution branch
// is unreachable from an integration test: those always configure a
// `state_directory`, and only an unset one with no determinable home reaches it.
// That the outcomes actually leave the library as events and results is covered
// per command in `tests/dotfile_service_tests.rs`.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SelfieConfigBuilder;
    use crate::fs::{AbsentReason, DirectoryState, MockFileSystem};

    const STATE_DIR: &str = "/state";
    const STATE_FILE: &str = "/state/deploy-state.yml";

    fn config_with_state_dir() -> SelfieConfig {
        SelfieConfigBuilder::default()
            .environment("test")
            .package_directory("/packages")
            .state_directory(PathBuf::from(STATE_DIR))
            .build()
    }

    // A filesystem on which the state directory is a directory. Every test whose
    // subject is the state file rather than the directory starts here.
    fn under_a_state_directory() -> MockFileSystem {
        let mut fs = MockFileSystem::default();
        fs.mock_directories_exist();
        fs
    }

    // A configured directory that is not there is selfie's to create, exactly as
    // the default is, so the load is usable and empty and the first write creates
    // the directory. This is the behavior ADR-0005 decision 8.2 settles: refusing
    // here made a configured `state_directory` a setting the user had to create by
    // hand before any dotfile command would run.
    #[test]
    fn a_configured_state_directory_that_is_not_there_is_created_rather_than_refused() {
        let mut fs = MockFileSystem::default();
        fs.mock_directory_state(DirectoryState::Absent(AbsentReason::Empty));
        fs.mock_no_irregular_files();
        fs.expect_read_file().returning(|_| {
            Err(FileSystemError::IoError(std::sync::Arc::new(
                std::io::Error::new(std::io::ErrorKind::NotFound, "No such file"),
            )))
        });

        let state = state_of(load_deploy_state(&fs, &config_with_state_dir()));

        assert!(
            state.entries().is_empty(),
            "a directory that is not there holds no state: {state:?}"
        );
    }

    // Creating it is right, and saying nothing about it is not: a mistyped
    // `state_directory` reaches exactly this arm, and the run then reports every
    // deployed dotfile as untracked. The warning names the path and the setting, so
    // the two ways to get here can be told apart from the output.
    #[test]
    fn a_configured_state_directory_that_is_not_there_is_warned_about() {
        let mut fs = MockFileSystem::default();
        fs.mock_directory_state(DirectoryState::Absent(AbsentReason::Empty));
        fs.mock_no_irregular_files();
        fs.expect_read_file().returning(|_| {
            Err(FileSystemError::IoError(std::sync::Arc::new(
                std::io::Error::new(std::io::ErrorKind::NotFound, "No such file"),
            )))
        });

        let StateLoad::Usable(loaded) = load_deploy_state(&fs, &config_with_state_dir()) else {
            panic!("a directory that is not there is selfie's to create");
        };
        let warning = loaded
            .directory_warning()
            .expect("a configured directory that is not there is worth a word");

        assert!(
            warning.contains(STATE_DIR) && warning.contains("state_directory"),
            "the warning must name the path and the setting: {warning}"
        );
    }

    // The default path gets none, because a first run is its ordinary state and
    // nothing was typed to get it wrong. This is the control: without it, warning on
    // every state directory would pass the test above.
    #[test]
    fn an_unset_default_state_directory_is_not_warned_about() {
        let mut fs = MockFileSystem::default();
        fs.mock_directory_state(DirectoryState::Absent(AbsentReason::Empty));
        fs.mock_no_irregular_files();
        fs.expect_read_file().returning(|_| {
            Err(FileSystemError::IoError(std::sync::Arc::new(
                std::io::Error::new(std::io::ErrorKind::NotFound, "No such file"),
            )))
        });
        fs.expect_expand_path()
            .returning(|_| Ok(PathBuf::from("/home/me")));

        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory("/packages")
            .build();

        let StateLoad::Usable(loaded) = load_deploy_state(&fs, &config) else {
            panic!("the default is selfie's to create");
        };

        assert_eq!(
            loaded.directory_warning(),
            None,
            "an unset default is silent"
        );
    }

    // The same for the default path, which no setting names. Both take one rule.
    #[test]
    fn a_default_state_directory_that_is_not_there_is_created_rather_than_refused() {
        let mut fs = MockFileSystem::default();
        fs.mock_directory_state(DirectoryState::Absent(AbsentReason::Empty));
        fs.mock_no_irregular_files();
        fs.expect_read_file().returning(|_| {
            Err(FileSystemError::IoError(std::sync::Arc::new(
                std::io::Error::new(std::io::ErrorKind::NotFound, "No such file"),
            )))
        });
        fs.expect_expand_path()
            .returning(|_| Ok(PathBuf::from("/home/me")));

        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory("/packages")
            .build();

        let state = state_of(load_deploy_state(&fs, &config));

        assert!(state.entries().is_empty(), "got: {state:?}");
    }

    // A path something else occupies is still refused, and this is what makes the
    // change above a narrowing rather than a removal: creating the directory is the
    // remedy for nothing being there, and no remedy at all for a file in the way.
    // `create_dir_all` fails on it, and it would fail after the run had deployed.
    #[test]
    fn a_state_directory_with_a_file_in_the_way_is_refused() {
        let mut fs = MockFileSystem::default();
        fs.mock_directory_state(DirectoryState::Absent(AbsentReason::Occupied {
            kind: "regular file",
        }));

        let message = failure_of(load_deploy_state(&fs, &config_with_state_dir()));

        assert!(
            message.contains(STATE_DIR) && message.contains("is a regular file"),
            "the refusal must name the path and what is there: {message}"
        );
        assert!(
            !message.contains("does not exist") && !message.contains("is not there"),
            "a path something occupies was reported as absent: {message}"
        );
    }

    // A path nothing is known about may hold a deploy state, and starting from an
    // empty one would write over it. Refuses rather than guessing it is empty.
    #[test]
    fn a_state_directory_that_cannot_be_checked_is_refused() {
        let mut fs = MockFileSystem::default();
        fs.mock_directory_state(DirectoryState::Unknown(std::sync::Arc::new(
            std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        )));

        let message = failure_of(load_deploy_state(&fs, &config_with_state_dir()));

        assert!(
            message.contains(STATE_DIR) && message.contains("could not be checked"),
            "got: {message}"
        );
    }

    // A filesystem with the state directory but no state file in it: the stat
    // finds nothing irregular and the read reports `NotFound`.
    fn filesystem_without_the_state_file() -> MockFileSystem {
        let mut fs = under_a_state_directory();
        fs.mock_no_irregular_files();
        fs.expect_read_file().returning(|_| {
            Err(FileSystemError::IoError(std::sync::Arc::new(
                std::io::Error::new(std::io::ErrorKind::NotFound, "No such file"),
            )))
        });
        fs
    }

    // A state file that exists and reads back as `content`.
    fn filesystem_holding(content: &str) -> MockFileSystem {
        let mut fs = under_a_state_directory();
        fs.mock_no_irregular_files();
        fs.mock_read_file(PathBuf::from(STATE_FILE), content);
        fs
    }

    // The message of a load that must have failed.
    #[track_caller]
    fn failure_of(load: StateLoad) -> String {
        match load {
            StateLoad::Unusable(failure) => failure.to_string(),
            StateLoad::Usable(_) => panic!("the load was usable, so nothing was reported"),
        }
    }

    // The state of a load that must have succeeded.
    #[track_caller]
    fn state_of(load: StateLoad) -> DeployState {
        match load {
            StateLoad::Usable(loaded) => loaded.into_state(),
            StateLoad::Unusable(failure) => panic!("the load failed: {failure}"),
        }
    }

    // Text that must never reach a message. `KEY` is shaped like a path a real
    // state file holds; `VALUE` like a checksum. Both are distinctive enough
    // that a `contains` cannot match selfie's own wording.
    const KEY: &str = "zzz-recon-marker/id_rsa.conf";
    const VALUE: &str = "zzz-value-marker-9c1f";

    // The malformed shapes, built once. Each test below picks the shapes whose
    // error class it is about, so a fixture written slightly differently in two
    // places cannot make two tests disagree about what they cover.
    fn duplicate_key(key: &str) -> String {
        format!(
            "deployed:\n  {key}:\n    source: s\n    checksum: a\n    \
             deployed_at: b\n  {key}:\n    source: s\n    checksum: c\n    \
             deployed_at: d\n"
        )
    }

    fn entry_is_a_scalar(key: &str, value: &str) -> String {
        format!("deployed:\n  {key}: {value}\n")
    }

    fn entry_is_missing_a_field(key: &str, value: &str) -> String {
        format!("deployed:\n  {key}:\n    source: {value}\n")
    }

    fn unclosed_bracket(key: &str, value: &str) -> String {
        format!("{key}: [unclosed {value}\n")
    }

    const VALID_STATE_YAML: &str = "deployed:\n  /home/u/.config/app.toml:\n    source: myapp/config.toml\n    \
         checksum: abc\n    deployed_at: \"2026-01-01T00:00:00+00:00\"\n";

    // The positive control for every test below.
    //
    // Without it they would all pass against a `load_deploy_state` that reported
    // a failure unconditionally.
    #[test]
    fn a_valid_state_file_loads_its_entries() {
        let fs = filesystem_holding(VALID_STATE_YAML);

        let state = state_of(load_deploy_state(&fs, &config_with_state_dir()));

        assert!(state.get("/home/u/.config/app.toml").is_some());
    }

    // The first-run case, and the one branch that must stay usable and silent.
    //
    // Reporting it would fire on every fresh machine, for the ordinary condition
    // of never having deployed anything.
    #[test]
    fn an_absent_state_file_is_usable_and_empty() {
        let fs = filesystem_without_the_state_file();

        let state = state_of(load_deploy_state(&fs, &config_with_state_dir()));

        assert!(state.entries().is_empty());
    }

    // Absence is one error kind, not every failure to reach the file. A state
    // file beneath a parent the process cannot search, or one it cannot open,
    // is unusable: reading it as absent would let a run deploy what it can never
    // record. The test above is the control that `NotFound` alone stays usable.
    #[test]
    fn a_state_file_that_cannot_be_reached_is_unusable_not_absent() {
        let mut fs = under_a_state_directory();
        fs.mock_no_irregular_files();
        fs.expect_read_file().returning(|_| {
            Err(FileSystemError::IoError(std::sync::Arc::new(
                std::io::Error::new(std::io::ErrorKind::PermissionDenied, "Permission denied"),
            )))
        });

        let message = failure_of(load_deploy_state(&fs, &config_with_state_dir()));

        assert!(
            message.contains("Cannot read deploy state") && message.contains(STATE_FILE),
            "an unreachable file must be reported as unreadable, and named: {message}"
        );
    }

    // A file that exists and holds nothing is not a first run: a first run has no
    // file. Whitespace is nothing too, since the writer never emits a bare
    // newline. Both are reported as empty rather than as a shape problem, because
    // the remedy is to delete or restore the file rather than to repair it.
    #[test]
    fn an_empty_state_file_is_unusable_not_a_first_run() {
        for (name, content) in [("zero bytes", ""), ("whitespace only", "\n  \n")] {
            let fs = filesystem_holding(content);

            let message = failure_of(load_deploy_state(&fs, &config_with_state_dir()));

            assert!(
                message.contains("is empty") && message.contains(STATE_FILE),
                "{name}: must say the file is empty and name it: {message}"
            );
        }
    }

    // A document with no `deployed` mapping is a parse failure, not an empty
    // state: `deployed` has no default, so a comment-only or null document
    // cannot read as "nothing deployed".
    #[test]
    fn a_document_without_a_deployed_mapping_does_not_parse() {
        for (name, content) in [("comment only", "# nothing here\n"), ("null", "~\n")] {
            let fs = filesystem_holding(content);

            let message = failure_of(load_deploy_state(&fs, &config_with_state_dir()));

            assert!(
                message.contains("Cannot parse"),
                "{name}: must be a parse failure: {message}"
            );
        }
    }

    #[test]
    fn an_unparsable_state_file_is_named_in_the_message() {
        let fs = filesystem_holding("{{{{not valid yaml!!! garbage $$$");

        let message = failure_of(load_deploy_state(&fs, &config_with_state_dir()));

        assert!(
            message.contains(STATE_FILE),
            "the message must name the file: {message}"
        );
    }

    // The two conditions are distinguished, not merely both reported.
    //
    // Repairing malformed YAML and fixing permissions are different jobs, so one
    // message for both sends the reader to the wrong one. Asserted in a single test
    // because the property is that the two *differ*, which neither alone can see.
    #[test]
    fn an_unreadable_state_file_is_named_differently_from_an_unparsable_one() {
        let mut unreadable = under_a_state_directory();
        unreadable.mock_no_irregular_files();
        unreadable.expect_read_file().returning(|_| {
            Err(FileSystemError::IoError(std::sync::Arc::new(
                std::io::Error::other("permission denied"),
            )))
        });
        let unreadable_message =
            failure_of(load_deploy_state(&unreadable, &config_with_state_dir()));

        let corrupt = filesystem_holding("{{{{not valid yaml!!! garbage $$$");
        let corrupt_message = failure_of(load_deploy_state(&corrupt, &config_with_state_dir()));

        assert!(
            unreadable_message.contains("Cannot read"),
            "an I/O failure must say so: {unreadable_message}"
        );
        assert!(
            corrupt_message.contains("Cannot parse"),
            "a malformed file must say so: {corrupt_message}"
        );
        assert_ne!(
            unreadable_message, corrupt_message,
            "two different conditions reported identically"
        );
    }

    // A fifo at the state path is refused before it is opened. The mock has no
    // `read_file` expectation, so reaching the read is a panic rather than a
    // hang, and the failure names what was found rather than a read error.
    #[test]
    fn a_fifo_at_the_state_path_is_refused_before_it_is_read() {
        let mut fs = under_a_state_directory();
        fs.expect_irregular_target_refusal().returning(|path| {
            Some(FileSystemError::IrregularTarget {
                path: path.path().to_path_buf(),
                kind: "named pipe (fifo)",
            })
        });

        let message = failure_of(load_deploy_state(&fs, &config_with_state_dir()));

        assert!(
            message.contains("named pipe (fifo)") && message.contains(STATE_FILE),
            "the refusal must say what sits at the path, and where: {message}"
        );
        assert!(
            !message.contains("Cannot read deploy state '"),
            "a fifo was reported as a failed read: {message}"
        );
    }

    // Reachable only with no configured `state_directory` and no determinable home.
    //
    // `home()` is not on `FileSystem` -- it comes from the blanket `HomeDir` impl,
    // whose body is `expand_path("~")` -- so this stubs the method that one calls.
    // `MockFileSystem` has no `expect_home` to stub.
    #[test]
    fn a_state_file_whose_location_cannot_be_resolved_names_the_fix() {
        let mut fs = MockFileSystem::default();
        fs.expect_expand_path().returning(|_| {
            Err(FileSystemError::IoError(std::sync::Arc::new(
                std::io::Error::other("no home"),
            )))
        });
        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory("/packages")
            .build();

        let message = failure_of(load_deploy_state(&fs, &config));

        assert!(
            message.contains("Cannot locate"),
            "the message must say the file could not be located: {message}"
        );
        // The branch fires only when both routes to a path are closed, so the
        // fix is one of exactly two things and the message must name both.
        assert!(
            message.contains("state_directory") && message.contains("HOME"),
            "the message must say what would fix it: {message}"
        );
    }

    // A duplicated key must not reach the message.
    //
    // serde-saphyr interpolates a duplicated key into that error's own text, and
    // no snippet option governs it, so the classifier has to keep it out.
    //
    // The controls below are part of the assertion: an inverted `contains` passes
    // just as well against an empty message or an unreached branch.
    #[test]
    fn a_duplicate_key_does_not_reach_the_message() {
        let fs = filesystem_holding(&duplicate_key(KEY));

        let message = failure_of(load_deploy_state(&fs, &config_with_state_dir()));

        assert!(
            !message.contains(KEY),
            "the duplicated key was quoted into the message: {message}"
        );
        // `contains("line")` would pass on "at line , column " and on "line 0,
        // column 0" -- the sentinel the classifier's own guard exists to drop --
        // so the coordinates are asserted exactly.
        assert!(
            message.contains(STATE_FILE)
                && message.contains("Cannot parse")
                && message.contains("a key is listed twice")
                && message.contains("at line 6, column 3"),
            "the message must still identify the file, the condition and where: {message}"
        );
    }

    // The malformed shapes a deploy state file can take, scanned for their own
    // content.
    //
    // The uncovered arms of `ParseFailure::of` are worth naming, since a partial
    // list reads as a complete one: `MergeKeyNotAllowed`, the unbalanced-container
    // group, the two alias groups and `Eof` have no row, and `InvalidScalar` and
    // `IndentationError` cannot fire while every field here is a `String`.
    //
    // Every marker sits on the line its parse fails at, so a returning snippet
    // would quote it and fail the scan below.
    #[test]
    fn no_malformed_state_file_shape_quotes_its_contents() {
        const DUPLICATE: &str = "a key is listed twice";
        const WRONG_SHAPE: &str = "the file has the wrong shape";
        // The parser's own words. Only the bracket character reaches the message,
        // never the key or value the row plants around it.
        const UNCLOSED: &str = "unclosed bracket";
        const TABS: &str = "tabs disallowed within this context";

        let entry = "{source: x, checksum: y, deployed_at: z}";
        // YAML escapes, so the key this builds is ordinary UTF-8 text holding
        // U+00FF and U+00FE -- non-ASCII, not raw bytes.
        let non_ascii_key = format!("\"{KEY}\\xff\\xfe\"");

        let shapes: Vec<(&str, String, &str)> = vec![
            ("duplicate key", duplicate_key(KEY), DUPLICATE),
            (
                "duplicate key inside an anchor",
                format!("anchor: &a\n  {KEY}: 1\n  {KEY}: 2\ndeployed: *a\n"),
                DUPLICATE,
            ),
            (
                "duplicate key through a merge key",
                format!("anchor: &a\n  {KEY}: 1\n  {KEY}: 2\ndeployed:\n  <<: *a\n"),
                DUPLICATE,
            ),
            (
                "duplicate key that is itself an alias",
                format!("anchor: &a {KEY}\ndeployed:\n  *a : {entry}\n  *a : {entry}\n"),
                DUPLICATE,
            ),
            (
                "duplicate key holding non-ASCII characters",
                duplicate_key(&non_ascii_key),
                DUPLICATE,
            ),
            (
                "entry is a scalar",
                entry_is_a_scalar(KEY, VALUE),
                WRONG_SHAPE,
            ),
            (
                "field is a sequence",
                format!(
                    "deployed:\n  {KEY}:\n    source:\n      - {VALUE}\n    \
                     checksum: a\n    deployed_at: b\n"
                ),
                WRONG_SHAPE,
            ),
            (
                "field is null",
                format!(
                    "deployed:\n  {KEY}:\n    source: ~\n    checksum: a\n    \
                     deployed_at: b\n"
                ),
                "a value is empty where text is required",
            ),
            (
                "entry is missing a field",
                entry_is_missing_a_field(KEY, VALUE),
                "an entry is missing the field",
            ),
            (
                "deployed is a scalar",
                format!("deployed: {VALUE}\n"),
                WRONG_SHAPE,
            ),
            ("top level is a scalar", format!("{VALUE}\n"), WRONG_SHAPE),
            ("unclosed bracket", unclosed_bracket(KEY, VALUE), UNCLOSED),
            (
                "tab indentation",
                format!("deployed:\n\t{KEY}: {VALUE}\n"),
                TABS,
            ),
            (
                "unknown anchor",
                format!("deployed: *{KEY}\n"),
                "the file refers to an anchor it never defines",
            ),
            (
                "merge key against a scalar",
                format!("deployed:\n  {KEY}:\n    <<: {VALUE}\n"),
                "a merge key does not refer to a mapping or a list of mappings",
            ),
            (
                "!!binary that is not base64",
                format!(
                    "deployed:\n  {KEY}:\n    source: !!binary \"@@@@\"\n    \
                     checksum: a\n    deployed_at: b\n"
                ),
                "a !!binary value is not valid base64",
            ),
            (
                "!!binary that is not text",
                format!(
                    "deployed:\n  {KEY}:\n    source: !!binary \"//8=\"\n    \
                     checksum: a\n    deployed_at: b\n"
                ),
                "a !!binary value is not text",
            ),
            (
                "more than one document",
                format!("deployed: {{}}\n---\ndeployed: {VALUE}\n"),
                "the file holds more than one YAML document",
            ),
        ];

        for (name, yaml, condition) in shapes {
            let fs = filesystem_holding(&yaml);

            let message = match load_deploy_state(&fs, &config_with_state_dir()) {
                StateLoad::Unusable(failure) => failure.to_string(),
                StateLoad::Usable(_) => {
                    panic!("{name}: stopped being an error, so this row no longer tests anything")
                }
            };

            assert!(
                !message.contains(KEY) && !message.contains(VALUE),
                "{name}: the file's contents were quoted into the message: {message}"
            );
            assert!(
                message.contains("Cannot parse") && message.contains(STATE_FILE),
                "{name}: a malformed file must say so, and name itself: {message}"
            );
            assert!(
                message.contains(condition),
                "{name}: this row now reports a different condition, so it no \
                 longer covers the class it was added for: {message}"
            );
        }
    }

    // The classification survives, so the message is worth reading.
    //
    // Three failures a user fixes differently must not render alike. Compared
    // with the location cut off, because the three fixtures fail at three
    // different places: comparing whole messages, every kind could collapse to
    // one string and the differing line numbers would still tell them apart.
    #[test]
    fn a_parse_failure_names_its_condition() {
        let condition = |yaml: &str| {
            let fs = filesystem_holding(yaml);
            let message = failure_of(load_deploy_state(&fs, &config_with_state_dir()));
            let at = message
                .find(" at line ")
                .unwrap_or_else(|| panic!("a parse failure must say where it happened: {message}"));
            message[..at].to_string()
        };

        let duplicate = condition(&duplicate_key("a/b.conf"));
        let wrong_shape = condition(&entry_is_a_scalar("a/b.conf", "scalar"));
        let unparsable = condition(&unclosed_bracket("a/b.conf", "x"));

        assert_ne!(duplicate, wrong_shape);
        assert_ne!(wrong_shape, unparsable);
        assert_ne!(duplicate, unparsable);
    }

    // The two classes that forward the library's own `&'static str`.
    //
    // Those strings are the deserializer's vocabulary, not the file's -- the field
    // name comes from this crate's own derive, and "mapping start" names a YAML
    // event. Checked here rather than taken on the type's word, which is the same
    // trust the rest of this work withholds.
    #[test]
    fn no_passed_through_text_carries_input() {
        for (yaml, expected) in [
            (entry_is_a_scalar(KEY, VALUE), "expected mapping start"),
            (entry_is_missing_a_field(KEY, VALUE), "checksum"),
        ] {
            let fs = filesystem_holding(&yaml);

            let message = failure_of(load_deploy_state(&fs, &config_with_state_dir()));

            assert!(
                message.contains(expected),
                "the library's own text stopped being forwarded, so this test no \
                 longer proves anything about it: {message}"
            );
            assert!(
                !message.contains(KEY) && !message.contains(VALUE),
                "forwarded library text carried the file's content: {message}"
            );
        }
    }

    // A key whose length selfie does not control must not grow the message.
    //
    // An explicit key (`? <key>`) is not subject to YAML's 1024-byte simple-key
    // limit, so it can be arbitrarily long. Nothing is forwarded, so nothing
    // needs bounding.
    #[test]
    fn a_huge_duplicate_key_does_not_grow_the_message() {
        let key = "k".repeat(2500);
        let fs = filesystem_holding(&format!("? {key}\n: 1\n? {key}\n: 2\n"));

        let message = failure_of(load_deploy_state(&fs, &config_with_state_dir()));

        // Control: if this fixture stops producing a duplicate-key error, the
        // huge-key path goes untested and everything below still passes.
        assert!(
            message.contains("a key is listed twice"),
            "this fixture no longer exercises the huge-key path: {message}"
        );
        // Neither assertion below is redundant, and the numbers are why. The
        // invariant message measures under 200 bytes against the 300-byte bound,
        // so a leak of about 100 bytes of the key would satisfy the length check
        // alone; the scan is what catches those. The scan in turn only fires on
        // 32 consecutive key bytes, so the length check is what catches a long
        // leak that somehow broke the run up. A fragment shorter than 32 bytes
        // slips both -- the exact residual, and the reason to keep the pair.
        assert!(
            !message.contains(&"k".repeat(32)),
            "the key reached the message: {message}"
        );
        assert!(
            message.len() < 300,
            "the message grew with the file's content: {} bytes",
            message.len()
        );
    }

    // A serialization failure is typed apart from a write failure. Nothing can
    // make a map of strings fail to serialize, so this pins the wording of the
    // one arm that can fire by handing the writer a failing filesystem.
    #[test]
    fn a_failed_write_names_the_file_in_its_own_words() {
        let mut fs = filesystem_without_the_state_file();
        fs.expect_write_file_private().returning(|_, _| {
            Err(FileSystemError::IoError(std::sync::Arc::new(
                std::io::Error::other("disk full"),
            )))
        });
        let loaded = match load_deploy_state(&fs, &config_with_state_dir()) {
            StateLoad::Usable(loaded) => loaded,
            StateLoad::Unusable(failure) => panic!("an absent file must be usable: {failure}"),
        };

        let error = save_deploy_state(&fs, &loaded).expect_err("the write was made to fail");

        assert!(matches!(error, StateSaveError::Write { .. }));
        let message = error.to_string();
        assert!(
            message.contains("Cannot write deploy state") && message.contains(STATE_FILE),
            "{message}"
        );
        assert!(message.contains("disk full"), "{message}");
    }
}
