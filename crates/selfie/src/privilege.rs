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

/// Why selfie will not write dotfiles in this process.
///
/// Carries no data: there is exactly one thing being refused. It exists as a
/// type rather than a message so a caller cannot paraphrase it, and so a test
/// can assert the refusal happened without matching on prose.
///
/// Splits its wording into [`message`](Self::message) and
/// [`suggestion`](Self::suggestion) for the same reason
/// [`TargetRejection`](crate::fs::TargetRejection) does: the CLI has separate
/// channels for what went wrong and what to do about it, and joining them here
/// would forfeit that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SudoRefusal;

impl SudoRefusal {
    /// What is wrong, phrased to read after `"Error: "`.
    ///
    /// Names the consequence rather than the rule. "selfie will not run under
    /// sudo" tells someone they are blocked; the files landing root-owned in
    /// their home directory tells them why they did not want this.
    #[must_use]
    pub fn message(&self) -> &'static str {
        "Refusing to run under sudo: every dotfile in this run would be written as root, \
         including the ones under your home directory"
    }

    /// What to do instead.
    #[must_use]
    pub fn suggestion(&self) -> &'static str {
        "Re-run without sudo. selfie does not deploy to system paths yet; pass --allow-root only \
         if you intend every target to be written as root."
    }
}

/// Refuse a run that reached root through `sudo`.
///
/// The one place the rule lives. `allow_root` is the deliberate override, and is
/// the caller's to supply — the CLI from `--allow-root`, the MCP server never,
/// since an AI assistant has no reason to be driving selfie under sudo.
///
/// # Errors
///
/// [`SudoRefusal`] when the process is [`Elevation::Sudo`] and `allow_root` is
/// not set.
pub fn refuse_sudo<P: Privilege + ?Sized>(
    privilege: &P,
    allow_root: bool,
) -> Result<(), SudoRefusal> {
    if allow_root {
        return Ok(());
    }

    match privilege.elevation() {
        Elevation::Sudo => Err(SudoRefusal),
        Elevation::Root | Elevation::Unprivileged => Ok(()),
    }
}

/// The rule, over inputs a test can supply.
///
/// Separated from [`RealPrivilege`] because the alternative is mutating the
/// environment in a test, and `std::env::set_var` is `unsafe` in edition 2024
/// and racy against every other thread in the process.
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
            refuse_sudo(&fixed(Elevation::Sudo), false),
            Err(SudoRefusal)
        );
    }

    // The control that distinguishes this rule from "refuse at any euid 0". A
    // gate keyed on rootness alone passes the test above and fails this one,
    // which is the entire reason the rule reads `SUDO_UID` at all.
    #[test]
    fn a_real_root_run_is_allowed() {
        assert_eq!(refuse_sudo(&fixed(Elevation::Root), false), Ok(()));
    }

    #[test]
    fn an_ordinary_run_is_allowed() {
        assert_eq!(refuse_sudo(&fixed(Elevation::Unprivileged), false), Ok(()));
    }

    #[test]
    fn allow_root_overrides_the_refusal() {
        assert_eq!(refuse_sudo(&fixed(Elevation::Sudo), true), Ok(()));
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
            SudoRefusal.suggestion().contains("--allow-root"),
            "got: {}",
            SudoRefusal.suggestion()
        );
    }
}
