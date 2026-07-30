//! Scanning rendered output for a secret, in every rendering a leak can take.
//!
//! A leak test scanning for the secret's *text* has a blind spot the size of
//! `Debug`: `format!("{:?}", bytes)` renders `[115, 51, 99, ...]`, carrying the
//! same credential with the literal nowhere.
//!
//! Not hypothetical. `std::process::Output`'s `Debug` prints stdout as text when
//! it is valid UTF-8 and as a byte array when it is not (rustc 1.97.1,
//! `library/std/src/process.rs:1470`); `CommandOutput` derives `Debug` and holds
//! one; selfie supports binary dotfile content. So the same `{:?}` on the same
//! type is caught for one credential and invisible for another — and serde
//! renders `Vec<u8>` identically, giving the MCP server's JSON the same exit.
//!
//! Choosing renderings per call site rebuilds that blind spot with the next
//! test, so it is chosen here once: event `Debug`, tracing buffer, serialized
//! JSON alike.

/// How much of the secret a rendering has to reproduce to count as a leak.
///
/// A window rather than the whole value, because a *truncating* leak is still a
/// leak: a warning printing the first 200 bytes of a 4 KiB credential has to
/// fail. Twelve rather than eight because a short window collides with ordinary
/// fixture data — `test-env` matches `package_name: "test-env-pkg"`. Widening
/// raises that bar without removing it, which is why [`assert_secret_free`]
/// states what a leak-test secret has to look like.
///
/// **It measures two different windows, and a description naming only one is
/// describing half the scan.** The byte needle takes twelve *bytes* from
/// [`anchor`], before normalization, keeping interior whitespace — those render
/// as `32` or `10` and must stay contiguous with their neighbors. The text
/// needle takes twelve *characters* after [`squeeze`]. The two coincide only for
/// whitespace-free ASCII carrying no `\n`, `\r` or `\t` pair — necessary and not
/// sufficient on whitespace alone, because the escape pairs go too:
/// `ab\ncdefghijkl` is fourteen pure-ASCII bytes with no whitespace and its
/// windows still cover different spans.
const WINDOW: usize = 12;

/// How much context to show around a match, in each direction.
const EXCERPT_CONTEXT: usize = 60;

/// Collapse renderings that differ only in layout onto one form.
///
/// `{:#?}` puts each byte on its own line, and a pretty rendering formatted
/// *into* a string field arrives with those newlines escaped. Dropping both, and
/// all ASCII whitespace, lets one needle cover every layout.
///
/// **Idempotent, and it has to be.** Applied to needle and haystack alike, it
/// only puts the two in the same space if both reach the same *depth*. One pass
/// does not: deleting a space can bring a `\` against an `n` and *create* an
/// escape pair the next pass deletes. Mismatched depths failed both ways — a
/// needle degenerating to empty, where `find("")` matches every haystack, and an
/// over-normalized needle missing a whole credential sitting verbatim in an
/// event. Iterating to a fixpoint makes the depth equal whatever a caller does.
fn squeeze(text: &str) -> String {
    let mut current = squeeze_once(text);
    loop {
        let next = squeeze_once(&current);
        if next == current {
            return current;
        }
        current = next;
    }
}

/// One normalization pass.
///
/// Every step is a **deletion**, so a pass is either the identity or strictly
/// shorter, and [`squeeze`]'s loop terminates. Non-lengthening alone would not
/// be enough — a length-preserving rewrite like `.replace("xy", "yx")` never
/// lengthens and never converges. Keep this function deletions-only.
fn squeeze_once(text: &str) -> String {
    text.replace("\\n", "")
        .replace("\\r", "")
        .replace("\\t", "")
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect()
}

/// The secret from its first non-whitespace byte.
///
/// Leading whitespace is not distinctive enough to spend a window on, and does
/// not survive [`squeeze`] anyway. The invariant is the general one: **the guard
/// has to measure the same string the search uses.** Windowing before squeezing,
/// or checking one normalization and searching another, both yield an empty
/// needle — and `find("")` matches every haystack at offset 0.
///
/// Only *leading* whitespace is skipped. Interior whitespace renders as `32` or
/// `10` in a byte array, so dropping it would leave the byte needle no longer a
/// contiguous substring of the leaked rendering.
fn anchor(secret: &[u8]) -> &[u8] {
    let start = secret
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(secret.len());
    &secret[start..]
}

/// Every rendering of `secret` this module can recognize, each labeled for the
/// failure message.
///
/// Needles come back **already normalized**, in exactly the form
/// [`assert_secret_free`] searches with. Normalizing again at the call site is
/// what let the guard below measure one string while the search used another.
fn needles(secret: &[u8]) -> Vec<(&'static str, String)> {
    let anchored = anchor(secret);
    assert!(
        anchored.len() >= WINDOW,
        "a leak-test secret needs at least {WINDOW} bytes from its first \
         non-whitespace byte; a shorter one cannot be told from an ordinary \
         substring of a rendering"
    );

    // Both brackets are dropped, leaving the elements alone. The opening one
    // would anchor the needle to the start of an array, and a credential is
    // routinely only *part* of what leaks — a template renders `key: <secret>\n`,
    // so the secret's bytes sit in the middle of the buffer.
    let window = format!("{:?}", &anchored[..WINDOW]);
    let mut forms = vec![(
        "a Debug of the bytes",
        squeeze(window.trim_start_matches('[').trim_end_matches(']')),
    )];

    // The text form comes from the longest valid-UTF-8 prefix, not from
    // `from_utf8` on the whole secret: an ASCII-prefixed binary credential still
    // leaks as text, and testing the whole value would silently drop its text
    // needle.
    let text = match std::str::from_utf8(anchored) {
        Ok(text) => text,
        Err(e) => std::str::from_utf8(&anchored[..e.valid_up_to()])
            .expect("valid_up_to is a UTF-8 boundary"),
    };
    let squeezed: Vec<char> = squeeze(text).chars().collect();
    if squeezed.len() >= WINDOW {
        forms.push(("text", squeezed[..WINDOW].iter().collect()));
    } else {
        // A secret whose *whole* value is text owes us a text needle; one whose
        // text is only a short prefix of binary content does not, because the
        // byte needle already covers it.
        assert!(
            std::str::from_utf8(anchored).is_err(),
            "a textual leak-test secret needs at least {WINDOW} characters left \
             after normalization, and this one has {}: whitespace and the escape \
             pairs `\\n`, `\\r`, `\\t` are removed first, and a multi-byte \
             character counts once, not once per byte",
            squeezed.len()
        );
    }

    forms
}

/// A bounded window of `text` around a match at `at`.
///
/// The haystack can be a whole event stream or tracing buffer, and dumping it on
/// every failure buries the finding. The excerpt necessarily reproduces the
/// matched material, which is one more reason a leak test's secret has to be a
/// fixture value and never a real credential.
fn excerpt(text: &str, at: usize, len: usize) -> String {
    let mut start = at.saturating_sub(EXCERPT_CONTEXT);
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (at + len + EXCERPT_CONTEXT).min(text.len());
    while !text.is_char_boundary(end) {
        end += 1;
    }

    let lead = if start > 0 { "..." } else { "" };
    let trail = if end < text.len() { "..." } else { "" };
    format!("{lead}{}{trail}", &text[start..end])
}

/// Panic if `haystack` reproduces `secret` in any rendering this can recognize.
///
/// `context` names what was scanned ("an event", "a tracing record"), and lands
/// in the failure message.
///
/// # What a leak-test secret has to be
///
/// A fixture value, never a real credential: a failure prints an excerpt of what
/// matched. High-entropy, at least [`WINDOW`] bytes from its first non-whitespace
/// byte, and — if text — [`WINDOW`] *characters* still remaining after
/// [`squeeze`]. Count characters, not bytes: four emoji are sixteen
/// non-whitespace bytes but four characters, and are refused.
///
/// It must not read like a path, a package name, or an environment name. This is
/// a substring scan over a short window, so `selfie-deploy-token-01` matches the
/// ordinary log line `loading selfie-deploy package`. No window size fixes a
/// secret *shaped* like fixture data.
///
/// # What this cannot see
///
/// So that it is not over-trusted — `.claude/rules/secrets.md` says to assume
/// there is a fourth leak path, and this closes one rendering, not the class.
///
/// - **`String::from_utf8_lossy` of non-UTF-8 content — treat it as uncovered.**
///   Replacement characters are neither the bytes nor the text, so the byte
///   needle can never match a lossy rendering; only the text needle can, and only
///   while the readable prefix still holds [`WINDOW`] characters after
///   [`squeeze`]. Pinned both ways by
///   `catches_a_text_leak_of_an_ascii_prefixed_binary_secret` and
///   `a_lossy_rendering_escapes_a_whitespace_heavy_readable_prefix`.
/// - **Normalization is not context-free.** [`squeeze`] deletes `\n`, `\r` and
///   `\t` wherever they occur, so a backslash ending the text *around* a leak can
///   splice with an `n` opening the leak: the pair goes from the haystack but not
///   the needle, and the match is lost. Rare, and why this scan is a floor rather
///   than a proof.
/// - **Normalization bridges.** Dropping whitespace fuses what it separated, so a
///   needle can span two words never adjacent: `deploytokena1b2` matches
///   `Info { message: "deploy token a1b2 requested" }`. Only whitespace goes —
///   punctuation between `Debug` fields still separates them. Runs opposite to
///   the bullet above: that one loses a match, this one invents one.
/// - **Any rendering other than text and `Debug`-of-bytes**: base64, hex,
///   percent-encoding, JSON `\u` escapes, compression. Nothing here encodes
///   dotfile content today; add the form if something starts to.
/// - **`Debug` escapes other than `\n`, `\r`, `\t`.** A secret containing `"` or
///   `\` renders as `\"` or `\\`, which [`squeeze`] leaves alone, so the text
///   needle breaks across the escape. The byte needle is unaffected.
/// - **A partial leak not starting at the secret's first non-whitespace byte** —
///   the tail or middle of a credential passes, as does anything shorter than
///   [`WINDOW`].
/// - **Whatever the scanned rendering itself hides.** `ResolvedContent`'s
///   redacting `Debug` passes this while the same value could still leave through
///   `Display`, serde, or a direct write. Scan what the adapter actually emits.
/// - **Egress nothing scans at all**: `println!`, `eprintln!`, `dbg!`, files
///   other than the target, the network.
#[track_caller]
pub fn assert_secret_free(haystack: &str, secret: impl AsRef<[u8]>, context: &str) {
    let squeezed = squeeze(haystack);

    for (form, needle) in needles(secret.as_ref()) {
        // `needles` guarded the length of exactly this string. Re-normalizing
        // here is a no-op now that `squeeze` reaches a fixpoint, and this
        // assertion is what keeps that true rather than assumed. Two defenses
        // stand behind the same failure — a silent miss — and either catches it
        // alone: the fixpoint, and building needles pre-normalized so both sides
        // are squeezed to the same depth. They are **independent as detection
        // strategies but coupled through this assert**, which encodes the
        // fixpoint's invariant (the needle is already a fixpoint) rather than the
        // other's (both sides at equal depth). Break the fixpoint and this fires
        // before the scan can run, so the pair is not a free fallback.
        //
        // Concretely, in a debug build that turns a leak test into this
        // assertion rather than a `secret leaked into ...` report. That is
        // deliberate: it names the broken invariant instead of leaving you to
        // trace a scan result back to it. If you are here from a
        // `should_panic(expected = "as text")` test failing on this message, the
        // normalization is what broke, not the test.
        debug_assert_eq!(
            squeeze(&needle),
            needle,
            "needles must arrive normalized, or the guard measures the wrong string"
        );
        if let Some(at) = squeezed.find(&needle) {
            panic!(
                "secret leaked into {context} as {form}, at offset {at} of the \
                 normalized rendering:\n{}",
                excerpt(&squeezed, at, needle.len())
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "s3cr3t-v4lue-DO-NOT-LEAK";

    /// Eight newlines, then content. Windowing before squeezing left this with an
    /// empty text needle, and `contains("")` fails every assertion.
    const LEADING_WHITESPACE: &str = "\n\n\n\n\n\n\n\nhunter2-DO-NOT-LEAK";

    /// A `Started` event carries no content at all, and must never be reported.
    const CONTENT_FREE_EVENT: &str = "Started { operation_info: OperationInfo { \
         operation_type: DotfileApply, package_name: \"\", environment: \"test\" } }";

    // ─── The control ────────────────────────────────────────────────────────
    //
    // A scanner that catches nothing passes every `should_panic` test in this
    // file. These are the ones that fail if the scan is gutted.

    #[test]
    fn secret_free_text_passes() {
        assert_secret_free(
            "DotfileDeployed { target: \"/tmp/credentials\", bytes: <24 bytes> }",
            SECRET,
            "an event",
        );
    }

    /// The blocking bug: this reported a `Started` event as a credential leak.
    #[test]
    fn a_whitespace_leading_secret_does_not_match_a_content_free_event() {
        assert_secret_free(CONTENT_FREE_EVENT, LEADING_WHITESPACE, "an event");
    }

    /// A space between `\` and `n` becomes an escape pair once the space is
    /// dropped. Normalizing twice deleted the pair and left an *empty* needle,
    /// which `find` reports at offset 0 of anything — this haystack included.
    #[test]
    fn a_secret_whose_normalization_cascades_does_not_match_an_empty_haystack() {
        assert_secret_free("", "\\ n\\ n\\ n\\ n\\ n\\ n_tail_long_enough", "an event");
    }

    /// The same cascade left a two-character needle that matched fixture prose.
    #[test]
    fn a_secret_whose_normalization_cascades_does_not_match_ordinary_prose() {
        assert_secret_free(
            "Info { message: \"unable to read target\" }",
            "ab\\ n\\ n\\ n\\ n\\ ncd_long_enough_tail",
            "an event",
        );
    }

    /// The same mismatch **missed a leak**, which is the worse half of it: the
    /// whole credential sat verbatim in an event `Debug` and the scan stayed
    /// silent, because the needle had been normalized one pass deeper than the
    /// haystack it was searched in. This is the case the module exists to catch.
    #[test]
    #[should_panic(expected = "as text")]
    fn catches_a_verbatim_leak_of_a_secret_whose_normalization_cascades() {
        const CASCADING: &str = "AB\\ nCDEFGHIJKLMNOP";
        assert_secret_free(
            &format!("Warning {{ message: \"wrote {CASCADING}\" }}"),
            CASCADING,
            "an event",
        );
    }

    /// The guard has to measure the string the search uses, so [`squeeze`] must
    /// reach a fixpoint. One pass does not.
    #[test]
    fn squeeze_is_idempotent() {
        for input in [
            "\\ n\\ n\\ n",
            "a \\ n b",
            "\\\\ n",
            "\\ \\ n n",
            "plain text with spaces",
            "[115, 51, 99]",
            // `\^k n^k` collapses one pair per pass, so this one needs four.
            // Without it every input here converges within three, and an
            // implementation capped at three passes would pass the test.
            "\\\\\\\\nnnn",
        ] {
            let once = squeeze(input);
            assert_eq!(squeeze(&once), once, "not a fixpoint for {input:?}");
        }
    }

    /// Collides at a window of 8 (`test-env` is in `test-env-pkg`), not at 12.
    #[test]
    fn a_secret_prefixed_like_an_environment_name_does_not_collide() {
        assert_secret_free(
            "Started { package_name: \"test-env-pkg\", environment: \"test\" }",
            "test-environment-credential-xyz",
            "an event",
        );
    }

    /// Collides at a window of 8 (`~/.confi`), not at 12.
    #[test]
    fn a_secret_shaped_like_a_config_path_does_not_collide() {
        assert_secret_free(
            "DotfileDeployed { target: \"~/.config/nvim/init.lua\" }",
            "~/.config/selfie/token",
            "an event",
        );
    }

    // ─── Renderings that must be caught ─────────────────────────────────────

    #[test]
    #[should_panic(expected = "as text")]
    fn catches_the_literal() {
        assert_secret_free(&format!("wrote {SECRET}"), SECRET, "an event");
    }

    #[test]
    #[should_panic(expected = "as a Debug of the bytes")]
    fn catches_a_debug_of_the_bytes() {
        assert_secret_free(
            &format!("wrote {:?}", SECRET.as_bytes()),
            SECRET,
            "an event",
        );
    }

    #[test]
    #[should_panic(expected = "as a Debug of the bytes")]
    fn catches_a_debug_of_a_vec() {
        assert_secret_free(
            &format!("wrote {:?}", SECRET.as_bytes().to_vec()),
            SECRET,
            "an event",
        );
    }

    /// A truncating leak. Shorter than [`WINDOW`] would pass — a documented bound.
    #[test]
    #[should_panic(expected = "as a Debug of the bytes")]
    fn catches_a_truncated_debug_of_the_bytes() {
        assert_secret_free(
            &format!("wrote {:?}", &SECRET.as_bytes()[..16]),
            SECRET,
            "an event",
        );
    }

    #[test]
    #[should_panic(expected = "as text")]
    fn catches_a_truncated_text_rendering() {
        assert_secret_free(&format!("wrote {}", &SECRET[..16]), SECRET, "an event");
    }

    #[test]
    #[should_panic(expected = "as a Debug of the bytes")]
    fn catches_a_pretty_debug_of_the_bytes() {
        assert_secret_free(
            &format!("wrote {:#?}", SECRET.as_bytes()),
            SECRET,
            "an event",
        );
    }

    /// A pretty rendering formatted into a string field, then `Debug`ged again:
    /// the newlines arrive escaped.
    #[test]
    #[should_panic(expected = "as a Debug of the bytes")]
    fn catches_an_escaped_pretty_debug_of_the_bytes() {
        let message = format!("{:#?}", SECRET.as_bytes());
        assert_secret_free(
            &format!("Warning {{ message: {message:?} }}"),
            SECRET,
            "an event",
        );
    }

    /// A template renders the credential into a larger file, so the leaked array
    /// holds the secret's bytes in the middle. A needle anchored to the start of
    /// the array misses this — it did, before this test existed.
    #[test]
    #[should_panic(expected = "as a Debug of the bytes")]
    fn catches_a_debug_of_bytes_holding_the_secret_in_the_middle() {
        let rendered = format!("key: {SECRET}\n");
        assert_secret_free(
            &format!("wrote {:?}", rendered.as_bytes()),
            SECRET,
            "an event",
        );
    }

    /// A non-UTF-8 credential has no text rendering, so the byte form is the only
    /// one — the case a text-only scan could never have caught.
    #[test]
    #[should_panic(expected = "as a Debug of the bytes")]
    fn catches_a_debug_of_binary_secret_bytes() {
        let secret: Vec<u8> = vec![
            0x00, 0xff, 0xfe, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0b,
        ];
        assert_secret_free(&format!("wrote {secret:?}"), &secret, "an event");
    }

    /// Binary content behind an ASCII prefix still has a text rendering. Deciding
    /// the text form from `from_utf8` on the *whole* secret would drop it.
    #[test]
    #[should_panic(expected = "as text")]
    fn catches_a_text_leak_of_an_ascii_prefixed_binary_secret() {
        let mut secret = b"api-key-prefix-".to_vec();
        secret.extend_from_slice(&[0xff, 0xfe, 0x00]);
        assert_secret_free(
            &format!("wrote {}", String::from_utf8_lossy(&secret)),
            &secret,
            "an event",
        );
    }

    /// The other side of that boundary, and a documented hole rather than a bug.
    ///
    /// This prefix is 14 bytes — longer than [`WINDOW`] — but squeezes to seven
    /// non-whitespace characters, so no text needle is built, and a lossy
    /// rendering reproduces neither the bytes nor a needle. The bound is on
    /// non-whitespace characters, not bytes; asserting the escape here is what
    /// keeps the doc comment honest about which one.
    #[test]
    fn a_lossy_rendering_escapes_a_whitespace_heavy_readable_prefix() {
        let mut secret = b"a b c d e f g ".to_vec();
        secret.extend_from_slice(&[0xff, 0xfe, 0x00]);
        assert_secret_free(
            &format!("wrote {}", String::from_utf8_lossy(&secret)),
            &secret,
            "an event",
        );
    }

    #[test]
    #[should_panic(expected = "as a Debug of the bytes")]
    fn catches_a_debug_leak_of_a_whitespace_leading_secret() {
        assert_secret_free(
            &format!("wrote {:?}", LEADING_WHITESPACE.as_bytes()),
            LEADING_WHITESPACE,
            "an event",
        );
    }

    #[test]
    #[should_panic(expected = "as text")]
    fn catches_a_text_leak_of_a_whitespace_leading_secret() {
        assert_secret_free(
            &format!("wrote {LEADING_WHITESPACE}"),
            LEADING_WHITESPACE,
            "an event",
        );
    }

    // ─── Caller requirements ────────────────────────────────────────────────

    #[test]
    #[should_panic(expected = "at least 12 bytes from its first non-whitespace byte")]
    fn refuses_a_secret_too_short_to_scan_for() {
        assert_secret_free("some output", "test", "an event");
    }

    /// Twelve bytes, four characters, no whitespace at all. The refusal is right;
    /// the message has to explain the count that actually applies.
    #[test]
    #[should_panic(expected = "a multi-byte character counts once, not once per byte")]
    fn refuses_a_multibyte_secret_of_too_few_characters() {
        assert_secret_free(
            "some output",
            "\u{8a8d}\u{8a3c}\u{6a29}\u{9650}",
            "an event",
        );
    }

    #[test]
    #[should_panic(
        expected = "at least 12 characters left after normalization, and this one has 10"
    )]
    fn refuses_a_secret_that_is_mostly_whitespace() {
        assert_secret_free("some output", "abc def ghi j", "an event");
    }

    // ─── The message ────────────────────────────────────────────────────────

    /// A haystack is a whole event stream; the failure must not be one.
    #[test]
    fn the_failure_message_is_bounded() {
        let haystack = format!("{}{SECRET}{}", "x".repeat(5_000), "y".repeat(5_000));
        let message = std::panic::catch_unwind(|| {
            assert_secret_free(&haystack, SECRET, "an event");
        })
        .expect_err("the leak must be caught");
        let message = message
            .downcast_ref::<String>()
            .expect("a formatted panic message");

        assert!(
            message.len() < 400,
            "the excerpt must be bounded, got {} bytes",
            message.len()
        );
        assert!(
            message.contains("..."),
            "an elided excerpt says so: {message}"
        );
    }
}
