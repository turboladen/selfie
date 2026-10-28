// A test repository must not pick up anything of the developer's git: their
// global or system configuration, excludes, templates or identity, or what an
// outer git exports to its hooks. Tests use the isolated git adapter, which
// reads none of it, while a user's run uses the default adapter, which reads
// it as git does. This binary plants a marker for each source in the process
// environment and checks three readers: plain git, the default adapter and the
// isolated one.
//
// It holds one test, because it sets the process environment: with no other
// test in the binary, nothing runs while the variables change.

use std::fs;
use std::path::Path;
use std::process::Command;

use selfie::git::{GitStatusProvider, GitSyncProvider, GixGitAdapter};

// git run as a developer's shell runs it, with the whole process environment.
// Only the controls use it: each one shows that a marker works, so the isolated
// reader's check can only pass because it ignored the marker. It fails the test
// if git does, so a broken fixture cannot pass for a marker nothing read.
fn plain_git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

// git as a test runs it, for setting up fixtures and for the isolated checks.
fn isolated_git(dir: &Path, args: &[&str]) -> String {
    let output = test_common::isolated_git()
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn a_test_repository_reads_no_global_git_configuration() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();

    // The fixtures, made with isolated git before any marker is planted.
    let repo = dir.join("repo");
    fs::create_dir(&repo).unwrap();
    isolated_git(&repo, &["init", "--quiet"]);
    fs::write(repo.join("marker.txt"), "x\n").unwrap();
    let outer = dir.join("outer");
    fs::create_dir(&outer).unwrap();
    isolated_git(&outer, &["init", "--quiet"]);
    isolated_git(&outer, &["remote", "add", "outer-marker", "/nonexistent"]);

    let ignore = dir.join("global-ignore");
    fs::write(&ignore, "marker.txt\n").unwrap();
    let global = dir.join("global-config");
    fs::write(
        &global,
        format!(
            "[core]\n\texcludesFile = {}\n[remote \"global-marker\"]\n\turl = /nonexistent\n",
            ignore.display()
        ),
    )
    .unwrap();
    let system = dir.join("system-config");
    fs::write(
        &system,
        "[remote \"system-marker\"]\n\turl = /nonexistent\n",
    )
    .unwrap();
    // Git reads `$XDG_CONFIG_HOME/git/ignore` whenever no `core.excludesFile`
    // is configured.
    let xdg = dir.join("xdg");
    fs::create_dir_all(xdg.join("git")).unwrap();
    fs::write(xdg.join("git/ignore"), "xdg-marker.txt\n").unwrap();
    // A template directory whose hook `git init` copies into a new repository.
    let templates = dir.join("templates");
    fs::create_dir_all(templates.join("hooks")).unwrap();
    fs::write(templates.join("hooks/template-marker"), "#!/bin/sh\n").unwrap();
    // SAFETY: this binary holds one test, so no other thread reads the
    // environment while it changes.
    unsafe {
        // The controls must see only what this test plants, whatever the runner
        // exports.
        for inherited in [
            "GIT_CONFIG_NOSYSTEM",
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_CONFIG",
        ] {
            std::env::remove_var(inherited);
        }
        std::env::set_var("GIT_CONFIG_GLOBAL", &global);
        std::env::set_var("GIT_CONFIG_SYSTEM", &system);
        std::env::set_var("XDG_CONFIG_HOME", &xdg);
        std::env::set_var("GIT_TEMPLATE_DIR", &templates);
        // An outer git passes its `-c` settings to its children through these.
        std::env::set_var(
            "GIT_CONFIG_PARAMETERS",
            "'remote.marker-param.url'='/nonexistent'",
        );
        std::env::set_var("GIT_CONFIG_COUNT", "1");
        std::env::set_var("GIT_CONFIG_KEY_0", "remote.marker-count.url");
        std::env::set_var("GIT_CONFIG_VALUE_0", "/nonexistent");
        // A variable no setting names: only an empty environment keeps it out.
        std::env::set_var("GIT_AUTHOR_DATE", "2001-02-03T04:05:06Z");
        std::env::set_var("GIT_AUTHOR_NAME", "author-marker");
        std::env::set_var("GIT_AUTHOR_EMAIL", "author-marker@example.invalid");
        std::env::set_var("GIT_COMMITTER_NAME", "author-marker");
        std::env::set_var("GIT_COMMITTER_EMAIL", "author-marker@example.invalid");
    }

    // The controls: plain git reads every marker.
    assert_eq!(
        plain_git(&repo, &["remote"]).lines().collect::<Vec<_>>(),
        [
            "global-marker",
            "marker-count",
            "marker-param",
            "system-marker"
        ]
    );
    assert_eq!(
        plain_git(&repo, &["check-ignore", "marker.txt"]).trim(),
        "marker.txt"
    );
    // Git reads the XDG ignore file once no global file names an excludes
    // file, as when the global file alone is replaced.
    let ignored_by_xdg = Command::new("git")
        .args(["check-ignore", "--quiet", "xdg-marker.txt"])
        .current_dir(&repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .status()
        .unwrap();
    assert_eq!(ignored_by_xdg.code(), Some(0));
    let plain_new = dir.join("plain-new");
    plain_git(dir, &["init", "--quiet", "plain-new"]);
    assert!(plain_new.join(".git/hooks/template-marker").exists());
    plain_git(
        &plain_new,
        &["commit", "--quiet", "--allow-empty", "-m", "m"],
    );
    assert_eq!(
        plain_git(&plain_new, &["log", "-1", "--format=%an %aI"]).trim(),
        "author-marker 2001-02-03T04:05:06Z"
    );

    // A user's adapter reads the global configuration, as git does: the remote
    // it names first, and the excludes file that hides the marker, through the
    // git CLI, gix's status and the status provider.
    let user = GixGitAdapter::default();
    let info = user.discover_repo(&repo).unwrap();
    assert_eq!(
        info.remote_name.as_deref(),
        Some("global-marker"),
        "{info:?}"
    );
    let status = user.repo_status(&repo).unwrap();
    assert!(
        !status.untracked.iter().any(|p| p.ends_with("marker.txt")),
        "{status:?}"
    );
    let listed = user.status_for_directory(&repo).unwrap();
    assert!(
        !listed.files.keys().any(|p| p.ends_with("marker.txt")),
        "{listed:?}"
    );

    // A test's adapter reads none of it, by any of the three routes.
    let isolated = test_common::isolated_git_adapter();
    let info = isolated.discover_repo(&repo).unwrap();
    assert_eq!(info.remote_name, None, "{info:?}");
    let status = isolated.repo_status(&repo).unwrap();
    assert!(
        status.untracked.iter().any(|p| p.ends_with("marker.txt")),
        "{status:?}"
    );
    let listed = isolated.status_for_directory(&repo).unwrap();
    assert!(
        listed.files.keys().any(|p| p.ends_with("marker.txt")),
        "{listed:?}"
    );

    // A test's git reads no system file even when one is named, since the
    // recipe turns the system configuration off rather than only clearing it.
    let listed = test_common::isolated_git()
        .env("GIT_CONFIG_SYSTEM", &system)
        .args(["remote"])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(listed.status.success(), "{listed:?}");
    assert!(
        !String::from_utf8_lossy(&listed.stdout).contains("system-marker"),
        "{listed:?}"
    );

    // A test's git command applies no XDG ignore file, copies no template, and
    // commits as the fixed test identity at the real time.
    let ignored = test_common::isolated_git()
        .args(["check-ignore", "--quiet", "xdg-marker.txt"])
        .current_dir(&repo)
        .status()
        .unwrap();
    assert_eq!(ignored.code(), Some(1));
    let isolated_new = dir.join("isolated-new");
    isolated_git(dir, &["init", "--quiet", "isolated-new"]);
    assert!(!isolated_new.join(".git/hooks").exists());
    isolated_git(
        &isolated_new,
        &["commit", "--quiet", "--allow-empty", "-m", "m"],
    );
    let author = isolated_git(&isolated_new, &["log", "-1", "--format=%an %aI"]);
    assert!(author.starts_with("selfie-test "), "{author}");
    assert!(!author.contains("2001-02-03"), "{author}");

    // An outer git's hook in a worktree exports `GIT_DIR`, and `GIT_CONFIG`
    // redirects `git config`. Plain git follows both; a test's git follows
    // neither.
    let redirected = dir.join("redirected-config");
    // SAFETY: as above, this binary holds one test.
    unsafe {
        std::env::set_var("GIT_DIR", outer.join(".git"));
        std::env::set_var("GIT_CONFIG", &redirected);
    }
    assert!(plain_git(&repo, &["remote"]).contains("outer-marker"));
    plain_git(&repo, &["config", "user.name", "plain"]);
    assert!(fs::read_to_string(&redirected).unwrap().contains("plain"));
    let info = isolated.discover_repo(&repo).unwrap();
    assert_eq!(info.remote_name, None, "{info:?}");
    isolated_git(&repo, &["config", "user.name", "isolated"]);
    assert!(
        !fs::read_to_string(&redirected)
            .unwrap()
            .contains("isolated")
    );
    assert!(
        fs::read_to_string(repo.join(".git/config"))
            .unwrap()
            .contains("isolated")
    );
}
