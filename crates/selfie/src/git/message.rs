//! Text from a git operation, redacted and bounded before it enters an error.
//!
//! Git's stderr reaches an AI assistant's transcript: a failed push or pull
//! surfaces through the `selfie_sync_push` / `selfie_sync_pull` MCP tools. A
//! remote URL can carry a credential in its userinfo component, and a
//! non-interactive git echoes that URL when it needs a password it cannot ask
//! for. [`GitMessage`] is the one place that text is cleaned, and its private
//! field is what makes that the only way to build one.

use std::borrow::Cow;

use crate::commands::BoundedText;

/// What replaces a userinfo component.
const REDACTION: &str = "***";

/// Text from a git operation, with URL userinfo redacted and length bounded.
///
/// # What this is for
///
/// A remote URL of the shape `https://<token>@host/repo.git` is what
/// `gh auth setup-git` and most CI documentation produce, and git echoes it
/// verbatim when it wants a password and has no way to prompt:
///
/// ```text
/// fatal: could not read Password for 'http://<token>@127.0.0.1:8731': terminal prompts disabled
/// ```
///
/// That is reached by any non-interactive git — no tty, no working askpass —
/// which is exactly how the MCP server runs it. Note that git *does* strip the
/// userinfo from its `unable to access` and `Authentication failed for`
/// messages when a password is present, so this is not the only shape worth
/// covering but it is the one that demonstrably leaks.
///
/// # What is and is not enforced
///
/// The field is private and both constructors clean their input, so no
/// struct-variant literal — library, adapter, or test — can put raw git output
/// into [`GitSyncError`](super::GitSyncError) or
/// [`GitStatusError`](super::GitStatusError). That is the whole point of the
/// type: `.claude/rules/verification.md` asks for invariants the compiler
/// holds rather than ones a doc comment states.
///
/// What it covers, and what it deliberately does not, is on
/// [`redact_credentials`].
///
/// # Why `Debug` is derived
///
/// Deliberately, and for the same reason as
/// [`BoundedText`](crate::commands::BoundedText): this is text selfie
/// *forwards* rather than content it holds back, and it has already been
/// redacted. `.claude/rules/secrets.md` prescribes scanning an event's `Debug`
/// output for a secret, so a hand-written `Debug` printing `<N bytes>` would
/// hide forwarded git output from that scan — a credential arriving in a shape
/// this does not cover would go unseen instead of caught.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitMessage(String);

impl GitMessage {
    /// Clean a rendered error or a literal.
    ///
    /// For gix errors, `io::Error`s, panic payloads and fixed strings. Cleaning
    /// a literal is a no-op, which is why every construction site can afford to
    /// go through here rather than deciding per site whether its input is
    /// trusted.
    #[must_use]
    pub fn new(message: impl std::fmt::Display) -> Self {
        Self::clean(&message.to_string())
    }

    /// Clean a process's raw stderr bytes.
    ///
    /// Decodes lossily, because git's stderr is not guaranteed UTF-8 and a
    /// failure has to stay reportable either way.
    #[must_use]
    pub fn from_stderr(stderr: &[u8]) -> Self {
        Self::clean(&String::from_utf8_lossy(stderr))
    }

    /// Redact **then** bound. The order is load-bearing, not incidental.
    ///
    /// [`BoundedText::bound`] keeps both ends and elides the middle. A cut
    /// falling inside a URL would strand `https://user:TOK` in the kept head
    /// with the `@` gone — so bounding first can *manufacture* a leak that the
    /// redactor, run afterwards, has no anchor left to find. Redacting first
    /// cannot fail that way: there is no credential left for the elision to
    /// split.
    ///
    /// The cost is decoding the whole of stderr before bounding it. That adds
    /// no unboundedness: `Command::output` has already buffered all of it.
    fn clean(text: &str) -> Self {
        Self(BoundedText::bound(redact_credentials(text).as_bytes()).into_string())
    }

    /// Borrow the cleaned text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for GitMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Replace the userinfo component of every URL-like token in `text`.
///
/// # Why this is not a URL parser
///
/// The input is free-text stderr, not a URL, so the job is editing a span
/// inside prose — and nothing in the dependency tree finds URLs *inside* prose.
/// Both candidate crates would also force a parse-and-reserialize round trip
/// that rewrites the rest of git's message; `gix-url` documents that round trip
/// as lossy (`gix-url/src/lib.rs:227`, where the scp and `ssh://` forms
/// collapse together) and warns against reconstructing an instance at all.
///
/// **`gix::Url`'s redacting `Display` is not a substitute.** Read its source,
/// not its summary: `gix-url/src/impls.rs` clones the URL, sets
/// `password = Some("redacted")`, and **leaves `user` untouched**. Its own docs
/// say so — `gix-url/src/lib.rs:82-85` warns it "does not cover other risks,
/// such as passing a personal access token as a username in an application
/// that logs usernames". A token-as-username is the shape that actually leaks
/// here, so using it would produce a fix that passes a `user:pass` test and
/// leaks every personal access token. Do not "simplify" this into `gix::Url`.
///
/// # What is covered
///
/// The userinfo component, **both halves**, wherever it appears:
///
/// | input                        | output                  |
/// |------------------------------|-------------------------|
/// | `scheme://user:pass@host/p`  | `scheme://***@host/p`   |
/// | `scheme://token@host/p`      | `scheme://***@host/p`   |
/// | `user@host:path` (scp-style) | `***@host:path`         |
/// | `user:pass@host`             | `***@host`              |
/// | `https://u:p@ss@host/r`      | `https://***@host/r`    |
/// | `https://user@host/a@b`      | `https://***@host/a@b`  |
///
/// The last row is why the search is scoped to the authority. `rfind('@')` over
/// the whole token would take the `@` in the *path* and render the host as
/// `https://***b`, destroying the one part of the message worth reading.
///
/// **Every authority in a token is redacted, not just the first** — one token
/// can hold two URLs, as `url.<base>.insteadOf` values do. See
/// [`redact_token`] for the two leaks that scoping to the first one caused.
///
/// # What is deliberately not covered
///
/// Stated rather than implied, because a redaction that looks complete is worse
/// than one whose edges are known:
///
/// - **A credential outside a userinfo** — `Authorization: Bearer <token>`, a
///   credential-helper echo, a `GIT_TRACE`/`GIT_CURL_VERBOSE` header dump.
///   There is no `@` to anchor on and no general shape. Matching known token
///   prefixes instead would be a provider allowlist that fails open for every
///   provider not on it, while creating the impression the class is closed.
/// - **Userinfo containing raw whitespace** (`https://u:my pass@h/`). The token
///   split lands inside it and the head survives. A URL must percent-encode
///   those, so a real remote cannot take this shape.
/// - **Userinfo containing a raw `/`** (`https://user:pa/ssTOKEN@host/r`) — the
///   authority is cut at that `/`, the `@` falls outside it, and the token
///   passes through untouched. Also not reachable from a valid remote, but
///   [`GitMessage::new`] wraps gix errors too, so it is not purely theoretical.
/// - **Over-redaction, accepted.** In a token with no scheme the redaction can
///   only start at the token itself, so everything before the `@` is treated as
///   userinfo — `url.<token>@internal:…` loses its `url.` config-key prefix
///   along with the credential. `git@github.com` becomes `***@github.com`
///   and the address in git's "please tell me who you are" is redacted too.
///   Keeping those would mean deciding some usernames are safe, which is the
///   same fails-open allowlist refused above; it has to be refused in both
///   directions.
fn redact_credentials(text: &str) -> String {
    // The overwhelmingly common case, and it borrows nothing and allocates once.
    if !text.contains('@') {
        return text.to_string();
    }

    let mut out = String::with_capacity(text.len());
    let mut token_start: Option<usize> = None;

    for (i, c) in text.char_indices() {
        if c.is_whitespace() {
            if let Some(start) = token_start.take() {
                out.push_str(&redact_token(&text[start..i]));
            }
            out.push(c);
        } else if token_start.is_none() {
            token_start = Some(i);
        }
    }

    if let Some(start) = token_start {
        out.push_str(&redact_token(&text[start..]));
    }

    out
}

/// Redact the userinfo of **every** authority in one whitespace-delimited token.
///
/// Surrounding punctuation is left alone, so git's single-quoted URLs survive
/// as `'http://***@host:8731':` rather than being mangled into unreadability.
///
/// # Why this loops
///
/// One token can hold more than one URL, and an earlier version stopped after
/// the first authority — appending everything past it verbatim. That leaked
/// two ways, and neither was in the uncovered list:
///
/// - `https://host/redirect?to=https://user:TOKEN@other/repo` — the first
///   authority (`host`) has no `@`, so the whole token was returned untouched
///   and the second URL's credential survived **in full**.
/// - `https://proxy@host/https://user:TOKEN@real/r` — the first authority was
///   redacted and the second was not.
///
/// A single embedded URL is not exotic: `url.<base>.insteadOf` is the standard
/// way to inject a credential-bearing remote, and its value is one token
/// containing two URLs. Scanning to the end of the token is what closes it.
fn redact_token(token: &str) -> Cow<'_, str> {
    let mut out: Option<String> = None;
    // How much of `token` has already been copied into `out`.
    let mut copied_to = 0;
    // The token may *open* with an authority — scp-style `user@host:path`, or a
    // bare `user:pass@host`. This candidate is unconditional: gating it on the
    // token containing no `://` anywhere missed `user@host:https://elsewhere/`,
    // where the search succeeded on the later scheme and the leading credential
    // was never examined at all.
    let mut authority_start = 0;

    loop {
        // An authority ends at the first `/` after its start, which begins the
        // path. For the opening candidate of a `scheme://…` token that lands
        // inside the `://` itself, leaving `scheme:` — which holds no `@`, so
        // the candidate simply finds nothing and the scheme pass below does the
        // work.
        let authority_end = token[authority_start..]
            .find('/')
            .map_or(token.len(), |i| authority_start + i);

        // The last `@` at an index *below* `authority_end` — never `rfind` over
        // the whole token, which would take an `@` from the path and drag the
        // host into the redaction. Last rather than first so a password
        // containing an `@` goes whole instead of leaving its tail behind.
        if let Some(offset) = token[authority_start..authority_end].rfind('@') {
            let at = authority_start + offset;

            // An empty userinfo (`https://@host`) has nothing to hide, and
            // rewriting it to `***@host` would invent a credential that was
            // never there.
            if at > authority_start {
                let buffer = out.get_or_insert_with(|| String::with_capacity(token.len()));
                buffer.push_str(&token[copied_to..authority_start]);
                buffer.push_str(REDACTION);
                copied_to = at;
            }
        }

        // Advance from this authority's *start*, not from `authority_end`: for
        // `https://…` the first `/` sits inside the `://` itself, so resuming
        // after it would skip the very scheme being looked for.
        //
        // Each step moves forward by at least three bytes, so this terminates.
        // The next start also always exceeds `copied_to`: a `://` carries a `/`,
        // so `authority_end` of this pass is at most one past that `:`, and the
        // next start is two beyond it.
        authority_start = match token[authority_start..].find("://") {
            Some(i) => authority_start + i + 3,
            None => break,
        };
    }

    match out {
        Some(mut buffer) => {
            buffer.push_str(&token[copied_to..]);
            Cow::Owned(buffer)
        }
        None => Cow::Borrowed(token),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::runner::MAX_BOUNDED_BYTES;
    use test_common::assert_secret_free;

    /// High-entropy, 24 characters, and shaped like nothing else in a fixture —
    /// `.claude/rules/secrets.md` refuses a secret that reads like a path, a
    /// package name, or an environment name.
    const TOKEN: &str = "Zk9qP2mW7xR4tL6vB1nH3jD5";

    // ─── Redaction: the shapes that must be covered ─────────────────────────

    #[test]
    fn redacts_a_user_and_password_userinfo() {
        let out = redact_credentials(&format!("https://someone:{TOKEN}@host/repo.git"));

        assert_secret_free(&out, TOKEN, "a redacted git message");
        assert_eq!(out, "https://***@host/repo.git");
    }

    /// The shape that actually leaks. This is git 2.50.1's own output, verbatim,
    /// from a non-interactive fetch against a remote whose URL carries a token
    /// as its username — which is what `gh auth setup-git` writes.
    #[test]
    fn redacts_a_token_used_as_the_username() {
        let out = redact_credentials(&format!(
            "fatal: could not read Password for 'http://{TOKEN}@127.0.0.1:8731': \
             terminal prompts disabled"
        ));

        assert_secret_free(&out, TOKEN, "a redacted git message");
        assert_eq!(
            out,
            "fatal: could not read Password for 'http://***@127.0.0.1:8731': \
             terminal prompts disabled"
        );
    }

    /// The same message over IPv6. Pinned because the bracketed host is what a
    /// "simplification" that splits the authority on `:` to find the host would
    /// break, and it would break silently.
    #[test]
    fn redacts_a_token_from_a_bracketed_ipv6_authority() {
        let out = redact_credentials(&format!(
            "fatal: could not read Password for 'http://{TOKEN}@[::1]:55036': \
             terminal prompts disabled"
        ));

        assert_secret_free(&out, TOKEN, "a redacted git message");
        assert_eq!(
            out,
            "fatal: could not read Password for 'http://***@[::1]:55036': \
             terminal prompts disabled"
        );
    }

    #[test]
    fn redacts_scp_style_userinfo() {
        assert_eq!(
            redact_credentials(&format!("{TOKEN}@host:org/repo.git")),
            "***@host:org/repo.git"
        );
    }

    /// Last `@` in the authority, not the first: a first-match rule leaves the
    /// tail of the password (`ss`) behind.
    #[test]
    fn redacts_a_password_containing_an_at_sign() {
        let out = redact_credentials(&format!("https://user:p@{TOKEN}@host/repo.git"));

        assert_secret_free(&out, TOKEN, "a redacted git message");
        assert_eq!(out, "https://***@host/repo.git");
    }

    /// …and scoped to the authority, not the whole token. `rfind` over the token
    /// would yield `https://***b` and take the host with it.
    #[test]
    fn leaves_an_at_sign_in_the_path_alone_and_keeps_the_host() {
        assert_eq!(
            redact_credentials("https://someone@host/a@b"),
            "https://***@host/a@b"
        );
    }

    /// Two URLs in one whitespace-delimited token, the first *without* a
    /// credential. A scan that stopped at the first authority returned this
    /// token untouched and leaked the second credential in full — it did, and
    /// a reviewer caught it rather than a test.
    #[test]
    fn redacts_a_credential_url_embedded_after_a_path() {
        let out = redact_credentials(&format!(
            "https://host/redirect?to=https://user:{TOKEN}@other/repo"
        ));

        assert_secret_free(&out, TOKEN, "a redacted git message");
        assert_eq!(out, "https://host/redirect?to=https://***@other/repo");
    }

    /// The same gap with a credential in *both* authorities: stopping after the
    /// first redacted one and forwarded the other.
    #[test]
    fn redacts_every_authority_in_a_single_token() {
        let out = redact_credentials(&format!(
            "https://proxy@host/https://user:{TOKEN}@real/r.git"
        ));

        assert_secret_free(&out, TOKEN, "a redacted git message");
        assert_eq!(out, "https://***@host/https://***@real/r.git");
    }

    /// `url.<base>.insteadOf` is the standard way to inject a credential-bearing
    /// remote, and its value is one token holding two URLs. A gix config error
    /// echoing it reaches [`GitMessage::new`].
    #[test]
    fn redacts_an_insteadof_style_configuration_value() {
        let out = redact_credentials(&format!(
            "url.https://x:{TOKEN}@host/.insteadOf=https://host/"
        ));

        assert_secret_free(&out, TOKEN, "a redacted git message");
        assert_eq!(out, "url.https://***@host/.insteadOf=https://host/");
    }

    /// A credential in a *bare* authority that is followed, in the same token,
    /// by an unrelated `scheme://`. Looking for the opening authority only when
    /// the token held no `://` at all meant the search succeeded on the later
    /// scheme and this credential was never examined — the token came back
    /// byte-for-byte unchanged. Found by review, not by a test.
    #[test]
    fn redacts_a_bare_authority_followed_by_a_later_scheme() {
        let out = redact_credentials(&format!("{TOKEN}@evil.example:https://good.example/"));

        assert_secret_free(&out, TOKEN, "a redacted git message");
        assert_eq!(out, "***@evil.example:https://good.example/");
    }

    /// The same gap in its realistic dress: an scp-style `insteadOf` base, whose
    /// value is a plain URL.
    ///
    /// Note the `url.` config-key prefix goes with the userinfo. A bare
    /// authority has no scheme to anchor on, so the redaction can only start at
    /// the token, and everything before the `@` is treated as userinfo. That is
    /// the documented over-redaction tradeoff, and the safe direction: telling a
    /// config-key prefix apart from a username means deciding which text before
    /// an `@` is harmless, which is the fails-open guess this module refuses to
    /// make.
    #[test]
    fn redacts_an_scp_style_insteadof_base() {
        let out = redact_credentials(&format!("url.{TOKEN}@internal:.insteadOf=https://host/"));

        assert_secret_free(&out, TOKEN, "a redacted git message");
        assert_eq!(out, "***@internal:.insteadOf=https://host/");
    }

    #[test]
    fn redacts_every_url_across_multiple_lines() {
        let out = redact_credentials(&format!(
            "warning: URL 'https://one:{TOKEN}@host/r' uses plaintext credentials\n\
             fatal: could not read Password for 'https://{TOKEN}@other/r'\n"
        ));

        assert_secret_free(&out, TOKEN, "a redacted git message");
        assert_eq!(
            out,
            "warning: URL 'https://***@host/r' uses plaintext credentials\n\
             fatal: could not read Password for 'https://***@other/r'\n"
        );
    }

    // ─── Redaction: the control against over-redaction ──────────────────────

    /// git 2.50.1 already strips the userinfo from *this* message. It has to
    /// come back byte-identical, or the tests above would still pass with a
    /// redactor that simply blanked everything — and the error would stop being
    /// diagnosable, which is the failure mode nobody notices until an incident.
    #[test]
    fn leaves_credential_free_git_output_byte_identical() {
        for line in [
            "fatal: Authentication failed for 'http://127.0.0.1:8731/o/r.git/'",
            "fatal: unable to access 'https://127.0.0.1:1/o/r.git/': Failed to connect",
            "error: failed to push some refs to 'origin'",
            "! [rejected]        main -> main (non-fast-forward)",
            "hint: Updates were rejected because the tip of your current branch is behind",
        ] {
            assert_eq!(redact_credentials(line), line, "rewrote a clean message");
        }
    }

    /// An empty userinfo is not a credential, and rewriting it to `***@host`
    /// would invent one.
    #[test]
    fn leaves_an_empty_userinfo_alone() {
        assert_eq!(redact_credentials("https://@host/r"), "https://@host/r");
    }

    // ─── Redaction: the stated holes, pinned so they stay stated ────────────

    /// Documented in [`redact_credentials`] as uncovered. Asserting it keeps the
    /// doc comment honest: if someone widens the rule, this fails and they have
    /// to update the prose rather than leave it overclaiming.
    #[test]
    fn a_slash_inside_the_userinfo_is_not_covered() {
        let leaky = format!("https://user:pa/ss{TOKEN}@host/repo.git");

        assert_eq!(
            redact_credentials(&leaky),
            leaky,
            "if this now redacts, update the `not covered` list in the doc comment"
        );
    }

    /// Likewise. Both halves of the split are shown, so the partial nature of
    /// the miss is on the record rather than merely asserted.
    #[test]
    fn whitespace_inside_the_userinfo_is_not_covered() {
        assert_eq!(
            redact_credentials("https://user:my pass@host/r"),
            "https://user:my ***@host/r"
        );
    }

    // ─── GitMessage: bound, and the order it runs in ────────────────────────

    #[test]
    fn a_long_git_stderr_is_bounded() {
        let stderr = "e".repeat(MAX_BOUNDED_BYTES * 2);

        let message = GitMessage::from_stderr(stderr.as_bytes());

        assert!(
            message.as_str().contains("bytes elided"),
            "expected an elision marker, got {} bytes",
            message.as_str().len()
        );
    }

    /// The ordering invariant, and the reason [`GitMessage::clean`] redacts
    /// first. The credential straddles the elision cut, so a bound-then-redact
    /// implementation keeps the start of the token in the head — with the `@`
    /// in the elided middle, leaving the redactor nothing to anchor on.
    /// Redacting first cannot fail that way.
    ///
    /// **The fixture has to be placed, not guessed.** `assert_secret_free`
    /// matches a twelve-character window, so the cut has to fall exactly twelve
    /// characters into the token: any later and the whole credential lands in
    /// the elided middle, the scan finds nothing, and the test passes under a
    /// bound-then-redact implementation while appearing to assert the opposite.
    /// It did, until the mutation for this test was run.
    #[test]
    fn bounding_cannot_split_a_credential_because_redaction_runs_first() {
        /// The window `assert_secret_free` scans for, spelled out because the
        /// placement below is arithmetic on it rather than a round number.
        const SCAN_WINDOW: usize = 12;
        const URL_PREFIX: &str = "https://user:";

        let head = "b".repeat(MAX_BOUNDED_BYTES / 2 - URL_PREFIX.len() - SCAN_WINDOW);
        let tail = "c".repeat(MAX_BOUNDED_BYTES * 2);
        let stderr = format!("{head}{URL_PREFIX}{TOKEN}@host/repo.git {tail}");

        let message = GitMessage::from_stderr(stderr.as_bytes());

        assert_secret_free(message.as_str(), TOKEN, "a bounded git message");
        assert_secret_free(
            &format!("{message:?}"),
            TOKEN,
            "the Debug of a bounded git message",
        );
        // The control: the elision really did happen, so the scan above was not
        // passing on a message that was never near the bound.
        assert!(
            message.as_str().contains("bytes elided"),
            "the fixture must exceed the bound, or this tests nothing"
        );
    }

    // ─── GitMessage: the constructors clean their input ─────────────────────

    #[test]
    fn new_redacts_a_rendered_error() {
        let message = GitMessage::new(format!("open: https://user:{TOKEN}@host/r is unreadable"));

        assert_secret_free(message.as_str(), TOKEN, "a GitMessage built from a Display");
        assert_eq!(message.as_str(), "open: https://***@host/r is unreadable");
    }

    #[test]
    fn from_stderr_redacts_invalid_utf8_output() {
        let mut stderr = format!("fatal: 'https://{TOKEN}@host/r' ").into_bytes();
        stderr.extend_from_slice(&[0xff, 0xfe]);

        let message = GitMessage::from_stderr(&stderr);

        assert_secret_free(message.as_str(), TOKEN, "a GitMessage built from raw bytes");
        assert!(message.as_str().starts_with("fatal: 'https://***@host/r'"));
    }

    #[test]
    fn display_and_as_str_agree() {
        let message = GitMessage::new("discover repository");

        assert_eq!(message.to_string(), message.as_str());
    }
}
