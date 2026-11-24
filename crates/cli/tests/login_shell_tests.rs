// What a user whose `SHELL` selfie cannot use sees: their commands run with
// `/bin/sh -l`, and one warning says so. The shell named here does not exist,
// so a run that still tried it would fail rather than pass.

pub mod common;

use common::{
    CLEAN, RunOutput, SELFIE_ENV, sandboxed_std_command, setup_default_test_config, write_package,
};

const MISSING_TCSH: &str = "/nonexistent/tcsh";
const WARNING: &str = "which selfie cannot run commands through";

// As `common::run_sandboxed`, with `SHELL` set to `shell`.
fn run_with_shell(temp: &tempfile::TempDir, shell: &str, args: &[&str]) -> RunOutput {
    let output = sandboxed_std_command(temp)
        .env("SHELL", shell)
        .args(args)
        .output()
        .unwrap();
    RunOutput {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

// A package whose check runs before and after its install, so a warning
// printed per command would show three times.
fn write_tool(temp: &tempfile::TempDir) {
    let marker = temp.path().join("installed");
    write_package(
        temp,
        "tool",
        &format!("touch {}", marker.display()),
        Some(&format!("test -f {}", marker.display())),
        &[],
    );
}

// Runs `args` with `SHELL` set to `shell`, and asserts that nothing warns.
fn assert_no_warning(shell: &str, args: &[&str]) {
    let temp = setup_default_test_config();
    write_tool(&temp);

    let run = run_with_shell(&temp, shell, args);

    assert_eq!(run.code, Some(CLEAN), "{}\n{}", run.stdout, run.stderr);
    assert_eq!(run.stderr.matches("SHELL is").count(), 0, "{}", run.stderr);
}

#[test]
fn install_under_tcsh_runs_with_sh_and_warns_once() {
    let temp = setup_default_test_config();
    write_tool(&temp);

    let run = run_with_shell(&temp, MISSING_TCSH, &["package", "install", "tool"]);

    assert_eq!(run.code, Some(CLEAN), "{}\n{}", run.stdout, run.stderr);
    assert!(temp.path().join("installed").exists(), "{}", run.stderr);
    assert_eq!(run.stderr.matches(WARNING).count(), 1, "{}", run.stderr);
    assert_eq!(
        run.stderr.matches(MISSING_TCSH).count(),
        1,
        "{}",
        run.stderr
    );
}

#[test]
fn install_under_a_shell_selfie_uses_does_not_warn() {
    assert_no_warning("/bin/sh", &["package", "install", "tool"]);
}

#[test]
fn a_command_that_runs_nothing_does_not_warn() {
    assert_no_warning(MISSING_TCSH, &["spec", "list"]);
}

#[test]
fn apply_under_tcsh_deploys_command_content_and_warns_once() {
    let temp = setup_default_test_config();
    let target = temp.path().join("generated");
    std::fs::write(
        temp.path().join("packages/gen.yaml"),
        format!(
            "name: gen\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\ndotfiles:\n  \
             - command: \"echo hi\"\n    target: \"{}\"\n  \
             - command: \"echo again\"\n    target: \"{}\"\n",
            target.display(),
            temp.path().join("generated-too").display()
        ),
    )
    .unwrap();

    let run = run_with_shell(&temp, MISSING_TCSH, &["apply", "--yes"]);

    assert_eq!(run.code, Some(CLEAN), "{}\n{}", run.stdout, run.stderr);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "hi\n");
    assert_eq!(run.stderr.matches(WARNING).count(), 1, "{}", run.stderr);
}
