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
//!   …))]` overrides in `schema.rs`, each commented `HAND-KEPT SEAM` — one per
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
    // A second root: `LocalProviderBlock` is declared in `schema.rs` and
    // crosses the wire as an IPC return (`ipc::commands::opencode_derive_
    // provider`) and as an opaque `harness[<id>].ext` row, so it is NOT
    // reachable by walking `Settings`' fields — but the frontend imports it
    // from `settings/types` like any other settings type.
    LocalProviderBlock::export_all(&cfg).expect("export LocalProviderBlock");

    let ts_path = dir.join("settings.ts");
    let generated = std::fs::read_to_string(&ts_path).expect("ts-rs wrote settings.ts");
    write_if_changed(&ts_path, &generated);

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
