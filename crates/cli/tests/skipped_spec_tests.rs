pub mod common;

use std::fs;

use common::{CLEAN, FAILED, sandboxed_command, setup_default_test_config};

// What every command that enumerates specs prints when it skips one it could not
// parse. Three commands share one sentence, and these are the only assertions on
// its bytes. `spec validate --all` names the spec in its results instead, which
// the last tests here hold it to.
//
// They are indifferent to who renders it -- the library's shared helper, or the
// CLI adapter that calls it -- and that is the point: they fail if the bytes
// change, whoever produces them.
fn sandbox_with_one_unparsable_spec() -> tempfile::TempDir {
    let temp_dir = setup_default_test_config();
    let packages_dir = temp_dir.path().join("packages");
    fs::create_dir_all(&packages_dir).unwrap();
    fs::write(
        packages_dir.join("good.yml"),
        "name: good\nenvironments:\n  test-env:\n    install: \"true\"\n",
    )
    .unwrap();
    fs::write(
        packages_dir.join("creds.yml"),
        "name: creds\nenvironments: {oops\n",
    )
    .unwrap();
    temp_dir
}

fn stderr_of(temp_dir: &tempfile::TempDir, args: &[&str]) -> String {
    let output = sandboxed_command(temp_dir)
        .args(args)
        .assert()
        .get_output()
        .stderr
        .clone();
    String::from_utf8(output).expect("stderr must be UTF-8")
}

// The sentence itself, on the three commands that emit it.
#[test]
fn every_enumerating_command_names_the_skipped_spec_the_same_way() {
    let temp_dir = sandbox_with_one_unparsable_spec();
    // The tail of the sentence, not the absolute path: a temp dir resolves through
    // /private on macOS, and the prefix is asserted separately below.
    let expected = "creds.yml: YAML parsing error: unclosed bracket '{' at line 2, column 15";

    for args in [
        &["package", "audit", "--all"][..],
        &["apply", "--dry-run"][..],
        &["dotfiles", "drift"][..],
    ] {
        let stderr = stderr_of(&temp_dir, args);
        assert!(
            stderr.contains("Skipping package file "),
            "{args:?} must name what it skipped, got: {stderr}"
        );
        assert!(
            stderr.contains(expected),
            "{args:?} must print the shared reason, got: {stderr}"
        );
    }
}

// The file is named once across the whole line: the sentence prefixes the path,
// and the reason must not name it again.
//
// Asserted here, against real output, because the claim is about what a reader
// sees. The Debug of a typed event resembles nothing a user reads, so the same
// assertion made against an event stream would pass while the terminal printed
// the file twice.
#[test]
fn a_skipped_spec_is_named_once_on_the_line_that_reports_it() {
    let temp_dir = sandbox_with_one_unparsable_spec();

    for args in [
        &["package", "audit", "--all"][..],
        &["apply", "--dry-run"][..],
        &["dotfiles", "drift"][..],
    ] {
        let stderr = stderr_of(&temp_dir, args);
        let line = stderr
            .lines()
            .find(|line| line.contains("Skipping package file"))
            .unwrap_or_else(|| panic!("{args:?} printed no skip line, got: {stderr}"));

        assert_eq!(
            line.matches("creds.yml").count(),
            1,
            "{args:?}: the file must be named once, got: {line}"
        );
    }
}

// A spec that parses is still processed. Without this, a regression that skipped
// everything would satisfy both tests above.
//
// Counted rather than searched for: `good` is a substring of the skip line that
// names it, so any assertion phrased as "says good, or does not skip good" holds
// for every possible output and can never fail.
#[test]
fn the_readable_spec_is_still_processed() {
    let temp_dir = sandbox_with_one_unparsable_spec();
    let stderr = stderr_of(&temp_dir, &["package", "audit", "--all"]);

    let skipped: Vec<&str> = stderr
        .lines()
        .filter(|line| line.contains("Skipping package file"))
        .collect();

    assert_eq!(
        skipped.len(),
        1,
        "exactly one spec must be skipped, got: {stderr}"
    );
    assert!(
        skipped[0].contains("creds.yml"),
        "the skipped spec must be the unparsable one, got: {}",
        skipped[0]
    );

    let output = sandboxed_command(&temp_dir)
        .args(["spec", "validate", "--all"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("1 package(s) validated"), "{stdout}");
}

// `spec validate --all` names a spec it could not parse once, in its results, and
// prints no skip line for it: the skip line would name the file a second time,
// above everything else the run printed.
#[test]
fn validate_all_names_an_unparsable_spec_once_in_its_results() {
    let temp_dir = sandbox_with_one_unparsable_spec();

    let output = sandboxed_command(&temp_dir)
        .args(["spec", "validate", "--all"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let both = format!("{stdout}{stderr}");

    assert_eq!(output.status.code(), Some(FAILED), "{both}");
    assert_eq!(
        both.lines()
            .filter(|line| line.contains("Skipping package file"))
            .count(),
        0,
        "{both}"
    );
    // The file is named once, as the label of its entry in the results. Its row
    // in the results table names the spec, not the file.
    assert_eq!(both.matches("creds.yml").count(), 1, "{both}");
    assert_eq!(
        stdout
            .lines()
            .filter(|line| line.contains("creds") && line.contains("unparsable"))
            .count(),
        1,
        "{stdout}"
    );
    assert_eq!(
        stdout
            .lines()
            .filter(|line| line.contains("✗") && line.ends_with("packages/creds.yml"))
            .count(),
        1,
        "{stdout}"
    );
}

// The exit status and the terminal text are what the library tests cannot see.
// A handler that claimed the failure event would print it and exit clean.
fn apply_output(temp_dir: &tempfile::TempDir, name: &str) -> (Option<i32>, String) {
    let output = sandboxed_command(temp_dir)
        .args(["apply", name])
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    (output.status.code(), text)
}

// Naming the unparsable spec must not read as "no such package": the file is
// there, and the skip line above the failure says why it could not be used.
#[test]
fn apply_naming_an_unparsable_spec_fails_and_says_it_could_not_be_loaded() {
    let temp_dir = sandbox_with_one_unparsable_spec();

    let (code, text) = apply_output(&temp_dir, "creds");

    assert_eq!(code, Some(FAILED), "output was: {text}");
    assert!(
        text.contains("Package 'creds' could not be loaded"),
        "output was: {text}"
    );
    assert!(!text.contains("No package named"), "output was: {text}");
}

#[test]
fn apply_naming_no_package_fails() {
    let temp_dir = sandbox_with_one_unparsable_spec();

    let (code, text) = apply_output(&temp_dir, "nope");

    assert_eq!(code, Some(FAILED), "output was: {text}");
    assert!(
        text.contains("No package named 'nope' was found"),
        "output was: {text}"
    );
}

fn drift_output(temp_dir: &tempfile::TempDir) -> (Option<i32>, String, String) {
    let output = sandboxed_command(temp_dir)
        .args(["dotfiles", "drift"])
        .output()
        .unwrap();
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

// A spec `dotfiles drift` could not load is a refusal: none of its dotfiles was
// examined, so the check fails rather than printing a clean line under the warning
// that names the spec. The clean line and the failed one carry the same sentence
// and differ by marker; both are the answer, on stdout.
#[test]
fn drift_does_not_claim_a_clean_check_over_a_spec_it_could_not_load() {
    let temp_dir = sandbox_with_one_unparsable_spec();

    let (code, stdout, stderr) = drift_output(&temp_dir);

    assert!(
        stderr.contains("creds.yml"),
        "must name the spec it could not load: {stderr}"
    );
    assert!(
        !stdout.contains("✓ Dotfile drift check"),
        "must not claim a clean check over an unloaded spec: {stdout}"
    );
    assert!(
        stdout.contains("✗ Dotfile drift check") && stdout.contains("1 refused"),
        "must count the spec it could not load as refused: {stdout}"
    );
    assert_eq!(code, Some(FAILED), "output was: {stdout}{stderr}");
}

// Control for the assertion above: with every spec loadable the clean check is
// still printed, to stdout, with nothing refused. Without this, a
// build that never prints the line at all would satisfy the negative.
#[test]
fn drift_reports_a_clean_check_when_every_spec_loads() {
    let temp_dir = setup_default_test_config();
    let packages_dir = temp_dir.path().join("packages");
    fs::create_dir_all(&packages_dir).unwrap();
    fs::write(
        packages_dir.join("good.yml"),
        "name: good\nenvironments:\n  test-env:\n    install: \"true\"\n",
    )
    .unwrap();

    let (code, stdout, stderr) = drift_output(&temp_dir);

    assert!(
        stdout.contains("✓ Dotfile drift check"),
        "a check with every spec loaded is clean: {stdout}"
    );
    assert!(
        stdout.contains("0 refused"),
        "nothing was skipped, and the line says so: {stdout}"
    );
    assert_eq!(code, Some(CLEAN), "output was: {stdout}{stderr}");
}

// A named apply that deploys nothing here says nothing about a spec it could not
// load: no target of its could share one with it.
#[test]
fn a_named_apply_deploying_nothing_does_not_report_another_spec_it_could_not_load() {
    let temp_dir = sandbox_with_one_unparsable_spec();

    let output = sandboxed_command(&temp_dir)
        .args(["apply", "good"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!stderr.contains("creds.yml"), "{stderr}");
    assert_eq!(output.status.code(), Some(CLEAN), "{stderr}");
}

// `selfie apply` with no name counts a spec it could not load as a refusal and
// exits `FAILED`, while a named apply of a loadable spec is not held to account for an
// unrelated file.
#[test]
fn apply_all_exits_1_over_a_spec_it_could_not_load_and_a_named_apply_does_not() {
    let temp_dir = sandbox_with_one_unparsable_spec();

    let all = sandboxed_command(&temp_dir)
        .args(["apply"])
        .output()
        .unwrap();
    let named = sandboxed_command(&temp_dir)
        .args(["apply", "good"])
        .output()
        .unwrap();

    let all_stdout = String::from_utf8_lossy(&all.stdout);
    assert!(all_stdout.contains("1 refused"), "{all_stdout}");
    assert_eq!(all.status.code(), Some(FAILED), "{all_stdout}");
    assert_eq!(
        named.status.code(),
        Some(CLEAN),
        "{}",
        String::from_utf8_lossy(&named.stderr)
    );
}

// A run's stdout, then its stderr.
fn combined(output: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

// The sandbox above, with `good` deploying a file, so a named apply and an apply of
// everything both have a target they could not compare with `creds.yml`'s.
fn sandbox_deploying_beside_an_unparsable_spec() -> tempfile::TempDir {
    let temp_dir = sandbox_with_one_unparsable_spec();
    let packages_dir = temp_dir.path().join("packages");
    fs::write(packages_dir.join("good.conf"), "GOOD").unwrap();
    fs::write(
        packages_dir.join("good.yml"),
        format!(
            "name: good\nenvironments:\n  test-env:\n    install: \"true\"\ndotfiles:\n  \
             - source: good.conf\n    target: {}\n",
            temp_dir.path().join("good.conf").display()
        ),
    )
    .unwrap();
    temp_dir
}

// A named apply names the spec it could not load once, as one it could not check
// for a shared target, and still succeeds.
#[test]
fn a_named_apply_names_another_spec_it_could_not_check() {
    let temp_dir = sandbox_deploying_beside_an_unparsable_spec();

    let output = sandboxed_command(&temp_dir)
        .args(["apply", "good", "--dry-run"])
        .output()
        .unwrap();
    let both = combined(&output);

    assert_eq!(output.status.code(), Some(CLEAN), "{both}");
    let naming: Vec<&str> = both
        .lines()
        .filter(|line| line.contains("creds.yml"))
        .collect();
    assert_eq!(
        naming,
        [
            "⚠ Could not check packages/creds.yml for an entry deploying to a target this apply \
          deploys: it could not be loaded"
        ],
        "{both}"
    );
}

// Applying everything names the spec on its skip line and again on the line
// saying no target was compared with it: twice, and no more.
#[test]
fn applying_everything_names_a_spec_it_could_not_check_after_skipping_it() {
    let temp_dir = sandbox_deploying_beside_an_unparsable_spec();

    let output = sandboxed_command(&temp_dir)
        .args(["apply", "--dry-run"])
        .output()
        .unwrap();
    let both = combined(&output);

    assert_eq!(output.status.code(), Some(FAILED), "{both}");
    assert_eq!(both.matches("creds.yml").count(), 2, "{both}");
    assert!(
        both.lines().any(|line| line
            == "⚠ Collisions with these were not checked, since selfie could not use them: \
                packages/creds.yml"),
        "{both}"
    );
}
