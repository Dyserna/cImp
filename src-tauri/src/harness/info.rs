//! V40 Phase F — **the registry, as the frontend receives it** (locked
//! decisions 7, 11 and 27).
//!
//! Before this module the window held a second copy of the roster: `AI_TABS`,
//! `RESERVED_AI_TAB_IDS`, `isShellTab`, two `order` arrays, a `HARNESS_LABELS`
//! map, a hand-written mirror of `command_is`, per-id `{:else if}` bodies and a
//! CSS class per harness — none of which the Rust registry could see, and all
//! of which a third harness would have had to be added to by hand.
//!
//! [`HarnessInfo`] is that roster as data, served once at startup by the
//! `harness_list` command. It carries three kinds of thing:
//!
//! * **identity** — `id`, `label`, `tab_ids`, `binaries`, `consumer`: the
//!   answers to every "which harness is this?" the window asks;
//! * **features** — what core mounts for it (locked decision 6), so a panel is
//!   mounted by a declaration rather than by an `id === 'claude'`;
//! * **affordances** — the strings ([`HarnessAffordances`]).
//!
//! Plus each harness's declared `ext` fields, which subsumes the Phase B
//! `harness_settings_schema` command (its own doc comment said this would
//! happen).
//!
//! # The fixture
//!
//! `the_committed_registry_fixture_matches_the_registry` writes this payload to
//! `fixtures/harness/registry.json` and fails when the file on disk differs.
//! `vitest` loads that file and asserts the TypeScript unions cover it (locked
//! decision 11), so a descriptor field, a feature or a harness added in Rust
//! without its TS mirror is a red `npm test` rather than a runtime `undefined`.

use serde::Serialize;

use super::plugin::{HarnessAffordances, LocalProviderVar, SettingKind};
use super::registry::{descriptors, HarnessDescriptor, HarnessFeature};

/// One harness, as the window sees it. Wire mirror of [`HarnessDescriptor`] +
/// [`HarnessAffordances`].
#[derive(Serialize)]
pub struct HarnessInfo {
    /// The registry id — the key into `Settings.harness`, a server's `access`
    /// map, and the CHP `agent` discriminator.
    pub id: &'static str,
    /// What a human calls it.
    pub label: &'static str,
    /// Reserved built-in tab ids, in canonical order.
    pub tab_ids: Vec<&'static str>,
    /// The binaries whose file stem identifies this harness — what the
    /// frontend's "which harness runs in this tab?" lookup compares against.
    pub binaries: &'static [&'static str],
    /// What core mounts for this harness beyond the neutral path.
    pub features: Vec<&'static str>,
    /// The MCP consumer token.
    pub consumer: &'static str,
    /// The strings and UI facts the window used to hard-code.
    pub affordances: AffordancesView,
    /// This harness's declared `ext` fields, in declaration order. Empty is an
    /// ordinary answer: such a harness gets an empty section and no UI work.
    pub fields: Vec<SettingFieldView>,
    /// The injection features whose app-wide switch is THIS harness's `ext` row
    /// rather than a field in core (locked decision 6).
    ///
    /// Without it the frontend's spawn-signature mirror had to know which
    /// harness owned the harness-scoped gate, by name, to read its L2 cell —
    /// the last per-harness branch in `settings/types.ts`.
    pub scoped_features: Vec<ScopedFeatureView>,
}

/// One injection feature whose app-wide switch lives on a harness's `ext`.
/// Wire mirror of `harness::plugin::ScopedFeature`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopedFeatureView {
    /// The feature's stable wire key — the same string the per-tab override
    /// cells and the `/status` rows use.
    pub feature: &'static str,
    /// The key inside `Settings::harness[<id>].ext` holding its app-wide value.
    pub ext_key: &'static str,
}

/// Wire mirror of [`HarnessAffordances`]. `camelCase` because it is read only
/// by TypeScript, and every field name here is a property on a TS interface.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AffordancesView {
    pub new_session_command: Option<&'static str>,
    pub tool_list_refresh: Option<&'static str>,
    pub web_tools: &'static [&'static str],
    pub state_dirs: &'static [&'static str],
    pub install_hint: Option<&'static str>,
    pub docs_url: Option<&'static str>,
    pub attachment_format: &'static str,
    /// `null` = this harness has no local-provider control at all.
    pub local_provider: Option<Vec<LocalProviderVarView>>,
    pub local_provider_note: Option<&'static str>,
    pub local_provider_config_note: Option<&'static str>,
    /// The two `ext` keys the Offload card's local-provider block writes.
    /// `null` for a harness that does not declare `local_provider_config`.
    pub local_provider_config_block_key: Option<&'static str>,
    /// See [`Self::local_provider_config_block_key`].
    pub local_provider_config_auto_key: Option<&'static str>,
    pub statusline_rows: u8,
    pub attribution_template: &'static str,
    pub inject_mechanism: Option<&'static str>,
    pub default_command: &'static str,
    pub command_example: Option<&'static str>,
    pub accent: &'static str,
    pub tier: &'static str,
}

/// Wire mirror of [`LocalProviderVar`].
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalProviderVarView {
    pub name: &'static str,
    /// `null` = a credential; the preview prints an ellipsis instead of a value.
    pub ext_key: Option<&'static str>,
    pub only_when_set: bool,
}

/// One declared `ext` field. Wire-shaped mirror of
/// [`crate::harness::plugin::SettingField`] — the kind enum becomes a tag plus
/// an option list, because a TypeScript form wants a string to switch on.
#[derive(Serialize)]
pub struct SettingFieldView {
    /// The key inside `Settings::harness[<id>].ext`.
    pub key: &'static str,
    /// `"bool" | "int" | "text" | "path" | "enum" | "json"`.
    pub kind: &'static str,
    /// The allowed values, for `kind == "enum"`; empty otherwise.
    pub options: &'static [&'static str],
    /// The form label.
    pub label: &'static str,
    /// One sentence under the control. May be empty.
    pub hint: &'static str,
    /// The value an absent key reads as — what the form shows before the user
    /// has ever touched it, and what *Reset* restores.
    pub default: serde_json::Value,
    /// Whether flipping it needs a tab restart (the window shows the hint).
    pub spawn_baked: bool,
    /// A credential: the form masks it.
    pub secret: bool,
}

/// The name a [`HarnessFeature`] travels under. Spelled here rather than
/// derived, because these strings are a TypeScript union in `src/lib/harness.ts`
/// and a rename is a wire break the parity test has to be able to see.
fn feature_token(f: HarnessFeature) -> &'static str {
    match f {
        HarnessFeature::SessionUsage => "session_usage",
        HarnessFeature::ContextBar => "context_bar",
        HarnessFeature::FileArtifact => "file_artifact",
        HarnessFeature::UsagePush => "usage_push",
        HarnessFeature::LocalProviderConfig => "local_provider_config",
    }
}

fn affordances_view(a: &HarnessAffordances) -> AffordancesView {
    AffordancesView {
        new_session_command: a.new_session_command,
        tool_list_refresh: a.tool_list_refresh,
        web_tools: a.web_tools,
        state_dirs: a.state_dirs,
        install_hint: a.install_hint,
        docs_url: a.docs_url,
        attachment_format: a.attachment_format,
        local_provider: a.local_provider.map(|vars| {
            vars.iter()
                .map(|v: &LocalProviderVar| LocalProviderVarView {
                    name: v.name,
                    ext_key: v.ext_key,
                    only_when_set: v.only_when_set,
                })
                .collect()
        }),
        local_provider_note: a.local_provider_note,
        local_provider_config_note: a.local_provider_config_note,
        local_provider_config_block_key: a.local_provider_config_block_key,
        local_provider_config_auto_key: a.local_provider_config_auto_key,
        statusline_rows: a.statusline_rows,
        attribution_template: a.attribution_template,
        inject_mechanism: a.inject_mechanism,
        default_command: a.default_command,
        command_example: a.command_example,
        accent: a.accent,
        tier: a.tier,
    }
}

fn one(d: &'static HarnessDescriptor) -> HarnessInfo {
    let affordances = d.plugin.affordances();
    HarnessInfo {
        id: d.id,
        label: d.label,
        tab_ids: d.tab_ids().collect(),
        binaries: d.binaries,
        features: d.features.iter().copied().map(feature_token).collect(),
        consumer: d.consumer,
        affordances: affordances_view(&affordances),
        scoped_features: d
            .plugin
            .scoped_features()
            .iter()
            .map(|f| ScopedFeatureView {
                feature: f.feature.key(),
                ext_key: f.ext_key,
            })
            .collect(),
        fields: d
            .plugin
            .settings_schema()
            .iter()
            .map(|f| SettingFieldView {
                key: f.key,
                kind: match f.kind {
                    SettingKind::Bool => "bool",
                    SettingKind::Int => "int",
                    SettingKind::Text => "text",
                    SettingKind::Path => "path",
                    SettingKind::Enum(_) => "enum",
                    SettingKind::Json => "json",
                },
                options: match f.kind {
                    SettingKind::Enum(options) => options,
                    _ => &[],
                },
                label: f.label,
                hint: f.hint,
                default: f.default.to_json(),
                spawn_baked: f.spawn_baked,
                secret: f.secret,
            })
            .collect(),
    }
}

/// Every registered harness, in declaration order — what `harness_list` serves.
pub fn harness_list() -> Vec<HarnessInfo> {
    // `registry::descriptors()` rather than `HARNESSES` so a scoped test
    // harness reaches the window payload too (V40 Phase I) — in a release
    // build the two are the same iterator.
    descriptors().map(one).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::registry::HARNESSES;

    /// The path the fixture lives at, relative to `src-tauri/`.
    const FIXTURE: &str = "fixtures/harness/registry.json";

    fn rendered() -> String {
        let mut s = serde_json::to_string_pretty(&harness_list())
            .expect("the registry payload is plain data");
        s.push('\n');
        s
    }

    /// **The committed fixture is the registry** (locked decision 11).
    ///
    /// `vitest` cannot call into Rust, so the parity test reads a file — and a
    /// file is only evidence while something keeps it current. This test WRITES
    /// it and then fails if what was on disk differed, so the drift is a red
    /// `cargo test` with the fix already applied to the working tree: re-run,
    /// commit the fixture.
    #[test]
    fn the_committed_registry_fixture_matches_the_registry() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE);
        let before = std::fs::read_to_string(&path).unwrap_or_default();
        let now = rendered();
        if before != now {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).expect("fixtures/harness exists or can be made");
            }
            std::fs::write(&path, &now).expect("the fixture is writable");
            panic!(
                "{FIXTURE} was stale and has been REWRITTEN from the registry. Review the diff \
                 and commit it — `src/lib/harness.ts`'s unions are checked against this file by \
                 vitest, so a new harness, feature or affordance field needs its TypeScript \
                 mirror in the same change."
            );
        }
    }

    /// **A declared `usage_push` has a source to push** (locked decision 19).
    ///
    /// The frontend polls `harness_usage(<id>)` for the harness it finds this
    /// feature on. Declaring it without a `usage_source()` would make the widget
    /// poll a harness that can never answer — a permanently empty bottom bar
    /// with no error, which is the "computed then discarded" shape this
    /// milestone's tests exist to catch.
    #[test]
    fn a_declared_usage_push_has_a_source() {
        for d in HARNESSES {
            if d.features.contains(&HarnessFeature::UsagePush) {
                assert!(
                    d.plugin.usage_source().is_some(),
                    "{}: declares usage_push but has no usage source for the widget to read",
                    d.id
                );
            }
        }
    }

    /// **A declared `local_provider_config` has a writer** (locked decision 26).
    ///
    /// The Offload card mounts its *register this backend* button on this
    /// feature and then calls `offload_derive_local_provider` with the
    /// harness id. Declaring it without a `config_writer()` would put a button
    /// on screen whose only possible outcome is the backend's refusal.
    #[test]
    fn a_declared_config_writer_exists() {
        for d in HARNESSES {
            let declared = d.features.contains(&HarnessFeature::LocalProviderConfig);
            let has = d.plugin.config_writer().is_some();
            assert_eq!(
                declared, has,
                "{}: declares local_provider_config = {declared} but config_writer() = {has}",
                d.id
            );

            // V40 review F-6: …and the two `ext` keys that block writes are
            // DECLARED, not a convention shared with the Settings window by
            // comment. An undeclared key would be stored forever and read by
            // nobody while the real setting stayed at its default.
            let a = d.plugin.affordances();
            let schema = d.plugin.settings_schema();
            let field = |key: &str| schema.iter().find(|f| f.key == key).map(|f| f.kind);
            assert_eq!(
                declared,
                a.local_provider_config_block_key.is_some()
                    && a.local_provider_config_auto_key.is_some(),
                "{}: local_provider_config = {declared} but its two `ext` keys are {:?}/{:?}",
                d.id,
                a.local_provider_config_block_key,
                a.local_provider_config_auto_key
            );
            if let Some(key) = a.local_provider_config_block_key {
                assert_eq!(
                    field(key),
                    Some(crate::harness::plugin::SettingKind::Json),
                    "{}: `{key}` is the derived provider BLOCK — cImp writes it, the user never                      types it, so it has to be a declared `Json` field",
                    d.id
                );
            }
            if let Some(key) = a.local_provider_config_auto_key {
                assert_eq!(
                    field(key),
                    Some(crate::harness::plugin::SettingKind::Bool),
                    "{}: `{key}` is the auto-sync flag and has to be a declared `Bool` field",
                    d.id
                );
            }
        }
    }

    /// **Every accent is distinct where declared.**
    ///
    /// The accent replaces the `.esrc.claude` / `.esrc.opencode` CSS classes,
    /// which were distinct by construction. Two harnesses answering the same
    /// token would silently merge two lanes of the Events feed into one colour.
    #[test]
    fn accents_are_distinct_where_declared() {
        let mut seen: std::collections::BTreeSet<&str> = Default::default();
        for d in HARNESSES {
            let a = d.plugin.affordances();
            if a.accent.is_empty() {
                continue;
            }
            assert!(
                seen.insert(a.accent),
                "{}: accent {:?} is already another harness's",
                d.id,
                a.accent
            );
        }
    }

    /// **A local-provider declaration names real `ext` keys.**
    ///
    /// The preview reads `Settings.harness[<id>].ext[<key>]`; a key no field
    /// declares reads as absent and the preview would print an empty value for
    /// a variable the spawn really does set.
    #[test]
    fn local_provider_vars_name_declared_ext_keys() {
        for d in HARNESSES {
            let a = d.plugin.affordances();
            let Some(vars) = a.local_provider else {
                continue;
            };
            let declared: Vec<&str> = d.plugin.settings_schema().iter().map(|f| f.key).collect();
            for v in vars {
                let Some(key) = v.ext_key else { continue };
                assert!(
                    declared.contains(&key),
                    "{}: local-provider var {} reads ext key {key:?}, which no settings field \
                     declares",
                    d.id,
                    v.name
                );
            }
        }
    }

    /// **Every harness the window will render has a label and a command.**
    ///
    /// The Settings form prints `default_command` in its Command hint and the
    /// tab bar prints `label`; an empty one renders as a gap the user cannot
    /// interpret.
    #[test]
    fn every_harness_declares_what_the_window_prints() {
        for info in harness_list() {
            assert!(!info.label.is_empty(), "{}: no label", info.id);
            assert!(
                !info.affordances.default_command.is_empty(),
                "{}: no default command for the Settings hint",
                info.id
            );
        }
    }
}
