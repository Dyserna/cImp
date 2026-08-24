//! V42 Phase E — the settings wire types and their defaults, GENERATED.
//!
//! `src/lib/settings/types.ts` used to carry a hand-written mirror of every
//! struct in [`super::schema`]: ~1,850 lines of TypeScript that had to be
//! edited in lockstep with the Rust, guarded by a scatter of `include_str!`
//! field-name scans. This module replaces the mirror with codegen: one test
//! writes `src/lib/settings/generated/settings.ts` (types) and
//! `.../generated/defaults.json` (`Settings::default()`), both committed, and
//! CI re-runs it and fails on a diff.
//!
//! Design notes worth keeping in view:
//!
//! * **ts-rs is a dev-dependency**, and the derives are `#[cfg_attr(test,
//!   derive(ts_rs::TS))]`, so nothing about the shipped binary changes — the
//!   generator exists only in the test cfg.
//! * **One output file.** Every type carries `#[ts(export_to = "settings.ts")]`
//!   so ts-rs merges all declarations into a single alphabetically-sorted file.
//!   That is what makes `#[ts(type = "…")]` overrides able to name sibling
//!   generated types (`TerminalBackgroundSettings`) without an import.
//! * **`u64`/`usize` are `number`, never `bigint`** — see
//!   [`ts_rs::Config::with_large_int`]. No settings value can exceed 2^53.
//! * **`#[serde(default)]` does not make a TS field optional.** ts-rs only
//!   does that when `skip_serializing_if` is present too, which this schema
//!   never uses: the wire the frontend sees is always fully populated.
//! * The remaining hand-written seams are the `#[cfg_attr(test, ts(type =
//!   …))]` overrides in the `schema/` tree, each commented `HAND-KEPT SEAM` — one per
//!   type whose (de)serialize is hand-written, plus the deliberate handoff of
//!   the layout tree to the frontend's own `LayoutNode` union.

use std::path::PathBuf;

use ts_rs::{Config, TS};

use super::{LocalProviderBlock, Settings};

/// `<repo>/src/lib/settings/generated`, resolved from the crate manifest so the
/// test does not depend on the working directory `cargo test` was launched in.
fn generated_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("src")
        .join("lib")
        .join("settings")
        .join("generated")
}

/// Writes `bytes` only if they differ, so a green run leaves mtimes alone.
///
/// LF endings are written explicitly and pinned by a `.gitattributes` rule for
/// this directory: the committed bytes have to be what a Linux CI runner
/// regenerates, and an `autocrlf=true` Windows checkout that rewrote them would
/// make the CI diff-check fail for a reason that has nothing to do with drift.
///
/// One caller: `defaults.json`, which this module composes in memory. The
/// second caller was `settings.ts` — dead, since ts-rs writes that file
/// itself; see the note at its call site.
fn write_if_changed(path: &std::path::Path, contents: &str) {
    let normalised = contents.replace("\r\n", "\n");
    if std::fs::read_to_string(path).is_ok_and(|old| old == normalised) {
        return;
    }
    std::fs::write(path, normalised.as_bytes()).expect("write generated file");
}

/// Exports the whole `Settings` tree to `generated/settings.ts` and the
/// `Settings::default()` snapshot to `generated/defaults.json`.
///
/// `export_all` walks the dependency graph from the root, so a type added to
/// the tree is exported the moment it is reachable — there is no second list of
/// types to keep in step, which is the entire point of the exercise.
#[test]
fn settings_bindings_and_defaults_are_generated() {
    let dir = generated_dir();
    std::fs::create_dir_all(&dir).expect("create generated dir");

    // `with_large_int("number")`: locked decision 3 — `u64`/`usize` cross the
    // wire as JSON numbers and the frontend reads them as `number`. ts-rs would
    // otherwise emit `bigint`, which `JSON.parse` never produces.
    let cfg = Config::new().with_out_dir(&dir).with_large_int("number");
    Settings::export_all(&cfg).expect("export settings bindings");
    // A second root: `LocalProviderBlock` is declared in `schema/offload.rs` and
    // crosses the wire as an IPC return (`ipc::commands::opencode_derive_
    // provider`) and as an opaque `harness[<id>].ext` row, so it is NOT
    // reachable by walking `Settings`' fields — but the frontend imports it
    // from `settings/types` like any other settings type.
    LocalProviderBlock::export_all(&cfg).expect("export LocalProviderBlock");

    // `export_all` above already WROTE `settings.ts`. This used to read it back
    // and hand it to `write_if_changed`, which could not do anything: the
    // content had just come from that same file, so the comparison always
    // matched and the write never happened — a CRLF normalisation that ran on
    // text ts-rs had emitted with LF two lines earlier (V42 review,
    // dropped-at-cap). The read-back is kept, and turned into the check the
    // dead call was standing in for: `.gitattributes` marks this directory
    // `-text`, so a CR byte in what the generator emits would make CI's
    // byte-exact diff fail on every Windows run, for a reason that has nothing
    // to do with drift.
    let ts_path = dir.join("settings.ts");
    let generated = std::fs::read_to_string(&ts_path).expect("ts-rs wrote settings.ts");
    assert!(
        !generated.contains('\r'),
        "ts-rs emitted CR bytes into settings.ts; the committed bytes are LF and \
         `.gitattributes` marks this directory `-text`, so CI would fail the diff on \
         every Windows run"
    );

    let defaults =
        serde_json::to_string_pretty(&Settings::default()).expect("Settings::default serializes");
    write_if_changed(&dir.join("defaults.json"), &format!("{defaults}\n"));
}

/// The generator is a no-op on a second run: `cargo test` twice in a row must
/// not produce a diff, or the CI check would fail on every unrelated PR.
///
/// Byte-stability is not free — a hand-rolled emission over a `HashMap` would
/// reorder between runs. ts-rs sorts both imports and declarations, and
/// `Settings::default()` serialises through `BTreeMap`s, so the only way this
/// regresses is a future addition that iterates something unordered.
#[test]
fn regenerating_the_bindings_changes_nothing() {
    let dir = generated_dir();
    let cfg = Config::new().with_out_dir(&dir).with_large_int("number");

    let ts = Settings::export_to_string(&cfg).expect("render Settings");
    let again = Settings::export_to_string(&cfg).expect("render Settings twice");
    assert_eq!(ts, again, "the type rendering is not deterministic");

    let json = serde_json::to_string_pretty(&Settings::default()).expect("defaults serialize");
    let json_again =
        serde_json::to_string_pretty(&Settings::default()).expect("defaults serialize twice");
    assert_eq!(json, json_again, "the defaults rendering is not deterministic");
}

// ───────────────────────────────────────────────────────────────────────────
// V42 review, RV-8: the HAND-KEPT SEAMS.
//
// Phase E deleted `types.ts`'s hand-written mirror and the `include_str!`
// field-name scans that watched it, on the correct grounds that asserting a
// generator generated is ceremony. But four types do NOT reach TypeScript
// through the generator's own understanding of them. Their (de)serialize is
// written by hand, ts-rs cannot read a wire word off a hand-written impl, and
// the answer is a `#[cfg_attr(test, ts(...))]` override in the `schema/` tree /
// `injection.rs` that RESTATES what the impl does. Each of those is a
// hand-written mirror of exactly the kind Phase E deleted — one line long and
// inside an attribute — and after the retirement nothing checked any of them.
//
// These join the two halves at the one place they can be joined: run the real
// `Serialize` and require the bytes it produces to be spelled in the TypeScript
// the frontend actually compiles against. A `rename_all` typo, a variant added
// to the Rust and not to the union, a `"disabled"` renamed on one side — each
// fails here instead of rendering `undefined` at runtime.
// ───────────────────────────────────────────────────────────────────────────

/// The generated bindings, as the frontend compiles them.
///
/// `include_str!`, deliberately: the property is about the COMMITTED file —
/// the one Vite bundles — not about what the generator would produce if asked
/// again. Reading it at compile time means a seam broken and regenerated in
/// the same `cargo test` invocation is caught on the NEXT compile rather than
/// that one, which is fine and is not a gap: the regenerated file has to be
/// committed for the CI bindings gate to pass, and CI compiles against what
/// was committed. A seam can therefore not reach `main` unnoticed; it can only
/// be noticed one build after it is typed.
const GENERATED_TS: &str = include_str!("../../../src/lib/settings/generated/settings.ts");

/// One `export type <name> = …;` declaration out of `src`, without the
/// surrounding file.
///
/// Slicing matters for the same reason it does in `harness::health`: a
/// substring search over 3,000 lines of generated prose can be satisfied by a
/// doc comment three types away, which makes an assertion look strong and be
/// vacuous.
fn ts_declaration<'a>(src: &'a str, name: &str) -> &'a str {
    let decl = format!("export type {name} = ");
    let at = src
        .find(&decl)
        .unwrap_or_else(|| panic!("`{decl}` is not in the generated bindings"));
    let body = &src[at + decl.len()..];
    let end = body
        .find(";\n")
        .unwrap_or_else(|| panic!("`{decl}` is never terminated"));
    &body[..end]
}

/// The double-quoted literals of a TS string-literal union, in order.
fn union_members(decl: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = decl;
    while let Some(i) = rest.find('"') {
        let after = &rest[i + 1..];
        let Some(j) = after.find('"') else { break };
        out.push(after[..j].to_string());
        rest = &after[j + 1..];
    }
    out
}

/// **`Override` — `#[serde(into = "String")]` plus a hand-written
/// `Deserialize`, mirrored by `ts(rename_all = "lowercase")`.**
///
/// ts-rs sees through neither impl, so the three lowercase words in the
/// generated union come from that attribute and from nothing else.
#[test]
fn the_injection_override_union_is_exactly_what_the_enum_serializes() {
    use super::injection::Override;

    let mut wire: Vec<String> = [Override::Inherit, Override::On, Override::Off]
        .iter()
        .map(|v| {
            let json = serde_json::to_value(v).expect("Override serializes");
            json.as_str()
                .unwrap_or_else(|| panic!("`{json}` is not a JSON string — the TS says it is"))
                .to_string()
        })
        .collect();

    let mut declared = union_members(ts_declaration(GENERATED_TS, "InjectionOverride"));
    wire.sort();
    declared.sort();
    assert_eq!(
        declared, wire,
        "`InjectionOverride`'s TS union and what `Override` actually serializes have \
         drifted — the union is restated by hand in a `ts(rename_all = …)` attribute, so \
         nothing else would notice"
    );

    // …and the hand-written parse accepts every word the impl emits. A cell the
    // frontend writes back must not read as `inherit` because the two halves of
    // one seam disagree about spelling.
    for word in wire {
        assert_eq!(
            serde_json::to_value(Override::parse(&word)).expect("re-serializes"),
            serde_json::Value::String(word.clone()),
            "`{word}` does not round-trip through the hand-written parse"
        );
    }
}

/// **`BackgroundOverride` — a hand-written `Serialize` emitting EITHER the
/// literal `"disabled"` OR a config object**, mirrored by an explicit
/// `ts(type = …)` on the field.
///
/// Spelled at TWO sites in `schema/tabs.rs` (`AiToolTabFields`, `ShellTabFields`),
/// which is the drift this catches: an override edited at one and not the
/// other was invisible to everything.
#[test]
fn the_background_override_seam_matches_both_spelled_sites() {
    use super::{BackgroundOverride, TerminalBackgroundSettings};

    // The `Disabled` half: the TS literal must BE the serialized bytes.
    let disabled = serde_json::to_string(&BackgroundOverride::Disabled)
        .expect("BackgroundOverride::Disabled serializes");
    assert_eq!(disabled, "\"disabled\"", "the literal half of the seam moved");

    // The `Custom` half: an object, whose keys the named TS type declares.
    let custom = serde_json::to_value(BackgroundOverride::Custom(
        TerminalBackgroundSettings::default(),
    ))
    .expect("BackgroundOverride::Custom serializes");
    let custom = custom
        .as_object()
        .expect("the Custom variant crosses the wire as an object");
    let declared_fields = ts_declaration(GENERATED_TS, "TerminalBackgroundSettings");
    assert!(!custom.is_empty(), "an empty object would satisfy the loop");
    for key in custom.keys() {
        assert!(
            declared_fields.contains(&format!("{key}: ")),
            "`{key}` crosses the wire but `TerminalBackgroundSettings` does not declare it"
        );
    }

    // Both spelled sites carry the same seam text, starting with the literal
    // the serializer actually emits.
    let expected = format!("{disabled} | TerminalBackgroundSettings | null");
    let sites: Vec<&str> = GENERATED_TS
        .match_indices("background_override: ")
        .map(|(at, m)| &GENERATED_TS[at + m.len()..])
        .collect();
    assert_eq!(
        sites.len(),
        2,
        "`background_override` is declared on two tab kinds; found {}",
        sites.len()
    );
    for site in sites {
        assert!(
            site.starts_with(&expected),
            "a `background_override` seam does not read `{expected}` — the two spelled \
             sites have drifted, or the hand-written Serialize has"
        );
    }
}

/// **`NotificationSlot` — a hand-written `Deserialize` that also accepts the
/// pre-v1.11 bare string.**
///
/// The derived `Serialize` is what the TS declares, and the legacy input shape
/// deliberately does NOT appear in it: the frontend always writes the object,
/// and widening the TS to `string | {…}` would invite it to write the shape
/// this build only reads for migration.
#[test]
fn the_notification_slot_seam_declares_the_shape_it_writes() {
    use super::NotificationSlot;

    let slot = serde_json::to_value(NotificationSlot::enabled("{tab} is idle"))
        .expect("NotificationSlot serializes");
    let slot = slot.as_object().expect("the wire shape is an object");
    let declared = ts_declaration(GENERATED_TS, "NotificationSlot");
    assert!(!slot.is_empty(), "an empty object would satisfy the loop");
    for key in slot.keys() {
        assert!(
            declared.contains(&format!("{key}: ")),
            "`{key}` crosses the wire but the TS `NotificationSlot` does not declare it"
        );
    }
    assert!(
        declared.starts_with('{') && !declared.contains('|'),
        "the TS declares a union: the legacy bare-string shape is READ for migration and \
         must never be offered to the frontend as something to write — {declared}"
    );

    // The migration input really is still accepted — the reason the seam is
    // hand-written at all.
    let legacy: NotificationSlot =
        serde_json::from_value(serde_json::json!("{tab} is idle")).expect("legacy shape loads");
    assert!(legacy.enabled && legacy.text == "{tab} is idle");
    let blank: NotificationSlot =
        serde_json::from_value(serde_json::json!("")).expect("the blank legacy shape loads");
    assert!(!blank.enabled, "an empty legacy text meant disabled");
}

/// **`AiTabId` — a hand-written `Serialize` emitting the bare registry id**,
/// mirrored by a `ts(type = "import('../../tabs/types').AiTabId[]")` seam on
/// `enabled_ai_tabs`.
///
/// Two halves rot independently: the seam can stop naming a real declaration,
/// and `tabs/types.ts` can stop declaring the bare string the impl emits.
#[test]
fn the_ai_tab_id_seam_points_at_a_bare_string_the_registry_can_produce() {
    use super::AiTabId;
    const TAB_TYPES: &str = include_str!("../../../src/lib/tabs/types.ts");

    assert!(
        GENERATED_TS.contains("enabled_ai_tabs: import('../../tabs/types').AiTabId[]"),
        "the `enabled_ai_tabs` seam no longer names the frontend's `AiTabId`"
    );
    assert!(
        TAB_TYPES.contains("export type AiTabId = string;"),
        "the seam points at `tabs/types.ts`'s `AiTabId`, which must stay the bare wire \
         string the hand-written Serialize emits — a narrowed union there would reject a \
         harness whose tab id this build learned from the registry"
    );

    // …and it really does emit a bare string, for every id the registry claims.
    // Vacuity guard first: an empty roster would satisfy the loop.
    let ids = super::schema::canonical_ai_tab_order();
    assert!(!ids.is_empty(), "the registry claims no AI tab ids at all");
    for id in ids {
        let json = serde_json::to_value(id).expect("AiTabId serializes");
        assert_eq!(
            json,
            serde_json::Value::String(id.as_str().to_string()),
            "`AiTabId` must cross the wire as the bare id"
        );
        // The refusal half, which the derived enum impl used to give for free.
        assert!(
            serde_json::from_value::<AiTabId>(json).is_ok(),
            "a registry id must deserialize back"
        );
    }
    assert!(
        serde_json::from_value::<AiTabId>(serde_json::json!("not-a-harness-tab")).is_err(),
        "an id no descriptor claims must be refused, or a hand-edited file could name a \
         tab nothing can materialise"
    );
}
