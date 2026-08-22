//! V40 Phase B — **Claude Code's own settings**: the fields core stores
//! opaquely in `Settings::harness["claude"].ext` and never names.
//!
//! Locked decision 6. Two blocks lived in core `Settings` before this, and
//! neither was ever read by anything but this directory:
//!
//! * `Settings::statusline` — one `bool` gating the `--settings` overlay that
//!   points Claude Code's `statusLine.command` at `cimp --statusline`. A
//!   harness with no status-line contract has nothing to toggle, so a core
//!   field for it was a Claude field with a neutral name.
//! * `Settings::claude_local` — `base_url` / `auth_token` / `model_alias`, the
//!   `ANTHROPIC_*` env synthesis for a tab pointed at a local proxy. The struct
//!   even carried a hand-rolled `Debug` to redact the token; that redaction is
//!   now the [`SettingField::secret`] column, applied by
//!   `HarnessSettings`'s `Debug` for every plugin at once.
//!
//! Both are declared here instead. Core validates them against
//! [`FIELDS`] at the parse boundary, renders the Settings section from it, and
//! folds the `spawn_baked` ones into this harness's spawn signature
//! automatically — which is the point of the `spawn_baked` column: the flag and
//! its restart-hint entry are ONE declaration, not two lists that can disagree
//! (the V32 F-27 / V38 M-3 failure class).

use crate::harness::plugin::{SettingDefault, SettingField, SettingKind};
use crate::settings::Settings;

/// `ext` key: inject the `statusLine` overlay (was `Settings::statusline`).
pub const STATUSLINE: &str = "statusline";
/// `ext` key: local-provider base URL (was `claude_local.base_url`).
pub const LOCAL_BASE_URL: &str = "local.base_url";
/// `ext` key: local-provider auth token (was `claude_local.auth_token`).
pub const LOCAL_AUTH_TOKEN: &str = "local.auth_token";
/// `ext` key: local-provider model alias (was `claude_local.model_alias`).
pub const LOCAL_MODEL_ALIAS: &str = "local.model_alias";

/// Claude Code's declared settings, in the order the Settings section renders
/// them.
pub const FIELDS: &[SettingField] = &[
    SettingField {
        key: STATUSLINE,
        kind: SettingKind::Bool,
        label: "Context status line",
        hint: "Inject a session-scoped `--settings` overlay pointing Claude Code's status line \
               at cImp's own themed context-usage bar. Merges with your own Claude Code \
               settings; your global ~/.claude config is left untouched.",
        default: SettingDefault::Bool(true),
        spawn_baked: true,
        secret: false,
    },
    SettingField {
        key: LOCAL_BASE_URL,
        kind: SettingKind::Text,
        label: "Local provider — base URL",
        hint: "Used only by tabs with `use_local_provider` on: synthesized into \
               ANTHROPIC_BASE_URL at spawn. Typically a LiteLLM proxy bridging to Ollama or \
               LM Studio.",
        default: SettingDefault::Text("http://localhost:4000"),
        spawn_baked: true,
        secret: false,
    },
    SettingField {
        key: LOCAL_AUTH_TOKEN,
        kind: SettingKind::Text,
        label: "Local provider — auth token",
        hint: "Synthesized into ANTHROPIC_AUTH_TOKEN. Stored cleartext in settings.json — \
               local proxies typically accept dummy tokens.",
        default: SettingDefault::Text("sk-dummy"),
        spawn_baked: true,
        secret: true,
    },
    SettingField {
        key: LOCAL_MODEL_ALIAS,
        kind: SettingKind::Text,
        label: "Local provider — model alias",
        hint: "Optional. Synthesized into ANTHROPIC_MODEL, which some proxies honour; empty \
               leaves the model to the --model flag.",
        default: SettingDefault::Text(""),
        spawn_baked: true,
        secret: false,
    },
];

/// Whether the status-line overlay is injected into this harness's tabs.
pub fn statusline_enabled(s: &Settings) -> bool {
    s.harness_ext_bool(super::plugin::me(), STATUSLINE)
}

/// The three local-provider values, in `ANTHROPIC_BASE_URL` /
/// `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_MODEL` order.
///
/// One reader for all three, so the env synthesis and the spawn signature
/// cannot disagree about which keys carry them.
pub fn local_provider(s: &Settings) -> [String; 3] {
    let me = super::plugin::me();
    [
        s.harness_ext_str(me, LOCAL_BASE_URL),
        s.harness_ext_str(me, LOCAL_AUTH_TOKEN),
        s.harness_ext_str(me, LOCAL_MODEL_ALIAS),
    ]
}
