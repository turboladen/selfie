//! Which shell a login runner runs, and how it asks that shell to log in.

use std::{
    ffi::{OsStr, OsString},
    fmt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::parts::{Composed, Parts};

/// The shell a login runner runs a command with when `SHELL` names a shell
/// that cannot run it.
pub(super) const FALLBACK_SHELL: &str = "/bin/sh";

/// The content recipe a shell is given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Recipe {
    Posix,
    Fish,
}

impl Recipe {
    /// The recipe for `shell`, which is fish's only when `shell` is fish.
    // Getting this wrong in either direction is safe: a POSIX shell treated as
    // fish still separates, and fish given the POSIX recipe writes `exec`'s
    // manual page ahead of the start marker, where extraction discards it.
    pub(super) fn of(shell: &Path) -> Self {
        if Family::of(shell) == Family::Fish {
            Self::Fish
        } else {
            Self::Posix
        }
    }
}

/// How a shell named by `SHELL` takes a login flag and a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    Posix,
    Fish,
    Nu,
    Csh,
}

impl Family {
    // A login shell's `argv[0]` starts with `-`, and a `SHELL` copied from one
    // keeps it. csh and tcsh refuse `-l` beside `-c`; every shell not named here
    // is passed `-l -c`.
    fn of(shell: &Path) -> Self {
        let Some(name) = shell.file_name().and_then(OsStr::to_str) else {
            return Self::Posix;
        };
        match name.strip_prefix('-').unwrap_or(name) {
            "csh" | "tcsh" => Self::Csh,
            "fish" => Self::Fish,
            "nu" => Self::Nu,
            _ => Self::Posix,
        }
    }
}

// No program is installed under its login name, so `-zsh` runs `zsh`, found on
// PATH, and `/bin/-zsh` runs `/bin/zsh`.
fn without_login_dash(shell: PathBuf) -> PathBuf {
    let stripped = shell
        .file_name()
        .and_then(OsStr::to_str)
        .and_then(|name| name.strip_prefix('-'))
        .map(|name| shell.with_file_name(name));
    stripped.unwrap_or(shell)
}

/// What a login runner runs, resolved from `SHELL`.
#[derive(Debug)]
pub(super) struct LoginShell {
    /// The program ordinary commands run with.
    pub(super) command: PathBuf,
    /// The flag that program takes to log in.
    pub(super) login_flag: &'static str,
    /// The program a command whose stdout becomes content runs with.
    pub(super) content: PathBuf,
    /// What selfie replaced, if it replaced anything.
    pub(super) fallback: Option<ShellFallback>,
}

impl LoginShell {
    /// Resolve the value of `SHELL`, which is `None` when it is unset.
    pub(super) fn resolve(shell: Option<OsString>) -> Self {
        let Some(shell) = shell.filter(|shell| !shell.is_empty()).map(PathBuf::from) else {
            return Self::fallen_back(ShellFallback::Unset);
        };
        match Family::of(&shell) {
            Family::Posix | Family::Fish => Self::native(without_login_dash(shell)),
            // nu runs commands, but the content recipes are POSIX sh and fish.
            Family::Nu => Self {
                command: without_login_dash(shell.clone()),
                login_flag: "--login",
                content: PathBuf::from(FALLBACK_SHELL),
                fallback: Some(ShellFallback::ContentUnsupported(shell)),
            },
            Family::Csh => Self::fallen_back(ShellFallback::Unsupported(shell)),
        }
    }

    fn native(shell: PathBuf) -> Self {
        Self {
            content: shell.clone(),
            command: shell,
            login_flag: "-l",
            fallback: None,
        }
    }

    fn fallen_back(fallback: ShellFallback) -> Self {
        Self {
            command: PathBuf::from(FALLBACK_SHELL),
            login_flag: "-l",
            content: PathBuf::from(FALLBACK_SHELL),
            fallback: Some(fallback),
        }
    }
}

/// Says that a login runner runs some or all commands with `/bin/sh -l`
/// instead of the shell `SHELL` names, or because `SHELL` names none.
///
/// [`kind`](Self::kind) is its label, the variant's name in snake_case.
#[derive(Debug, Clone, PartialEq, Eq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum ShellFallback {
    /// `SHELL` is unset or empty, so every command runs with `/bin/sh -l`.
    Unset,
    /// `SHELL` names csh or tcsh, which refuse a login flag beside a command,
    /// so every command runs with `/bin/sh -l`.
    Unsupported(PathBuf),
    /// `SHELL` names a shell that runs commands but cannot run the recipe that
    /// captures a command's stdout as dotfile content, so only those commands
    /// run with `/bin/sh -l`.
    ContentUnsupported(PathBuf),
}

impl ShellFallback {
    /// Returns this fallback's label.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        self.into()
    }

    /// The shell `SHELL` named, or `None` when it named none.
    #[must_use]
    pub fn shell(&self) -> Option<&Path> {
        match self {
            Self::Unset => None,
            Self::Unsupported(shell) | Self::ContentUnsupported(shell) => Some(shell),
        }
    }

    /// Whether ordinary commands run with the fallback, rather than only the
    /// commands whose stdout becomes content.
    pub(super) fn covers_every_command(&self) -> bool {
        !matches!(self, Self::ContentUnsupported(_))
    }
}

crate::parts::display_from_parts!(ShellFallback);

impl Composed for ShellFallback {
    fn parts(&self) -> Parts<'_> {
        fn named(shell: &Path) -> Parts<'_> {
            Parts::new().text("SHELL is ").path(shell)
        }
        match self {
            Self::Unset => Parts::new().text(format!(
                "SHELL is unset or empty, so selfie runs commands with {FALLBACK_SHELL} -l."
            )),
            Self::Unsupported(shell) => named(shell).text(format!(
                ", which selfie cannot run commands through, so it runs them with \
                 {FALLBACK_SHELL} -l instead."
            )),
            Self::ContentUnsupported(shell) => named(shell).text(format!(
                ", which cannot run the commands that produce dotfile content, so selfie \
                 runs those with {FALLBACK_SHELL} -l instead."
            )),
        }
    }
}

/// Who is told, once, that a runner is using a fallback shell.
///
/// A runner and every clone of it share one notice, so the first command the
/// fallback affects, whichever of them runs it, is the only one that tells.
#[derive(Clone)]
pub struct FallbackNotice {
    tell: Arc<dyn Fn(&ShellFallback) + Send + Sync>,
    told: Arc<AtomicBool>,
}

impl FallbackNotice {
    /// Build a notice from `tell`, which is called at most once and may be
    /// called from any thread.
    pub fn new(tell: impl Fn(&ShellFallback) + Send + Sync + 'static) -> Self {
        Self {
            tell: Arc::new(tell),
            told: Arc::new(AtomicBool::new(false)),
        }
    }

    // `swap` rather than a load and a store: commands run concurrently, and two
    // that both loaded `false` would both tell.
    pub(super) fn tell(&self, fallback: &ShellFallback) {
        if !self.told.swap(true, Ordering::AcqRel) {
            (self.tell)(fallback);
        }
    }
}

impl fmt::Debug for FallbackNotice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FallbackNotice")
    }
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsStr,
        sync::{Barrier, atomic::AtomicUsize},
        time::Duration,
    };

    use super::super::{ShellCommandRunner, content};
    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(5);

    fn resolved(shell: &str) -> LoginShell {
        LoginShell::resolve(Some(OsString::from(shell)))
    }

    #[test]
    fn a_posix_shell_or_fish_runs_everything_itself() {
        for shell in [
            "/bin/bash",
            "/bin/zsh",
            "/bin/sh",
            "/bin/dash",
            "/bin/ksh",
            "/usr/bin/ksh93",
            "/usr/bin/mksh",
            "/bin/ash",
            "/opt/homebrew/bin/fish",
        ] {
            let login = resolved(shell);
            assert_eq!(login.command, Path::new(shell), "{shell}");
            assert_eq!(login.content, Path::new(shell), "{shell}");
            assert_eq!(login.login_flag, "-l", "{shell}");
            assert_eq!(login.fallback, None, "{shell}");
        }
    }

    #[test]
    fn a_shell_selfie_does_not_name_keeps_its_own_login() {
        for shell in [
            "/usr/bin/oksh",
            "/usr/bin/yash",
            "/usr/local/bin/xonsh",
            "/opt/x/myshell",
        ] {
            let login = resolved(shell);
            assert_eq!(login.command, Path::new(shell), "{shell}");
            assert_eq!(login.content, Path::new(shell), "{shell}");
            assert_eq!(login.login_flag, "-l", "{shell}");
            assert_eq!(login.fallback, None, "{shell}");
        }
    }

    #[test]
    fn nu_logs_in_with_its_long_flag_and_hands_content_to_sh() {
        let login = resolved("/usr/local/bin/nu");

        assert_eq!(login.command, Path::new("/usr/local/bin/nu"));
        assert_eq!(login.login_flag, "--login");
        assert_eq!(login.content, Path::new(FALLBACK_SHELL));
        assert_eq!(
            login.fallback,
            Some(ShellFallback::ContentUnsupported(PathBuf::from(
                "/usr/local/bin/nu"
            )))
        );
    }

    #[test]
    fn csh_tcsh_or_no_shell_falls_back_to_sh() {
        for shell in ["/bin/csh", "/bin/tcsh", "/usr/local/bin/tcsh"] {
            let login = resolved(shell);
            assert_eq!(login.command, Path::new(FALLBACK_SHELL), "{shell}");
            assert_eq!(login.content, Path::new(FALLBACK_SHELL), "{shell}");
            assert_eq!(login.login_flag, "-l", "{shell}");
            assert_eq!(
                login.fallback,
                Some(ShellFallback::Unsupported(PathBuf::from(shell))),
                "{shell}"
            );
        }
        for unset in [None, Some(OsString::new())] {
            let login = LoginShell::resolve(unset.clone());
            assert_eq!(login.command, Path::new(FALLBACK_SHELL), "{unset:?}");
            assert_eq!(login.fallback, Some(ShellFallback::Unset), "{unset:?}");
        }
    }

    #[test]
    fn a_login_dash_before_the_name_is_dropped_from_the_program() {
        let login = resolved("-zsh");
        assert_eq!(login.command, Path::new("zsh"));
        assert_eq!(login.content, Path::new("zsh"));
        assert_eq!(login.fallback, None);

        let login = resolved("/opt/homebrew/bin/-fish");
        assert_eq!(login.command, Path::new("/opt/homebrew/bin/fish"));
        assert_eq!(login.content, Path::new("/opt/homebrew/bin/fish"));

        let login = resolved("-nu");
        assert_eq!(login.command, Path::new("nu"));
        assert_eq!(login.login_flag, "--login");

        let login = resolved("-tcsh");
        assert_eq!(login.command, Path::new(FALLBACK_SHELL));
        assert_eq!(
            login.fallback,
            Some(ShellFallback::Unsupported(PathBuf::from("-tcsh")))
        );
    }

    #[test]
    fn fish_content_is_recognized_by_file_name() {
        assert_eq!(
            Recipe::of(Path::new("/opt/homebrew/bin/fish")),
            Recipe::Fish
        );
        assert_eq!(Recipe::of(Path::new("fish")), Recipe::Fish);
        assert_eq!(Recipe::of(Path::new("-fish")), Recipe::Fish);
        assert_eq!(Recipe::of(Path::new("/bin/sh")), Recipe::Posix);
        assert_eq!(Recipe::of(Path::new("/bin/zsh")), Recipe::Posix);
        assert_eq!(Recipe::of(Path::new("/bin/fisher")), Recipe::Posix);
    }

    #[test]
    fn each_fallback_names_what_it_replaced_and_nothing_it_did_not() {
        let unsupported = ShellFallback::Unsupported(PathBuf::from("/bin/tcsh")).to_string();
        assert!(unsupported.contains("/bin/tcsh"), "{unsupported}");
        assert!(unsupported.contains("/bin/sh -l"), "{unsupported}");

        // nu does run ordinary commands, so its warning must not say otherwise.
        let content = ShellFallback::ContentUnsupported(PathBuf::from("/x/nu")).to_string();
        assert!(content.contains("/x/nu"), "{content}");
        assert!(content.contains("dotfile content"), "{content}");
        assert!(!content.contains("cannot run commands"), "{content}");

        let unset = ShellFallback::Unset.to_string();
        assert!(unset.contains("unset or empty"), "{unset}");
    }

    #[test]
    fn a_fallback_is_labeled_by_its_kind() {
        assert_eq!(ShellFallback::Unset.kind(), "unset");
        assert_eq!(
            ShellFallback::Unsupported(PathBuf::from("/bin/csh")).kind(),
            "unsupported"
        );
        assert_eq!(
            ShellFallback::ContentUnsupported(PathBuf::from("/x/nu")).kind(),
            "content_unsupported"
        );
    }

    fn login_runner(shell: Option<&str>) -> ShellCommandRunner {
        ShellCommandRunner::resolving_login_shell(shell.map(OsString::from), TIMEOUT)
    }

    // Returns the program and arguments `runner` runs `cmd` with.
    fn command_of(runner: &ShellCommandRunner) -> (PathBuf, Vec<String>) {
        let cmd = runner.build_command("cmd", Path::new("/"));
        let std = cmd.as_std();
        (
            PathBuf::from(std.get_program()),
            std.get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect(),
        )
    }

    // The shell a content command's wrapper hands the recipe to, and the
    // wrapper itself.
    fn content_of(runner: &ShellCommandRunner) -> (PathBuf, String) {
        let cmd = runner.build_content_command("recipe", Path::new("/"), content::CAPTURE_FD);
        let std = cmd.as_std();
        let shell = std
            .get_envs()
            .find(|(key, _)| *key == OsStr::new(content::SHELL_VAR))
            .and_then(|(_, value)| value)
            .map(PathBuf::from)
            .expect("the content command names its shell");
        let wrapper = std
            .get_args()
            .nth(1)
            .map(|arg| arg.to_string_lossy().into_owned())
            .expect("the content command has a wrapper");
        (shell, wrapper)
    }

    #[test]
    fn a_login_runner_passes_each_family_its_own_flag() {
        for shell in ["/bin/zsh", "/bin/ash", "/opt/homebrew/bin/fish"] {
            assert_eq!(
                command_of(&login_runner(Some(shell))),
                (
                    PathBuf::from(shell),
                    vec!["-l".into(), "-c".into(), "cmd".into()]
                ),
                "{shell}"
            );
        }
        assert_eq!(
            command_of(&login_runner(Some("/usr/local/bin/nu"))),
            (
                PathBuf::from("/usr/local/bin/nu"),
                vec!["--login".into(), "-c".into(), "cmd".into()]
            )
        );
    }

    #[test]
    fn a_login_runner_on_csh_or_no_shell_runs_sh() {
        for shell in [Some("/bin/tcsh"), None] {
            let runner = login_runner(shell);
            assert_eq!(
                command_of(&runner),
                (
                    PathBuf::from(FALLBACK_SHELL),
                    vec!["-l".into(), "-c".into(), "cmd".into()]
                ),
                "{shell:?}"
            );
            assert_eq!(
                content_of(&runner).0,
                Path::new(FALLBACK_SHELL),
                "{shell:?}"
            );
            assert!(runner.shell_fallback().is_some(), "{shell:?}");
        }
    }

    #[test]
    fn a_login_runner_on_nu_hands_content_to_sh() {
        let runner = login_runner(Some("/usr/local/bin/nu"));

        let (shell, wrapper) = content_of(&runner);
        assert_eq!(shell, Path::new(FALLBACK_SHELL));
        assert!(wrapper.contains(r#"" -l -c ""#), "{wrapper}");
        assert_eq!(
            runner.shell_fallback(),
            Some(&ShellFallback::ContentUnsupported(PathBuf::from(
                "/usr/local/bin/nu"
            )))
        );
    }

    #[test]
    fn a_login_runner_on_zsh_runs_content_with_zsh() {
        let runner = login_runner(Some("/bin/zsh"));
        assert_eq!(content_of(&runner).0, Path::new("/bin/zsh"));
        assert_eq!(runner.shell_fallback(), None);
    }

    #[test]
    fn a_plain_runner_keeps_the_shell_it_was_given() {
        let runner = ShellCommandRunner::new("/x/noisy-shell", TIMEOUT);

        assert_eq!(
            command_of(&runner),
            (
                PathBuf::from("/x/noisy-shell"),
                vec!["-c".into(), "cmd".into()]
            )
        );
        assert_eq!(content_of(&runner).0, Path::new("/x/noisy-shell"));
        assert_eq!(runner.shell_fallback(), None);
    }

    // Builds a runner on `shell` whose notice counts how often it is told.
    fn counted(shell: Option<&str>) -> (ShellCommandRunner, Arc<AtomicUsize>) {
        let told = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&told);
        let runner =
            login_runner(shell).with_shell_fallback_notice(FallbackNotice::new(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
            }));
        (runner, told)
    }

    #[test]
    fn a_fallback_is_told_once_across_commands_and_clones() {
        let (runner, told) = counted(Some("/bin/tcsh"));
        let clone = runner.clone();

        command_of(&runner);
        command_of(&clone);
        content_of(&runner);
        command_of(&runner);

        assert_eq!(told.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_runner_without_a_fallback_tells_nothing() {
        let (runner, told) = counted(Some("/bin/zsh"));

        command_of(&runner);
        content_of(&runner);

        assert_eq!(told.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn concurrent_first_commands_tell_once() {
        let (runner, told) = counted(None);
        let start = Arc::new(Barrier::new(16));

        std::thread::scope(|scope| {
            for _ in 0..16 {
                let runner = runner.clone();
                let start = Arc::clone(&start);
                scope.spawn(move || {
                    start.wait();
                    command_of(&runner);
                });
            }
        });

        assert_eq!(told.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn nu_tells_only_when_content_runs() {
        let (runner, told) = counted(Some("/usr/local/bin/nu"));

        command_of(&runner);
        command_of(&runner);
        command_of(&runner);
        assert_eq!(
            told.load(Ordering::SeqCst),
            0,
            "an ordinary command ran under nu"
        );

        content_of(&runner);
        content_of(&runner);
        assert_eq!(told.load(Ordering::SeqCst), 1);
    }
}
