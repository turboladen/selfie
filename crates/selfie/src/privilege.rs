//! Whether the running process holds privilege it was not meant to write with.
//!
//! selfie never elevates, so a target it cannot write fails cleanly. The hazard
//! is the workaround: `sudo selfie apply` has no per-entry scope, so every entry
//! in the run is written as root — including the `~/` ones, which land
//! root-owned inside the user's own home directory.
//!
//! It is worse than a permissions mistake, for two reasons that are the argument
//! for refusing rather than warning:
//!
//! - `~` does not resolve to a fixed place under sudo. Expansion reads `$HOME`
//!   first, and sudo's `$HOME` handling is a sudoers policy that varies by
//!   platform, so on an `env_reset` machine the dotfiles land in `/root` and
//!   apply reports **success**. There is nothing for a warning to key on.
//! - The damage outlives the run. Deploy state is written owner-only, so a sudo
//!   run leaves it `root:root 0600`; the next ordinary run cannot read it and
//!   proceeds with an empty state, re-prompting every conflict. If the state
//!   directory did not exist beforehand, that one does not self-heal at all.
//!
//! `sync push` and `sync pull` are refused for a related reason and a worse
//! outcome: they deploy nothing, but they commit, fetch and merge as root,
//! leaving root-owned objects, refs and index entries inside a repository the
//! user owns. A root-owned state file is replaced by the next successful run,
//! because it comes from a user-owned temporary file. Git objects are not.

/// How much privilege the process holds, and where it came from.
///
/// The distinction between the two root cases is the whole rule.
/// [`Root`](Self::Root) is a legitimate design — a container, CI, or root
/// managing root's own dotfiles — and refusing it would put real uses behind a
/// flag to prevent an accident that does not happen there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Elevation {
    /// Not root.
    Unprivileged,
    /// Root, with no sign of `sudo`.
    Root,
    /// Root, reached through `sudo`.
    Sudo,
}

/// Port reporting the privilege the process is running with.
///
/// A port rather than an inline environment read so the policy stays
/// library-owned — the MCP server needs the same refusal the CLI gets — and so
/// a test can simulate running under sudo without being root.
#[cfg_attr(any(test, feature = "with_mocks"), mockall::automock)]
pub trait Privilege {
    /// The privilege this process holds.
    fn elevation(&self) -> Elevation;
}

/// What a refused run would have written as root.
///
/// The refusal is the same rule either way, but the two consequences share no
/// wording: a `sync push` writes no dotfile at all, so telling someone their home
/// directory is at stake describes a run that was never going to happen. A
/// message that confidently names the wrong damage is worse than a vague one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteScope {
    /// Dotfiles, the deploy state file, and the dotfiles repository.
    Dotfiles,
    /// Commits, refs and index entries in the user's git repository.
    Repository,
}

/// Why selfie will not write in this process.
///
/// Carries only [`WriteScope`]: there is one rule, and the scope decides how its
/// consequence reads. It exists as a type rather than a message so a caller
/// cannot paraphrase it, and so a test can assert the refusal happened without
/// matching on prose.
///
/// Splits its wording into [`message`](Self::message) and
/// [`suggestion`](Self::suggestion) for the same reason
/// [`TargetRejection`](crate::fs::TargetRejection) does: the CLI has separate
/// channels for what went wrong and what to do about it, and joining them here
/// would forfeit that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SudoRefusal(WriteScope);

impl SudoRefusal {
    /// What is wrong, phrased to read after `"Error: "`.
    ///
    /// Names the consequence rather than the rule. "selfie will not run under
    /// sudo" tells someone they are blocked; the files landing root-owned where
    /// they have to live with them tells them why they did not want this.
    #[must_use]
    pub fn message(&self) -> &'static str {
        match self.0 {
            WriteScope::Dotfiles => {
                "Refusing to run under sudo: every dotfile in this run would be written as root, \
                 including the ones under your home directory"
            }
            WriteScope::Repository => {
                "Refusing to run under sudo: this would leave root-owned objects, refs and index \
                 entries inside a repository you own, which ordinary git then fails on"
            }
        }
    }

    /// What to do instead.
    ///
    /// Does not say system paths are unsupported. An absolute target is a
    /// documented form, and selfie writes one whenever the running user can —
    /// what it has no way to express is elevating for *one* entry.
    #[must_use]
    pub fn suggestion(&self) -> &'static str {
        match self.0 {
            WriteScope::Dotfiles => {
                "Re-run without sudo. selfie has no per-entry privilege scope, so a target you \
                 cannot write stays one failed entry; pass --allow-root only if you intend every \
                 target in the run to be written as root."
            }
            WriteScope::Repository => {
                "Re-run without sudo. Pass --allow-root only if this repository is meant to be \
                 root-owned."
            }
        }
    }
}

/// Refuse a run that reached root through `sudo`.
///
/// The one place the rule lives. Private, and deliberately so: [`RootPolicy`] is
/// the only way to apply it, which is what keeps a second service from growing
/// its own slightly different gate.
fn refuse_sudo<P: Privilege + ?Sized>(
    privilege: &P,
    allow_root: bool,
    scope: WriteScope,
) -> Result<(), SudoRefusal> {
    if allow_root {
        return Ok(());
    }

    match privilege.elevation() {
        Elevation::Sudo => Err(SudoRefusal(scope)),
        Elevation::Root | Elevation::Unprivileged => Ok(()),
    }
}

/// A privilege port and the override that can excuse it, as one value.
///
/// The two are meaningless apart — a port with no override refuses cases the
/// user asked for, and an override with no port excuses nothing — so they travel
/// together and a half-configured gate cannot be constructed. Every service that
/// refuses under sudo holds one of these rather than the pair, which is what
/// stops two services drifting onto two slightly different rules.
///
/// The override is off unless an adapter turns it on: the CLI does, from
/// `--allow-root`, and nothing else does.
#[derive(Debug, Clone, Copy, Default)]
pub struct RootPolicy<P> {
    privilege: P,
    allow_root: bool,
}

impl<P: Privilege> RootPolicy<P> {
    /// A policy that refuses a run reached through `sudo`.
    pub fn new(privilege: P) -> Self {
        Self {
            privilege,
            allow_root: false,
        }
    }

    /// Excuse a run reached through `sudo`, deliberately.
    #[must_use]
    pub fn allowing_root(mut self) -> Self {
        self.allow_root = true;
        self
    }

    /// The refusal this run must report instead of doing any work, if any.
    ///
    /// `scope` is what the caller would have written, and only affects the
    /// wording — the rule is the same for both.
    ///
    /// Callers evaluate this *before* spawning, so `P` never has to cross an
    /// async boundary and needs no `'static` or `Clone` bound of its own.
    #[must_use]
    pub fn refusal(&self, scope: WriteScope) -> Option<SudoRefusal> {
        refuse_sudo(&self.privilege, self.allow_root, scope).err()
    }
}

/// The rule, over inputs a test can supply.
///
/// Separated from [`RealPrivilege`] because the alternative is mutating the
/// environment in a test, and `std::env::set_var` is `unsafe` in edition 2024
/// and racy against every other thread in the process.
///
/// Only the `cfg(unix)` impl and the tests call it, so a non-unix build without
/// them would warn it dead — and clippy runs with `-D warnings`.
#[cfg(any(unix, test))]
fn classify(is_root: bool, under_sudo: bool) -> Elevation {
    match (is_root, under_sudo) {
        (false, _) => Elevation::Unprivileged,
        (true, false) => Elevation::Root,
        (true, true) => Elevation::Sudo,
    }
}

/// The privilege of the actual process.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealPrivilege;

/// `SUDO_UID` is what distinguishes "someone reached for sudo to get past an
/// `EACCES`" from "this process is root because that is the design". `doas`, `su`
/// and `pkexec` do not set it and so are not caught; that is accepted, because
/// sudo is what the workaround reaches for and an allowlist covering every
/// escalation tool would still miss one. **Do not expand this into
/// privilege-tool detection.**
///
/// Presence alone counts, including an empty value. Sudo sets it to a number, so
/// an empty one is already strange; treating it as sudo refuses more, which is
/// the safe direction.
#[cfg(unix)]
impl Privilege for RealPrivilege {
    fn elevation(&self) -> Elevation {
        classify(
            nix::unistd::Uid::effective().is_root(),
            std::env::var_os("SUDO_UID").is_some(),
        )
    }
}

/// Windows has no euid and no sudo, so there is nothing here to refuse.
#[cfg(not(unix))]
impl Privilege for RealPrivilege {
    fn elevation(&self) -> Elevation {
        Elevation::Unprivileged
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed(elevation: Elevation) -> MockPrivilege {
        let mut privilege = MockPrivilege::new();
        privilege.expect_elevation().returning(move || elevation);
        privilege
    }

    #[test]
    fn a_sudo_run_is_refused() {
        assert_eq!(
            refuse_sudo(&fixed(Elevation::Sudo), false, WriteScope::Dotfiles),
            Err(SudoRefusal(WriteScope::Dotfiles))
        );
    }

    // The control that distinguishes this rule from "refuse at any euid 0". A
    // gate keyed on rootness alone passes the test above and fails this one,
    // which is the entire reason the rule reads `SUDO_UID` at all.
    #[test]
    fn a_real_root_run_is_allowed() {
        assert_eq!(
            refuse_sudo(&fixed(Elevation::Root), false, WriteScope::Dotfiles),
            Ok(())
        );
    }

    #[test]
    fn an_ordinary_run_is_allowed() {
        assert_eq!(
            refuse_sudo(&fixed(Elevation::Unprivileged), false, WriteScope::Dotfiles),
            Ok(())
        );
    }

    #[test]
    fn allow_root_overrides_the_refusal() {
        assert_eq!(
            refuse_sudo(&fixed(Elevation::Sudo), true, WriteScope::Dotfiles),
            Ok(())
        );
    }

    // The public surface, which is what every service actually calls. Testing
    // only `refuse_sudo` would leave a `RootPolicy` that ignores its own
    // override, or never consults the port, entirely uncovered.
    #[test]
    fn the_policy_carries_both_halves() {
        assert_eq!(
            RootPolicy::new(fixed(Elevation::Sudo)).refusal(WriteScope::Dotfiles),
            Some(SudoRefusal(WriteScope::Dotfiles))
        );
        assert_eq!(
            RootPolicy::new(fixed(Elevation::Sudo))
                .allowing_root()
                .refusal(WriteScope::Dotfiles),
            None
        );
        assert_eq!(
            RootPolicy::new(fixed(Elevation::Root)).refusal(WriteScope::Dotfiles),
            None
        );
        assert_eq!(
            RootPolicy::new(fixed(Elevation::Unprivileged)).refusal(WriteScope::Dotfiles),
            None
        );
    }

    #[test]
    fn sudo_is_root_plus_sudo_uid_and_nothing_less() {
        assert_eq!(classify(true, true), Elevation::Sudo);
        assert_eq!(classify(true, false), Elevation::Root);
        // SUDO_UID is inherited by anything sudo starts, so a non-root process
        // can carry one. It is not elevation and must not be read as any.
        assert_eq!(classify(false, true), Elevation::Unprivileged);
        assert_eq!(classify(false, false), Elevation::Unprivileged);
    }

    // The refusal has to say what to do, not only that it happened, and the
    // override is the only way past it -- a message that omits the flag leaves
    // the deliberate case with no route.
    #[test]
    fn the_suggestion_names_the_override() {
        assert!(
            SudoRefusal(WriteScope::Dotfiles)
                .suggestion()
                .contains("--allow-root"),
            "got: {}",
            SudoRefusal(WriteScope::Dotfiles).suggestion()
        );
    }
}
