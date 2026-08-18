//! Text from a git operation, redacted and bounded before it enters an error.
//!
//! Git's stderr reaches an AI assistant's transcript through the
//! `selfie_sync_push` / `selfie_sync_pull` MCP tools. A remote URL can carry a
//! credential in its userinfo component, and a non-interactive git echoes that
//! URL when it wants a password it cannot prompt for:
//!
//! ```text
//! fatal: could not read Password for 'http://<token>@127.0.0.1:8731': terminal prompts disabled
//! ```
//!
//! That shape is produced by `gh auth setup-git` and most CI documentation, and
//! reached by any git with no tty and no askpass — which is how the MCP server
//! runs it. [`GitMessage`] is the one place that text is cleaned.

use std::borrow::Cow;

use crate::commands::BoundedText;

/// What replaces a userinfo component.
const REDACTION: &str = "***";

/// Text from a git operation, with URL userinfo redacted and length bounded.
///
/// The field is private and both constructors clean their input, so nothing can
/// put raw git output into [`GitSyncError`](super::GitSyncError) or
/// [`GitStatusError`](super::GitStatusError).
///
/// What the redaction covers, and what it does not, is on `redact_credentials`.
// `Debug` is derived on purpose, for the same reason as `BoundedText`: this is
// text selfie forwards rather than content it holds back, and it has already
// been redacted. Leak tests scan an event's `Debug` output for a secret, so a
// hand-written `Debug` printing `<N bytes>` would hide forwarded git output
// from that scan — a credential arriving in a shape the redactor misses would
// go unseen instead of caught.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitMessage(String);

impl GitMessage {
    /// Clean a rendered error or a literal — a gix error, an `io::Error`, a
    /// panic payload, a fixed string.
    ///
    /// Cleaning a literal is a no-op, so every construction site can go through
    /// here rather than deciding per site whether its input is trusted.
    #[must_use]
    pub fn new(message: impl std::fmt::Display) -> Self {
        Self::clean(&message.to_string())
    }

    /// Clean a process's raw stderr bytes.
    ///
    /// Decodes lossily; git's stderr is not guaranteed UTF-8, and a failure has
    /// to stay reportable either way.
    #[must_use]
    pub fn from_stderr(stderr: &[u8]) -> Self {
        Self::clean(&String::from_utf8_lossy(stderr))
    }

    fn clean(text: &str) -> Self {
        // Redact first, then bound. `BoundedText::bound` keeps both ends and
        // elides the middle, so a cut falling inside a URL strands
        // `https://user:TOK` in the kept head with the `@` gone — bounding
        // first can manufacture a leak that the redactor then has no anchor
        // left to find. Redacting first cannot fail that way.
        //
        // The cost is decoding the whole of stderr before bounding it, which
        // adds no unboundedness: `Command::output` has already buffered it all.
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

/// Replace the userinfo component of every URL-like token in `text`, both
/// halves of it, wherever it appears.
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
/// A token can hold more than one authority, and each is redacted.
/// [`authority_starts`] gives the offsets where one may begin.
///
/// # Not covered
///
/// A redaction that looks complete is worse than one whose edges are known, so
/// these are stated rather than implied. Every gap below that this function can
/// be shown to miss is pinned by its own test; the first has no shape to write
/// one against.
///
/// - A credential outside a userinfo — `Authorization: Bearer <token>`, a
///   credential-helper echo, a `GIT_TRACE` or `GIT_CURL_VERBOSE` header dump.
///   There is no `@` to anchor on.
/// - Userinfo containing raw whitespace (`https://u:my pass@h/`). With the
///   whitespace immediately before the `@` the miss is total: the token split
///   leaves a piece holding the credential and no `@` at all, so it passes
///   through byte for byte. A newline mid-stderr is likelier than a space.
/// - Userinfo containing a raw `/` (`https://user:pa/ssTOKEN@host/r`). The
///   authority is cut at that `/`, so the `@` falls outside it.
/// - An authority introduced by a path separator —
///   `https://host/a/b/<token>@evil:p`, and the protocol-relative
///   `//<token>@host/r.git`.
///
/// A valid remote must percent-encode whitespace and `/`, so the middle two are
/// not reachable from one. [`GitMessage::new`] wraps gix errors too, which is
/// why they are not purely theoretical.
///
/// Over-redaction is accepted in the other direction. With no scheme the
/// redaction can only start at the token itself, so everything before the `@`
/// is userinfo: `url.<token>@internal` loses its `url.` config-key prefix, and
/// `git@github.com` becomes `***@github.com`.
// Not a URL parser, and not `gix::Url`:
//
// - The input is free-text stderr, so the job is editing a span inside prose.
// - A parse-and-reserialize round trip rewrites the rest of git's message, and
//   `gix-url` documents that round trip as lossy.
// - `gix::Url`'s redacting `Display` blanks `password` and leaves `user`
//   untouched. A token-as-username is the shape that leaks here, so it gives a
//   fix that passes a `user:pass` test and leaks every access token.
//
// The remaining gaps cannot be closed without opening a worse one: see below.
fn redact_credentials(text: &str) -> String {
    // Without an `@` anywhere there is no userinfo to find, so the whole
    // per-token scan below can be skipped.
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

/// Redact the userinfo at each candidate authority in one whitespace-delimited
/// token, from the earliest candidate that reaches each `@`.
///
/// Surrounding punctuation is left alone, so git's single-quoted URLs survive
/// as `'http://***@host:8731':` rather than being mangled into unreadability.
/// Not every position in the token is examined; [`authority_starts`] has the
/// ones that are, and [`redact_credentials`] what that leaves uncovered.
fn redact_token(token: &str) -> Cow<'_, str> {
    // No `@`, no userinfo — and no candidate list to build.
    if !token.contains('@') {
        return Cow::Borrowed(token);
    }

    let mut out: Option<String> = None;
    // How much of `token` has already been copied into `out`.
    let mut copied_to = 0;

    // Iterate over every candidate, never just the first. Stopping after the
    // first authority leaks two ways:
    //
    //   https://host/redirect?to=https://user:TOKEN@other/repo
    //     first authority has no `@`, so the second URL's credential survives
    //   https://proxy@host/https://user:TOKEN@real/r
    //     first authority redacted, second forwarded
    //
    // Considering only `://`-introduced authorities leaks a third way, for one
    // embedded after `=` or `,`.
    for authority_start in authority_starts(token) {
        // A delimiter can sit inside a userinfo -- `=` is base64 padding -- and a
        // later candidate then points into a span already redacted. Skipping it
        // keeps the earliest start, which is the widest span.
        //
        // Preferring the latest candidate that reaches an `@` destroys less of the
        // message but under-redacts: a delimiter inside the userinfo moves the
        // start forward and emits the text before it verbatim. RFC 3986 makes `=`,
        // `,` and `&` sub-delims, so that span is legal userinfo.
        //
        // The price is context, which is a diagnostic cost, not a credential one.
        if authority_start < copied_to {
            continue;
        }

        // An authority ends at the first `/` after its start, which begins the
        // path. For the opening candidate of a `scheme://…` token that lands
        // inside the `://` itself, leaving `scheme:` — which holds no `@`, so
        // that candidate finds nothing and the post-`://` one does the work.
        let authority_end = token[authority_start..]
            .find('/')
            .map_or(token.len(), |i| authority_start + i);

        // The last `@` at an index *below* `authority_end` — never `rfind` over
        // the whole token, which would take an `@` from the path and drag the
        // host into the redaction. Last rather than first so a password
        // containing an `@` goes whole instead of leaving its tail behind.
        let Some(offset) = token[authority_start..authority_end].rfind('@') else {
            continue;
        };
        let at = authority_start + offset;

        // An empty userinfo (`https://@host`) has nothing to hide, and rewriting
        // it to `***@host` would invent a credential that was never there.
        if at <= authority_start {
            continue;
        }

        let buffer = out.get_or_insert_with(|| String::with_capacity(token.len()));
        buffer.push_str(&token[copied_to..authority_start]);
        buffer.push_str(REDACTION);
        copied_to = at;
    }

    match out {
        Some(mut buffer) => {
            buffer.push_str(&token[copied_to..]);
            Cow::Owned(buffer)
        }
        None => Cow::Borrowed(token),
    }
}

/// Every offset in `token` where an authority may begin, ascending and unique.
// Built as one list rather than walked inline: merging these three sequences by
// hand is how the previous two under-redactions happened.
//
// `/` is deliberately not among the delimiters. Adding it would make every path
// segment a fresh authority and rewrite `https://host/a@b` into `https://***b`,
// destroying the host, which
// `leaves_an_at_sign_in_the_path_alone_and_keeps_the_host` exists to prevent.
// The price is that an authority introduced by a path separator is never seen.
fn authority_starts(token: &str) -> Vec<usize> {
    // The token itself, for scp-style `user@host:path` and bare
    // `user:pass@host`. Unconditional: gating it on the token holding no `://`
    // anywhere missed `user@host:https://elsewhere/`, where the scheme search
    // succeeded further along and the leading credential was never examined.
    let mut starts = vec![0];

    // After every `=` and `,`, where a credential-bearing URL gets embedded in
    // a larger token — `url.<base>.insteadOf=<url>` is the standard way to
    // inject one, and comma-joined values appear in config dumps. Both bytes
    // are ASCII, so `i + 1` is always a character boundary.
    //
    // Both are also legal unencoded inside a userinfo, being RFC 3986
    // sub-delims, so a candidate generated here frequently points into a
    // credential rather than before one. That is harmless only because
    // `redact_token` takes the earliest start and skips the rest. Do not add a
    // delimiter here without re-reading that.
    starts.extend(
        token
            .bytes()
            .enumerate()
            .filter(|(_, byte)| matches!(byte, b'=' | b','))
            .map(|(i, _)| i + 1),
    );

    // After every `://`, the ordinary URL case. Searching onward from each
    // match rather than from the previous authority's end matters: for
    // `https://…` the first `/` sits inside the `://` itself, so resuming past
    // it would skip the very scheme being looked for.
    //
    // Each step advances at least three bytes, so this terminates.
    let mut from = 0;
    while let Some(i) = token[from..].find("://") {
        let start = from + i + 3;
        starts.push(start);
        from = start;
        if from >= token.len() {
            break;
        }
    }

    starts.sort_unstable();
    starts.dedup();
    starts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::runner::MAX_BOUNDED_BYTES;
    use test_common::assert_secret_free;

    // High-entropy, 24 characters, and shaped like nothing else in a fixture, so
    // it cannot read as a path, a package name or an environment name and match
    // one of those instead of a leak.
    const TOKEN: &str = "Zk9qP2mW7xR4tL6vB1nH3jD5";

    // A credential that **contains the candidate delimiters**, which [`TOKEN`]
    // does not. Base64 pads with `=`, so a very large class of real tokens
    // carries one by construction — and a rule preferring the tightest span
    // emits everything before that `=` verbatim. No test using `TOKEN` alone
    // can see that class; thirteen mutations sailed through the gap.
    const TOKEN_WITH_DELIMITERS: &str = "c2VjcmV0S2V5,MQ==dEs9Zx";

    // ─── Redaction: the shapes that must be covered ─────────────────────────

    #[test]
    fn redacts_a_user_and_password_userinfo() {
        let out = redact_credentials(&format!("https://someone:{TOKEN}@host/repo.git"));

        assert_secret_free(&out, TOKEN, "a redacted git message");
        assert_eq!(out, "https://***@host/repo.git");
    }

    // The shape that actually leaks. This is git 2.50.1's own output, verbatim,
    // from a non-interactive fetch against a remote whose URL carries a token
    // as its username — which is what `gh auth setup-git` writes.
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

    // The same message over IPv6. Pinned because the bracketed host is what a
    // "simplification" that splits the authority on `:` to find the host would
    // break, and it would break silently.
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

    // Last `@` in the authority, not the first: a first-match rule leaves the
    // tail of the password (`ss`) behind.
    #[test]
    fn redacts_a_password_containing_an_at_sign() {
        let out = redact_credentials(&format!("https://user:p@{TOKEN}@host/repo.git"));

        assert_secret_free(&out, TOKEN, "a redacted git message");
        assert_eq!(out, "https://***@host/repo.git");
    }

    // …and scoped to the authority, not the whole token. `rfind` over the token
    // would yield `https://***b` and take the host with it.
    #[test]
    fn leaves_an_at_sign_in_the_path_alone_and_keeps_the_host() {
        assert_eq!(
            redact_credentials("https://someone@host/a@b"),
            "https://***@host/a@b"
        );
    }

    // Two URLs in one whitespace-delimited token, the first *without* a
    // credential. A scan that stopped at the first authority returned this
    // token untouched and leaked the second credential in full — it did, and
    // a reviewer caught it rather than a test.
    #[test]
    fn redacts_a_credential_url_embedded_after_a_path() {
        let out = redact_credentials(&format!(
            "https://host/redirect?to=https://user:{TOKEN}@other/repo"
        ));

        assert_secret_free(&out, TOKEN, "a redacted git message");
        assert_eq!(out, "https://host/redirect?to=https://***@other/repo");
    }

    // The same gap with a credential in *both* authorities: stopping after the
    // first redacted one and forwarded the other.
    #[test]
    fn redacts_every_authority_in_a_single_token() {
        let out = redact_credentials(&format!(
            "https://proxy@host/https://user:{TOKEN}@real/r.git"
        ));

        assert_secret_free(&out, TOKEN, "a redacted git message");
        assert_eq!(out, "https://***@host/https://***@real/r.git");
    }

    // `url.<base>.insteadOf` is the standard way to inject a credential-bearing
    // remote, and its value is one token holding two URLs. A gix config error
    // echoing it reaches [`GitMessage::new`].
    #[test]
    fn redacts_an_insteadof_style_configuration_value() {
        let out = redact_credentials(&format!(
            "url.https://x:{TOKEN}@host/.insteadOf=https://host/"
        ));

        assert_secret_free(&out, TOKEN, "a redacted git message");
        assert_eq!(out, "url.https://***@host/.insteadOf=https://host/");
    }

    // A credential in a *bare* authority that is followed, in the same token,
    // by an unrelated `scheme://`. Looking for the opening authority only when
    // the token held no `://` at all meant the search succeeded on the later
    // scheme and this credential was never examined — the token came back
    // byte-for-byte unchanged. Found by review, not by a test.
    #[test]
    fn redacts_a_bare_authority_followed_by_a_later_scheme() {
        let out = redact_credentials(&format!("{TOKEN}@evil.example:https://good.example/"));

        assert_secret_free(&out, TOKEN, "a redacted git message");
        assert_eq!(out, "***@evil.example:https://good.example/");
    }

    // The same gap in its realistic dress: an scp-style `insteadOf` base, whose
    // value is a plain URL.
    //
    // Note the `url.` config-key prefix goes with the userinfo. A bare
    // authority has no scheme to anchor on, so the redaction can only start at
    // the token, and everything before the `@` is treated as userinfo. That is
    // the documented over-redaction tradeoff, and the safe direction: telling a
    // config-key prefix apart from a username means deciding which text before
    // an `@` is harmless, which is the fails-open guess this module refuses to
    // make.
    #[test]
    fn redacts_an_scp_style_insteadof_base() {
        let out = redact_credentials(&format!("url.{TOKEN}@internal:.insteadOf=https://host/"));

        assert_secret_free(&out, TOKEN, "a redacted git message");
        assert_eq!(out, "***@internal:.insteadOf=https://host/");
    }

    // The `insteadOf` VALUE, after the `=`. Considering only `://`-introduced
    // authorities left this one unexamined — and `insteadOf` is the very shape
    // this module cites as its motivating case, so the doc named a leak.
    #[test]
    fn redacts_a_credential_in_an_insteadof_value() {
        let out = redact_credentials(&format!(
            "url.https://github.com/.insteadOf={TOKEN}@github.com:"
        ));

        assert_secret_free(&out, TOKEN, "a redacted git message");
        assert_eq!(out, "url.https://github.com/.insteadOf=***@github.com:");
    }

    // A second authority joined by a comma, with the first already redacted —
    // leak 2's exact shape a third time, in the delimiter that config dumps use.
    #[test]
    fn redacts_a_comma_joined_second_authority() {
        let out = redact_credentials(&format!("git+ssh://ok@host/x,{TOKEN}@evil:p"));

        assert_secret_free(&out, TOKEN, "a redacted git message");
        assert_eq!(out, "git+ssh://***@host/x,***@evil:p");
    }

    // A query parameter, covered by the same `=` delimiter.
    #[test]
    fn redacts_a_credential_in_a_query_parameter() {
        let out = redact_credentials(&format!("https://h/?u=x&next={TOKEN}@evil:1"));

        assert_secret_free(&out, TOKEN, "a redacted git message");
        // `x&next=` goes with the credential. Every byte of it is legal
        // userinfo under RFC 3986, so nothing distinguishes query context from
        // credential material; keeping it is a guess that leaks when wrong.
        assert_eq!(out, "https://h/?u=***@evil:1");
    }

    // The regression a tightest-span rule caused: `=` inside the userinfo moved
    // the redaction start past most of the credential and emitted the rest
    // verbatim. Fifteen of twenty characters survived.
    #[test]
    fn a_delimiter_inside_the_userinfo_does_not_shorten_the_redaction() {
        let out = redact_credentials(&format!("https://user:{TOKEN_WITH_DELIMITERS}@host/r"));

        assert_secret_free(&out, TOKEN_WITH_DELIMITERS, "a redacted git message");
        assert_eq!(out, "https://***@host/r");
    }

    // The same, with the delimiter in a username rather than a password, and
    // reached through the scp-style candidate rather than the `://` one.
    #[test]
    fn a_delimiter_inside_an_scp_style_userinfo_does_not_shorten_the_redaction() {
        let out = redact_credentials(&format!("{TOKEN_WITH_DELIMITERS}@host:org/repo.git"));

        assert_secret_free(&out, TOKEN_WITH_DELIMITERS, "a redacted git message");
        assert_eq!(out, "***@host:org/repo.git");
    }

    // ─── The residual hole, pinned so it stays stated ───────────────────────

    // `/` is deliberately not a candidate delimiter, so an authority introduced
    // by a path separator is never examined. This is the direct price of
    // `leaves_an_at_sign_in_the_path_alone_and_keeps_the_host`: any rule that
    // catches this one also destroys that one's host. Asserting the miss keeps
    // the uncovered list honest — widen the rule and this fails, forcing the
    // prose to be updated rather than left overclaiming.
    #[test]
    fn an_authority_after_a_path_separator_is_not_covered() {
        for leaky in [
            format!("https://host/a/b/{TOKEN}@evil:p"),
            format!("//{TOKEN}@host/r.git"),
            format!("/{TOKEN}@host:p"),
        ] {
            assert_eq!(
                redact_credentials(&leaky),
                leaky,
                "if this now redacts, update the `not covered` list in the doc comment"
            );
        }
    }

    // The whitespace hole at its worst. The pinned partial miss below sits a
    // few lines away and reads like the whole story; it is not. With the
    // whitespace immediately before the `@`, the piece holding the credential
    // contains no `@` at all and passes through byte for byte.
    #[test]
    fn whitespace_immediately_before_the_at_sign_misses_totally() {
        let leaky = format!("https://user:{TOKEN}\n@host/r");

        assert_eq!(
            redact_credentials(&leaky),
            leaky,
            "if this now redacts, update the `not covered` list in the doc comment"
        );
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

    // git 2.50.1 already strips the userinfo from *this* message. It has to
    // come back byte-identical, or the tests above would still pass with a
    // redactor that simply blanked everything — and the error would stop being
    // diagnosable, which is the failure mode nobody notices until an incident.
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

    // An empty userinfo is not a credential, and rewriting it to `***@host`
    // would invent one.
    #[test]
    fn leaves_an_empty_userinfo_alone() {
        assert_eq!(redact_credentials("https://@host/r"), "https://@host/r");
    }

    // ─── Redaction: the stated holes, pinned so they stay stated ────────────

    // Documented in [`redact_credentials`] as uncovered. Asserting it keeps the
    // doc comment honest: if someone widens the rule, this fails and they have
    // to update the prose rather than leave it overclaiming.
    #[test]
    fn a_slash_inside_the_userinfo_is_not_covered() {
        let leaky = format!("https://user:pa/ss{TOKEN}@host/repo.git");

        assert_eq!(
            redact_credentials(&leaky),
            leaky,
            "if this now redacts, update the `not covered` list in the doc comment"
        );
    }

    // Likewise. Both halves of the split are shown, so the partial nature of
    // the miss is on the record rather than merely asserted.
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

    // The ordering invariant, and why [`GitMessage::clean`] redacts first. The
    // credential straddles the elision cut, so bound-then-redact keeps the start
    // of the token in the head with the `@` in the elided middle, leaving the
    // redactor nothing to anchor on.
    //
    // The fixture has to be placed, not guessed: `assert_secret_free` matches a
    // twelve-character window, so the cut must fall exactly twelve characters
    // into the token. Any later and the whole credential lands in the elided
    // middle, and the test passes under bound-then-redact while appearing not to.
    #[test]
    fn bounding_cannot_split_a_credential_because_redaction_runs_first() {
        // The window `assert_secret_free` scans for, spelled out because the
        // placement below is arithmetic on it rather than a round number.
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
