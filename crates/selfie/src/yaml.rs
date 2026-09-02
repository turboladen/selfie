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
    // `ParseFailure` is what keeps the file's text out of the answer; suppressing
    // the snippet means the parser never builds the quoted window in the first
    // place. Set here rather than at each call site so a new caller cannot omit it.
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
/// string, save for the bracket or single character a scanner failure stopped on;
/// the location is a pair of numbers.
///
/// **This is not a guarantee that nothing about the file escapes** — see the note
/// beside the struct for what the location and that character disclose.
// What the location and the forwarded character disclose, which is why the doc
// above stops short of an absolute.
//
// Both numbers are derived from the file. The column is an offset into the
// offending line, so it encodes that line's layout: for `  <key>: <value>` a type
// mismatch reports `len(key) + 5`, giving away the key's length. A scanner failure
// adds the bracket or character it stopped on -- one character, escaped.
//
// Accepted, because a location is what makes the message actionable and the file
// is owner-only. Do not restate this as "nothing the file contained".
#[derive(Debug, Clone)]
pub struct ParseFailure {
    wording: Wording,
    location: Option<(u64, u64)>,
}

/// Where a [`ParseFailure`]'s sentence comes from.
// Two shapes rather than one `String`, so the audited surface is one variant wide.
// `Classified` cannot carry file content at all: both halves are `&'static str`.
// `Parser` can, which is why every value it holds has to come from the gate in
// `parser_wording` rather than from anywhere a message happens to be available.
#[derive(Debug, Clone)]
enum Wording {
    /// Selfie's own vocabulary, optionally followed by a noun the deserializer
    /// supplied -- a field name or a type name, never a value from the file.
    Classified {
        kind: &'static str,
        detail: Option<&'static str>,
    },
    /// The YAML parser's own sentence.
    Parser(String),
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

        // The parser's own wording says far more than "not valid YAML" for what a
        // hand-edited file actually hits -- a stray colon, an unclosed bracket, a
        // tab in indentation. Those arrive as a scanner error, which the
        // classification below sees only as one undifferentiated variant.
        if let E::ExternalMessage { source, .. } = error
            && let serde_saphyr::ExternalMessageSource::Parser(scan) = source.as_ref()
            && let Some(wording) = parser_wording(scan)
        {
            return Self {
                wording: Wording::Parser(wording),
                location: located(error),
            };
        }

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

        Self {
            wording: Wording::Classified { kind, detail },
            location: located(error),
        }
    }
}

// The scanner's own sentence for `scan`, or `None` where that sentence could quote
// the file. Every kind named writes a fixed string apart from a bracket or the
// offending character -- the disclosure the column number already makes.
//
// Named one by one rather than deducted from a catch-all: `ErrorKind` is
// `#[non_exhaustive]`, so a catch-all forwards whatever a dependency bump adds.
// An unnamed kind falls to the classification instead, which quotes nothing.
//
// Read from `kind()`, not the `msg` beside it, which appends the include chain
// when a parser stack has one.
fn parser_wording(scan: &serde_saphyr::granit_parser::ScanError) -> Option<String> {
    use serde_saphyr::granit_parser::ErrorKind;

    let kind = scan.kind();
    match kind {
        ErrorKind::TooManyComments
        | ErrorKind::UnexpectedEofFlowSequence
        | ErrorKind::UnexpectedEofFlowMapping
        | ErrorKind::UnexpectedEofImplicitFlowMapping
        | ErrorKind::UnexpectedEofBlockSequence
        | ErrorKind::UnexpectedEofBlockMapping
        | ErrorKind::UnexpectedEof
        | ErrorKind::ExpectedStreamStart
        | ErrorKind::DuplicateVersionDirective
        | ErrorKind::UnsupportedYamlMajorVersion
        | ErrorKind::DuplicateTagDirective
        | ErrorKind::ExpectedDocumentStart
        | ErrorKind::MissingDocumentEndBeforeDirective
        | ErrorKind::AnchorCountOverflow
        | ErrorKind::UnknownAnchor
        | ErrorKind::ExpectedNodeContent
        | ErrorKind::ExpectedBlockMappingKey
        | ErrorKind::ExpectedFlowMappingSeparator
        | ErrorKind::ExpectedFlowSequenceSeparator
        | ErrorKind::ExpectedBlockSequenceEntry
        | ErrorKind::UndeclaredTagHandle
        | ErrorKind::MissingIncludeResolver
        | ErrorKind::MultipleDocumentsUnsupported
        | ErrorKind::InputOffsetsWithoutSlice
        | ErrorKind::InputSlicingUnavailable
        | ErrorKind::ExpectedTagBang
        | ErrorKind::ExpectedTagDirectiveBang
        | ErrorKind::InvalidGlobalTagCharacter
        | ErrorKind::SimpleKeyExpected
        | ErrorKind::InvalidSimpleKey
        | ErrorKind::InvalidDocumentEnd
        | ErrorKind::InvalidIndentation
        | ErrorKind::BomInsideDocument
        | ErrorKind::UnexpectedCharacter { .. }
        | ErrorKind::TabNotAllowed
        | ErrorKind::TabInBlockIndentation
        | ErrorKind::CommentInterceptedScalar
        | ErrorKind::ExpectedWhitespace
        | ErrorKind::CommentNotSeparated
        | ErrorKind::InvalidDirectiveTerminator
        | ErrorKind::MissingYamlVersionSeparator
        | ErrorKind::MissingDirectiveName
        | ErrorKind::InvalidDirectiveName
        | ErrorKind::YamlVersionTooLong
        | ErrorKind::MissingYamlVersion
        | ErrorKind::InvalidTagDirectiveTerminator
        | ErrorKind::InvalidTagTerminator
        | ErrorKind::MissingTagUri
        | ErrorKind::UnclosedVerbatimTag
        | ErrorKind::InvalidTagEscape
        | ErrorKind::InvalidTagUtf8LeadingByte
        | ErrorKind::InvalidTagUtf8TrailingByte
        | ErrorKind::InvalidTagUtf8
        | ErrorKind::MissingAnchorOrAliasName
        | ErrorKind::MisplacedFlowCollectionEnd
        | ErrorKind::MismatchedFlowCollectionEnd { .. }
        | ErrorKind::UnclosedFlowCollection { .. }
        | ErrorKind::RecursionLimitExceeded
        | ErrorKind::BlockEntryInFlowCollection
        | ErrorKind::BlockSequenceEntryNotAllowed
        | ErrorKind::InvalidBlockEntryWhitespace
        | ErrorKind::ZeroBlockScalarIndent
        | ErrorKind::InvalidBlockScalarHeader
        | ErrorKind::TabAtBlockScalarStart
        | ErrorKind::InvalidBlockScalarIndent
        | ErrorKind::DocumentIndicatorInQuotedScalar
        | ErrorKind::UnclosedQuotedScalar
        | ErrorKind::TabInIndentation
        | ErrorKind::InvalidQuotedScalarIndent
        | ErrorKind::InvalidTrailingSingleQuotedScalar
        | ErrorKind::InvalidTrailingDoubleQuotedScalar
        | ErrorKind::UnknownQuotedScalarEscape
        | ErrorKind::InvalidQuotedScalarHexEscape
        | ErrorKind::InvalidLowSurrogateHexEscape
        | ErrorKind::InvalidLowSurrogate
        | ErrorKind::MissingLowSurrogate
        | ErrorKind::UnpairedLowSurrogate
        | ErrorKind::InvalidUnicodeEscape
        | ErrorKind::InvalidFlowScalarIndent
        | ErrorKind::PlainScalarStartsWithDashFlowIndicator
        | ErrorKind::TabInPlainScalar
        | ErrorKind::UnexpectedEndOfPlainScalar
        | ErrorKind::MappingKeyNotAllowed
        | ErrorKind::FlowMappingValueAdjacentCollection
        | ErrorKind::InvalidMappingValueWhitespace
        | ErrorKind::InvalidColonPlacement
        | ErrorKind::MappingValueNotAllowed => Some(kind.to_string()),

        // `InputIo`, `InputDecoding` and `InputByteLimitExceeded` interpolate what
        // an input adapter supplied and `Custom` what an include resolver
        // supplied. Anything else is a kind this list has never seen.
        _ => None,
    }
}

// Line 0 is the library's "unknown" sentinel. Comparing against `Location::UNKNOWN`
// instead would miss it: that constant carries a span and a source id, which the
// derived `PartialEq` compares as well, so a located-but-line-0 value would render
// as "at line 0, column 0".
fn located(error: &serde_saphyr::Error) -> Option<(u64, u64)> {
    error
        .location()
        .filter(|l| l.line() != 0)
        .map(|l| (l.line(), l.column()))
}

impl std::fmt::Display for ParseFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.wording {
            Wording::Classified { kind, detail } => {
                f.write_str(kind)?;
                if let Some(detail) = detail {
                    write!(f, " {detail}")?;
                }
            }
            Wording::Parser(sentence) => f.write_str(sentence)?,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::collections::HashMap;

    // Shaped like a package spec, because the failures worth covering are the ones a
    // hand-edited spec produces.
    #[derive(Debug, Deserialize)]
    #[expect(
        dead_code,
        reason = "the fields shape what serde asks the parser for; nothing reads them back"
    )]
    struct Spec {
        name: String,
        #[serde(default)]
        dotfiles: Vec<HashMap<String, String>>,
        environments: HashMap<String, Environment>,
    }

    // `deny_unknown_fields` is what reaches the serde variants that quote a key
    // from the file rather than a name from this struct. Without it no fixture
    // below reaches them, and the scan for planted text has nothing to catch.
    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    #[expect(
        dead_code,
        reason = "the field shape is the point; nothing reads the parsed value back"
    )]
    struct Environment {
        install: String,
    }

    // A secret-shaped string, planted three lines above every failure below. It is
    // not a secret itself; it is the kind of thing a `command:` entry holds, and it
    // is what a source snippet would quote if one ever came back.
    const PLANTED: &str = "op://vault/private/DEPLOY_TOKEN";

    const CATCH_ALL: &str = "the file is not valid YAML";

    fn spec_with(tail: &str) -> String {
        format!(
            "name: pkg\ndotfiles:\n  - command: op read {PLANTED}\n    target: ~/.npmrc\n{tail}"
        )
    }

    // Every shape a malformed spec takes, and the sentence each one earns.
    //
    // Two assertions pulling opposite ways: no row may quote the planted string,
    // and every row must earn its exact sentence. A row that regresses to the
    // catch-all loses the diagnosis without losing anything a reader would notice.
    //
    // The last row earns the catch-all and is pinned to it. `SerdeUnknownField`
    // names the key it found, and that key is the file's.
    #[test]
    fn every_malformed_spec_is_diagnosed_without_quoting_the_file() {
        let cases: &[(&str, &str, &str)] = &[
            (
                "misplaced colon",
                "environments: {a: b: c}\n",
                "the file has the wrong shape, expected mapping start",
            ),
            (
                "unclosed flow sequence",
                "environments: [oops\n",
                "the file has the wrong shape, expected mapping start",
            ),
            (
                "unclosed flow mapping",
                "environments: {oops\n",
                "unclosed bracket '{'",
            ),
            (
                "mismatched bracket",
                "environments: [oops}\n",
                "the file has the wrong shape, expected mapping start",
            ),
            (
                "unexpected character",
                "environments: @nope\n",
                "unexpected character: `@'",
            ),
            (
                "unclosed quoted scalar",
                "environments: \"oops\n",
                "invalid indentation in multiline quoted scalar",
            ),
            (
                "tab in indentation",
                "environments:\n\tmacos: {}\n",
                "tabs disallowed within this context (block indentation)",
            ),
            (
                "block scalar indent",
                "environments: |\n  a\n b\n",
                "the file has the wrong shape, expected mapping start",
            ),
            (
                "unknown escape",
                "environments: \"a\\qb\"\n",
                "while parsing a quoted scalar, found unknown escape character",
            ),
            (
                "unknown anchor",
                "environments: *ghost\n",
                "the file refers to an anchor it never defines",
            ),
            (
                "bad directive",
                "%YAML x.y\n---\nenvironments: {}\n",
                "while scanning a YAML directive, did not find expected version number",
            ),
            (
                "undeclared tag handle",
                "environments: !e!thing {}\n",
                "the handle wasn't declared",
            ),
            (
                "block entry in flow",
                "environments: [- a]\n",
                "the file has the wrong shape, expected mapping start",
            ),
            (
                "byte order mark inside",
                "environments: {}\n\u{feff}extra: 1\n",
                "a BOM must not appear inside a document",
            ),
            (
                "tab after a key",
                "environments:\n  macos:\n\tinstall: x\n",
                "tabs disallowed within this context (block indentation)",
            ),
            (
                "missing field",
                "",
                "an entry is missing the field environments",
            ),
            (
                "duplicate key",
                "environments: {}\nenvironments: {}\n",
                "a key is listed twice",
            ),
            (
                "wrong shape",
                "environments: 3\n",
                "the file has the wrong shape, expected mapping start",
            ),
            (
                "multiple documents",
                "environments: {}\n---\nname: two\n",
                "the file holds more than one YAML document",
            ),
            (
                "value where a mapping belongs",
                "environments:\n  macos: op://vault/private/other\n",
                "the file has the wrong shape, expected mapping start",
            ),
            (
                "unknown field named for a vault",
                "environments:\n  macos:\n    install: x\n    op_vault_field: y\n",
                CATCH_ALL,
            ),
        ];

        for (label, tail, expected) in cases {
            let failure = parse::<Spec>(&spec_with(tail)).expect_err("fixture must fail to parse");
            let rendered = failure.to_string();

            for fragment in ["vault", "private", "DEPLOY_TOKEN", "npmrc", "op read"] {
                assert!(
                    !rendered.contains(fragment),
                    "{label}: quoted `{fragment}` from the file: {rendered}"
                );
            }

            let sentence = rendered
                .split_once(" at line ")
                .map_or(rendered.as_str(), |(sentence, _)| sentence);
            assert_eq!(&sentence, expected, "{label}");
        }
    }

    // The gate in `parser_wording`, from the other side: a message that would carry
    // a value is refused rather than trimmed.
    #[test]
    fn a_parser_message_carrying_adapter_text_is_refused() {
        let scan = serde_saphyr::granit_parser::ScanError::new(
            serde_saphyr::granit_parser::Marker::new(0, 1, 0),
            format!("resolver said {PLANTED}"),
        );

        assert!(
            parser_wording(&scan).is_none(),
            "an adapter-supplied message must not become the failure's wording"
        );
    }

    // A location is reported as numbers, for a caller that has its own field for it.
    #[test]
    fn a_located_failure_answers_with_the_line_and_column() {
        let failure = parse::<Spec>("name: pkg\nenvironments: 3\n").expect_err("must fail");

        assert_eq!(failure.location(), Some((2, 15)));
    }
}
