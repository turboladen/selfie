//! Showing dotfile sources relative to the directory they were read from.

use crate::terminal_text::{Command, Shown, lit, noted, shown};
use std::path::Path;

use selfie::package::event::{BaseKind, DotfileSource, RepoPath};

use crate::display_manager::{Channel, DisplayManager, DisplayPath, shorten_path};

/// The line naming a base directory, for relative source paths printed below
/// it: "Packages: ~/…" or "Dotfiles: ~/…".
pub(crate) fn base_directory_line(kind: BaseKind, directory: &Path) -> Shown {
    let label = match kind {
        BaseKind::PackageDirectory => lit!("Packages"),
        BaseKind::DotfilesDirectory => lit!("Dotfiles"),
    };
    shown!("{}: {}", label, shorten_path(&directory.to_string_lossy()))
}

/// A source as one line shows it once its base directory is known: the path
/// relative to the base with any var names, or the command. A file under no
/// known base is shown in full, and a recorded spelling is marked as one. A
/// path is shown as a [`DisplayPath`]; a command is a [`Command`] on one line,
/// which keeps its blanks so it still pastes as it runs, with its note when it
/// reads as another.
pub(crate) fn relative_text(source: &DotfileSource) -> SourceText {
    match source {
        DotfileSource::File(RepoPath { base: None, .. })
        | DotfileSource::Template {
            file: RepoPath { base: None, .. },
            ..
        } => SourceText::Plain(shorten_path(&source.relative().to_string()).into()),
        DotfileSource::File(_) | DotfileSource::Template { .. } => {
            SourceText::Plain(DisplayPath::new(&source.relative().to_string()).into())
        }
        DotfileSource::Command(command) => command_source(command),
        // No base is known, so the line says so: under a heading printed for an
        // earlier line, a bare spelling would read as relative to it.
        DotfileSource::Recorded(spelling) => SourceText::Plain(shown!(
            "{} (as recorded, directory unknown)",
            DisplayPath::new(spelling)
        )),
    }
}

/// Holds a source as a line shows it: a path, or the command of a command
/// source, whose note is shown where the line is placed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SourceText {
    /// Holds a path, a template's path or a recorded spelling, as shown.
    Plain(Shown),
    /// Holds a command source's command, which is shown after `command: `.
    Command(Command),
}

impl SourceText {
    /// Returns the source on one line, without the note. A caller placing the
    /// line also places [`note`](Self::note), or calls
    /// [`with_note`](Self::with_note) or [`noted`](Self::noted) instead.
    pub(crate) fn text(&self) -> Shown {
        match self {
            Self::Plain(text) => text.clone(),
            Self::Command(command) => shown!("command: {}", command),
        }
    }

    /// Returns the command's note when the source is a command that reads as
    /// another.
    pub(crate) fn note(&self) -> Option<Shown> {
        match self {
            Self::Plain(_) => None,
            Self::Command(command) => command.note(),
        }
    }

    /// Returns `sentence`, a sentence naming the source, followed by the
    /// command's note on a line of its own when there is one.
    pub(crate) fn noted(&self, sentence: Shown) -> Shown {
        match self {
            Self::Plain(_) => sentence,
            Self::Command(command) => noted(sentence, command),
        }
    }

    /// Returns the source on one line, followed by its note on a second line
    /// when there is one.
    pub(crate) fn with_note(&self) -> Shown {
        self.noted(self.text())
    }
}

/// Returns a command source, which shows `command` on one line.
pub(crate) fn command_source(command: &str) -> SourceText {
    SourceText::Command(Command::one_line(command))
}

/// `source` as its line shows it, printing its base directory's line first on
/// `channel` when the previous source on that channel was under another base,
/// or none.
// The library reports each source with the base it was read from, so this
// never guesses. Apply and drift walk every package-directory spec before any
// standalone one, so each heading prints once; were that order to change, a
// heading would print again instead of a path showing under the wrong one.
pub(crate) fn label(
    display: &DisplayManager,
    channel: Channel,
    source: &DotfileSource,
) -> SourceText {
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
        DotfileSource::File(RepoPath {
            base: Some(SourceBase {
                kind,
                directory: PathBuf::from(directory),
            }),
            path: PathBuf::from(path),
        })
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
        label(display, Channel::Stdout, source).text().to_string()
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

    // A command source is a command, not a path: its blanks show as written, so
    // it still pastes as it runs, and a line break in it shows as an escape.
    // The blank reads as a space, so the note shows it escaped, and the note
    // holds the command alone, not its label.
    #[test]
    fn a_command_source_keeps_its_blanks_on_one_line() {
        let source = DotfileSource::Command("printf 'a\u{A0}b'\nexit".to_string());

        let shown = relative_text(&source);

        assert_eq!(shown.text(), "command: printf 'a\u{A0}b'\\nexit");
        assert_eq!(
            shown.note().map(Shown::into_string).as_deref(),
            Some("printf 'a\\u{a0}b'\\nexit")
        );
        let plain = relative_text(&DotfileSource::Command("echo token".to_string()));
        assert_eq!(plain.note(), None);
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
            file: RepoPath {
                base: Some(SourceBase {
                    kind: BaseKind::PackageDirectory,
                    directory: PathBuf::from("/r/p"),
                }),
                path: PathBuf::from("git/config.tmpl"),
            },
            vars: vec!["email".to_string()],
        };
        assert_eq!(
            relative_text(&source).text().to_string(),
            "git/config.tmpl (vars: email)"
        );
    }

    // Every kind of source shows a control character as its escape, so a file
    // name or command cannot act on the terminal through a row.
    #[test]
    fn a_source_shows_control_characters_as_escapes() {
        let name = "a\u{1b}]0;pwned\u{7}b\u{1b}[2Jc.conf";
        let sources = [
            file(BaseKind::PackageDirectory, "/r/p", name),
            DotfileSource::File(RepoPath {
                base: None,
                path: PathBuf::from(format!("/elsewhere/{name}")),
            }),
            DotfileSource::Template {
                file: RepoPath {
                    base: None,
                    path: PathBuf::from(name),
                },
                vars: vec!["email".to_string()],
            },
            DotfileSource::Command(format!("cat {name}")),
            DotfileSource::Recorded(name.to_string()),
        ];

        for source in &sources {
            let shown = relative_text(source).text().to_string();
            assert!(!shown.chars().any(char::is_control), "{shown:?}");
            assert!(
                shown.contains("a\\u{1b}]0;pwned\\u{7}b\\u{1b}[2Jc.conf"),
                "{shown:?}"
            );
        }
    }

    #[test]
    fn a_file_under_no_base_is_shown_in_full() {
        let source = DotfileSource::File(RepoPath {
            base: None,
            path: PathBuf::from("/elsewhere/x"),
        });
        assert_eq!(relative_text(&source).text().to_string(), "/elsewhere/x");
    }
}
