//! This module gives tests a git adapter and a `git` command that read none of
//! the developer's git configuration.

use selfie::git::GixGitAdapter;

/// A [`GixGitAdapter`] that reads only each repository's own git
/// configuration, so a test repository picks up none of the developer's
/// global or system settings: hooks, includes, excludes, identity or signing.
///
/// Tests outside the `selfie` crate take their git adapter from here.
#[must_use]
#[expect(
    clippy::disallowed_methods,
    reason = "the one place a test obtains the isolated adapter"
)]
pub fn isolated_git_adapter() -> GixGitAdapter {
    GixGitAdapter::isolated()
}

/// A `git` command that runs as [`isolated_git_adapter`]'s git does: in an
/// environment that starts empty and adds back only `PATH`, a fixed test
/// identity and settings that turn off every global, system and template
/// source.
#[must_use]
#[expect(
    clippy::disallowed_methods,
    reason = "the one place a test obtains an isolated git command"
)]
pub fn isolated_git() -> std::process::Command {
    let mut git = std::process::Command::new("git");
    selfie::git::isolate_git_config(&mut git);
    git
}
