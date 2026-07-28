//! Named-value substitution for dotfile templates.
//!
//! Substitution is by name only: there are no conditionals, loops, includes, or
//! expressions. This is a direct scan rather than an embedded template engine, so
//! the restriction holds by construction — there is no parser in which control
//! flow could be written, and no engine features (file includes, environment
//! access) reachable by default. See ADR-0004.

use std::collections::{BTreeMap, BTreeSet};

/// Whether `name` is a legal variable name: `[A-Za-z_][A-Za-z0-9_]*`.
///
/// Shared with validation, which rejects illegal names before apply.
pub(crate) fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Find the byte offset of `needle` in `haystack`, if present.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Every well-formed placeholder name appearing in `template`.
///
/// Text that looks like a placeholder but does not contain a legal name is
/// ignored, so files that legitimately use brace syntax do not produce spurious
/// names.
pub(crate) fn placeholders(template: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    // The rendered output is discarded; only the names matter here. Copying a
    // template-sized input is cheap enough not to warrant a second scanner.
    scan(template, |name| {
        found.insert(name.to_string());
        None
    });
    found
}

/// Render `template`, replacing `{{ name }}` for each name present in `bindings`.
///
/// Placeholders whose name is absent from `bindings` are copied verbatim, so no
/// escape syntax is needed for files that contain brace syntax of their own.
/// Substituted bytes are never rescanned.
///
/// Returns bytes rather than a string: a bound value is not guaranteed to be
/// UTF-8, and lossily decoding one would corrupt it.
// Exercised by this module's tests; its production consumer is apply-time content
// resolution, which lands next.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn render(template: &str, bindings: &BTreeMap<String, Vec<u8>>) -> Vec<u8> {
    scan(template, |name| bindings.get(name).map(Vec::as_slice))
}

/// Walk `template`, offering each well-formed placeholder name to `lookup`.
///
/// When `lookup` returns bytes they replace the placeholder; when it returns
/// `None` the placeholder is copied verbatim.
///
/// A `{{` with no closing `}}` causes the remainder to be searched each time one
/// is seen, which is quadratic in the worst case. Templates are small and this
/// keeps the scanner to one pass of straightforward code.
fn scan<'a, F>(template: &str, mut lookup: F) -> Vec<u8>
where
    F: FnMut(&str) -> Option<&'a [u8]>,
{
    let bytes = template.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i..].starts_with(b"{{")
            && let Some(offset) = find(&bytes[i + 2..], b"}}")
            && let Ok(inner) = std::str::from_utf8(&bytes[i + 2..i + 2 + offset])
        {
            let name = inner.trim();
            if is_valid_name(name)
                && let Some(value) = lookup(name)
            {
                out.extend_from_slice(value);
                // Advance past the closing braces: substituted bytes are not
                // rescanned, so a value containing `{{ … }}` cannot expand.
                i += 2 + offset + 2;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bindings(pairs: &[(&str, &[u8])]) -> BTreeMap<String, Vec<u8>> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.to_vec()))
            .collect()
    }

    #[test]
    fn substitutes_a_declared_name() {
        let out = render("key: {{ api_key }}\n", &bindings(&[("api_key", b"tok123")]));
        assert_eq!(out, b"key: tok123\n");
    }

    #[test]
    fn accepts_placeholders_without_inner_whitespace() {
        let out = render("key: {{api_key}}\n", &bindings(&[("api_key", b"tok123")]));
        assert_eq!(out, b"key: tok123\n");
    }

    #[test]
    fn leaves_undeclared_placeholders_verbatim() {
        let out = render(
            "a: {{ other }}\nb: {{ api_key }}\n",
            &bindings(&[("api_key", b"t")]),
        );
        assert_eq!(out, b"a: {{ other }}\nb: t\n");
    }

    #[test]
    fn substitution_is_single_pass() {
        // A value containing a placeholder must not be rescanned.
        let out = render(
            "{{ a }}",
            &bindings(&[("a", b"{{ b }}"), ("b", b"LEAKED-BY-RESCAN")]),
        );
        assert_eq!(out, b"{{ b }}");
    }

    #[test]
    fn preserves_non_utf8_values() {
        let out = render("k: {{ v }}", &bindings(&[("v", &[0xff, 0xfe])]));
        assert_eq!(out, b"k: \xff\xfe");
    }

    #[test]
    fn collects_placeholder_names() {
        let found = placeholders("{{ a }} {{b}} {{ not-a-name }} {{ a }}");
        assert_eq!(found.into_iter().collect::<Vec<_>>(), vec!["a", "b"]);
    }

    #[test]
    fn an_unclosed_placeholder_is_copied_verbatim() {
        let out = render("key: {{ api_key\nrest", &bindings(&[("api_key", b"tok")]));
        assert_eq!(out, b"key: {{ api_key\nrest");
        assert!(placeholders("key: {{ api_key\nrest").is_empty());
    }

    #[test]
    fn an_empty_placeholder_name_is_not_a_placeholder() {
        let out = render("a {{}} b {{ }} c", &bindings(&[("", b"X")]));
        assert_eq!(out, b"a {{}} b {{ }} c");
        assert!(placeholders("a {{}} b {{ }} c").is_empty());
    }

    #[test]
    fn a_name_starting_with_a_digit_is_not_a_placeholder() {
        assert!(!is_valid_name("1st"));
        let out = render("{{ 1st }}", &bindings(&[("1st", b"X")]));
        assert_eq!(out, b"{{ 1st }}");
    }

    #[test]
    fn a_value_may_be_empty_without_disturbing_the_surrounding_text() {
        let out = render("a{{ v }}b", &bindings(&[("v", b"")]));
        assert_eq!(out, b"ab");
    }
}
