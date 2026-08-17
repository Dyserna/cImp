//! **`{{key}}` substitution for the generated harness artifacts** (V35 Phase M,
//! design `DESIGN-harness-plugin-architecture.md` § 5.1).
//!
//! The L1 artifacts cImp emits used to be `format!()` strings inside Rust, with
//! every JS brace doubled (`{{`/`}}`). That is hostile to the one thing this
//! milestone exists for — *reading a diff when upstream changes*. Phase M moves
//! the artifact into a real file next to its emitter
//! (`harness/opencode/templates/plugin.js`, `include_str!`ed) and leaves this
//! module holding the two hundred lines that are actually a program: which keys
//! exist, and what fills them.
//!
//! # The contract
//!
//! * A placeholder is exactly `{{key}}`, where `key` matches
//!   `[a-z0-9_.]+`. Anything else is not a placeholder and is copied through.
//! * Substitution is **one left-to-right pass**. A value is never rescanned, so
//!   a tool name or refusal string that happens to contain `{{` cannot inject a
//!   second placeholder.
//! * **Values arrive already escaped for their target syntax.** Every value the
//!   OpenCode plugin substitutes is produced by `serde_json::to_string` — a
//!   whole JS literal, quotes included — which is the discipline the old
//!   `format!()` generator was careful about and the thing § 5.1 names as what
//!   must not regress: *a tool name added later must never be able to malform
//!   the emitted JS*. [`json_lit`] is that call, in one place, with the reason
//!   attached.
//! * An **unknown** `{{key}}` is left in the output **verbatim**, never replaced
//!   by an empty string, and logged at `error!`. A typo must not be able to
//!   quietly emit a plugin whose gate constant is missing; leaving the marker
//!   makes the emitted module fail loudly at parse instead. The real defence is
//!   one layer up — `template_keys` plus the per-template key-set tests fail
//!   `cargo test` before such a file can ever be written.
//!
//! # Why not a templating crate
//!
//! Two artifacts, one syntax, no loops or conditionals — every branch in the
//! generated plugin is a baked `const`, deliberately, so the file a reviewer
//! reads on disk is the file that runs. A dependency here would add a parser to
//! the TCB (design § 5, D7) to save forty lines.

#[cfg(test)]
use std::collections::BTreeSet;

/// Substitute `{{key}}` occurrences from `values`, in one left-to-right pass.
///
/// `values` is a slice rather than a map so the caller's list *is* the
/// documented key set, in a reviewable order, and the key-set tests can compare
/// it against the template directly.
///
/// `declared` is that same set as the caller's published constant. It is used
/// only to make the unknown-key diagnostic actionable — a `{{typo}}` reaching
/// here has already escaped three tests, so the log line has to say what the
/// available slots were rather than merely that one was wrong.
pub(crate) fn render(
    template_name: &str,
    template: &str,
    declared: &[&'static str],
    values: &[(&'static str, String)],
) -> String {
    let mut out = String::with_capacity(template.len() + 2048);
    let bytes = template.as_bytes();
    let mut i = 0usize;
    while i < template.len() {
        if bytes[i] == b'{' && bytes.get(i + 1) == Some(&b'{') {
            if let Some((key, end)) = placeholder_at(template, i) {
                match values.iter().find(|(k, _)| *k == key) {
                    Some((_, v)) => out.push_str(v),
                    None => {
                        // Never an empty string: an absent gate constant is a
                        // silently disarmed control, a `{{typo}}` left in place
                        // is a parse error the harness reports on load.
                        tracing::error!(
                            target: "harness",
                            template = template_name,
                            key,
                            declared = declared.join(", "),
                            "harness template: unknown substitution key left unsubstituted \
                             (the emitted artifact will not parse — this is a build defect, see \
                             harness::render)"
                        );
                        out.push_str(&template[i..end]);
                    }
                }
                i = end;
                continue;
            }
        }
        // Not a placeholder — copy this byte through. Indexing by byte is safe
        // because `{` and `}` are ASCII and can never occur inside a UTF-8
        // continuation byte.
        let ch = template[i..].chars().next().unwrap_or('{');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Every `{{key}}` appearing in a template, deduplicated.
///
/// The input to the drift tests: the template's key set must equal the
/// generator's, in both directions — an unknown key means a typo that would
/// emit a broken artifact, an unused key means a value the generator computes
/// and throws away (global principle 3).
///
/// `#[cfg(test)]` because that comparison is the *only* caller and must stay so:
/// production code renders a template, it never introspects one. The enforcement
/// point is the build, not a runtime check that could be made lenient.
#[cfg(test)]
pub(crate) fn template_keys(template: &str) -> BTreeSet<&str> {
    let mut keys = BTreeSet::new();
    let bytes = template.as_bytes();
    let mut i = 0usize;
    while i < template.len() {
        if bytes[i] == b'{' && bytes.get(i + 1) == Some(&b'{') {
            if let Some((key, end)) = placeholder_at(template, i) {
                keys.insert(key);
                i = end;
                continue;
            }
        }
        i += 1;
    }
    keys
}

/// Parse `{{key}}` starting at `i`; returns the key and the index just past the
/// closing `}}`. `None` when the run of braces is not a well-formed placeholder,
/// which is how a JS object literal (`{{ at: 0 }` never occurs, but `{{` could
/// appear in a comment) stays uninterpreted.
fn placeholder_at(template: &str, i: usize) -> Option<(&str, usize)> {
    let rest = &template[i + 2..];
    let close = rest.find("}}")?;
    let key = &rest[..close];
    if key.is_empty()
        || !key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
    {
        return None;
    }
    Some((key, i + 2 + close + 2))
}

/// A value's JS/JSON literal — quotes, escaping and all.
///
/// **Never hand-quote a substitution value.** The refusal constants contain
/// apostrophes and em dashes, tab ids are one rename away from carrying a quote,
/// and the tool tables are reviewed Rust data that a later contributor extends
/// without ever opening the template. An escaping bug here is a syntax error in
/// a file the harness loads at startup, i.e. a disarmed security control that
/// reports itself as on.
///
/// The fallback is deliberately a valid literal rather than a panic: this runs
/// on the tab-spawn path, and `serde_json` cannot fail on a `&str` anyway.
pub(crate) fn json_lit<T: serde::Serialize + ?Sized>(v: &T, fallback: &str) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| fallback.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vals(pairs: &[(&'static str, &str)]) -> Vec<(&'static str, String)> {
        pairs.iter().map(|(k, v)| (*k, (*v).to_string())).collect()
    }

    #[test]
    fn substitutes_known_keys_and_leaves_js_braces_alone() {
        let t = "const A = {{cimp.token}};\nfn() { return {a: 1}; }\n";
        let out = render("t", t, &["cimp.token"], &vals(&[("cimp.token", "\"abc\"")]));
        assert_eq!(out, "const A = \"abc\";\nfn() { return {a: 1}; }\n");
    }

    /// The rule that makes a typo loud instead of silent: an unknown key is
    /// echoed, never blanked. `{{cimp.tokn}}` in an emitted plugin is a parse
    /// error; an empty string there is a gate constant that reads `undefined`.
    #[test]
    fn an_unknown_key_survives_verbatim_rather_than_emitting_nothing() {
        let out = render("t", "x = {{cimp.tokn}};", &["cimp.token"], &vals(&[("cimp.token", "1")]));
        assert_eq!(out, "x = {{cimp.tokn}};");
    }

    /// One pass, never two: a substituted value that itself contains `{{` is
    /// output, not rescanned. Values come from reviewed Rust today, but the
    /// property is what lets that stay a convenience rather than a trust
    /// requirement.
    #[test]
    fn a_value_containing_a_placeholder_is_not_rescanned() {
        let out = render(
            "t",
            "{{cimp.token}}",
            &["cimp.token"],
            &vals(&[("cimp.token", "\"{{cimp.token}}\"")]),
        );
        assert_eq!(out, "\"{{cimp.token}}\"");
    }

    #[test]
    fn template_keys_finds_each_placeholder_once() {
        let keys = template_keys("{{a.b}} {{c}} {{a.b}} {NOT} {{ spaced }} {{}}");
        assert_eq!(keys.into_iter().collect::<Vec<_>>(), vec!["a.b", "c"]);
    }

    /// Multi-byte characters sit between the placeholders in every real
    /// template (the plugin's comments are full of em dashes), so the copy path
    /// must be char-safe, not byte-safe.
    #[test]
    fn non_ascii_between_placeholders_round_trips() {
        let out = render("t", "— {{k}} — ✓", &["k"], &vals(&[("k", "v")]));
        assert_eq!(out, "— v — ✓");
    }

    #[test]
    fn json_lit_quotes_and_escapes() {
        assert_eq!(json_lit("a\"b", "\"\""), "\"a\\\"b\"");
        assert_eq!(json_lit(&["read", "bash"], "[]"), "[\"read\",\"bash\"]");
    }
}
