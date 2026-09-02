//! Reading YAML, and describing a file that would not parse.
//!
//! Every YAML file selfie reads comes through [`parse`], which suppresses
//! serde-saphyr's source snippets and answers a [`ParseFailure`] rather than the
//! parser's own error. No parse failure here quotes the text it was reading.
//!
//! That rule is one rule and not two. A deploy state file is machine-written and
//! its keys are dotfile source paths, so quoting it is egress. A package file is
//! hand-authored, and quoting one would seem harmless -- except that a package
//! file carries `command:` and `vars:` entries naming credential stores and vault
//! items, and serde-saphyr's snippet quotes a window of lines around the failure
//! rather than the failing line alone. Both files therefore get the same answer.
//!
//! What still escapes is a failure class, a noun drawn from the deserializer's
//! own vocabulary, and a line and column. See [`ParseFailure`] for why that is
//! accepted.

use serde::de::DeserializeOwned;

/// Read `content` as `T`.
///
/// Suppresses source snippets, so a failure describes the shape of the problem
/// and where it is without quoting what was there.
///
/// # Errors
///
/// [`ParseFailure`] when `content` is not valid YAML, or does not describe a `T`.
pub fn parse<T: DeserializeOwned>(content: &str) -> Result<T, ParseFailure> {
    // The only call of serde-saphyr in the workspace that is not a test fixture,
    // which is what makes the no-quoting rule checkable rather than a convention.
    // `clippy.toml` denies the rest.
    #[allow(
        clippy::disallowed_methods,
        reason = "the single entry point the lint exists to funnel callers into"
    )]
    // `with_snippet` is the whole of the no-quoting rule. Set here rather than at
    // each call site so a new caller cannot omit it.
    let parsed = serde_saphyr::from_str_with_options(
        content,
        serde_saphyr::options! {
            with_snippet: false
        },
    );
    parsed.map_err(|e| ParseFailure::of(&e))
}

/// Reports why a YAML file could not be parsed, without quoting the file.
///
/// Renders the failure class and the line and column. The class is a fixed
/// string; the location is a pair of numbers. Neither carries the file's text.
///
/// **This is not a guarantee that nothing about the file escapes** — see the note
/// beside the struct for what the location discloses.
// What the location discloses, which is why the doc above stops short of an
// absolute.
//
// Both numbers are derived from the file. The column is an offset into the
// offending line, so it encodes that line's layout: for `  <key>: <value>` a type
// mismatch reports `len(key) + 5`, giving away the key's length. The line number
// is weaker but the same kind of thing. Bytes do not escape; two lengths do.
//
// Accepted, because a location is what makes the message actionable and the file
// is owner-only. Do not restate this as "nothing the file contained".
#[derive(Debug, Clone)]
pub struct ParseFailure {
    kind: &'static str,
    detail: Option<&'static str>,
    location: Option<(u64, u64)>,
}

impl ParseFailure {
    /// Classify `error` without reading any of the parsed content out of it.
    fn of(error: &serde_saphyr::Error) -> Self {
        use serde_saphyr::Error as E;

        // Unwrap the snippet wrapper before matching. `parse` suppresses snippets,
        // so nothing wraps today -- but if that option is ever dropped, every
        // *located* error arrives as `WithSnippet` and every arm below falls to the
        // catch-all, collapsing the classification to one kind with nothing to
        // notice it by. Located, not every: the library wraps only when the error
        // carries a location, so the ones that do not -- an IO failure, invalid
        // UTF-8 -- would still classify correctly, which is the half that would
        // make the collapse look intermittent.
        let error = error.without_snippet();

        // Every arm yields `&'static str`, so no arm can carry a key, a value or a
        // tag out of the error. `Box::leak` of a formatted string would also
        // compile, so this stops an accidental leak, not a deliberate one.
        //
        // The three `Some` arms forward the deserializer's own vocabulary rather
        // than the file's; `no_passed_through_text_carries_input` holds them to it.
        //
        // `serde_saphyr::Error` is `#[non_exhaustive]`, so the catch-all is
        // required and a future variant lands there rather than rendering whatever
        // it interpolates.
        let (kind, detail) = match error {
            E::DuplicateMappingKey { .. } => ("a key is listed twice", None),
            // Each kind below is worded so the library's own noun can follow it
            // directly: "the file has the wrong shape, expected" and "mapping
            // start" read as a single sentence.
            E::Unexpected { expected, .. } => {
                ("the file has the wrong shape, expected", Some(*expected))
            }
            E::SerdeMissingField { field, .. } => ("an entry is missing the field", Some(*field)),
            // Unreachable while every field of a state file is a `String`. It is
            // kept so that if that stops being true, the error is classified here
            // rather than falling silently to the catch-all.
            E::InvalidScalar { ty, .. } => ("a value is not a valid", Some(*ty)),
            E::NullIntoString { .. } => ("a value is empty where text is required", None),
            // The hint this variant carries is content-free but names
            // `from_multiple`, a serde-saphyr entry point the reader cannot call.
            E::MultipleDocuments { .. } => ("the file holds more than one YAML document", None),
            E::Eof { .. } => ("the file ends sooner than expected", None),

            // These carry a `Location` and nothing else, so naming them cannot
            // leak: there is no field to leak from. Collapsing them into the
            // catch-all cost a diagnosis for no safety.
            //
            // Check that field list before adding to this group. It is the
            // safety argument, so a variant that does not fit it does not just
            // sit here harmlessly -- it invites the next person to forward a
            // field on the strength of a sentence that was never true of it.
            E::UnknownAnchor { .. } => ("the file refers to an anchor it never defines", None),
            E::MergeValueNotMapOrSeqOfMaps { .. } => (
                "a merge key does not refer to a mapping or a list of mappings",
                None,
            ),
            E::MergeKeyNotAllowed { .. } => ("a merge key is not allowed here", None),
            E::UnexpectedMappingEnd { .. }
            | E::UnexpectedSequenceEnd { .. }
            | E::ContainerEndMismatch { .. } => {
                ("the file has an unbalanced list or mapping", None)
            }
            E::InvalidBinaryBase64 { .. } => ("a !!binary value is not valid base64", None),
            E::BinaryNotUtf8 { .. } => ("a !!binary value is not text", None),
            E::TaggedScalarCannotDeserializeIntoString { .. } => {
                ("a tagged value cannot be read as text", None)
            }

            // Unlike the group above, this one carries `required` and `actual`,
            // the indentation width found in the file -- content-derived, the
            // same residual class as the location. Nothing forwards it, because
            // the arm binds `..`; it sits apart rather than inside a group whose
            // stated property is "no field to leak from".
            //
            // Unreachable as well: `require_indent` defaults to `Unchecked` and
            // selfie sets only `with_snippet`, so bad indentation arrives as a
            // scanner error. Kept for the same reason as `InvalidScalar`.
            E::IndentationError { .. } => ("the file's indentation is wrong", None),

            // Named apart because they describe different conditions, not
            // because a bomb reaches them -- it does not. A 745-byte
            // billion-laughs surfaces as `AliasError` wrapping a budget breach
            // and lands in the catch-all; heavy alias reuse surfaces as
            // `Budget`. These three are kept distinct so that if they ever do
            // fire they do not inherit a size wording that is wrong for them.
            E::AliasReplayLimitExceeded { .. }
            | E::AliasExpansionLimitExceeded { .. }
            | E::AliasReplayStackDepthExceeded { .. } => (
                "the file's aliases expand to far more than it contains",
                None,
            ),
            // Not a size condition at all: an internal counter wrapped.
            E::AliasReplayCounterOverflow { .. } => (
                "the file could not be expanded; an internal limit was reached",
                None,
            ),
            E::Budget { .. } => ("the file is too large or too complex", None),
            _ => ("the file is not valid YAML", None),
        };

        // Line 0 is the library's "unknown" sentinel. Comparing against
        // `Location::UNKNOWN` instead would miss it: that constant carries a span
        // and a source id, which the derived `PartialEq` compares as well, so a
        // located-but-line-0 value would render as "at line 0, column 0".
        let location = error
            .location()
            .filter(|l| l.line() != 0)
            .map(|l| (l.line(), l.column()));

        Self {
            kind,
            detail,
            location,
        }
    }
}

impl std::fmt::Display for ParseFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.kind)?;
        if let Some(detail) = self.detail {
            write!(f, " {detail}")?;
        }
        if let Some((line, column)) = self.location {
            write!(f, " at line {line}, column {column}")?;
        }
        Ok(())
    }
}

impl ParseFailure {
    /// Where the parser stopped, as a line and column, when it reported one.
    #[must_use]
    pub fn location(&self) -> Option<(u64, u64)> {
        self.location
    }
}

impl std::error::Error for ParseFailure {}
