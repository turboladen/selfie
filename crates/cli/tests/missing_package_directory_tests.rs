pub mod common;

use common::{sandboxed_command, setup_default_test_config};

// A configured package directory that does not exist is an ordinary mistake — a
// typo, a machine where the dotfiles repo has not been cloned yet — and the fix
// is one of three things the CLI already knows. The commands covered here offer
// all three. `dotfiles list` reads the same directory and offers none, which is
// a gap rather than a rule this file states.
//
// These fail if that guidance is lost, whichever command loses it.
fn sandbox_without_a_package_directory() -> tempfile::TempDir {
    let temp_dir = setup_default_test_config();
    let packages_dir = temp_dir.path().join("packages");
    if packages_dir.exists() {
        std::fs::remove_dir_all(&packages_dir).unwrap();
    }
    temp_dir
}

// Both streams, because the two halves land on different ones: the condition is
// an error and goes to stderr, the remedy is a suggestion and goes to stdout.
// `spec list` has the same split.
fn output_of(temp_dir: &tempfile::TempDir, args: &[&str]) -> String {
    let output = sandboxed_command(temp_dir)
        .args(args)
        .assert()
        .get_output()
        .clone();
    let mut both = String::from_utf8(output.stderr).expect("stderr must be UTF-8");
    both.push_str(&String::from_utf8(output.stdout).expect("stdout must be UTF-8"));
    both
}

#[test]
fn the_deploying_commands_name_the_missing_directory_and_how_to_fix_it() {
    let temp_dir = sandbox_without_a_package_directory();

    for args in [
        &["apply"][..],
        &["apply", "some-package"][..],
        &["dotfiles", "drift"][..],
    ] {
        let output = output_of(&temp_dir, args);

        assert!(
            output.contains("Package directory at") && output.contains("does not exist"),
            "{args:?} must name the directory and what is there, got: {output}"
        );
        // All three remedies, because which one applies depends on why it is
        // missing and the command cannot know that.
        assert!(
            output.contains("mkdir -p")
                && output.contains("package_directory")
                && output.contains("--package-directory"),
            "{args:?} must give all three remedies, got: {output}"
        );
        // The sentence a flattened error produces: it names what failed and
        // nothing about what to do next.
        assert!(
            !output.contains("Failed to load packages"),
            "{args:?} must not fall back to the untyped sentence, got: {output}"
        );
    }
}

// The remedy is pasted, so the path in it has to survive a shell. Both halves are
// asserted: the separator that stops a leading dash being read as options, and the
// quoting that holds a space together. Without `--` the first fails; without the
// quoting the second does.
#[test]
fn a_package_directory_holding_a_space_is_quoted_in_the_remedy() {
    let temp_dir = setup_default_test_config();
    let packages_dir = temp_dir.path().join("my packages");
    let config = temp_dir
        .path()
        .join(".config")
        .join("selfie")
        .join("config.yaml");
    let existing = std::fs::read_to_string(&config).unwrap();
    let rewritten: String = existing
        .lines()
        .map(|line| {
            if line.starts_with("package_directory:") {
                format!("package_directory: {}", packages_dir.display())
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&config, format!("{rewritten}\n")).unwrap();

    let output = output_of(&temp_dir, &["spec", "list"]);

    let command = format!("mkdir -p -- '{}'", packages_dir.display());
    let line = output
        .lines()
        .find(|line| line.contains(&command))
        .unwrap_or_else(|| {
            panic!("the remedy must name the path as one quoted word, got: {output}")
        });

    // A shell word ends at whitespace, so anything touching the closing quote is part
    // of it: a trailing comma makes the pasted command create a directory whose name
    // ends in a comma, and the `contains` above passes either way. Nothing may follow
    // the command on its line.
    assert!(
        line.trim_end().ends_with(&command),
        "the command must end its line, or what follows joins the shell word: {line}"
    );
}

// A listing command gives the same guidance, so a regression in the shared arm
// cannot pass by breaking only the commands that deploy.
#[test]
fn a_listing_command_still_gives_the_same_guidance() {
    let temp_dir = sandbox_without_a_package_directory();
    let output = output_of(&temp_dir, &["spec", "list"]);

    assert!(
        output.contains("does not exist") && output.contains("mkdir -p"),
        "got: {output}"
    );
}

// A package directory given as a relative path whose first component is a dangling
// symlink. `mkdir -p` cannot create through the link, so the sentence names the link
// as the component in the way and offers no creation command. The relative spelling
// matters: its shallowest ancestor is the empty path, which must not read as missing.
#[test]
fn a_relative_package_directory_below_a_dangling_symlink_names_the_link() {
    let temp_dir = setup_default_test_config();
    std::os::unix::fs::symlink(temp_dir.path().join("nowhere"), temp_dir.path().join("dl"))
        .unwrap();

    let output = sandboxed_command(&temp_dir)
        .current_dir(temp_dir.path())
        .args(["-p", "dl/pkgs", "spec", "list"])
        .assert()
        .get_output()
        .clone();
    let mut both = String::from_utf8(output.stderr).unwrap();
    both.push_str(&String::from_utf8(output.stdout).unwrap());

    assert!(
        both.contains("dl/pkgs is below dl, which is not a directory"),
        "the link must be named as the component in the way, got: {both}"
    );
    assert!(
        !both.contains("mkdir -p"),
        "mkdir -p cannot create through a dangling link, got: {both}"
    );
}

// `spec remove` against a package directory that a file occupies says what is at the
// directory. Blaming a spec that failed to load would send the user looking for a
// file that is not there.
#[test]
fn spec_remove_names_a_file_at_the_package_directory() {
    let temp_dir = sandbox_without_a_package_directory();
    let packages = temp_dir.path().join("packages");
    std::fs::write(&packages, "not a directory\n").unwrap();

    let output = output_of(&temp_dir, &["spec", "remove", "vim"]);

    // The configured path is canonicalized when the configuration loads, so the
    // sentence names the resolved spelling.
    let named = std::fs::canonicalize(&packages).unwrap();
    assert!(
        output.contains(&format!(
            "Cannot remove 'vim': {} is not a directory, it is a regular file",
            named.display()
        )),
        "the directory and what is there must be named, got: {output}"
    );
    assert!(
        !output.contains("could not load that spec"),
        "no spec failed to load, got: {output}"
    );
}
