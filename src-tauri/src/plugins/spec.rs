//! V38 Phase G — the doc⇄code drift tests for
//! [`docs/TOOL-PLUGINS.md`](../../../../docs/TOOL-PLUGINS.md).
//!
//! The document is the contract a plugin author reads: which characters are
//! refused, which environment variables a child sees, which paths are never
//! granted, what the caps are. Every one of those claims is a constant
//! somewhere in this crate, and a document that stops describing the build is
//! worse than no document — an author would write against it and be surprised
//! at run time.
//!
//! So this module is the same `include_str!` two-sources-of-truth idiom
//! `harness::chp::tests::the_doc_states_this_version` uses, widened: the doc
//! marks each machine-checked list with an HTML anchor, and each test below
//! parses one anchor and compares it to the table it describes. **The two move
//! in one commit or neither.**
//!
//! The parse is deliberately dumb and newline-agnostic (a Windows checkout is
//! CRLF and CI's is too, so a `\r` must never be part of a compared value), and
//! failures name the anchor plus both sides of the difference — a drift test
//! that only says "not equal" costs more than it saves.

use std::collections::BTreeSet;

use crate::plugins::manifest::{
    self, LegacyAuditParser, Provenance, RuntimeReq, SandboxReq, ToolKind, Transport,
};

/// The document, at compile time. Path is relative to this file
/// (`src-tauri/src/plugins/`), up to the repo root.
const DOC: &str = include_str!("../../../docs/TOOL-PLUGINS.md");

/// The values of one `<!-- drift:NAME -->` block: the fenced block that follows
/// the anchor, minus `#` comment lines and blanks, split on whitespace.
///
/// Whitespace-splitting is what lets a block be one line (`& | ; …`) or one
/// value per line (the env table) without two parsers. `\r` is stripped first,
/// so the same file reads identically on a CRLF and an LF checkout.
fn block(anchor: &str) -> Vec<String> {
    let needle = format!("<!-- drift:{anchor} -->");
    let start = DOC
        .find(&needle)
        .unwrap_or_else(|| panic!("docs/TOOL-PLUGINS.md has no `{needle}` anchor"));
    let after = &DOC[start + needle.len()..];
    let open = after
        .find("```")
        .unwrap_or_else(|| panic!("`{needle}` is not followed by a fenced block"));
    // Past the fence's own line.
    let body_start = after[open..]
        .find('\n')
        .map(|nl| open + nl + 1)
        .unwrap_or_else(|| panic!("`{needle}`'s fenced block never opens"));
    let body_len = after[body_start..]
        .find("```")
        .unwrap_or_else(|| panic!("`{needle}`'s fenced block is never closed"));
    after[body_start..body_start + body_len]
        .lines()
        .map(|l| l.trim_end_matches('\r').trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .flat_map(|l| l.split_whitespace())
        .map(str::to_string)
        .collect()
}

/// The block as a set, with duplicates refused — a value listed twice makes the
/// comparison below ambiguous about which side is short.
fn block_set(anchor: &str) -> BTreeSet<String> {
    let values = block(anchor);
    let set: BTreeSet<String> = values.iter().cloned().collect();
    assert_eq!(
        set.len(),
        values.len(),
        "docs/TOOL-PLUGINS.md's `{anchor}` block lists a value twice"
    );
    set
}

/// Compare a documented set against a code-derived one, naming both directions.
fn same_set(anchor: &str, code: &BTreeSet<String>, fix: &str) {
    let doc = block_set(anchor);
    let missing: Vec<&String> = code.difference(&doc).collect();
    let extra: Vec<&String> = doc.difference(code).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "docs/TOOL-PLUGINS.md's `{anchor}` block and the code disagree.\n  \
         in the code but not in the doc: {missing:?}\n  \
         in the doc but not in the code: {extra:?}\n\
         Fix BOTH in one commit — {fix}"
    );
}

/// A `key = value` block, as pairs. Values are compared as strings so a cap and
/// a count are pinned the same way.
fn kv(anchor: &str) -> Vec<(String, String)> {
    let needle = format!("<!-- drift:{anchor} -->");
    let start = DOC.find(&needle).expect("anchor");
    let after = &DOC[start..];
    let open = after.find("```").expect("fence");
    let body_start = after[open..].find('\n').map(|nl| open + nl + 1).expect("fence");
    let body_len = after[body_start..].find("```").expect("close");
    after[body_start..body_start + body_len]
        .lines()
        .map(|l| l.trim_end_matches('\r').trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            let (k, v) = l
                .split_once('=')
                .unwrap_or_else(|| panic!("`{anchor}` line is not `key = value`: {l}"));
            (k.trim().to_string(), v.trim().to_string())
        })
        .collect()
}

/// The wire name serde gives a value — derived, never a second hand-kept list,
/// so a rename in the enum cannot leave this test agreeing with a stale doc.
fn wire<T: serde::Serialize>(v: T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|j| j.as_str().map(str::to_string))
        .expect("these enums all serialize as strings")
}

/// The manifest version the doc describes is the one this build accepts.
#[test]
fn the_doc_states_the_manifest_version_this_build_validates() {
    let needle = format!(
        "**Manifest version:** `manifest_version = {}`",
        manifest::MANIFEST_VERSION
    );
    assert!(
        DOC.contains(&needle),
        "docs/TOOL-PLUGINS.md must state the manifest version it describes, exactly as \
         `{needle}` — MANIFEST_VERSION is {} and the doc does not say so",
        manifest::MANIFEST_VERSION
    );
    for other in 0..(manifest::MANIFEST_VERSION + 3) {
        if other == manifest::MANIFEST_VERSION {
            continue;
        }
        assert!(
            !DOC.contains(&format!("**Manifest version:** `manifest_version = {other}`")),
            "docs/TOOL-PLUGINS.md states two manifest versions"
        );
    }
}

/// The three closed vocabularies an author types into a manifest, against the
/// wire names serde actually accepts.
#[test]
fn the_documented_enums_are_the_wire_names() {
    let kinds: BTreeSet<String> = [
        ToolKind::Audit,
        ToolKind::Security,
        ToolKind::Check,
        ToolKind::Command,
    ]
    .iter()
    .map(|k| wire(*k))
    .collect();
    same_set("kinds", &kinds, "`ToolKind`'s serde names are the authority");

    let postures: BTreeSet<String> = [
        SandboxReq::Required,
        SandboxReq::Optional,
        SandboxReq::Unsupported,
    ]
    .iter()
    .map(|s| wire(*s))
    .collect();
    same_set("sandbox", &postures, "`SandboxReq`'s serde names are the authority");

    let transports: BTreeSet<String> = [Transport::Stdout, Transport::ReportFile]
        .iter()
        .map(|t| wire(*t))
        .collect();
    same_set(
        "transports",
        &transports,
        "`Transport`'s serde names are the authority",
    );

    // The `as_str` spellings the rows use must match the wire names too —
    // otherwise the doc describes one vocabulary and the Events lane another.
    assert_eq!(wire(ToolKind::Audit), ToolKind::Audit.as_str());
    assert_eq!(wire(SandboxReq::Optional), SandboxReq::Optional.as_str());
}

/// Every documented `runtime` value is a real one, and every value that names a
/// PROFILE names one the sandbox actually owns — which is the property that
/// keeps the field a request against a cImp-owned table rather than a
/// grant-widening primitive.
#[test]
fn the_documented_runtimes_are_real_profiles() {
    let all = [
        RuntimeReq::None,
        RuntimeReq::Python,
        RuntimeReq::Node,
        RuntimeReq::Java,
        RuntimeReq::Dotnet,
        RuntimeReq::Go,
        RuntimeReq::Rust,
        RuntimeReq::Auto,
    ];
    let code: BTreeSet<String> = all.iter().map(|r| wire(*r)).collect();
    same_set("runtime", &code, "`RuntimeReq`'s serde names are the authority");

    let profiles: BTreeSet<&str> = crate::sandbox::RUNTIME_PROFILES
        .iter()
        .map(|p| p.id)
        .collect();
    for req in all {
        if matches!(req, RuntimeReq::None | RuntimeReq::Auto) {
            continue;
        }
        assert!(
            profiles.contains(req.as_str()),
            "the doc offers `runtime: {}` but `sandbox::RUNTIME_PROFILES` has no such profile — \
             a declared runtime that names nothing would silently grant nothing",
            req.as_str()
        );
    }
    // The wire name and the row spelling are the same string, so a manifest
    // value and the grant row that mentions it read alike.
    for req in all {
        assert_eq!(wire(req), req.as_str());
    }
}

/// The findings parsers a user plugin may NOT select. Documented as
/// builtin-only, and enforced — not merely described.
#[test]
fn the_legacy_findings_parsers_are_documented_and_refused() {
    let code: BTreeSet<String> = [
        LegacyAuditParser::TyposJsonl,
        LegacyAuditParser::KnipJson,
        LegacyAuditParser::MacheteText,
    ]
    .iter()
    .map(|p| p.as_str().to_string())
    .collect();
    same_set(
        "legacy-parsers",
        &code,
        "`LegacyAuditParser::WIRE` is the authority",
    );

    for name in block("legacy-parsers") {
        let text = audit_manifest_with(&format!(r#""parser": "{name}""#));
        let err = manifest::parse(&text, Provenance::User)
            .expect_err("a user plugin may not select a legacy findings parser");
        assert!(
            matches!(err.error, manifest::ValidationError::ParserNotSarif { .. }),
            "`{name}` was not refused as a non-SARIF parser: {err}"
        );
    }
}

/// `builtin` and `ingest` are stamped or reserved by cImp, never claimed by a
/// scanned file. `ingest` has no implementation yet (it arrives with the
/// embedded built-ins); documenting it as reserved is only honest if a file
/// carrying it is refused TODAY, which `deny_unknown_fields` sees to.
#[test]
fn the_reserved_fields_are_refused_in_a_user_manifest() {
    for field in block("reserved-fields") {
        let text = audit_manifest_with(&format!(r#""{field}": "anything""#));
        assert!(
            manifest::parse(&text, Provenance::User).is_err(),
            "`{field}` must not load from a scanned manifest — it is stamped or reserved by              cImp, never claimed by a file"
        );
    }
}

/// The caps the doc states are the caps the code applies.
#[test]
fn the_documented_caps_are_the_constants() {
    let doc = kv("caps");
    let code: Vec<(&str, String)> = vec![
        ("manifest_version", manifest::MANIFEST_VERSION.to_string()),
        ("manifest_max_bytes", manifest::MAX_MANIFEST_BYTES.to_string()),
        ("identity_max_chars", manifest::MAX_NAME_CHARS.to_string()),
        // The lower bound is not a constant — it is the shape of the check
        // (`secs == 0` is refused), so 1 is the smallest accepted value and the
        // assertion below proves it rather than restating it.
        ("timeout_min_secs", "1".to_string()),
        ("timeout_max_secs", manifest::MAX_TIMEOUT_SECS.to_string()),
        (
            "check_timeout_floor_secs",
            crate::checks::MIN_TIMEOUT_SECS.to_string(),
        ),
        (
            "check_timeout_default_secs",
            crate::checks::CheckDef::default().timeout_secs.to_string(),
        ),
        (
            "audit_timeout_default_secs",
            crate::settings::CodeAuditSettings::default()
                .timeout_secs
                .to_string(),
        ),
        (
            "audit_output_max_bytes",
            crate::audit::runner::MAX_OUTPUT_BYTES.to_string(),
        ),
        (
            "audit_findings_report_cap",
            crate::audit::mcp::MAX_FINDINGS.to_string(),
        ),
        (
            "audit_report_max_bytes",
            crate::audit::mcp::MAX_RESULT_BYTES.to_string(),
        ),
        (
            "audit_event_findings_per_tool",
            crate::audit::runner::EVENT_FINDINGS_PER_TOOL_CAP.to_string(),
        ),
        (
            "run_command_timeout_secs",
            crate::offload::tools::run_command::TIMEOUT
                .as_secs()
                .to_string(),
        ),
        (
            "run_command_output_max_bytes",
            crate::offload::tools::run_command::MAX_OUTPUT_BYTES.to_string(),
        ),
    ];

    let doc_keys: BTreeSet<&str> = doc.iter().map(|(k, _)| k.as_str()).collect();
    let code_keys: BTreeSet<&str> = code.iter().map(|(k, _)| *k).collect();
    assert_eq!(
        doc_keys, code_keys,
        "docs/TOOL-PLUGINS.md's `caps` block and this test name different caps"
    );
    for (key, expected) in &code {
        let stated = doc
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .expect("key sets already compared equal");
        assert_eq!(
            stated, expected,
            "docs/TOOL-PLUGINS.md says `{key} = {stated}`; the code says {expected}"
        );
    }

    // The two ends of the documented timeout range, exercised rather than
    // asserted from a constant: the doc promises 1..=MAX is accepted and that
    // both 0 and MAX+1 are refused.
    for (secs, ok) in [
        (0u64, false),
        (1, true),
        (manifest::MAX_TIMEOUT_SECS, true),
        (manifest::MAX_TIMEOUT_SECS + 1, false),
    ] {
        let text = audit_manifest_with(&format!(r#""timeout_secs": {secs}"#));
        let got = manifest::parse(&text, Provenance::User);
        assert_eq!(
            got.is_ok(),
            ok,
            "`timeout_secs: {secs}` should {} — the doc states 1..={}",
            if ok { "load" } else { "be refused" },
            manifest::MAX_TIMEOUT_SECS
        );
    }
}

/// The shell-injection screen: the refused set, exactly.
#[test]
fn the_documented_shell_screen_is_the_code_screen() {
    let code: BTreeSet<String> = crate::checks::plugin::SHELL_UNSAFE
        .iter()
        .map(|c| c.to_string())
        .collect();
    same_set(
        "shell-unsafe",
        &code,
        "`checks::plugin::SHELL_UNSAFE` is the authority; every refusal has to be a character a \
         shell would read as syntax",
    );

    // The deliberate NON-refusals. These are the ones a reviewer is most likely
    // to "fix" by adding, so the doc's reasoning is pinned as a negative claim.
    //
    // Counted first: a negative claim over an empty list passes for the wrong
    // reason, and this block's values are single characters, one of which is the
    // format's own comment marker.
    let allowed_chars: Vec<String> = block("shell-allowed");
    assert!(
        allowed_chars.len() >= 6,
        "docs/TOOL-PLUGINS.md's `shell-allowed` block parsed as {allowed_chars:?} — a \
         deliberate-non-refusal list that parses to (almost) nothing asserts nothing"
    );
    for allowed in allowed_chars {
        for ch in allowed.chars() {
            assert!(
                !crate::checks::plugin::SHELL_UNSAFE.contains(&ch),
                "docs/TOOL-PLUGINS.md documents `{ch}` as deliberately NOT refused, but \
                 SHELL_UNSAFE now refuses it. If the refusal is right, the doc's reasoning \
                 (globs and braces are legitimate; `#` can only truncate) has to be revisited \
                 rather than deleted."
            );
        }
    }
}

/// The credential stores no grant may name.
#[test]
fn the_documented_grant_refusals_are_the_rules() {
    let code: BTreeSet<String> = crate::sandbox::GRANT_REFUSAL_RULES
        .iter()
        .map(|r| r.suffix.join("/"))
        .collect();
    same_set(
        "grant-refusals",
        &code,
        "`sandbox::GRANT_REFUSAL_RULES` is the authority; the doc lists each rule's trailing \
         components joined with `/`",
    );
    // A rule with no reason does not compile, and a reason nobody can read is
    // not a reason: every row must carry non-empty prose, since it is what the
    // user's refusal row says.
    for rule in crate::sandbox::GRANT_REFUSAL_RULES {
        assert!(!rule.why.trim().is_empty(), "a refusal rule has no reason");
        assert!(!rule.suffix.is_empty(), "a refusal rule matches everything");
    }
}

/// The environment ceiling a sandboxed child sees.
#[test]
fn the_documented_environment_is_the_child_env_table() {
    let code: BTreeSet<String> = crate::sandbox::child_env::CHILD_ENV
        .iter()
        .map(|g| g.name.to_string())
        .collect();
    same_set(
        "child-env",
        &code,
        "`sandbox::child_env::CHILD_ENV` is the authority — a name added there is a name a \
         plugin's child can now read, which is exactly what this document exists to state",
    );
}

/// The identity charsets, exercised rather than described.
///
/// § 2.1 states two character classes in prose, and prose is exactly what
/// drifts: `valid_id` and `valid_version` are four lines each, and either could
/// be widened by a well-meaning fix without anyone re-reading the document that
/// promises what a plugin author may type. So every row of the block is a real
/// value fed to the real validator, and the verdict it claims is asserted.
#[test]
fn the_documented_identity_charsets_are_the_validator() {
    let mut names = 0usize;
    let mut versions = 0usize;
    for row in block("identity-charset") {
        let mut parts = row.splitn(3, ':');
        let (field, verdict, value) = (
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
        );
        // `<empty>` is the one value a whitespace-split block cannot spell.
        let value = if value == "<empty>" { "" } else { value };
        let accept = match verdict {
            "accept" => true,
            "refuse" => false,
            other => panic!(
                "docs/TOOL-PLUGINS.md's `identity-charset` row `{row}` has verdict `{other}` \
                 (expected `accept` or `refuse`)"
            ),
        };
        let text = match field {
            "name" => {
                names += 1;
                identity_manifest(value, "1.0.0")
            }
            "version" => {
                versions += 1;
                identity_manifest("acme", value)
            }
            other => panic!(
                "docs/TOOL-PLUGINS.md's `identity-charset` row `{row}` names field `{other}` \
                 (expected `name` or `version`)"
            ),
        };
        let got = manifest::parse(&text, Provenance::User);
        assert_eq!(
            got.is_ok(),
            accept,
            "the doc says `{field}` `{value}` should {}; the validator disagreed ({:?})",
            if accept { "load" } else { "be refused" },
            got.as_ref().err().map(|e| e.error.clone())
        );
        // A refusal has to be about IDENTITY, not about some other rule the
        // sample happened to trip — otherwise the row would pass for the wrong
        // reason and the charset would be free to drift underneath it.
        if let Err(e) = got {
            assert!(
                matches!(e.error, manifest::ValidationError::Identity(_)),
                "`{field}` `{value}` was refused, but not as an identity error: {e}"
            );
        }
    }
    // A block that lost its rows would pass vacuously.
    assert!(
        names >= 4 && versions >= 4,
        "docs/TOOL-PLUGINS.md's `identity-charset` block parsed as {names} name rows and \
         {versions} version rows — a charset claim needs samples on both sides of both fields"
    );
}

/// A minimal, valid manifest with a caller-chosen identity, so the charset test
/// goes through the real parse path rather than calling the two predicates
/// directly (which would prove they exist, not that anything consults them).
fn identity_manifest(name: &str, version: &str) -> String {
    format!(
        r#"{{
          "manifest_version": 1,
          "name": "{name}",
          "version": "{version}",
          "categories": [{{ "id": "c", "label": "C", "tools": ["t"] }}],
          "tools": [{{ "id": "t", "label": "T", "kind": "command" }}]
        }}"#
    )
}

/// A minimal, valid audit-kind manifest with one extra field spliced in, so the
/// tests above exercise the real parse path (`deny_unknown_fields` included)
/// rather than a hand-built struct that can never carry an unknown key.
fn audit_manifest_with(extra: &str) -> String {
    format!(
        r#"{{
          "manifest_version": 1,
          "name": "acme",
          "version": "1.0.0",
          "categories": [{{ "id": "c", "label": "C", "tools": ["t"] }}],
          "tools": [{{
            "id": "t", "label": "T", "kind": "audit",
            "argv": ["--sarif", "{{root}}"],
            {extra}
          }}]
        }}"#
    )
}
