pub mod common;

use std::fs;

use common::{add_package, sandboxed_command, setup_default_test_config};
use predicates::prelude::*;
use selfie::package::PackageBuilder;

const SELFIE_ENV: &str = "test-env";

#[test]
fn test_package_list_empty() {
    // Test with no packages
    let temp_dir = setup_default_test_config();
    let packages_dir = temp_dir.path().join("packages");
    fs::create_dir_all(&packages_dir).unwrap();

    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["package", "list"]);

    // Should succeed but not list any packages
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("No packages found."));
}

#[test]
fn test_package_list_single_package() {
    let temp_dir = setup_default_test_config();

    // Create a single package
    let package = PackageBuilder::default()
        .name("test-package")
        .environment(SELFIE_ENV, |b| b.install("echo 'Hello'"))
        .build();

    add_package(&temp_dir, &package);

    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["package", "list"]);

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("test-package"));
}

#[test]
fn test_package_list_multiple_packages() {
    let temp_dir = setup_default_test_config();

    // Create multiple packages
    let packages = vec![
        PackageBuilder::default()
            .name("package-a")
            .environment(SELFIE_ENV, |b| b.install("echo 'Install A'"))
            .build(),
        PackageBuilder::default()
            .name("package-b")
            .environment(SELFIE_ENV, |b| b.install("echo 'Install B'"))
            .build(),
        PackageBuilder::default()
            .name("package-c")
            .environment("other-env", |b| b.install("echo 'Install C'"))
            .build(),
    ];

    for package in &packages {
        add_package(&temp_dir, package);
    }

    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["package", "list"]);

    // Should list only packages relevant to current environment (package-a and package-b)
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("package-a"))
        .stdout(predicate::str::contains("package-b"));

    // package-c should NOT be listed since it doesn't support current environment
    let output = cmd.assert().success().get_output().stdout.clone();
    let output_str = String::from_utf8_lossy(&output);
    assert!(!output_str.contains("package-c"));
}

#[test]
fn test_package_list_with_invalid_yaml() {
    let temp_dir = setup_default_test_config();

    // Create a valid package
    let package = PackageBuilder::default()
        .name("valid-package")
        .environment(SELFIE_ENV, |b| b.install("echo 'Valid'"))
        .build();

    add_package(&temp_dir, &package);

    // Add an invalid package file
    let packages_dir = temp_dir.path().join("packages");
    let invalid_path = packages_dir.join("invalid-package.yaml");
    let invalid_yaml = r#"
    name: "invalid-package"
    invalid_yaml: :::
    "#;

    fs::write(invalid_path, invalid_yaml).unwrap();

    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["package", "list"]);

    // Should show the valid package but report error for invalid one
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("valid-package"))
        .stdout(predicate::str::contains("invalid-package"));
}

#[test]
fn test_package_list_different_environments() {
    let temp_dir = setup_default_test_config();

    // Create packages with different environment configurations
    let packages = vec![
        // Package with current environment
        PackageBuilder::default()
            .name("current-env-package")
            .environment(SELFIE_ENV, |b| b.install("echo 'Current'"))
            .build(),
        // Package with multiple environments including current
        PackageBuilder::default()
            .name("multi-env-package")
            .environment(SELFIE_ENV, |b| b.install("echo 'Multi current'"))
            .environment("other-env", |b| b.install("echo 'Multi other'"))
            .build(),
        // Package without the current environment
        PackageBuilder::default()
            .name("different-env-package")
            .environment("other-env", |b| b.install("echo 'Different'"))
            .build(),
    ];

    for package in &packages {
        add_package(&temp_dir, package);
    }

    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["package", "list"]);

    // Should show only packages relevant to current environment
    let output = cmd.assert().success().get_output().stdout.clone();
    let output_str = String::from_utf8_lossy(&output);

    // Verify only relevant packages are shown
    assert!(output_str.contains("current-env-package"));
    assert!(output_str.contains("multi-env-package"));

    // different-env-package should NOT be shown since it doesn't support current environment
    assert!(!output_str.contains("different-env-package"));
}

#[test]
fn test_package_list_with_no_color_flag() {
    let temp_dir = setup_default_test_config();

    let package = PackageBuilder::default()
        .name("test-package")
        .environment(SELFIE_ENV, |b| b.install("echo 'Hello'"))
        .build();

    add_package(&temp_dir, &package);

    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["--no-color", "package", "list"]);

    // Should not contain ANSI color codes
    let output = cmd.assert().success().get_output().stdout.clone();
    let output_str = String::from_utf8_lossy(&output);
    assert!(!output_str.contains("\x1B["), "Output: {output_str}");
}

#[test]
fn test_package_list_shows_status() {
    let temp_dir = setup_default_test_config();

    // Create a package with a check command
    let package = PackageBuilder::default()
        .name("test-package-with-check")
        .environment(SELFIE_ENV, |b| {
            b.install("echo 'Installing'")
                .check(Some("echo 'check command' > /dev/null && exit 0"))
        })
        .build();

    add_package(&temp_dir, &package);

    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["package", "list"]);

    // Should contain the package name and a status indicator
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("test-package-with-check"))
        .stdout(predicate::str::contains("Installed"));
}

#[test]
fn test_package_list_shows_no_check_status() {
    let temp_dir = setup_default_test_config();

    // Create a package without a check command
    let package = PackageBuilder::default()
        .name("no-check-package")
        .environment(SELFIE_ENV, |b| b.install("echo 'Installing'"))
        .build();

    add_package(&temp_dir, &package);

    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["package", "list"]);

    // Should show the package name and "No check" status
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("no-check-package"))
        .stdout(predicate::str::contains("No check"));
}

#[test]
fn test_package_list_non_existent_directory() {
    let temp_dir = setup_default_test_config();

    // Remove the packages directory that was created
    let packages_dir = temp_dir.path().join("packages");
    fs::remove_dir_all(&packages_dir).unwrap();
    // fs::remove_dir_all(&packages_dir).ok();

    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["package", "list"]);

    // Should fail with appropriate error about missing directory
    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("does not exist"));
}

#[test]
fn test_package_list_all_flag_environment_ordering() {
    let temp_dir = setup_default_test_config();

    // Create packages with multiple environments in different orders
    let packages = vec![
        // Package where current environment is not first alphabetically
        PackageBuilder::default()
            .name("bacon")
            .environment("arch-home", |b| b.install("echo 'Install on arch'"))
            .environment(SELFIE_ENV, |b| b.install("echo 'Install on test-env'"))
            .build(),
        // Package where current environment is first alphabetically
        PackageBuilder::default()
            .name("bat")
            .environment(SELFIE_ENV, |b| b.install("echo 'Install on test-env'"))
            .environment("ubuntu-server", |b| b.install("echo 'Install on ubuntu'"))
            .build(),
    ];

    for package in &packages {
        add_package(&temp_dir, package);
    }

    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["package", "list", "--all"]);

    let output = cmd.assert().success().get_output().stdout.clone();
    let output_str = String::from_utf8_lossy(&output);

    // For bacon: current environment (test-env) should come first, then arch-home
    let bacon_line = output_str
        .lines()
        .find(|line| line.contains("bacon"))
        .expect("bacon package should be in output");

    // In streaming spinner format, environments are shown in parentheses at end of line
    assert!(
        bacon_line.contains("*test-env"),
        "Current environment should be marked for bacon: {bacon_line}"
    );
    assert!(
        bacon_line.contains("arch-home"),
        "Should contain arch-home for bacon: {bacon_line}"
    );

    // For bat: current environment (test-env) should come first, then ubuntu-server
    let bat_line = output_str
        .lines()
        .find(|line| line.contains("bat"))
        .expect("bat package should be in output");

    assert!(
        bat_line.contains("*test-env"),
        "Current environment should be marked for bat: {bat_line}"
    );
    assert!(
        bat_line.contains("ubuntu-server"),
        "Should contain ubuntu-server for bat: {bat_line}"
    );
}

#[test]
fn test_package_list_all_flag_shows_all_packages() {
    let temp_dir = setup_default_test_config();

    // Create packages with different environment support
    let packages = vec![
        // Package with current environment
        PackageBuilder::default()
            .name("current-env-package")
            .environment(SELFIE_ENV, |b| b.install("echo 'Current'"))
            .build(),
        // Package without current environment
        PackageBuilder::default()
            .name("different-env-package")
            .environment("other-env", |b| b.install("echo 'Different'"))
            .build(),
    ];

    for package in &packages {
        add_package(&temp_dir, package);
    }

    // Test default behavior (only relevant packages)
    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["package", "list"]);

    let output = cmd.assert().success().get_output().stdout.clone();
    let output_str = String::from_utf8_lossy(&output);

    assert!(output_str.contains("current-env-package"));
    assert!(!output_str.contains("different-env-package"));

    // Test --all flag behavior (all packages)
    let mut cmd_all = sandboxed_command(&temp_dir);
    cmd_all.args(["package", "list", "--all"]);

    let output_all = cmd_all.assert().success().get_output().stdout.clone();
    let output_all_str = String::from_utf8_lossy(&output_all);

    assert!(output_all_str.contains("current-env-package"));
    assert!(output_all_str.contains("different-env-package"));
    // In streaming format, environments are shown in parentheses on each line
    assert!(
        output_all_str.contains("other-env"),
        "Should show environment names in --all mode"
    );
}

#[test]
fn test_package_list_all_flag_not_relevant_status() {
    let temp_dir = setup_default_test_config();

    // Create a package that doesn't support the current environment
    let package_not_relevant = PackageBuilder::default()
        .name("not-relevant-package")
        .environment("other-env", |b| {
            b.install("echo 'Install on other-env'")
                .check(Some("echo 'check on other-env'"))
        })
        .build();

    // Create a package that supports current environment but has no check
    let package_no_check = PackageBuilder::default()
        .name("no-check-package")
        .environment(SELFIE_ENV, |b| b.install("echo 'Install on test-env'"))
        .build();

    add_package(&temp_dir, &package_not_relevant);
    add_package(&temp_dir, &package_no_check);

    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["package", "list", "--all"]);

    let output = cmd.assert().success().get_output().stdout.clone();
    let output_str = String::from_utf8_lossy(&output);

    // Package not relevant to current environment should show N/A
    let not_relevant_line = output_str
        .lines()
        .find(|line| line.contains("not-relevant-package"))
        .expect("not-relevant-package should be in output");
    assert!(
        not_relevant_line.contains("N/A"),
        "Package not relevant should show N/A: {not_relevant_line}"
    );

    // Package with no check command should show "No check"
    let no_check_line = output_str
        .lines()
        .find(|line| line.contains("no-check-package"))
        .expect("no-check-package should be in output");
    assert!(
        no_check_line.contains("No check"),
        "Package with no check should show 'No check': {no_check_line}"
    );
}

#[test]
fn test_package_list_default_behavior_filters_by_environment() {
    let temp_dir = setup_default_test_config();

    // Create a package that supports current environment but has no check
    let package_no_check = PackageBuilder::default()
        .name("no-check-package")
        .environment(SELFIE_ENV, |b| b.install("echo 'Install on test-env'"))
        .build();

    // Create a package that supports current environment with check
    let package_with_check = PackageBuilder::default()
        .name("with-check-package")
        .environment(SELFIE_ENV, |b| {
            b.install("echo 'Install on test-env'")
                .check(Some("echo 'check on test-env'"))
        })
        .build();

    add_package(&temp_dir, &package_no_check);
    add_package(&temp_dir, &package_with_check);

    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["package", "list"]);

    let output = cmd.assert().success().get_output().stdout.clone();
    let output_str = String::from_utf8_lossy(&output);

    // Both packages should be shown since they support current environment
    assert!(output_str.contains("no-check-package"));
    assert!(output_str.contains("with-check-package"));

    // Package with no check command should show "No check"
    let no_check_line = output_str
        .lines()
        .find(|line| line.contains("no-check-package"))
        .expect("no-check-package should be in output");
    assert!(
        no_check_line.contains("No check"),
        "Package with no check should show 'No check': {no_check_line}"
    );
}

#[test]
fn test_package_list_environment_mismatch_shows_stats() {
    let temp_dir = setup_default_test_config();

    // Create packages that support different environments but not the current one
    let packages = vec![
        PackageBuilder::default()
            .name("macos-package")
            .environment("macos", |b| b.install("echo 'Install on macOS'"))
            .build(),
        PackageBuilder::default()
            .name("ubuntu-package")
            .environment("ubuntu", |b| b.install("echo 'Install on Ubuntu'"))
            .environment("debian", |b| b.install("echo 'Install on Debian'"))
            .build(),
        PackageBuilder::default()
            .name("multi-env-package")
            .environment("windows", |b| b.install("echo 'Install on Windows'"))
            .environment("macos", |b| b.install("echo 'Install on macOS'"))
            .environment("ubuntu", |b| b.install("echo 'Install on Ubuntu'"))
            .build(),
    ];

    for package in packages {
        add_package(&temp_dir, &package);
    }

    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["package", "list"]);

    cmd.assert()
        .success()
        .stdout(predicate::str::contains(
            "No packages found for environment 'test-env'.",
        ))
        .stdout(predicate::str::contains(
            "Packages by environment in this directory:",
        ))
        .stdout(predicate::str::contains("Environment"))
        .stdout(predicate::str::contains("Package Count"))
        .stdout(predicate::str::contains("macos"))
        .stdout(predicate::str::contains("ubuntu"))
        .stdout(predicate::str::contains("windows"))
        .stdout(predicate::str::contains("debian"))
        // The advice is about the run, not the answer, so it is on stderr.
        .stderr(predicate::str::contains("Suggestion"))
        .stderr(predicate::str::contains("--environment <env>"))
        .stderr(predicate::str::contains("--all"))
        .stdout(predicate::str::contains("Suggestion").not());
}

// The row's Package column is the file's own name, so a reason that named the file
// again would print it twice across one line. The reason has no path in it, and
// this is where that shows.
#[test]
fn an_invalid_package_row_names_the_file_once_and_fits_one_line() {
    let temp_dir = setup_default_test_config();
    let packages_dir = temp_dir.path().join("packages");
    fs::create_dir_all(&packages_dir).unwrap();
    fs::write(
        packages_dir.join("broken.yml"),
        "name: broken\ndotfiles:\n  - command: op read op://vault/private/token\n    \
         target: ~/.creds\nenvironments: {oops\n",
    )
    .unwrap();

    let output = sandboxed_command(&temp_dir)
        .args(["package", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("stdout must be UTF-8");

    let row = stdout
        .lines()
        .find(|line| line.contains("broken"))
        .unwrap_or_else(|| panic!("the invalid package must be listed, got: {stdout}"));

    assert_eq!(
        row.matches("broken").count(),
        1,
        "the file must be named once in the row, got: {row}"
    );
    // The whole listing, not the row alone: text the row does not have room for
    // lands on the lines after it, so a scan bounded by the row reports a clean
    // run over a message that quoted the spec.
    //
    // Fragments no path can hold, because the listing also prints the sandbox
    // directory and a bare word could match its random name.
    assert!(
        !stdout.contains("op://") && !stdout.contains("op read"),
        "the listing must not quote the file, got: {stdout}"
    );
    // The whole reason, head and tail, on the row the name column is on. A message
    // that still carried a source snippet would put its tail on later lines, and
    // the column layout would survive that unremarked.
    assert!(
        row.contains("YAML parsing error") && row.contains("at line 5, column 15"),
        "the row must carry the entire reason, got: {row}"
    );
}

// A spec carrying a key that shadows `environments:` parses, and every command
// reading its environments must say so rather than read the decoy. The fixture
// keeps a real `environments:` too, so the refusal comes from the key alone.
mod a_spec_selfie_will_not_read {
    use super::*;

    fn write_shadowed(temp_dir: &tempfile::TempDir) {
        fs::write(
            temp_dir.path().join("packages").join("shadowed.yml"),
            format!(
                "name: shadowed\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n\
                 _environments:\n  {SELFIE_ENV}:\n    install: \"echo decoy\"\n"
            ),
        )
        .unwrap();
    }

    fn combined(output: &std::process::Output) -> String {
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }

    #[test]
    fn package_list_names_it_as_refused_and_exits_0() {
        let temp_dir = setup_default_test_config();
        write_shadowed(&temp_dir);

        let output = sandboxed_command(&temp_dir)
            .args(["package", "list"])
            .output()
            .unwrap();

        let text = combined(&output);
        assert_eq!(output.status.code(), Some(0), "{text}");
        assert!(text.contains("shadowed"), "{text}");
        assert!(text.contains("refused"), "{text}");
        assert!(text.contains("_environments"), "{text}");
        assert!(
            text.contains("0 valid package(s) and 1 refused package(s)"),
            "{text}"
        );
        assert!(!text.contains("No packages found"), "{text}");
    }

    #[test]
    fn spec_list_names_it_as_refused() {
        let temp_dir = setup_default_test_config();
        write_shadowed(&temp_dir);

        let output = sandboxed_command(&temp_dir)
            .args(["spec", "list"])
            .output()
            .unwrap();

        let text = combined(&output);
        assert_eq!(output.status.code(), Some(0), "{text}");
        assert!(text.contains("Refused: shadowed"), "{text}");
        assert!(text.contains("_environments"), "{text}");
    }

    // Described with the reason in place of the environments, and exits 0.
    #[test]
    fn spec_info_shows_the_reason_instead_of_the_environments() {
        let temp_dir = setup_default_test_config();
        write_shadowed(&temp_dir);

        let output = sandboxed_command(&temp_dir)
            .args(["spec", "info", "shadowed"])
            .output()
            .unwrap();

        let text = combined(&output);
        assert_eq!(output.status.code(), Some(0), "{text}");
        assert!(text.contains("Refused"), "{text}");
        assert!(text.contains("_environments"), "{text}");
        assert!(!text.contains("Environments"), "{text}");
    }

    #[test]
    fn package_status_refuses_it_and_exits_1() {
        let temp_dir = setup_default_test_config();
        write_shadowed(&temp_dir);

        let output = sandboxed_command(&temp_dir)
            .args(["package", "status", "shadowed"])
            .output()
            .unwrap();

        let text = combined(&output);
        assert_eq!(output.status.code(), Some(1), "{text}");
        assert!(text.contains("_environments"), "{text}");
    }
}

// The listing is a table: a package's status and, under `--all`, its
// environments are cells of their own, and an unreadable or refused spec is a
// row whose Status says why.
mod table {
    use super::*;

    fn listing(args: &[&str]) -> String {
        let temp_dir = setup_default_test_config();
        let packages = temp_dir.path().join("packages");
        fs::write(
            packages.join("bat.yaml"),
            "name: bat\nenvironments:\n  test-env:\n    install: \"true\"\n    check: \"true\"\n  other-env:\n    install: \"true\"\n",
        )
        .unwrap();
        fs::write(packages.join("broken.yaml"), "name: [unterminated\n").unwrap();
        fs::write(packages.join("noenv.yaml"), "name: noenv\n").unwrap();
        let output = sandboxed_command(&temp_dir).args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        // Paths printed in a row are relative to the package directory.
        assert!(
            !stdout
                .lines()
                .any(|l| l.contains("┆") && l.contains(temp_dir.path().to_str().unwrap())),
            "{stdout}"
        );
        stdout
    }

    fn row<'a>(stdout: &'a str, name: &str) -> Vec<&'a str> {
        let line = stdout
            .lines()
            .find(|l| l.contains(&format!(" {name} ")) && l.contains('┆'))
            .unwrap_or_else(|| panic!("no row for {name}:\n{stdout}"));
        line.trim_matches(|c| c == '│' || c == ' ')
            .split('┆')
            .map(str::trim)
            .collect()
    }

    #[test]
    fn all_puts_status_and_environments_in_their_own_cells() {
        let stdout = listing(&["package", "list", "--all"]);

        let bat = row(&stdout, "bat");
        assert_eq!(bat[1], "bat", "{bat:?}");
        assert_eq!(bat[2], "Installed", "{bat:?}");
        assert!(
            bat[3].contains("*test-env") && bat[3].contains("other-env"),
            "{bat:?}"
        );
        assert!(!stdout.contains("Installed ("), "{stdout}");
    }

    // Control: without `--all` there is no Environments column.
    #[test]
    fn without_all_there_is_no_environments_column() {
        let stdout = listing(&["package", "list"]);

        assert!(!stdout.contains("Environments"), "{stdout}");
        assert_eq!(row(&stdout, "bat").len(), 3, "{stdout}");
    }

    #[test]
    fn an_unreadable_or_refused_spec_is_a_row_saying_why() {
        let all = listing(&["package", "list", "--all"]);
        let broken = row(&all, "broken");
        assert!(broken[2].starts_with("unparsable: "), "{broken:?}");
        assert_eq!(broken[3], "-", "{broken:?}");

        // A spec with no environment is refused for this one.
        let stdout = listing(&["package", "list"]);
        let noenv = row(&stdout, "noenv");
        assert!(
            noenv[2].starts_with("refused (noenv.yaml): ")
                && noenv[2].contains("At least one environment"),
            "{noenv:?}"
        );
    }

    // The directory a refused row's path is relative to is named before the
    // table, so output cut short still says where the file is.
    #[test]
    fn the_package_directory_is_named_before_the_table() {
        let stdout = listing(&["package", "list"]);

        let directory = stdout
            .lines()
            .position(|l| l.starts_with("Packages: "))
            .unwrap_or_else(|| panic!("{stdout}"));
        let first_row = stdout.lines().position(|l| l.contains('┆')).unwrap();
        assert!(directory < first_row, "{stdout}");
    }

    // The counts are the summary line's, printed once.
    #[test]
    fn the_counts_are_printed_once() {
        let stdout = listing(&["package", "list"]);

        assert_eq!(stdout.matches("refused package(s)").count(), 1, "{stdout}");
    }
}
