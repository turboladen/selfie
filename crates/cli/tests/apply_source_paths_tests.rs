pub mod common;

use std::fs;

use common::{SELFIE_ENV, sandboxed_command, setup_default_test_config};

// `apply` names each base directory once and shows every source relative to it.
// A source printed whole repeats the directory on every line; one printed as the
// spec spells it reads as a path the reader cannot resolve.

// Two package entries, one of them a template (its `echo` var is never run by a
// dry run), and, with `standalone`, one spec in a dotfiles directory.
fn sandbox(standalone: bool) -> tempfile::TempDir {
    let temp_dir = setup_default_test_config();
    let root = temp_dir.path();
    let packages = root.join("packages");

    fs::create_dir_all(packages.join("bat")).unwrap();
    fs::write(packages.join("bat").join("config"), "cfg\n").unwrap();
    fs::write(
        packages.join("bat.yaml"),
        format!(
            "name: bat\ndotfiles:\n  - source: bat/config\n    target: ~/.config/bat/config\n\
             environments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"
        ),
    )
    .unwrap();

    fs::create_dir_all(packages.join("git")).unwrap();
    fs::write(packages.join("git").join("gitconfig.tmpl"), "x\n").unwrap();
    fs::write(
        packages.join("git.yaml"),
        format!(
            "name: git\ndotfiles:\n  - source: git/gitconfig.tmpl\n    target: ~/.gitconfig\n    \
             vars:\n      email: echo me\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"
        ),
    )
    .unwrap();

    if standalone {
        let dotfiles = root.join("dotfiles");
        fs::create_dir_all(&dotfiles).unwrap();
        fs::write(dotfiles.join("zshrc"), "z\n").unwrap();
        fs::write(
            dotfiles.join("zsh.yaml"),
            "name: zsh\ndotfiles:\n  - source: zshrc\n    target: ~/.zshrc\n",
        )
        .unwrap();
        let config_path = root.join(".config/selfie/config.yaml");
        let mut config = fs::read_to_string(&config_path).unwrap();
        config.push_str(&format!("dotfiles_directory: {}\n", dotfiles.display()));
        fs::write(&config_path, config).unwrap();
    }

    temp_dir
}

fn run(temp_dir: &tempfile::TempDir, args: &[&str]) -> (Option<i32>, String, String) {
    let output = sandboxed_command(temp_dir).args(args).output().unwrap();
    (
        output.status.code(),
        String::from_utf8(output.stdout).expect("stdout must be UTF-8"),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn position(lines: &[&str], needle: &str) -> usize {
    let found: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains(needle))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        found.len(),
        1,
        "{needle:?} must be on exactly one line:\n{}",
        lines.join("\n")
    );
    found[0]
}

#[test]
fn apply_names_each_directory_once_and_sources_relative_to_it() {
    let temp_dir = sandbox(true);
    let (code, stdout, stderr) = run(&temp_dir, &["apply", "--dry-run"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    let lines: Vec<&str> = stdout.lines().collect();

    let packages = position(&lines, "Packages: ");
    let bat = position(&lines, "bat/config → ");
    let git = position(&lines, "git/gitconfig.tmpl (vars: email) → ");
    let dotfiles = position(&lines, "Dotfiles: ");
    let zsh = position(&lines, "zshrc → ");

    // Each heading sits above the lines relative to it, and below the ones that
    // are not.
    assert!(packages < bat && packages < git, "{stdout}");
    assert!(bat.max(git) < dotfiles && dotfiles < zsh, "{stdout}");

    // No entry line's source repeats a directory. Only the source half: the
    // target is expanded against the canonical home, which on macOS need not be
    // the `HOME` the sandbox set.
    let root = temp_dir.path().display().to_string();
    for line in lines.iter().filter(|line| line.contains(" → ")) {
        let (source, _) = line.split_once(" → ").unwrap();
        assert!(!source.contains(&root) && !source.contains("~/"), "{line}");
    }
}

// Control: with nothing from the dotfiles directory, it is not named.
#[test]
fn apply_names_only_the_directories_its_entries_come_from() {
    let temp_dir = sandbox(false);
    let (code, stdout, stderr) = run(&temp_dir, &["apply", "--dry-run"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");

    assert_eq!(
        stdout.lines().filter(|l| l.contains("Packages: ")).count(),
        1,
        "{stdout}"
    );
    assert_eq!(
        stdout.lines().filter(|l| l.contains("Dotfiles: ")).count(),
        0,
        "{stdout}"
    );
}

// A conflict the prompt showed and the user declined is not shown again. Here
// there is no terminal, so the prompt shows the block and declines.
#[test]
fn a_declined_conflict_is_shown_once() {
    let temp_dir = sandbox(false);
    let (code, stdout, stderr) = run(&temp_dir, &["apply", "--yes"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    fs::write(temp_dir.path().join(".config/bat/config"), "edited\n").unwrap();
    fs::write(temp_dir.path().join("packages/bat/config"), "changed\n").unwrap();

    let (_, stdout, stderr) = run(&temp_dir, &["apply", "bat"]);

    let conflicts = format!("{stdout}{stderr}")
        .lines()
        .filter(|line| line.contains("Conflict: "))
        .count();
    assert_eq!(conflicts, 1, "stdout:\n{stdout}\nstderr:\n{stderr}");

    // The diff names the source as the line above it does, relative to the
    // heading.
    let labels: Vec<&str> = stdout.lines().filter(|l| l.contains("+++ ")).collect();
    assert!(
        labels.iter().any(|l| l.ends_with("+++ bat/config")),
        "{stdout}"
    );
    let root = temp_dir.path().display().to_string();
    assert!(!labels.iter().any(|l| l.contains(&root)), "{stdout}");
}
