//! V32 Phase G (locked decision 16) — the **no-raw-reads tripwire**.
//!
//! # The invariant
//!
//! Every V32 enforcement site resolves its switch through
//! [`settings::injection`](crate::settings::injection). **No enforcement site
//! reads a raw settings field.** That is what makes the three-level hierarchy a
//! hierarchy rather than a suggestion: a call site that reads
//! `offload.detection_signature_enabled` directly answers a different question
//! from `effective(Feature::Detection, scope, s)` — it silently ignores L1 and
//! L3 — and the master switch becomes a master switch of everything except the
//! newest thing.
//!
//! # Why a source scan and not a type
//!
//! The same reasoning as the Phase D channel-content tripwire
//! ([`crate::push_tripwire`]), and one extra reason specific to this invariant.
//!
//! The obvious type-level fix is to make the raw fields private and expose only
//! the resolver. It does not work here: [`Settings`](crate::settings::Settings)
//! is one serde-derived struct that the Settings window round-trips as a whole,
//! so every field on it must stay `pub` to be deserialized, edited by the UI and
//! written back. Privacy would have to be bought with a parallel
//! "settings-as-stored vs settings-as-read" type pair — a large change whose own
//! seams would need guarding.
//!
//! What a scan buys instead is the property that actually matters: **a new
//! reader cannot appear without a human deciding it may.** Adding a call site
//! fails the build until someone either routes it through the resolver or
//! records, in [`RAW_FIELDS`] below, why this particular read is not an
//! enforcement decision.
//!
//! # What the scan checks
//!
//! For each guarded field name, every occurrence in `src/**.rs` that is
//! - not inside a comment, and
//! - not inside a `#[cfg(test)]` item,
//!
//! must be in a file the field's [`RawField::allowed`] list names. Test code is
//! exempt because a test that *sets* `native_web_visibility` to exercise a mode
//! is not an enforcement site — it is the thing that proves one behaves.
//!
//! The scan guards itself the same way its sibling does: it fails if a guarded
//! field has vanished from the tree entirely (a rename that silently stopped the
//! watch is indistinguishable from a green suite otherwise) and if an
//! `allowed` entry stops matching anything.

use crate::push_tripwire::{in_comment, in_test_code, source_files, test_spans};

/// One guarded settings field: the identifier to search for, the files allowed
/// to contain it outside comments and tests, and the human note recording why
/// each exception is not an enforcement decision.
struct RawField {
    /// The literal to search for. Bare field names, which are unique in this
    /// tree — the resolved verdicts they feed are deliberately named
    /// differently (`AgentConfig::latch_active` / `canary_active`,
    /// `GatePolicy::latch`) so a scan for the STORED name cannot be satisfied
    /// by a read of the RESOLVED one. L1 is qualified because `protection`
    /// alone is far too common a word.
    needle: &'static str,
    allowed: &'static [&'static str],
    note: &'static str,
}

/// The declaration site. `settings/schema.rs` defines the fields, documents
/// them and prints them in the hand-rolled `Debug` impl; none of that is a
/// decision.
const SCHEMA: &str = "settings/schema.rs";
/// The resolver. The one module allowed to turn a stored value into an answer.
const RESOLVER: &str = "settings/injection.rs";

/// Every raw switch behind the V32 enable hierarchy, with its reviewed readers.
///
/// Adding a file here is a deliberate act: state why the read is *not* an
/// enforcement decision. If it is one, route it through
/// `settings::injection::effective` instead — that is the whole point.
const RAW_FIELDS: &[RawField] = &[
    // ── L1 ────────────────────────────────────────────────────────────────
    RawField {
        needle: "injection.protection",
        allowed: &[SCHEMA, RESOLVER],
        note: "The global master. Read by `decide` (which short-circuits on it), by \
               `master_enabled` for the surfaces that must show it as a switch, and by \
               `spawn_sig` so a master flip moves the restart signature even on an install with \
               no AI tabs.",
    },
    // ── L2, one per feature ───────────────────────────────────────────────
    RawField {
        needle: "taint_latch_enabled",
        allowed: &[SCHEMA, RESOLVER],
        note: "L2 for the taint latch. Enforcement reads `effective(Feature::TaintLatch, …)` — \
               the worker via `AgentConfig::latch_active`, the proxy via `GatePolicy::latch`.",
    },
    RawField {
        needle: "spotlighting_enabled",
        allowed: &[SCHEMA, RESOLVER],
        note: "L2 for the envelope. Enforcement reads it through `ResultCtx::spotlight` \
               (external results) and `CallGuards::spotlight_recall` (recalled memory).",
    },
    RawField {
        needle: "detection_enabled",
        allowed: &[SCHEMA, RESOLVER],
        note: "L2 parent of the two per-layer sub-toggles. Enforcement reads \
               `injection::detection_config`, which is also where the parent wins over them.",
    },
    RawField {
        needle: "ssrf_guard_enabled",
        allowed: &[SCHEMA, RESOLVER],
        note: "L2 for the SSRF screen. Enforcement reads it through `outbound::Policy::enabled`, \
               set once in `Policy::from_settings`.",
    },
    RawField {
        needle: "fetch_budgets_enabled",
        allowed: &[SCHEMA, RESOLVER],
        note: "L2 for the fetch budgets. Enforcement reads `injection::budget_limits`, which \
               returns 0/0 — the existing no-cap spelling — when it is off.",
    },
    RawField {
        needle: "canary_enabled",
        allowed: &[SCHEMA, RESOLVER],
        note: "L2 for the worker canary. The run-scoped verdict it feeds is deliberately named \
               `AgentConfig::canary_active`, so this scan cannot be satisfied by a read of the \
               resolved value.",
    },
    RawField {
        needle: "memory_quarantine_enabled",
        allowed: &[SCHEMA, RESOLVER],
        note: "L2 for memory quarantine. Enforcement reads it through `GatePolicy::quarantine`.",
    },
    RawField {
        needle: "consumer_hygiene_enabled",
        allowed: &[SCHEMA, RESOLVER],
        note: "L2 for the pinned OpenCode permissions + the guidance addendum. Enforcement reads \
               `tabs::config::consumer_hygiene_for`, a thin wrapper over `effective`; the \
               resolver also reads it in `spawn_sig` (spawn-baked ⇒ restart hint).",
    },
    RawField {
        needle: "terminal_escape_hygiene_enabled",
        allowed: &[SCHEMA, RESOLVER],
        note: "L2 for the escape stripper. App-wide (no L3 row); enforcement is \
               `OobContext::speak`, which calls `effective` at `Scope::App`.",
    },
    // ── Features whose L2 is not a boolean of its own ──────────────────────
    RawField {
        needle: "native_web_visibility",
        allowed: &[SCHEMA, RESOLVER],
        note: "The tri-mode IS the native-web feature's L2 (Phase G's reconciliation), so it is \
               guarded like one. `NativeWebMode::parse` and `native_web_mode` live in the \
               resolver for exactly that reason; `tabs::config` re-exports the type and calls \
               `native_web_for`, never the field.",
    },
    // ── Tuning knobs UNDER a feature switch ───────────────────────────────
    RawField {
        needle: "detection_signature_enabled",
        allowed: &[SCHEMA, RESOLVER],
        note: "Sub-toggle under `Feature::Detection`. Composed with the parent in \
               `injection::detection_config` — the only place that may, or the parent could be \
               honoured at one boundary and forgotten at the other.",
    },
    RawField {
        needle: "detection_classifier_enabled",
        allowed: &[SCHEMA, RESOLVER],
        note: "See `detection_signature_enabled`.",
    },
    RawField {
        needle: "detection_classifier_threshold",
        allowed: &[SCHEMA, RESOLVER],
        note: "Carried on the resolved `detection::Config` even when the layers are off, so the \
               Settings UI keeps showing the user's number rather than a zero.",
    },
    RawField {
        needle: "external_fetch_max_calls",
        allowed: &[SCHEMA, RESOLVER],
        note: "Tuning knob under `Feature::FetchBudgets`. Every consumer (the proxy, the worker, \
               the self-test, the app-down fallback) goes through `injection::budget_limits`.",
    },
    RawField {
        needle: "external_fetch_max_bytes",
        allowed: &[SCHEMA, RESOLVER],
        note: "See `external_fetch_max_calls`.",
    },
];

/// This file's own path: it names every guarded field in prose and in the
/// search literals above, none of which is a read.
const SELF: &str = "injection_tripwire.rs";

#[test]
fn no_enforcement_site_reads_a_raw_injection_switch() {
    let files = source_files();
    for field in RAW_FIELDS {
        let mut seen_in: Vec<&str> = Vec::new();
        let mut offenders: Vec<String> = Vec::new();
        for (rel, text) in &files {
            if rel == SELF {
                continue;
            }
            let spans = test_spans(text);
            let mut from = 0usize;
            while let Some(hit) = text[from..].find(field.needle) {
                let at = from + hit;
                from = at + field.needle.len();
                if in_comment(text, at) || in_test_code(&spans, at) {
                    continue;
                }
                if field.allowed.contains(&rel.as_str()) {
                    if !seen_in.contains(field.allowed.iter().find(|a| *a == rel).unwrap()) {
                        seen_in.push(field.allowed.iter().find(|a| *a == rel).unwrap());
                    }
                    continue;
                }
                let line = text[..at].chars().filter(|c| *c == '\n').count() + 1;
                offenders.push(format!("{rel}:{line}"));
            }
        }
        assert!(
            offenders.is_empty(),
            "V32 NO-RAW-READS INVARIANT (locked decision 16) — `{}` is read outside the \
             resolver at: {}\n\n\
             Every V32 enforcement site must call \
             `settings::injection::effective(feature, scope, settings)` (or one of its \
             resolved-value helpers: `budget_limits`, `detection_config`, `native_web_mode`). A \
             raw read answers a different question — it ignores the global master (L1) and the \
             per-scope override (L3) — so the switch the user flipped does not reach it.\n\n\
             If this read genuinely is NOT an enforcement decision, add the file to that field's \
             `allowed` list in `src/injection_tripwire.rs` with a note saying why.\n\n\
             Reviewed readers of `{}`: {}\n  ({})",
            field.needle,
            offenders.join(", "),
            field.needle,
            field.allowed.join(", "),
            field.note,
        );
        // Self-guard: a field that no longer appears anywhere has been renamed
        // or deleted, and the scan has quietly stopped watching it.
        assert!(
            !seen_in.is_empty(),
            "`{}` no longer appears in any reviewed file — if it was renamed, update \
             `RAW_FIELDS`; if it was deleted, remove the entry. A scan that watches nothing \
             passes for the wrong reason.",
            field.needle
        );
    }
}

/// The other half of the invariant: every feature the hierarchy knows about has
/// its L2 storage guarded above. A control added to
/// [`Feature`](crate::settings::injection::Feature) without a guarded field
/// would be resolvable but unprotected against a future raw reader — which is
/// exactly the drift this file exists to prevent, one level up.
#[test]
fn every_feature_has_a_guarded_l2_field() {
    use crate::settings::injection::Feature;
    for f in Feature::ALL {
        // Native-web's L2 is the tri-mode string, not a `<feature>_enabled`
        // flag — the Phase G reconciliation. Guarded under its own name.
        let needle = if *f == Feature::NativeWeb {
            "native_web_visibility".to_string()
        } else {
            format!("{}_enabled", f.key())
        };
        assert!(
            RAW_FIELDS.iter().any(|r| r.needle == needle),
            "`{:?}` has no guarded L2 field (`{needle}`) in RAW_FIELDS — add one, or the \
             resolver stops being the only way to read it.",
            f
        );
    }
}
