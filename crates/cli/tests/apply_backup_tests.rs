// What `selfie apply` tells you about the content it displaced.
//
// Asserted against the real binary's stdout rather than the event processor,
// because `DisplayManager` prints with `println!` and offers nothing to capture,
// so an in-process test cannot see the line a user reads.

pub mod common;

use common::{SELFIE_ENV, sandboxed_command, setup_test_config};
use tempfile::TempDir;

// A sandbox holding one package with one dotfile, its source, and its target.
//
// Returns the temp dir and the target path. `state_directory` is configured
// because a directory named in the config has to exist already, and the copies
// live inside it.
fn one_dotfile(
    source_content: &str,
    target_content: Option<&str>,
) -> (TempDir, std::path::PathBuf) {
    let temp = setup_test_config("");
    let root = temp.path();
    let target = root.join("target").join("config.toml");

    for dir in [
        root.join("packages").join("myapp"),
        root.join("state"),
        root.join("target"),
    ] {
        std::fs::create_dir_all(dir).unwrap();
    }

    std::fs::write(
        root.join(".config/selfie/config.yaml"),
        format!(
            "environment: {SELFIE_ENV}\npackage_directory: {}\nstate_directory: {}\n",
            root.join("packages").display(),
            root.join("state").display()
        ),
    )
    .unwrap();

    std::fs::write(root.join("packages/myapp/config.toml"), source_content).unwrap();
    std::fs::write(
        root.join("packages/myapp.yaml"),
        format!(
            "name: myapp\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\ndotfiles:\n  \
             - source: \"myapp/config.toml\"\n    target: \"{}\"\n",
            target.display()
        ),
    )
    .unwrap();

    if let Some(content) = target_content {
        std::fs::write(&target, content).unwrap();
    }

    (temp, target)
}

#[test]
fn an_overwrite_tells_you_where_the_previous_content_went() {
    let (temp, target) = one_dotfile("from-repo", Some("hand-edited"));

    let output = sandboxed_command(&temp)
        .args(["apply", "--yes"])
        .assert()
        .success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();

    assert!(
        stdout.contains("previous content copied to"),
        "the run must say where the displaced content went:\n{stdout}"
    );
    assert!(
        stdout.contains("state/backups"),
        "the copy must be named under the state directory:\n{stdout}"
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "from-repo");

    // The path it printed has to be the copy that exists, holding what was
    // displaced. A line naming a path nobody can open is worse than no line.
    let line = stdout
        .lines()
        .find(|line| line.contains("previous content copied to"))
        .expect("the line was found above");
    let printed = line
        .rsplit_once("copied to ")
        .expect("the line names a path")
        .1
        .trim();
    let resolved = printed.replacen('~', temp.path().to_str().unwrap(), 1);
    assert_eq!(
        std::fs::read_to_string(&resolved).unwrap(),
        "hand-edited",
        "printed {printed}"
    );
}

// Nothing was displaced, so nothing is announced. A line on every deploy would
// train the reader to skip the one that matters.
#[test]
fn deploying_to_a_new_target_announces_no_copy() {
    let (temp, _target) = one_dotfile("from-repo", None);

    let output = sandboxed_command(&temp)
        .args(["apply", "--yes"])
        .assert()
        .success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();

    assert!(
        !stdout.contains("previous content copied to"),
        "nothing was at the target:\n{stdout}"
    );
    assert!(
        !temp.path().join("state/backups").exists(),
        "no copy may be written for a target that did not exist"
    );
}
