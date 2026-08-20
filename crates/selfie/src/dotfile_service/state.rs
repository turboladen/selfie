//! Per-machine deploy state persistence and drift detection.
//!
//! Each time selfie deploys a config file, it records the source and deployed
//! checksums in a [`DeployState`] file (typically `~/.local/state/selfie/deploy-state.yml`,
//! i.e., under XDG_STATE_HOME).
//! On subsequent runs, these stored checksums are compared against the current
//! file contents to classify changes as one of four [`DriftType`] variants —
//! enabling the service layer to decide whether to deploy, skip, or flag a
//! conflict.
//!
//! The state file is intentionally per-machine and not version-controlled: it
//! reflects what was deployed *here*, which may differ from other machines
//! sharing the same config repository.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeployState {
    #[serde(default)]
    deployed: HashMap<String, DeployEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployEntry {
    source_checksum: String,
    deployed_checksum: String,
    deployed_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriftType {
    None,
    RepoChanged,
    TargetChanged,
    BothChanged,
    NotTracked,
}

impl std::fmt::Display for DriftType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DriftType::None => write!(f, "none"),
            DriftType::RepoChanged => write!(f, "repo changed"),
            DriftType::TargetChanged => write!(f, "target changed"),
            DriftType::BothChanged => write!(f, "both changed"),
            DriftType::NotTracked => write!(f, "not tracked"),
        }
    }
}

impl DeployState {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn entries(&self) -> &HashMap<String, DeployEntry> {
        &self.deployed
    }

    pub fn get(&self, source: &str) -> Option<&DeployEntry> {
        self.deployed.get(source)
    }

    pub fn record_deployment(&mut self, source: &str, checksum: &str) {
        self.deployed.insert(
            source.to_string(),
            DeployEntry {
                source_checksum: checksum.to_string(),
                deployed_checksum: checksum.to_string(),
                deployed_at: chrono::Utc::now().to_rfc3339(),
            },
        );
    }

    pub fn detect_drift(
        &self,
        source: &str,
        current_source_checksum: &str,
        current_target_checksum: &str,
    ) -> DriftType {
        let Some(entry) = self.deployed.get(source) else {
            return DriftType::NotTracked;
        };
        match (
            entry.source_checksum != current_source_checksum,
            entry.deployed_checksum != current_target_checksum,
        ) {
            (false, false) => DriftType::None,
            (true, false) => DriftType::RepoChanged,
            (false, true) => DriftType::TargetChanged,
            (true, true) => DriftType::BothChanged,
        }
    }
}

impl DeployEntry {
    pub fn source_checksum(&self) -> &str {
        &self.source_checksum
    }

    pub fn deployed_checksum(&self) -> &str {
        &self.deployed_checksum
    }
}

/// Reports why a deploy state file could not be parsed, without quoting the
/// file.
///
/// Renders the failure class and the line and column. The class is a fixed
/// string; the location is a pair of numbers. Neither carries the file's text.
///
/// Use this instead of a parse error's own `Display` anywhere the result can
/// reach a caller. **It is not a guarantee that nothing about the file escapes**
/// — see the note beside the struct for what the location discloses.
// What the location discloses, which is why the doc above stops short of an
// absolute:
//
// Both numbers are derived from the file. The column is an offset into the
// offending line, so it encodes that line's layout: for `  <key>: <value>` a
// type mismatch reports `len(key) + 5`, and a reader who knows the shape
// recovers the key's length exactly. The line number is weaker but the same
// kind of thing -- entries here are four lines each, so a failure at line 41
// says roughly ten dotfiles precede it. Bytes do not escape; two lengths do.
//
// Accepted, because a location is what makes the message actionable, and the
// alternative -- a class with no coordinates -- is not usefully better: the
// file is owner-only and an attacker who can read it does not need the warning.
//
// Do not restate any of this as "nothing the file contained". A comment at this
// site already warned against that restatement once; the change that removed
// the warning went on to make the claim, in a `///`, where it was harder to
// see.
pub(super) struct ParseFailure {
    kind: &'static str,
    detail: Option<&'static str>,
    location: Option<(u64, u64)>,
}

impl ParseFailure {
    /// Classify `error` without reading any of the parsed content out of it.
    pub(super) fn of(error: &serde_saphyr::Error) -> Self {
        use serde_saphyr::Error as E;

        // Unwrap the snippet wrapper before matching. `load_deploy_state` parses
        // with `with_snippet: false`, so nothing wraps today -- but if that option
        // is ever dropped, every *located* error arrives as `WithSnippet` and
        // every arm below falls to the catch-all, collapsing the classification to
        // one kind with nothing to notice it by. Located, not every: the library
        // wraps only when the error carries a location, so the ones that do not --
        // an IO failure, invalid UTF-8 -- would still classify correctly, which is
        // the half that would make the collapse look intermittent.
        let error = error.without_snippet();

        // Every arm yields `&'static str`, so no arm can carry a key, a value or
        // a tag out of the error. `Box::leak` of a formatted string is also
        // `&'static str` and would compile, so this stops an accidental leak and
        // not a deliberate one.
        //
        // The three `Some` arms forward the library's own `&'static str`, which is
        // the deserializer's vocabulary rather than the file's: the field names
        // come from this file's own derives, and "expected mapping start" names a
        // YAML event. `no_passed_through_text_carries_input` holds them to that.
        //
        // `serde_saphyr::Error` is `#[non_exhaustive]`, so the catch-all is
        // required, and a variant added by a future release lands there instead
        // of rendering whatever it interpolates. Nothing announces such a
        // variant; the message just gets less specific.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_state() {
        let state = DeployState::empty();
        assert!(state.entries().is_empty());
    }

    #[test]
    fn test_record_deployment() {
        let mut state = DeployState::empty();
        state.record_deployment("fnm/fish-conf.fish", "abc123");
        let entry = state.get("fnm/fish-conf.fish").unwrap();
        assert_eq!(entry.source_checksum(), "abc123");
        assert_eq!(entry.deployed_checksum(), "abc123");
    }

    #[test]
    fn test_roundtrip_serialization() {
        let mut state = DeployState::empty();
        state.record_deployment("a/b.txt", "hash1");
        let yaml = serde_saphyr::to_string(&state).unwrap();
        let loaded: DeployState = serde_saphyr::from_str(&yaml).unwrap();
        assert_eq!(loaded.entries().len(), 1);
    }

    #[test]
    fn test_detect_drift_no_change() {
        let mut state = DeployState::empty();
        state.record_deployment("a/b.txt", "hash1");
        assert_eq!(
            state.detect_drift("a/b.txt", "hash1", "hash1"),
            DriftType::None
        );
    }

    #[test]
    fn test_detect_drift_repo_changed() {
        let mut state = DeployState::empty();
        state.record_deployment("a/b.txt", "hash1");
        assert_eq!(
            state.detect_drift("a/b.txt", "hash2", "hash1"),
            DriftType::RepoChanged
        );
    }

    #[test]
    fn test_detect_drift_target_changed() {
        let mut state = DeployState::empty();
        state.record_deployment("a/b.txt", "hash1");
        assert_eq!(
            state.detect_drift("a/b.txt", "hash1", "hash_different"),
            DriftType::TargetChanged
        );
    }

    #[test]
    fn test_detect_drift_both_changed() {
        let mut state = DeployState::empty();
        state.record_deployment("a/b.txt", "hash1");
        assert_eq!(
            state.detect_drift("a/b.txt", "hash2", "hash3"),
            DriftType::BothChanged
        );
    }

    #[test]
    fn test_detect_drift_not_tracked() {
        let state = DeployState::empty();
        assert_eq!(
            state.detect_drift("unknown.txt", "hash1", "hash2"),
            DriftType::NotTracked
        );
    }
}
