//! Paths selfie writes to: the three functions that construct one, the checked
//! constructor a deploy path must go through, and why the distinction between
//! them is not cosmetic.
//!
//! carries the argument for why a target must reach a
//! writer unresolved. This module is the mechanism.

use std::path::{Path, PathBuf};

use crate::fs::filesystem::{FileSystem, FileSystemError};
use crate::paths::normalize_path;

/// The user's home directory, and nothing else.
///
/// [`expand_target_path`] takes this rather than a whole [`FileSystem`] so that
/// it *cannot* reach `canonicalize` or `expand_path`. Both resolve a path, and a
/// resolved target forfeits everything [`TargetPath`] exists to protect.
pub trait HomeDir {
    /// The user's home directory.
    ///
    /// # Errors
    ///
    /// Returns [`FileSystemError`] if the home directory cannot be determined.
    fn home(&self) -> Result<PathBuf, FileSystemError>;
}

impl<F: FileSystem + ?Sized> HomeDir for F {
    fn home(&self) -> Result<PathBuf, FileSystemError> {
        self.expand_path(Path::new("~"))
    }
}

/// A path selfie is going to write to, which nothing can resolve afterwards.
///
/// [`FileSystem::write_file_private`], [`FileSystem::write_file_no_follow`],
/// [`FileSystem::symlink_refusal`] and [`FileSystem::is_owner_only`] take this
/// instead of a `&Path`, so canonicalizing a path and then writing it fails to
/// compile. Each of those four applies to the path **as given**; handing one a
/// resolved path silently forfeits what it promises.
///
/// The guarantee is that you cannot resolve a `TargetPath` once you hold one --
/// not that the path inside was never resolved. There is deliberately no
/// `Deref`, so `Path`'s own `canonicalize`, `exists` and `metadata` are not *on*
/// the type. They stay one explicit [`path`](TargetPath::path) away, which is the
/// point: resolving has to be written down rather than reached by autoderef.
///
/// ```compile_fail
/// use selfie::fs::{RealFileSystem, expand_target_path};
///
/// let target = expand_target_path(&RealFileSystem, "/etc/hosts");
/// let _ = target.canonicalize();
/// ```
///
/// and only this module can mint one:
///
/// ```compile_fail
/// use selfie::fs::TargetPath;
///
/// let _ = TargetPath {
///     path: std::path::PathBuf::from("/etc/hosts"),
/// };
/// ```
///
/// Neither of those may fail for some unrelated reason -- a renamed export, a
/// bad import -- so this one has to build, naming every item the two above
/// import:
///
/// ```
/// use selfie::fs::{RealFileSystem, TargetPath, expand_target_path};
///
/// let target: TargetPath = expand_target_path(&RealFileSystem, "/etc/hosts");
/// assert_eq!(target.path(), std::path::Path::new("/etc/hosts"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetPath {
    path: PathBuf,
}

impl TargetPath {
    /// The path itself, for the reads and queries that do not need the guarantee.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A [`Display`](std::path::Display) for the path, as [`Path::display`] gives.
    #[must_use]
    pub fn display(&self) -> std::path::Display<'_> {
        self.path.display()
    }
}

/// Expand a deploy target without resolving its final component.
///
/// Expands a leading `~` and resolves `.`/`..` textually. It does **not**
/// canonicalize, and must not be made to: every writer this feeds refuses or
/// replaces a symlink at the path as given, and `canonicalize` returns the
/// link's destination, so the writer would be looking at an ordinary file. That
/// is why this takes a [`HomeDir`] rather than a [`FileSystem`].
///
/// Canonicalization forfeits the guarantee exactly when it *succeeds*. A
/// dangling link fails to canonicalize and so reaches the writer unresolved,
/// which is why reintroducing it here is caught by the tests whose links resolve
/// and not by the others.
///
/// Two consequences of never touching the filesystem, both intended: a symlinked
/// *parent* directory is still followed, and `a/link/../b` becomes `a/b` rather
/// than wherever `link` points.
///
/// `~user` is not supported. It falls through to the literal path, which is then
/// relative. This function does **not** refuse it: [`deploy_target`] does, by
/// name, and every path that deploys or tracks obtains its target from there
/// instead. Callers that only compare or display a target use this one
/// deliberately, because a spec may hold an entry the rule refuses and a
/// comparison still has to answer.
#[must_use]
pub fn expand_target_path<H: HomeDir + ?Sized>(home: &H, target: &str) -> TargetPath {
    let raw = if target.starts_with('~') {
        // Only the leading `~` component is expanded; everything after it is
        // joined unresolved, which is the whole point. A bare `~` or `~/` is the
        // whole path, so expansion does reach the final component there -- those
        // are directories and fail the write anyway.
        let (tilde, rest) = match target.split_once('/') {
            Some((tilde, rest)) => (tilde, Some(rest)),
            None => (target, None),
        };

        let expanded = if tilde == "~" { home.home().ok() } else { None };

        match expanded {
            Some(home) => match rest {
                Some(rest) => home.join(rest),
                None => home,
            },
            // No home directory, or a `~user` form. Falling back to the literal
            // path leaves it relative, and `deploy_target` then refuses it --
            // better than writing a credential into a directory literally named
            // `~` beneath the current directory.
            None => PathBuf::from(target),
        }
    } else {
        PathBuf::from(target)
    };

    TargetPath {
        path: normalize_path(&raw),
    }
}

/// A path *inside* a directory selfie manages, taken exactly as given.
///
/// The third and weakest constructor, and the only one that is not about a
/// deploy target at all. It exists because the writers and the refusal checks
/// take a [`TargetPath`] rather than a `&Path`, and selfie also writes to — and
/// reads from — paths it composed itself: the copy `track` places in the dotfiles
/// repository, the package YAML `save_package` writes, and the repository files
/// `apply` and `dotfiles drift` read back. Those need the same no-symlink,
/// no-fifo treatment, and there is otherwise no way to ask for it.
///
/// It promises **less** than the other two, and the difference is the whole
/// reason it is a separate function rather than an argument to them:
///
/// - No [`TargetRejection`] rule. It does not require an absolute path and does
///   not refuse `~user/…`.
/// - No expansion and no normalization. A leading `~` stays a literal `~`.
/// - It is `pub(crate)` and not re-exported, so `expand_target_path` remains the
///   only constructor outside this crate.
///
/// **Never call this on a user-supplied deploy target.** That is
/// [`deploy_target`]'s job, and routing one through here would drop the rule
/// every command applies to targets. The inputs here are paths selfie built from
/// a configured directory plus components it has already validated.
///
/// Deliberately takes a `&Path` and performs no I/O, so it cannot resolve
/// anything: what goes in is what the writer sees.
#[must_use]
pub(crate) fn repository_path(path: &Path) -> TargetPath {
    TargetPath {
        path: path.to_path_buf(),
    }
}

/// Why selfie will not deploy to a target.
///
/// The whole rule, and the only wording any command uses to refuse one, so
/// `selfie spec validate`, `selfie apply`, `dotfiles drift` and the track
/// commands cannot describe the same target differently.
///
/// The three causes are not all decidable at the same time, which is deliberate:
/// [`of`](TargetRejection::of) reads the target as written and so can run
/// offline, while [`NoHome`](TargetRejection::NoHome) depends on the machine and
/// can only be seen after expansion. They render through the same two methods
/// because they answer the same user question.
///
/// Deliberately implements neither `Display` nor `Error`, which is a departure
/// from this repository's `thiserror` convention and is the point: a caller that
/// could format this directly would, and each of the four sites needs the words
/// in a different frame -- with a field path and a suggestion in the validator,
/// after `"Skipping 'X': "` in a warning, after `"Cannot track 'X': "` in a
/// failure. Going through [`message`](Self::message) and
/// [`suggestion`](Self::suggestion) is what keeps those four in step. It also
/// keeps a `{:?}` of an error out of user-facing text, which
/// warns about for the failure types on this path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetRejection {
    /// `~user/...`. Selfie does not resolve another user's home directory.
    NamedUserHome,
    /// Neither absolute nor `~/`-relative.
    Relative,
    /// `~/...`, but the home directory could not be determined. Machine state
    /// rather than a defect in the spec, and so never produced by
    /// [`of`](TargetRejection::of).
    NoHome,
}

impl TargetRejection {
    /// The rule applied to the target *as written*.
    ///
    /// Needs no home directory, so `selfie spec validate` applies offline the
    /// same rule apply applies at deploy time. Cannot return
    /// [`NoHome`](Self::NoHome): that one is machine state, not spec state, and
    /// only [`deploy_target`]'s post-expansion check can see it.
    #[must_use]
    pub fn of(target: &str) -> Option<Self> {
        // A bare `~` and a `~/…` are the supported forms. Testing
        // `starts_with('~')` alone is what let `~alice/.gemrc` through the
        // validator and into a spec that then silently failed to deploy.
        if target == "~" || target.starts_with("~/") {
            return None;
        }

        if target.starts_with('~') {
            return Some(Self::NamedUserHome);
        }

        if Path::new(target).is_absolute() {
            return None;
        }

        Some(Self::Relative)
    }

    /// What is wrong, phrased to read after `"Dotfile "`, after
    /// `"Skipping 'X': "` and after `"Cannot track 'X': "` alike.
    ///
    /// Lowercase and starting with the subject it is about for that reason: one
    /// string serves the validator, the runtime warnings and the track failures,
    /// which is what stops the four wordings drifting apart again. It carries no
    /// trailing punctuation, so a caller joining it to
    /// [`suggestion`](Self::suggestion) supplies the stop itself.
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            Self::NamedUserHome => {
                "target uses the '~user' form; selfie does not resolve another user's home directory"
            }
            Self::Relative => "target path is not absolute; it must be absolute or start with '~/'",
            Self::NoHome => {
                "target starts with '~/' but the home directory could not be determined"
            }
        }
    }

    /// What to write instead.
    #[must_use]
    pub fn suggestion(self) -> &'static str {
        match self {
            Self::NamedUserHome => {
                "Use '~/…' for your own home directory, or an absolute path like '/etc/config'."
            }
            Self::Relative => {
                "Use '~/.config/file' for a path under your home directory, or an absolute path \
                 like '/etc/config'."
            }
            Self::NoHome => "Set HOME, or write the target as an absolute path.",
        }
    }
}

/// The constructor a deploy or track path must use.
///
/// Says *when* to call it rather than only what it does, because the question
/// this module exists to answer is "what must I call before writing to a
/// target?" and [`expand_target_path`] is the wrong answer to it. That one is
/// for callers who only compare or display a target; this one is for callers who
/// are going to act on it.
///
/// Applies [`TargetRejection::of`] first, then expands, then re-checks that what
/// came back is absolute. The re-check is not redundant: a `~/…` whose home
/// directory cannot be determined falls through to the literal relative path,
/// which the textual rule cannot see. It is also what keeps "a [`TargetPath`]
/// from `deploy_target` is absolute" true if the rule and the expander ever
/// drift apart.
///
/// Mints nothing itself -- it delegates to [`expand_target_path`], so the set of
/// functions that construct a [`TargetPath`] is unchanged.
///
/// # Errors
///
/// [`TargetRejection`] naming the form that was refused.
pub fn deploy_target<H: HomeDir + ?Sized>(
    home: &H,
    target: &str,
) -> Result<TargetPath, TargetRejection> {
    if let Some(rejection) = TargetRejection::of(target) {
        return Err(rejection);
    }

    let path = expand_target_path(home, target);

    if !path.path().is_absolute() {
        return Err(TargetRejection::NoHome);
    }

    Ok(path)
}

// The form of `target` to record in a spec: `~`-relative when it names a path
// under the home directory, so the entry means the same file on another machine.
//
// The inverse of `expand_target_path`, and the reason a tracked entry matches a
// hand-written one. A target outside the home directory is left absolute:
// `/etc/nginx.conf` names the same file everywhere already.
//
// Returns `target` unchanged when there is no home directory to measure against,
// since an unexpandable `~` is not something a recorded path can carry.
//
// Home is taken as `expand_path` gives it, which canonicalizes. A `$HOME` with a
// symlinked component that the caller names unresolved therefore fails to match
// and records absolute -- today's behavior, not a regression.
#[must_use]
pub(crate) fn portable_target<H: HomeDir + ?Sized>(home_dir: &H, target: &str) -> String {
    let Ok(home) = home_dir.home() else {
        return target.to_string();
    };

    // One lookup, used for both halves. Expanding against one home and
    // collapsing against another would leave the tilde in place or strip the
    // wrong prefix.
    let home = KnownHome(home);
    let expanded = expand_target_path(&home, target);

    collapse_home(&home.0, expanded.path())
}

// A home directory already in hand, so `portable_target` can hand the same one
// to `expand_target_path` instead of asking twice.
struct KnownHome(PathBuf);

impl HomeDir for KnownHome {
    fn home(&self) -> Result<PathBuf, FileSystemError> {
        Ok(self.0.clone())
    }
}

// Comparison must stay component-wise. `/home/steven` is not under `/home/steve`,
// but a byte-prefix strip yields `~n/.gemrc` -- a path that deploys somewhere
// else and reports success.
fn collapse_home(home: &Path, expanded: &Path) -> String {
    // A home with no normal component -- `HOME=/`, a container running as root --
    // would put every absolute path under it, `/etc/nginx.conf` included.
    if !home
        .components()
        .any(|c| matches!(c, std::path::Component::Normal(_)))
    {
        return expanded.to_string_lossy().into_owned();
    }

    match expanded.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => Path::new("~").join(rest).to_string_lossy().into_owned(),
        Err(_) => expanded.to_string_lossy().into_owned(),
    }
}

// The deploy state file's path.
//
// The second of the three constructors, and weaker than `expand_target_path`:
// the configured branch joins `state_dir` exactly as given, so what comes back is
// only as unresolved as what the caller was configured with, and
// `SelfieConfigBuilder::state_directory` accepts any `PathBuf`. `repository_path`
// is weaker still — it applies no rule at all.
//
// The fallback expands `~` alone rather than the whole path because
// `expand_path` canonicalizes, and a canonicalized state path lets
// `write_file_private` replace whatever a planted symlink points at instead of
// the link. Creating the directory first so canonicalization succeeds would
// remove the availability problem and keep the security one.
pub(crate) fn state_file_path<H: HomeDir + ?Sized>(
    home: &H,
    state_dir: Option<&Path>,
    filename: &str,
) -> Result<TargetPath, FileSystemError> {
    if let Some(state_dir) = state_dir {
        return Ok(TargetPath {
            path: state_dir.join(filename),
        });
    }

    // XDG_STATE_HOME (`~/.local/state/selfie`) per the XDG Base Directory
    // Specification: deploy state is per-machine, non-portable data.
    let home = home.home().map_err(|_| {
        FileSystemError::IoError(std::sync::Arc::new(std::io::Error::other(
            "Cannot determine home directory for deploy state file",
        )))
    })?;

    Ok(TargetPath {
        path: home
            .join(".local")
            .join("state")
            .join("selfie")
            .join(filename),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::MockFileSystem;

    #[test]
    fn an_absolute_target_is_left_alone() {
        let fs = MockFileSystem::default();
        let result = expand_target_path(&fs, "/tmp/some/file");
        assert_eq!(result.path(), Path::new("/tmp/some/file"));
    }

    #[test]
    fn a_leading_tilde_expands_and_the_rest_is_joined() {
        let mut fs = MockFileSystem::default();
        fs.mock_expand_path("~", "/home/user");
        let result = expand_target_path(&fs, "~/test-file");
        assert_eq!(result.path(), Path::new("/home/user/test-file"));
    }

    #[test]
    fn a_named_tilde_is_not_expanded() {
        let fs = MockFileSystem::default();
        let result = expand_target_path(&fs, "~alice/test-file");
        assert_eq!(result.path(), Path::new("~alice/test-file"));
    }

    /// A filesystem whose home directory cannot be determined.
    ///
    /// The only fixture that reaches `deploy_target`'s post-expansion re-check:
    /// every other input is decided by `TargetRejection::of` before expansion.
    fn no_home() -> MockFileSystem {
        let mut fs = MockFileSystem::default();
        fs.expect_expand_path().returning(|_| {
            Err(FileSystemError::IoError(std::sync::Arc::new(
                std::io::Error::other("no home"),
            )))
        });
        fs
    }

    #[test]
    fn a_named_user_target_is_refused_by_name() {
        let fs = MockFileSystem::default();
        assert_eq!(
            deploy_target(&fs, "~alice/.gemrc"),
            Err(TargetRejection::NamedUserHome)
        );
        // No slash is the other form the old validator accepted, since it tested
        // `starts_with('~')`. It never reaches the expander -- `of` refuses it
        // first, which is why this fixture needs no mocked home directory.
        assert_eq!(
            deploy_target(&fs, "~alice"),
            Err(TargetRejection::NamedUserHome)
        );
    }

    #[test]
    fn a_relative_target_is_refused() {
        let fs = MockFileSystem::default();
        assert_eq!(
            deploy_target(&fs, "relative/path.txt"),
            Err(TargetRejection::Relative)
        );
        assert_eq!(deploy_target(&fs, ""), Err(TargetRejection::Relative));
    }

    // The reason `deploy_target` re-checks after expanding rather than trusting
    // the textual rule: `~/…` is a supported form, so the rule passes it, and
    // only expansion can discover there is no home directory to expand it
    // against. Refusing it as `NoHome` rather than as `Relative` is the point --
    // the path the user wrote does start with `~/`, so telling them it is not
    // absolute describes a rule their input appears to satisfy.
    #[test]
    fn a_tilde_target_with_no_home_directory_is_refused() {
        assert_eq!(
            deploy_target(&no_home(), "~/.gemrc"),
            Err(TargetRejection::NoHome)
        );
    }

    #[test]
    fn an_absolute_target_and_a_tilde_target_are_accepted() {
        let fs = MockFileSystem::default();
        assert_eq!(
            deploy_target(&fs, "/etc/hosts").unwrap().path(),
            Path::new("/etc/hosts")
        );

        // The control: a `deploy_target` that refused everything would pass the
        // three tests above and fail here, and so would one that stopped
        // expanding.
        let mut fs = MockFileSystem::default();
        fs.mock_expand_path("~", "/home/user");
        assert_eq!(
            deploy_target(&fs, "~/test-file").unwrap().path(),
            Path::new("/home/user/test-file")
        );
    }

    // selfie-hkhb: the refusal for `~user/…` used to be the absolute-path
    // message, which describes a rule the input visibly satisfies -- it starts
    // with `~`. Naming the unsupported form is the fix, so a message that
    // reverts to restating absoluteness has to fail here.
    #[test]
    fn the_named_user_refusal_does_not_restate_the_absoluteness_rule() {
        let message = TargetRejection::NamedUserHome.message();
        assert!(message.contains("~user"), "got: {message}");
        assert!(!message.contains("is not absolute"), "got: {message}");
        assert!(!message.contains("must be absolute"), "got: {message}");
    }

    // The textual rule and the whole rule must agree on every target, or a spec
    // passes `selfie spec validate` and then fails to deploy -- selfie-jlum
    // exactly.
    //
    // What this can and cannot catch, measured rather than assumed. It catches
    // `of` wrongly *accepting* a form expansion cannot make absolute: the
    // post-expansion re-check then refuses it and the two sides disagree.
    // Restoring the pre-fix `starts_with('~')` acceptance fails this test;
    // deleting the `NamedUserHome` arm does **not** -- that downgrades `~alice`
    // to `Relative`, both sides still refuse it, and they still agree.
    // `a_named_user_target_is_refused_by_name` is what catches the downgrade, by
    // asserting *which* rejection comes back.
    //
    // It does not catch `of` wrongly *rejecting* a good form either, because the
    // early return makes both sides say "refused" in agreement; the accept
    // assertions in `an_absolute_target_and_a_tilde_target_are_accepted` cover
    // that. Nor `deploy_target` dropping the `of` call entirely -- for every row
    // here the re-check alone reproduces the same split.
    //
    // `NoHome` is excluded by construction: a home directory is mocked here, and
    // it is machine state rather than something a spec can be wrong about.
    //
    // `../x` is here to pin the agreement, not to describe what normalization
    // does with a leading `..` -- see selfie-girj, which is why this comment does
    // not say.
    #[test]
    fn the_textual_rule_and_the_deploy_rule_agree() {
        // `KnownHome` rather than a mock: `mock_expand_path` is `return_once`, and
        // this table expands `~` more than once.
        let fs = KnownHome(PathBuf::from("/home/user"));

        for target in [
            "~", "~/", "~/x", "~alice", "~alice/x", "/x", "x", "", "./x", "../x", "~/../..",
        ] {
            assert_eq!(
                TargetRejection::of(target).is_none(),
                deploy_target(&fs, target).is_ok(),
                "the two rules disagree about {target:?}"
            );
        }
    }

    fn collapsed(home: &str, expanded: &str) -> String {
        collapse_home(Path::new(home), Path::new(expanded))
    }

    #[test]
    fn a_target_under_home_is_recorded_with_a_tilde() {
        assert_eq!(
            collapsed(
                "/Users/sloveless",
                "/Users/sloveless/.config/ghostty/config"
            ),
            "~/.config/ghostty/config"
        );
    }

    #[test]
    fn a_home_that_is_only_a_string_prefix_is_not_collapsed() {
        assert_eq!(
            collapsed("/home/steve", "/home/steven/.gemrc"),
            "/home/steven/.gemrc"
        );
        assert_eq!(
            collapsed("/Users/sloveless", "/Users/sloveless-old/.config/x"),
            "/Users/sloveless-old/.config/x"
        );
    }

    #[test]
    fn a_root_home_does_not_swallow_a_system_path() {
        assert_eq!(collapsed("/", "/etc/nginx.conf"), "/etc/nginx.conf");
    }

    #[test]
    fn a_target_outside_home_is_recorded_unchanged() {
        assert_eq!(
            collapsed("/home/steve", "/etc/nginx.conf"),
            "/etc/nginx.conf"
        );
    }

    #[test]
    fn home_itself_collapses_to_a_bare_tilde() {
        assert_eq!(collapsed("/home/steve", "/home/steve"), "~");
    }

    #[test]
    fn a_hand_written_tilde_target_round_trips_unchanged() {
        let mut fs = MockFileSystem::default();
        fs.mock_expand_path("~", "/home/user");
        assert_eq!(
            portable_target(&fs, "~/.config/bat/config"),
            "~/.config/bat/config"
        );
    }

    // `docs/package-files.md` promises this. It holds only transitively, through
    // `expand_target_path`'s `normalize_path`, so nothing else asserts it.
    #[test]
    fn a_recorded_target_is_normalized() {
        let mut fs = MockFileSystem::default();
        fs.mock_expand_path("~", "/home/user");
        assert_eq!(portable_target(&fs, "~/.config/../.gemrc"), "~/.gemrc");
    }

    #[test]
    fn a_target_is_recorded_as_given_when_there_is_no_home() {
        let mut fs = MockFileSystem::default();
        fs.expect_expand_path().returning(|_| {
            Err(FileSystemError::IoError(std::sync::Arc::new(
                std::io::Error::other("no home"),
            )))
        });
        assert_eq!(
            portable_target(&fs, "/Users/sloveless/.gemrc"),
            "/Users/sloveless/.gemrc"
        );
    }

    // `repository_path` must stay the identity on the path it is handed. Written
    // as an absence test on purpose: the risk it guards is not a wrong result
    // today but someone later "unifying" the three constructors by routing this
    // one through `expand_target_path`. Each row below is a transformation that
    // function performs and this one must not, so the unification fails here
    // rather than silently rewriting a repository path.
    //
    // The `~` row is the one that bites hardest: a dotfiles directory literally
    // named `~` is legal, and expanding it would send a track copy into the home
    // directory instead.
    #[test]
    fn a_repository_path_is_taken_exactly_as_given() {
        for raw in [
            "/pkgs/fnm/config.fish",   // absolute, untouched
            "~/pkgs/fnm/config.fish",  // no tilde expansion
            "~alice/pkgs/config.fish", // no named-user handling
            "pkgs/fnm/config.fish",    // relative is accepted, not refused
            "/pkgs/./fnm/../fnm/c",    // no normalization
            "",                        // no emptiness rule
        ] {
            assert_eq!(
                repository_path(Path::new(raw)).path(),
                Path::new(raw),
                "repository_path transformed {raw:?}"
            );
        }
    }

    // A home directory is mocked here and ignored by construction: the function
    // takes no `HomeDir`, so this asserts the *type* cannot reach one. If someone
    // gives it a `HomeDir` parameter, this stops compiling rather than silently
    // starting to expand.
    #[test]
    fn a_repository_path_needs_no_home_directory() {
        let raw = Path::new("~/pkgs/config.fish");
        assert_eq!(repository_path(raw).path(), raw);
    }

    #[test]
    fn a_configured_state_directory_is_used_as_given() {
        let fs = MockFileSystem::default();
        let state_dir = PathBuf::from("/var/state");
        let result = state_file_path(&fs, Some(&state_dir), "deploy-state.yml").unwrap();
        assert_eq!(result.path(), Path::new("/var/state/deploy-state.yml"));
    }

    #[test]
    fn the_default_state_path_lands_under_xdg_state_home() {
        let mut fs = MockFileSystem::default();
        fs.mock_expand_path("~", "/home/user");
        let result = state_file_path(&fs, None, "deploy-state.yml").unwrap();
        assert_eq!(
            result.path(),
            Path::new("/home/user/.local/state/selfie/deploy-state.yml")
        );
    }
}
