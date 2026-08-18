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
/// The distinction that matters is not root-vs-not. It is whether this process
/// is running as a *different user* than the one who invoked it:
/// [`Root`](Self::Root) is a legitimate design — a container, CI, or root
/// managing root's own dotfiles — and refusing it would put real uses behind a
/// flag to prevent an accident that does not happen there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Elevation {
    /// Running as the invoking user, who is not root.
    Unprivileged,
    /// Root, and not by way of another user's `sudo`.
    Root,
    /// Running as someone other than the user who invoked it, via `sudo`.
    ///
    /// Covers `sudo selfie` and `sudo -u alice selfie` alike. The second is not
    /// root at all and does the same kind of damage — alice-owned files through
    /// *your* home directory, and a deploy state your next ordinary run cannot
    /// read.
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
                "Refusing to run under sudo: sudo switched user, so every dotfile in this run \
                 would be written by that user rather than by you — including the ones under \
                 your home directory"
            }
            WriteScope::Repository => {
                "Refusing to run under sudo: sudo switched user, so this would leave objects, \
                 refs and index entries owned by that user inside a repository you own, which \
                 ordinary git then fails on"
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
                 cannot write stays one failed entry; pass --allow-sudo only if you intend every \
                 target in the run to be written by the user sudo switched to."
            }
            WriteScope::Repository => {
                "Re-run without sudo. Pass --allow-sudo only if this repository is meant to be \
                 owned by the user sudo switched to."
            }
        }
    }
}

/// Refuse a run that reached root through `sudo`.
///
/// The one place the rule lives. Private, and deliberately so: [`SudoPolicy`] is
/// the only way to apply it, which is what keeps a second service from growing
/// its own slightly different gate.
fn refuse_sudo<P: Privilege + ?Sized>(
    privilege: &P,
    allow_sudo: bool,
    scope: WriteScope,
) -> Result<(), SudoRefusal> {
    if allow_sudo {
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
/// `--allow-sudo`, and nothing else does.
#[derive(Debug, Clone, Copy, Default)]
pub struct SudoPolicy<P> {
    privilege: P,
    allow_sudo: bool,
}

impl<P: Privilege> SudoPolicy<P> {
    /// A policy that refuses a run reached through `sudo`.
    pub fn new(privilege: P) -> Self {
        Self {
            privilege,
            allow_sudo: false,
        }
    }

    /// Excuse a run reached through `sudo`, deliberately.
    #[must_use]
    pub fn allowing_sudo(mut self) -> Self {
        self.allow_sudo = true;
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
        refuse_sudo(&self.privilege, self.allow_sudo, scope).err()
    }
}

/// The rule, over inputs a test can supply.
///
/// Separated from [`RealPrivilege`] because the alternative is mutating the
/// environment in a test, and `std::env::set_var` is `unsafe` in edition 2024
/// and racy against every other thread in the process.
///
/// Compares `SUDO_UID` against the *effective* uid rather than merely asking
/// whether it is set. Presence alone would miss `sudo -u alice`, which is not
/// root and does the same damage; and it would refuse a process that merely
/// inherited the variable and is still running as the user who set it — a shell
/// started under `sudo -u $USER`, or anything spawned from one.
///
/// An unparsable value is treated as `sudo`. It cannot be compared, and
/// refusing is the safe direction; a real `sudo` always sets a decimal uid, so
/// anything else was not written by the thing this rule is about.
///
/// Only the `cfg(unix)` impl and the tests call it, so a non-unix build without
/// them would warn it dead — and clippy runs with `-D warnings`.
#[cfg(any(unix, test))]
fn classify(euid: u32, sudo_uid: Option<&str>) -> Elevation {
    let invoked_by_someone_else = match sudo_uid {
        None => false,
        Some(raw) => raw
            .trim()
            .parse::<u32>()
            .map(|uid| uid != euid)
            .unwrap_or(true),
    };

    if invoked_by_someone_else {
        Elevation::Sudo
    } else if euid == 0 {
        Elevation::Root
    } else {
        Elevation::Unprivileged
    }
}

/// The privilege of the actual process.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealPrivilege;

/// `SUDO_UID` is what distinguishes "someone reached for sudo" from "this process
/// is running as this user because that is the design". `doas`, `su` and `pkexec`
/// do not set it and so are not caught; that is accepted, because sudo is what
/// the `EACCES` workaround reaches for and an allowlist covering every escalation
/// tool would still miss one. **Do not expand this into privilege-tool
/// detection.**
///
/// A non-UTF-8 value reaches `classify` as `None` rather than as a value that
/// cannot be compared — `to_str` fails and the run is allowed. That is the one
/// place this leans permissive, and deliberately: a uid is decimal ASCII, so a
/// non-UTF-8 `SUDO_UID` was not written by sudo and says nothing about how this
/// process was started.
#[cfg(unix)]
impl Privilege for RealPrivilege {
    fn elevation(&self) -> Elevation {
        let sudo_uid = std::env::var_os("SUDO_UID");
        classify(
            nix::unistd::Uid::effective().as_raw(),
            sudo_uid.as_ref().and_then(|v| v.to_str()),
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
    fn allow_sudo_overrides_the_refusal() {
        assert_eq!(
            refuse_sudo(&fixed(Elevation::Sudo), true, WriteScope::Dotfiles),
            Ok(())
        );
    }

    // The public surface, which is what every service actually calls. Testing
    // only `refuse_sudo` would leave a `SudoPolicy` that ignores its own
    // override, or never consults the port, entirely uncovered.
    #[test]
    fn the_policy_carries_both_halves() {
        assert_eq!(
            SudoPolicy::new(fixed(Elevation::Sudo)).refusal(WriteScope::Dotfiles),
            Some(SudoRefusal(WriteScope::Dotfiles))
        );
        assert_eq!(
            SudoPolicy::new(fixed(Elevation::Sudo))
                .allowing_sudo()
                .refusal(WriteScope::Dotfiles),
            None
        );
        assert_eq!(
            SudoPolicy::new(fixed(Elevation::Root)).refusal(WriteScope::Dotfiles),
            None
        );
        assert_eq!(
            SudoPolicy::new(fixed(Elevation::Unprivileged)).refusal(WriteScope::Dotfiles),
            None
        );
    }

    const ROOT: u32 = 0;
    const ME: u32 = 501;
    const ALICE: u32 = 1001;

    #[test]
    fn sudo_is_running_as_someone_other_than_the_invoker() {
        // The case this rule exists for.
        assert_eq!(classify(ROOT, Some("501")), Elevation::Sudo);

        // selfie-04fl: `sudo -u alice selfie apply`. Not root at all, and the
        // reason the rule compares uids instead of asking whether SUDO_UID is
        // set — the earlier version allowed this, writing alice-owned files
        // through the invoker's home directory.
        assert_eq!(classify(ALICE, Some("501")), Elevation::Sudo);

        // Real root: a container, CI, or root managing root's own dotfiles.
        assert_eq!(classify(ROOT, None), Elevation::Root);
        assert_eq!(classify(ME, None), Elevation::Unprivileged);
    }

    // SUDO_UID is inherited by everything a sudo session starts, so a process
    // running as the user who set it is not elevated and must not be read as
    // such — `sudo -u $USER`, or any descendant of it. Refusing here would break
    // ordinary use on a machine where the variable is simply present.
    #[test]
    fn an_inherited_sudo_uid_matching_the_current_user_is_not_elevation() {
        assert_eq!(classify(ME, Some("501")), Elevation::Unprivileged);
        // Root running `sudo` is still root, and its home directory is unchanged.
        assert_eq!(classify(ROOT, Some("0")), Elevation::Root);
    }

    // Unparsable cannot be compared, so it is refused. A real sudo always writes
    // a decimal uid; anything else was not written by the thing being detected,
    // and refusing is the safe direction.
    #[test]
    fn an_uncomparable_sudo_uid_is_refused() {
        for raw in ["", "  ", "nonsense", "-1", "501x", "99999999999999999999"] {
            assert_eq!(
                classify(ME, Some(raw)),
                Elevation::Sudo,
                "SUDO_UID={raw:?} should not have been comparable"
            );
        }

        // Control: surrounding whitespace is not corruption.
        assert_eq!(classify(ME, Some(" 501 ")), Elevation::Unprivileged);
    }

    // The bug this exists for shipped, and no test caught it -- running it under a
    // real euid 0 did. `sync push` refused with "every dotfile in this run would
    // be written as root, including the ones under your home directory", on a
    // command that writes no dotfile at all.
    //
    // Asserts what each scope must NOT say rather than its exact wording, so the
    // messages stay editable while a copy-paste between the arms fails here.
    #[test]
    fn each_scope_describes_its_own_damage() {
        let dotfiles = SudoRefusal(WriteScope::Dotfiles);
        let repository = SudoRefusal(WriteScope::Repository);

        assert_ne!(dotfiles.message(), repository.message());
        assert_ne!(dotfiles.suggestion(), repository.suggestion());

        for forbidden in ["dotfile", "home directory"] {
            assert!(
                !repository.message().contains(forbidden),
                "the repository refusal must not describe a dotfile run, got: {}",
                repository.message()
            );
        }

        // No wording may hard-code "root". The gate refuses `sudo -u alice` too,
        // which is not root and writes alice-owned files — so naming root
        // describes the wrong user on exactly the case selfie-04fl added.
        //
        // Both methods, not just `message`: a defect in `suggestion` alone is
        // invisible to a test that only reads `message`.
        //
        // The override flag is named `--allow-sudo` rather than `--allow-root`
        // for this same reason, so nothing here needs an exemption.
        for text in [
            dotfiles.message(),
            dotfiles.suggestion(),
            repository.message(),
            repository.suggestion(),
        ] {
            assert!(
                !text.contains("root"),
                "a refusal must not name root: sudo -u is refused too, got: {text}"
            );
        }

        // The control: without it, a `Dotfiles` arm emptied of the same words
        // would satisfy every assertion above.
        assert!(
            dotfiles.message().contains("dotfile"),
            "got: {}",
            dotfiles.message()
        );
        assert!(
            repository.message().contains("repository"),
            "got: {}",
            repository.message()
        );
    }

    // The refusal has to say what to do, not only that it happened, and the
    // override is the only way past it -- a message that omits the flag leaves
    // the deliberate case with no route.
    #[test]
    fn the_suggestion_names_the_override() {
        assert!(
            SudoRefusal(WriteScope::Dotfiles)
                .suggestion()
                .contains("--allow-sudo"),
            "got: {}",
            SudoRefusal(WriteScope::Dotfiles).suggestion()
        );
    }
}
