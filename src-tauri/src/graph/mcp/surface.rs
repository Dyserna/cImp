//! The **process-wide advertised-surface measurement** (V17 Phase E) and the
//! fingerprint that memoizes it.
//!
//! Nothing here is graph-specific: [`SurfaceStats`] measures what BOTH
//! consumers are advertised — the `graph_*` set, `run_check`, `run_command`
//! and the delegation-shaped tools — and [`native_surface_sig`] is the pulse
//! gate's one comparable value for "the native surface moved". V42 R8 moved it
//! out of the tool file for that reason.

use super::checks_tools::{checks_sig, commands_sig};
use super::current_settings;
use super::tools::tools;

/// V17 Phase E: the measured size of the advertised tool surface, for BOTH
/// consumers — the cloud Opus / OpenCode session ([`tools`], MCP shape) and the
/// local offload worker (`graph_tools::defs`, OpenAI shape). `*_chars` is the
/// serialized-JSON length (what actually rides in the tools block, cache-written
/// once per session); `*_tools` is the count. Both are computed **after** the
/// `lean_tools` filter, so toggling the lean surface moves these numbers by the
/// hidden tools' delta.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct SurfaceStats {
    pub mcp_tools: usize,
    pub mcp_chars: usize,
    pub offload_tools: usize,
    pub offload_chars: usize,
}

/// The exact settings that move [`surface_stats`] — every input that changes
/// what [`tools`] / `graph_tools::defs` advertise. Everything else in the specs
/// (`tool_specs`, the semantic/`run_check` specs, `LEAN_HIDDEN`) is static, so
/// two equal fingerprints ⇒ byte-identical [`SurfaceStats`]. The specs carry no
/// project-scoped text either (no paths/roots baked in), so the fingerprint
/// needs no cwd/root component — the derived booleans below fully determine the
/// output regardless of which project's settings produced them.
///
/// Coverage (read off the gating in [`tools`] and `graph_tools::defs`):
/// - `graph_enabled`  — gates the whole `graph_*` block in [`tools`].
/// - `semantic_search`— gates `graph_semantic_docs` in both.
/// - `embed_code_bodies` — gates `graph_semantic_code` in both.
/// - `lean_tools`     — drops [`LEAN_HIDDEN`] from both.
/// - `checks_sig`     — gates `run_check` in [`tools`] AND fixes its schema.
///   Emptiness alone is NOT enough: [`run_check_spec`] bakes the configured
///   check NAMES into `name`'s `enum`/description and flips `required` on the
///   one-vs-many boundary, so renaming a check or adding a second one changes
///   the advertised bytes without changing emptiness. Hashing the names (in
///   order) covers every input the spec reads — an empty list hashes to its own
///   distinct value, so this subsumes the old `has_checks` bool.
///   **V38 (invariant 10): the names hashed are the EFFECTIVE ones**, plugin
///   checks included. Hashing only `settings.checks` would have left the memo
///   serving a stale surface across every plugin enable, path change and
///   Rescan — the advertised bytes would move while the fingerprint stood
///   still, which is the one failure this type exists to prevent.
#[derive(Clone, Copy, PartialEq, Eq, Debug, std::hash::Hash)]
pub(super) struct SurfaceFingerprint {
    graph_enabled: bool,
    semantic_search: bool,
    embed_code_bodies: bool,
    lean_tools: bool,
    checks_sig: u64,
    /// **V38 F-3** — `run_command`'s two inputs, hashed together: the two
    /// per-consumer exposure switches and the runnable `command`-kind names.
    ///
    /// Both halves move the advertised bytes, and only together: flipping a
    /// switch adds or removes a whole tool, and configuring a command tool's
    /// path adds a value to its enum. Folding them into ONE component (rather
    /// than adding three fields) keeps this type's rule intact — it is a
    /// fingerprint, not a mirror of the settings — while `native_surface_sig`
    /// gets exactly what it needs: a value that changes iff a session's
    /// advertised list would.
    ///
    /// Both consumers' switches are hashed even though [`tools_for`] reads one:
    /// the pulse is per PROCESS, not per consumer, and a fingerprint that
    /// ignored the other half would leave an OpenCode session's surface moving
    /// with nothing to notice it.
    commands_sig: u64,
    /// **V39 Phase B** — the generated `delegate_task_<harness>` set: the tab
    /// roles that decide which of those tools exists, plus the worker gate's
    /// verdict.
    ///
    /// Same rule as `commands_sig`: ONE component for one advertised group,
    /// hashed from every input that moves it, so this type stays a fingerprint
    /// rather than a mirror of the settings. Without it, moving the Manual role
    /// from one tab to another would change what a child advertises while the
    /// pulse gate saw no move and every live session kept the old tool list
    /// until its next restart — which is exactly what locked decision 15
    /// promises does NOT happen ("takes effect on the next turn without
    /// restarting either tab").
    delegation_sig: u64,
}

impl SurfaceFingerprint {
    pub(super) fn of(settings: &crate::settings::Settings) -> Self {
        Self {
            graph_enabled: settings.graph.enabled,
            semantic_search: settings.graph.semantic_search,
            embed_code_bodies: settings.graph.embed_code_bodies,
            lean_tools: settings.graph.lean_tools,
            checks_sig: checks_sig(settings),
            commands_sig: commands_sig(settings),
            delegation_sig: delegation_sig(settings),
        }
    }
}

/// Hash what decides the delegation-shaped surface: the worker gate's verdict,
/// every AI tab holding the Manual role with the harness it belongs to (the
/// `delegate_task_*` set), and every tab holding the Remote-offload role with
/// its facade knobs (the `offload_task` backend list, V39 Phase C).
///
/// The gate is in here because a `"fail"` recorded against the input-profile
/// spike removes the whole group, and that is a surface move like any other.
/// Process-local memo key only, like [`checks_sig`].
pub(super) fn delegation_sig(settings: &crate::settings::Settings) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    crate::harness::contract::gate(
        crate::harness::contract::CAP_DELEGATION_WORKER,
        settings,
    )
    .blocked
    .hash(&mut h);
    for cfg in &settings.tabs {
        if let crate::settings::TabConfig::AiTool(c) = cfg {
            match c.delegation_role {
                crate::settings::DelegationRole::Manual => {
                    crate::tabs::tab_consumer(c).hash(&mut h);
                    // `None` (an unregistered command) hashes distinctly from
                    // every harness id, which is what it is.
                    c.id.hash(&mut h);
                    c.name.hash(&mut h);
                }
                // **V39 Phase C.** A facade moves `offload_task`'s advertised
                // BYTES: its name, tier and declared window are rendered into
                // the backend list, and the role appearing or disappearing adds
                // or removes an entry outright. Same component as the Manual
                // set because it is the same group — "what the delegation
                // configuration advertises" — and one fingerprint field per
                // advertised group is this type's rule.
                //
                // The tab NAME is NOT hashed here (V39 review L-2): the blank
                // fallback is `facade_default_name(id)` now, not the tab name,
                // so a rename moves nothing a driver can see — and hashing it
                // would pulse `tools/list` for every tab title the user edits.
                crate::settings::DelegationRole::RemoteOffload => {
                    c.id.hash(&mut h);
                    c.delegation_backend.name.hash(&mut h);
                    // V39 review L-6: the tier's own wire word, not
                    // `is_fast`. A bool is lossless for exactly two variants
                    // and silently collapses the moment a third is added —
                    // and the symptom of a collapsed fingerprint component is
                    // the one this whole type exists to prevent: a surface
                    // that moved while the pulse gate saw no move, so every
                    // live session keeps the old `offload_task` prose until
                    // its next restart.
                    serde_json::to_string(&c.delegation_backend.tier)
                        .unwrap_or_default()
                        .hash(&mut h);
                    c.delegation_backend.declared_context.hash(&mut h);
                }
                crate::settings::DelegationRole::None => {}
            }
        }
    }
    h.finish()
}

/// V38 Phase F — the NATIVE tool surface's fingerprint, for the pulse gate.
///
/// # Why the pulse needs this at all
///
/// The `change` frame a consumer receives as `tools/list_changed` covers the
/// whole of what a per-session child advertises, and until this phase only two
/// thirds of that could move it: the proxied MCP surface ([`PulseSource::Host`])
/// and the backend ready-set ([`PulseSource::Backend`]). The third — the tools
/// this process serves itself, of which `run_check`'s `name` enum is the only
/// project-dynamic part — had **never pulsed**. [`checks_sig`] existed, but it
/// is a memo key for the statistics poll, not a notifier: enabling a plugin
/// check, setting its path, editing `settings.checks` or rescanning the plugins
/// folder all moved the advertised bytes while every live session went on
/// showing the old enum until its next restart.
///
/// # It is the same fingerprint, hashed
///
/// Deliberately [`SurfaceFingerprint`] and not a parallel list of inputs: that
/// type is already defined as "every setting that moves what [`tools`]
/// advertises", and a second answer to the same question is the drift this file
/// spends a long comment preventing. The hash exists only because the gate wants
/// one comparable value per source.
///
/// # Per-process and per-cwd, and that is correct here
///
/// [`checks_sig`] resolves the effective check set against `current_dir()`, so
/// this value describes the surface of the process that computes it. The gate
/// runs in the APP, whose cwd is the launch directory — the same directory the
/// registry's per-project paths are keyed by and the audit fan-out scans. A
/// child process would compute its own answer for its own project, which is
/// what it should advertise; it just never asks, because it does not own a
/// pulse.
pub fn native_surface_sig(settings: &crate::settings::Settings) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    SurfaceFingerprint::of(settings).hash(&mut h);
    h.finish()
}

/// Process-wide memo for [`surface_stats`]: `(fingerprint, stats)`. `None` until
/// the first call; recomputed only when the fingerprint changes.
static SURFACE_CACHE: std::sync::OnceLock<
    std::sync::Mutex<Option<(SurfaceFingerprint, SurfaceStats)>>,
> = std::sync::OnceLock::new();

/// Do the actual rebuild+serialize of both advertised surfaces. Only reached on
/// a cache miss (settings changed) — see [`surface_stats`].
fn compute_surface_stats() -> SurfaceStats {
    let mcp = tools();
    let offload = crate::offload::tools::graph_tools::defs();
    SurfaceStats {
        mcp_tools: mcp.len(),
        mcp_chars: serde_json::to_string(&mcp).map(|s| s.len()).unwrap_or(0),
        offload_tools: offload.len(),
        offload_chars: serde_json::to_string(&offload)
            .map(|s| s.len())
            .unwrap_or(0),
    }
}

/// Measure the advertised tool surface for both consumers (V17 Phase E). Reads
/// live settings, so it reflects the current `lean_tools` / graph / checks state.
///
/// Memoized process-wide behind a [`SurfaceFingerprint`]: the value only changes
/// when settings toggle tools on/off, but this is polled every ~2 s by the
/// Overview section (via `graph_usage_advice`). So on the steady poll we compute
/// only the cheap fingerprint (a settings read that already happens) and reuse
/// the cached `SurfaceStats` instead of rebuilding + `serde_json::to_string`-ing
/// both full tool lists. A settings change flips the fingerprint and forces a
/// one-shot recompute, so the cache can never serve stale numbers.
pub fn surface_stats() -> SurfaceStats {
    let settings = current_settings();
    let fp = SurfaceFingerprint::of(&settings);
    let cell = SURFACE_CACHE.get_or_init(|| std::sync::Mutex::new(None));
    // Poisoning is harmless here (the cached value is immutable data), so recover
    // the guard rather than propagating a panic from an unrelated caller.
    let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((cached_fp, stats)) = guard.as_ref() {
        if *cached_fp == fp {
            return stats.clone();
        }
    }
    // Miss: recompute under the lock (rare — only on a settings toggle) and cache.
    let stats = compute_surface_stats();
    *guard = Some((fp, stats.clone()));
    stats
}

#[cfg(test)]
mod surface_tests {
    use super::super::tools::{lean_filter, run_tool, tool_specs, LEAN_HIDDEN};
    use super::*;
    use crate::graph::index::GraphIndex;
    use crate::graph::{parse_file, Lang};
    use crate::offload::toolclass::CallGuards;

    /// The lean-hidden five must all be real, dispatchable tool names, and none
    /// may be a workhorse — the guard the E0 decision rests on.
    #[test]
    fn lean_hidden_are_real_non_workhorse_tools() {
        let names: Vec<&str> = tool_specs().iter().map(|s| s.name).collect();
        for h in LEAN_HIDDEN {
            assert!(
                names.contains(h),
                "LEAN_HIDDEN tool `{h}` is not in tool_specs()"
            );
        }
        const WORKHORSES: &[&str] = &[
            "graph_find_symbol",
            "graph_callers",
            "graph_callees",
            "graph_outline",
            "graph_snippet",
            "graph_references",
            "graph_search_docs",
        ];
        for w in WORKHORSES {
            assert!(
                !LEAN_HIDDEN.contains(w),
                "workhorse `{w}` must never be lean-hidden"
            );
        }
        assert_eq!(LEAN_HIDDEN.len(), 5);
    }

    /// `lean_filter(_, true)` removes EXACTLY the hidden five and nothing else;
    /// `false` is a no-op.
    #[test]
    fn lean_filter_hides_exactly_lean_hidden() {
        let full: Vec<&str> = tool_specs().iter().map(|s| s.name).collect();
        let passed: Vec<&str> = lean_filter(tool_specs(), false)
            .iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(full, passed, "lean=false must be a no-op");

        let lean: Vec<String> = lean_filter(tool_specs(), true)
            .iter()
            .map(|s| s.name.to_string())
            .collect();
        let expected: Vec<String> = tool_specs()
            .iter()
            .filter(|s| !LEAN_HIDDEN.contains(&s.name))
            .map(|s| s.name.to_string())
            .collect();
        assert_eq!(lean, expected);
        for h in LEAN_HIDDEN {
            assert!(!lean.iter().any(|n| n == h), "`{h}` should be hidden");
        }
        assert_eq!(lean.len(), tool_specs().len() - LEAN_HIDDEN.len());
    }

    /// `surface_stats()` reports exactly the serialized len + count of what each
    /// consumer actually advertises.
    #[test]
    fn surface_stats_match_the_advertised_json() {
        let s = surface_stats();
        let mcp = tools();
        assert_eq!(s.mcp_tools, mcp.len());
        assert_eq!(s.mcp_chars, serde_json::to_string(&mcp).unwrap().len());
        let offload = crate::offload::tools::graph_tools::defs();
        assert_eq!(s.offload_tools, offload.len());
        assert_eq!(
            s.offload_chars,
            serde_json::to_string(&offload).unwrap().len()
        );
        assert!(s.mcp_chars >= 2);
    }

    /// Hiding is advertisement-only: `run_tool` still answers a hidden name —
    /// the dispatch path is name-driven and never consults `lean_tools`.
    #[test]
    fn dispatch_still_answers_a_hidden_name() {
        let dir = std::env::temp_dir().join(format!("lean-dispatch-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        idx.index_file_graph(&parse_file("src/x.rs", "pub fn lonely() {}\n", Lang::Rust))
            .expect("index");
        let out = run_tool(
            &idx,
            &dir,
            "graph_dead_exports",
            &serde_json::json!({}),
            50,
            200,
            None,
            None,
            CallGuards::clean(),
            crate::activity::Attribution::Unattributed,
        )
        .expect("hidden tool still dispatches");
        assert!(!out.starts_with("unknown graph tool"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Same ambient settings → repeated `surface_stats()` calls agree. The
    /// second call is served from the memo (can't observe the skipped rebuild
    /// directly), and must be byte-identical to the first.
    #[test]
    fn surface_stats_is_stable_across_calls() {
        let a = surface_stats();
        let b = surface_stats();
        assert_eq!(
            a, b,
            "cached surface stats must equal the first computation"
        );
    }

    /// The fingerprint must move when — and only when — a gating input changes,
    /// so the memo can never serve stale numbers past a settings toggle. Toggling
    /// each of the five gates flips the fingerprint; a non-gating field does not.
    #[test]
    fn fingerprint_covers_every_gating_input() {
        use crate::settings::Settings;
        let base = Settings::default();
        let base_fp = SurfaceFingerprint::of(&base);

        // Each gating toggle must produce a distinct fingerprint.
        let mut s = base.clone();
        s.graph.enabled = !s.graph.enabled;
        assert_ne!(
            SurfaceFingerprint::of(&s),
            base_fp,
            "graph.enabled must be in the fingerprint"
        );

        let mut s = base.clone();
        s.graph.semantic_search = !s.graph.semantic_search;
        assert_ne!(
            SurfaceFingerprint::of(&s),
            base_fp,
            "semantic_search must be in the fingerprint"
        );

        let mut s = base.clone();
        s.graph.embed_code_bodies = !s.graph.embed_code_bodies;
        assert_ne!(
            SurfaceFingerprint::of(&s),
            base_fp,
            "embed_code_bodies must be in the fingerprint"
        );

        let mut s = base.clone();
        s.graph.lean_tools = !s.graph.lean_tools;
        assert_ne!(
            SurfaceFingerprint::of(&s),
            base_fp,
            "lean_tools must be in the fingerprint"
        );

        let mut s = base.clone();
        s.checks = vec![crate::checks::CheckDef {
            name: "cargo".to_string(),
            cmd: "cargo check".to_string(),
            ..Default::default()
        }];
        assert_ne!(
            SurfaceFingerprint::of(&s),
            base_fp,
            "checks emptiness must be in the fingerprint"
        );

        // **V39 Phase C** — a Remote-offload tab changes what `offload_task`
        // advertises (one more backend, under a name of the user's choosing), so
        // setting the role, renaming the backend and re-tiering it must each
        // move the fingerprint. Without this the pulse gate would see no move
        // and every live session would keep the old backend list until its next
        // restart, which is exactly what locked decision 15 promises does not
        // happen.
        let facade = |name: &str, tier| {
            let mut s = base.clone();
            let mut tab = crate::settings::facade_tab("t1", name);
            if let crate::settings::TabConfig::AiTool(c) = &mut tab {
                c.delegation_backend.tier = tier;
            }
            s.tabs.push(tab);
            SurfaceFingerprint::of(&s)
        };
        let fp = facade("lan-worker-2", crate::settings::BackendTier::Quality);
        assert_ne!(fp, base_fp, "a Remote-offload tab must be in the fingerprint");
        assert_ne!(
            fp,
            facade("lan-worker-3", crate::settings::BackendTier::Quality),
            "renaming a facade backend changes the advertised bytes"
        );
        assert_ne!(
            fp,
            facade("lan-worker-2", crate::settings::BackendTier::Fast),
            "re-tiering a facade changes what the router is told about it"
        );

        // A field that does NOT change the advertised surface must NOT move it —
        // otherwise the cache would recompute needlessly on unrelated edits.
        let mut s = base.clone();
        s.graph.max_rows_per_query = s.graph.max_rows_per_query.wrapping_add(1);
        assert_eq!(
            SurfaceFingerprint::of(&s),
            base_fp,
            "a non-gating setting must not change the fingerprint"
        );
    }

    /// E5 helper: print the measured surface so the before/after editorial
    /// numbers are recordable via `-- --nocapture`. Always passes.
    #[test]
    fn print_surface_stats() {
        let s = surface_stats();
        eprintln!(
            "SURFACE_STATS mcp_tools={} mcp_chars={} offload_tools={} offload_chars={}",
            s.mcp_tools, s.mcp_chars, s.offload_tools, s.offload_chars
        );
    }
}
