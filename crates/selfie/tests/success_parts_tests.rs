// Each value a library sentence names is the part kind it is: an environment
// and a package are names, a path is a path, a command is a command, and no
// text holds any of them. A CLI escapes a name and a path alike today, so only
// this test tells a name shown as a path from one shown as a name.

use std::path::{Path, PathBuf};

use selfie::fs::target::StatePathError;
use selfie::fs::{AbsentReason, IoFailure};
use selfie::parts::{Composed, Part, Parts};

const ENVIRONMENT: &str = "ENVIRONMENT";
const PACKAGE: &str = "PACKAGE";
const PATH: &str = "/PATH";
const ROOT: &str = "/ROOT";

// How many of each kind `parts` held: names, paths and commands.
#[derive(Default)]
struct Kinds {
    names: usize,
    paths: usize,
    commands: usize,
}

impl Kinds {
    // Counts `parts`' named values, asserting each is a name from `names`, a
    // path from `paths` or under `ROOT`, or a command whose operand is from
    // `names`, and that no text holds any of them.
    fn count(
        &mut self,
        parts: &Parts,
        names: &[&str],
        paths: &[&str],
        value: &impl std::fmt::Debug,
    ) {
        for part in parts {
            match part {
                Part::Name(name) => {
                    assert!(names.contains(name), "{name:?} in {value:?}");
                    self.names += 1;
                }
                Part::Path(path) => {
                    assert!(
                        paths.iter().any(|whole| *path == Path::new(whole))
                            || path.starts_with(ROOT),
                        "{path:?} in {value:?}"
                    );
                    self.paths += 1;
                }
                Part::Command(command) => {
                    assert!(names.contains(&command.word()), "{command:?} in {value:?}");
                    self.commands += 1;
                }
                Part::Text(text) => assert!(
                    !names
                        .iter()
                        .chain(paths)
                        .chain([&ROOT])
                        .any(|named| text.contains(named)),
                    "{text:?} in {value:?}"
                ),
            }
        }
    }
}

#[test]
fn every_value_a_success_summary_names_is_its_kind() {
    let mut kinds = Kinds::default();
    for success in test_common::every_success(ENVIRONMENT, PACKAGE, PATH) {
        kinds.count(&success.parts(), &[ENVIRONMENT, PACKAGE], &[PATH], &success);
    }
    // Every value the fixtures name is counted, so a value pushed as text,
    // or a fixture added or lost, changes a count here.
    assert_eq!(
        (kinds.names, kinds.paths, kinds.commands),
        (34, 4, 0),
        "names, paths and commands"
    );
}

#[test]
fn every_value_a_deploy_state_sentence_names_is_its_kind() {
    let mut kinds = Kinds::default();
    for value in test_common::every_state_value(ROOT) {
        let parts = match &value {
            test_common::StateValue::Load(failure) => failure.parts(),
            test_common::StateValue::Warning(warning) => warning.parts(),
            test_common::StateValue::Failure(failure) => failure.parts(),
        };
        kinds.count(&parts, &["tool"], &[], &value);
    }
    for cause in test_common::every_cause(&|leaf| PathBuf::from(format!("{ROOT}/{leaf}"))) {
        kinds.count(&cause.parts(), &[], &[], &cause);
    }
    let io = test_common::permission_denied;
    let state_path = StatePathError::NotAbsolute(PathBuf::from(format!("{ROOT}/state")));
    kinds.count(&state_path.parts(), &[], &[], &state_path);
    let dangling = AbsentReason::DanglingSymlink {
        points_to: Some(PathBuf::from(format!("{ROOT}/gone"))),
    };
    kinds.count(&dangling.parts(), &[], &[], &dangling);
    let io_failure = IoFailure::at(Path::new(&format!("{ROOT}/deploy-state.yml")), io());
    kinds.count(&io_failure.parts(), &[], &[], &io_failure);

    // Every value the fixtures name is counted, so a value pushed as text,
    // or a fixture added or lost, changes a count here. The track failure
    // offers the one command.
    assert_eq!(
        (kinds.names, kinds.paths, kinds.commands),
        (0, 88, 1),
        "names, paths and commands"
    );
}
