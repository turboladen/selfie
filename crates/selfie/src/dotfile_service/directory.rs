//! What selfie says about the standalone dotfiles directory.

use std::path::Path;

/// The refusal for tracking a standalone dotfile into the dotfiles directory at
/// `path`, which does not exist.
pub(crate) fn track_refusal(path: &Path) -> String {
    format!(
        "Cannot track a standalone dotfile: the dotfiles directory does not exist: {} {}",
        path.display(),
        create_suggestion(path)
    )
}

/// How to create the dotfiles directory at `path`.
fn create_suggestion(path: &Path) -> String {
    format!("Create it with: mkdir -p {}", path.display())
}
