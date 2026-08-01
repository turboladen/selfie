//! Separating a command's own stdout from the shell's.
//!
//! Two mechanisms, because they draw different boundaries: the extra descriptor
//! separates by *source*, which is the only thing that catches output arriving at
//! an unpredictable time (a job the profile backgrounded), and the markers
//! separate by *position*, which is the only thing that catches what the shell
//! writes after the command on the descriptor the command itself used.
//!
//! **A startup file that redirects [`CAPTURE_FD`] receives the content**, and
//! selfie's own capture then comes up empty and fails closed. That is a known
//! limit against an accidental collision, not a boundary against a hostile
//! profile — one of those already owns the session. See
//! .

use uuid::Uuid;

/// Environment variable carrying the shell to run the command with.
pub(super) const SHELL_VAR: &str = "SELFIE_CONTENT_SHELL";

/// Environment variable carrying the recipe the shell is given.
pub(super) const COMMAND_VAR: &str = "SELFIE_CONTENT_CMD";

/// Descriptor the command's output is captured on.
///
/// A startup file that redirects this descriptor is handed the content, so the
/// number is chosen to be one nothing else is likely to want: not 3 or 4, the
/// conventional first free ones, and not 9, the `flock` lock-file convention. It
/// cannot be 10 or above — `dash`, `zsh` and `ksh` all reject those, and `dash`
/// is `/bin/sh` on Linux.
///
/// **Fixed, not chosen per run.** A single descriptor makes a collision total and
/// therefore diagnosable — and documentable, which a varying one is not. Varying
/// it would turn the same collision into a fraction of runs failing, which gets
/// retried rather than investigated while the rest of the credential copies keep
/// landing wherever the profile pointed it.
pub(super) const CAPTURE_FD: u8 = 8;

/// What the wrapping `/bin/sh` does before the user's shell exists.
///
/// The redirection has to happen here rather than in the recipe: by the time the
/// recipe runs, the profile has been sourced and anything it backgrounded already
/// holds the real stdout.
pub(super) fn wrapper(fd: u8, login: bool) -> String {
    let login_flag = if login { " -l" } else { "" };
    format!(
        r#"exec {fd}>&1 1>/dev/null; exec "${shell}"{login_flag} -c "${command}""#,
        shell = SHELL_VAR,
        command = COMMAND_VAR,
    )
}

/// The pair of markers delimiting one command's output.
///
/// Random per invocation: both halves reach the inner shell's arguments, so a
/// command that dumps its own command line reproduces them, and extraction takes
/// the first start and the last end so such a dump stays inside the content.
pub(super) struct Markers {
    start: String,
    end: String,
}

impl Markers {
    /// `simple` is 32 hex digits with no hyphens, so a marker cannot begin with
    /// `-` and holds nothing a shell rewrites.
    pub(super) fn new() -> Self {
        Self {
            start: format!("sfo{}", Uuid::new_v4().simple()),
            end: format!("sfc{}", Uuid::new_v4().simple()),
        }
    }

    pub(super) fn start(&self) -> &str {
        &self.start
    }

    pub(super) fn end(&self) -> &str {
        &self.end
    }
}

/// Whether `shell` is fish, which needs the other recipe.
///
/// Getting this wrong in either direction is safe: a POSIX shell treated as fish
/// still separates, and fish given the POSIX recipe writes `exec`'s manual page
/// ahead of the start marker, where extraction discards it.
pub(super) fn is_fish(shell: &str) -> bool {
    std::path::Path::new(shell)
        .file_name()
        .is_some_and(|name| name == "fish")
}

/// The recipe a POSIX shell is given.
///
/// The `trap` displaces any `EXIT` trap the profile installed, which is why a
/// profile that prints on exit prints nothing here.
///
/// **The user's command is last, and nothing may follow it**: a command ending in
/// a line continuation or a comment would eat whatever came next.
pub(super) fn posix_recipe(command: &str, markers: &Markers, fd: u8) -> String {
    format!(
        "exec >&{fd} {fd}>&-\ntrap 'printf %s {end}' EXIT\nprintf %s {start}\n{command}",
        end = markers.end(),
        start = markers.start(),
    )
}

/// The recipe `shell` is given: fish needs the other one.
pub(super) fn recipe(shell: &str, command: &str, markers: &Markers, fd: u8) -> String {
    if is_fish(shell) {
        fish_recipe(command, markers, fd)
    } else {
        posix_recipe(command, markers, fd)
    }
}

/// The recipe fish is given.
///
/// fish has no `exec` taking only redirections — a bare `exec >&N` prints the
/// `exec(1)` manual page, onto the capture descriptor — so the redirection goes
/// on a block. The end marker follows that block rather than coming from a trap,
/// and the command's status is carried across it by hand.
///
/// The block means the user's command is not last here: one ending in a backslash
/// swallows the `end`, and fish then refuses the whole recipe rather than
/// deploying anything.
pub(super) fn fish_recipe(command: &str, markers: &Markers, fd: u8) -> String {
    format!(
        "begin\nprintf %s {start}\n{command}\nend >&{fd}\n\
         set -l __selfie_status $status\nprintf %s {end} >&{fd}\nexit $__selfie_status",
        start = markers.start(),
        end = markers.end(),
    )
}

/// A command's own output, taken out of what was captured.
pub(super) struct Extracted {
    pub(super) content: Vec<u8>,
    /// Bytes that reached the capture descriptor before the command's output.
    pub(super) discarded_before: usize,
    /// Whether the end of the command's output was established.
    pub(super) tail_verified: bool,
}

/// Take the command's output out of `captured`, or `None` if it is not in there.
///
/// `None` is a refusal rather than a fallback: without the start marker there is
/// no telling how much of `captured` the command wrote.
///
/// Works in place — `captured[a..b].to_vec()` would leave two copies of a
/// credential alive at once.
pub(super) fn extract(mut captured: Vec<u8>, markers: &Markers) -> Option<Extracted> {
    let start = markers.start().as_bytes();
    let discarded_before = find(&captured, start)?;
    captured.drain(..discarded_before + start.len());

    let tail_verified = match rfind(&captured, markers.end().as_bytes()) {
        Some(at) => {
            captured.truncate(at);
            true
        }
        None => false,
    };

    Some(Extracted {
        content: captured,
        discarded_before,
        tail_verified,
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .rposition(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn markers() -> Markers {
        Markers {
            start: "sfoSTART".to_string(),
            end: "sfcEND".to_string(),
        }
    }

    #[test]
    fn extract_takes_only_what_lies_between_the_markers() {
        let got = extract(b"banner\nsfoSTARTsecretsfcEND".to_vec(), &markers()).unwrap();

        assert_eq!(got.content, b"secret");
        assert_eq!(got.discarded_before, "banner\n".len());
        assert!(got.tail_verified);
    }

    #[test]
    fn extract_reports_nothing_discarded_when_the_shell_was_silent() {
        let got = extract(b"sfoSTARTsecretsfcEND".to_vec(), &markers()).unwrap();

        assert_eq!(got.content, b"secret");
        assert_eq!(got.discarded_before, 0);
    }

    #[test]
    fn extract_refuses_a_capture_with_no_start_marker() {
        assert!(extract(b"just some output".to_vec(), &markers()).is_none());
    }

    #[test]
    fn extract_keeps_content_and_reports_an_unverified_tail_without_an_end_marker() {
        let got = extract(b"sfoSTARTsecretmycleanup".to_vec(), &markers()).unwrap();

        assert_eq!(got.content, b"secretmycleanup");
        assert!(!got.tail_verified);
    }

    #[test]
    fn extract_splits_at_the_first_start_so_a_command_echoing_one_keeps_it() {
        let got = extract(b"sfoSTARTps says sfoSTART tosfcEND".to_vec(), &markers()).unwrap();

        assert_eq!(got.content, b"ps says sfoSTART to");
    }

    #[test]
    fn extract_splits_at_the_last_end_so_a_command_echoing_one_keeps_it() {
        let got = extract(b"sfoSTARTps says sfcEND tosfcEND".to_vec(), &markers()).unwrap();

        assert_eq!(got.content, b"ps says sfcEND to");
    }

    #[test]
    fn extract_keeps_content_byte_exact() {
        let mut captured = b"sfoSTART".to_vec();
        captured.extend_from_slice(&[0x00, 0xff, 0x0a, 0x80]);
        captured.extend_from_slice(b"sfcEND");

        let got = extract(captured, &markers()).unwrap();

        assert_eq!(got.content, vec![0x00, 0xff, 0x0a, 0x80]);
    }

    #[test]
    fn markers_are_shell_safe_and_unique() {
        let a = Markers::new();
        let b = Markers::new();

        assert_ne!(a.start(), b.start());
        assert_ne!(a.start(), a.end());
        for marker in [a.start(), a.end()] {
            assert!(
                marker.bytes().all(|byte| byte.is_ascii_alphanumeric()),
                "{marker} must survive a shell word untouched"
            );
        }
    }

    #[test]
    fn the_capture_descriptor_is_usable_and_unconventional() {
        // Single digit: dash, zsh and ksh reject 10 and above, and dash is
        // `/bin/sh` on Linux. Not 3 or 4 (first free by convention) and not 9
        // (`flock`'s).
        assert!((5..=8).contains(&CAPTURE_FD), "{CAPTURE_FD} is not usable");
    }

    #[test]
    fn fish_gets_the_block_recipe_and_everything_else_the_posix_one() {
        // The one decision no real-shell test can cover: CI has no fish, so
        // sending it the POSIX recipe — which makes it print the exec(1) manual
        // page into the capture — would go unnoticed here.
        let m = markers();

        assert!(recipe("/opt/homebrew/bin/fish", "cmd", &m, 8).starts_with("begin\n"));
        assert!(recipe("/bin/sh", "cmd", &m, 8).starts_with("exec >&8"));
        assert!(recipe("/bin/zsh", "cmd", &m, 8).starts_with("exec >&8"));
    }

    #[test]
    fn the_posix_recipe_puts_the_users_command_last() {
        let recipe = posix_recipe("op read op://vault/item", &markers(), 7);

        assert!(
            recipe.ends_with("op read op://vault/item"),
            "nothing may follow the command: {recipe}"
        );
    }

    #[test]
    fn the_posix_recipe_marks_both_ends_and_captures_on_the_given_descriptor() {
        let recipe = posix_recipe("cmd", &markers(), 7);

        assert!(recipe.starts_with("exec >&7 7>&-\n"));
        assert!(recipe.contains("trap 'printf %s sfcEND' EXIT"));
        assert!(recipe.contains("printf %s sfoSTART\ncmd"));
    }

    #[test]
    fn the_fish_recipe_marks_both_ends_and_keeps_the_commands_status() {
        let recipe = fish_recipe("cmd", &markers(), 7);

        assert_eq!(
            recipe,
            "begin\nprintf %s sfoSTART\ncmd\nend >&7\n\
             set -l __selfie_status $status\nprintf %s sfcEND >&7\nexit $__selfie_status"
        );
    }

    #[test]
    fn the_wrapper_moves_the_pipe_before_running_the_users_shell() {
        assert_eq!(
            wrapper(7, true),
            r#"exec 7>&1 1>/dev/null; exec "$SELFIE_CONTENT_SHELL" -l -c "$SELFIE_CONTENT_CMD""#
        );
        assert_eq!(
            wrapper(9, false),
            r#"exec 9>&1 1>/dev/null; exec "$SELFIE_CONTENT_SHELL" -c "$SELFIE_CONTENT_CMD""#
        );
    }

    #[test]
    fn fish_is_recognized_by_file_name() {
        assert!(is_fish("/opt/homebrew/bin/fish"));
        assert!(is_fish("fish"));
        assert!(!is_fish("/bin/sh"));
        assert!(!is_fish("/bin/zsh"));
        assert!(!is_fish("/bin/fisher"));
    }
}
