//! Showing dotfile sources relative to the directory they were read from.

use std::path::Path;

use selfie::package::event::{BaseKind, DotfileSource};

use crate::display_manager::{Channel, DisplayManager, shorten_path};

/// The line naming a base directory, for relative source paths printed below
/// it: "Packages: ~/…" or "Dotfiles: ~/…".
pub(crate) fn base_directory_line(kind: BaseKind, directory: &Path) -> String {
    let label = match kind {
        BaseKind::PackageDirectory => "Packages",
        BaseKind::DotfilesDirectory => "Dotfiles",
    };
    format!(
        "{label}: {}",
        shorten_path(&directory.display().to_string())
    )
}

/// A source as one line shows it once its base directory is known: the path
/// relative to the base with any var names, or the command. A file under no
/// known base is shown in full, and a recorded spelling is marked as one.
pub(crate) fn relative_text(source: &DotfileSource) -> String {
    match source {
        DotfileSource::File { base: None, .. } | DotfileSource::Template { base: None, .. } => {
            shorten_path(&source.relative().to_string())
        }
        DotfileSource::File { .. } | DotfileSource::Template { .. } | DotfileSource::Command(_) => {
            source.relative().to_string()
        }
        // No base is known, so the line says so: under a heading printed for an
        // earlier line, a bare spelling would read as relative to it.
        DotfileSource::Recorded(spelling) => format!("{spelling} (as recorded, directory unknown)"),
    }
}

/// `source` as its line shows it, printing its base directory's line first on
/// `channel` when the previous source on that channel was under another base,
/// or none.
// The library reports each source with the base it was read from, so this
// never guesses. Apply and drift walk every package-directory spec before any
// standalone one, so each heading prints once; were that order to change, a
// heading would print again instead of a path showing under the wrong one.
pub(crate) fn label(display: &DisplayManager, channel: Channel, source: &DotfileSource) -> String {
    if let Some(base) = source.base()
        && display.swap_heading(channel, base.kind)
    {
        print_heading(display, channel, base.kind, &base.directory);
    }
    relative_text(source)
}

/// Print `kind`'s heading on `channel` whatever came before, and record it, so
/// lines printed after it are headed against it.
pub(crate) fn force_heading(
    display: &DisplayManager,
    channel: Channel,
    kind: BaseKind,
    directory: &Path,
) {
    display.swap_heading(channel, kind);
    print_heading(display, channel, kind, directory);
}

fn print_heading(display: &DisplayManager, channel: Channel, kind: BaseKind, directory: &Path) {
    let line = base_directory_line(kind, directory);
    match channel {
        Channel::Stdout => display.print_info(line),
        Channel::Stderr => display.print_run_note(line),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use selfie::package::event::SourceBase;

    use super::*;

    fn file(kind: BaseKind, directory: &str, path: &str) -> DotfileSource {
        DotfileSource::File {
            base: Some(SourceBase {
                kind,
                directory: PathBuf::from(directory),
            }),
            path: PathBuf::from(path),
        }
    }

    fn headings(display: &DisplayManager) -> Vec<String> {
        display
            .printed()
            .into_iter()
            .map(|(_, line)| line)
            .filter(|line| line.starts_with("Packages:") || line.starts_with("Dotfiles:"))
            .collect()
    }

    fn out(display: &DisplayManager, source: &DotfileSource) -> String {
        label(display, Channel::Stdout, source)
    }

    #[test]
    fn each_base_is_named_before_its_first_source() {
        let display = DisplayManager::new(false);

        assert_eq!(
            out(
                &display,
                &file(BaseKind::PackageDirectory, "/r/p", "bat/config")
            ),
            "bat/config"
        );
        out(
            &display,
            &file(BaseKind::PackageDirectory, "/r/p", "fd/config"),
        );
        assert_eq!(
            out(
                &display,
                &file(BaseKind::DotfilesDirectory, "/r/d", "zshrc")
            ),
            "zshrc"
        );

        assert_eq!(
            headings(&display),
            vec!["Packages: /r/p".to_string(), "Dotfiles: /r/d".to_string()]
        );
    }

    // Were packages to follow a standalone spec, the heading prints again rather
    // than leaving a package source under "Dotfiles:".
    #[test]
    fn a_base_named_again_after_another_is_headed_again() {
        let display = DisplayManager::new(false);

        out(&display, &file(BaseKind::PackageDirectory, "/r/p", "a"));
        out(&display, &file(BaseKind::DotfilesDirectory, "/r/d", "b"));
        out(&display, &file(BaseKind::PackageDirectory, "/r/p", "c"));

        assert_eq!(headings(&display).len(), 3, "{:?}", headings(&display));
    }

    // A heading goes to the stream of the line it heads, and each stream is
    // headed on its own.
    #[test]
    fn each_stream_gets_its_own_heading() {
        use crate::display_manager::Channel;

        let display = DisplayManager::new(false);
        let source = file(BaseKind::PackageDirectory, "/r/p", "a");

        out(&display, &source);
        label(&display, Channel::Stderr, &source);

        let headings: Vec<(Channel, String)> = display
            .printed()
            .into_iter()
            .filter(|(_, line)| line.starts_with("Packages:"))
            .collect();
        assert_eq!(
            headings,
            vec![
                (Channel::Stdout, "Packages: /r/p".to_string()),
                (Channel::Stderr, "Packages: /r/p".to_string()),
            ]
        );
    }

    // A heading forced by another printer, such as the conflict prompt, counts:
    // the next line under a different base is headed again.
    #[test]
    fn a_forced_heading_resets_the_next_one() {
        let display = DisplayManager::new(false);
        out(&display, &file(BaseKind::PackageDirectory, "/r/p", "a"));
        force_heading(
            &display,
            Channel::Stdout,
            BaseKind::DotfilesDirectory,
            Path::new("/r/d"),
        );

        out(&display, &file(BaseKind::PackageDirectory, "/r/p", "b"));

        assert_eq!(
            headings(&display),
            vec![
                "Packages: /r/p".to_string(),
                "Dotfiles: /r/d".to_string(),
                "Packages: /r/p".to_string()
            ]
        );
    }

    #[test]
    fn a_command_and_a_recorded_spelling_get_no_heading() {
        let display = DisplayManager::new(false);

        assert_eq!(
            out(&display, &DotfileSource::Command("echo token".to_string())),
            "command: echo token"
        );
        assert_eq!(
            out(&display, &DotfileSource::Recorded("vim/vimrc".to_string())),
            "vim/vimrc (as recorded, directory unknown)"
        );
        assert!(headings(&display).is_empty());
    }

    #[test]
    fn a_template_keeps_its_var_names() {
        let source = DotfileSource::Template {
            base: Some(SourceBase {
                kind: BaseKind::PackageDirectory,
                directory: PathBuf::from("/r/p"),
            }),
            path: PathBuf::from("git/config.tmpl"),
            vars: vec!["email".to_string()],
        };
        assert_eq!(relative_text(&source), "git/config.tmpl (vars: email)");
    }

    #[test]
    fn a_file_under_no_base_is_shown_in_full() {
        let source = DotfileSource::File {
            base: None,
            path: PathBuf::from("/elsewhere/x"),
        };
        assert_eq!(relative_text(&source), "/elsewhere/x");
    }
}
