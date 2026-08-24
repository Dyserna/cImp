//! V38 Phase A — the `plugin` Events lane: what a plugin scan puts in front of
//! the user.
//!
//! Row **building** is separate from row **recording** (the
//! `offload::supervisor::lifecycle_record` precedent), so what a scan produces
//! is assertable without touching the on-disk activity store — the alternative
//! is a feed tested by re-deriving it beside itself.
//!
//! Column conventions, which the frontend mirrors (`src/lib/activity.ts`,
//! `EventsView.svelte`):
//! * `source` — the manifest **file name** for a per-file row (two joined by
//!   `+` for a conflict): the file is the thing the user opens to fix this.
//!   [`SCAN_SOURCE`] for the summary row, whose subject is the folder.
//! * `tool` — the verb: [`REJECTED`], [`CONFLICT`], [`RESCAN`], [`OVERLAY`].
//! * `target` — the human-readable *why*: the rejection reason, or
//!   `loaded N · rejected M` for the summary.
//! * `chars` — always 0. A plugin row has no payload, and printing "0 chars"
//!   would dress a structural zero up as a measurement (the `offload_server`
//!   lesson); the frontend's `rowMeta` arm suppresses it.
//! * `ms` — the scan's duration on the summary row, 0 on per-file rows.
//! * `ok` — `false` for every rejection. On the SUMMARY row it is the
//!   **folder's** health, not the scan's: the scan always completes, so
//!   reporting that would be a green row that says nothing. A single glance at
//!   the summary answering "is my plugins folder clean?" is the useful claim,
//!   and it is the one pinned by test.
//! * `root` — always empty. A plugin folder is genuinely not about a project —
//!   one of the two things that sentinel is documented to mean
//!   (`ActivityEntry::root`).
//! * `tab` — always `Attribution::Headless`. Scanning is cImp's own work; it
//!   has no tab behind it even when the user pressed Rescan, and inventing one
//!   would be a worse lie than the honest "no tab".

use crate::activity::{ActivityEntry, ActivityKind, ActivityRecord, Attribution};

use super::loader::{PluginError, PluginErrorKind, PluginSet};

/// The `source` of the per-scan summary row: the folder, not a file.
pub const SCAN_SOURCE: &str = "plugins";

/// `tool` verbs. Closed set — the frontend keys nothing off them today, but a
/// verb renamed here without the row conventions following is exactly the drift
/// the `offload_server` lane documented.
pub const REJECTED: &str = "rejected";
pub const CONFLICT: &str = "conflict";
pub const RESCAN: &str = "rescan";
/// V38 Phase B: a project's `.cimp/config.json` carried machine-scope
/// tool-plugin fields, and they were ignored on load.
pub const OVERLAY: &str = "overlay";
/// V38 Phase G: a registered `command`-kind tool could not supersede the
/// `command_allowlist` because it has no binary path configured.
pub const SKIPPED: &str = "skipped";

/// How much of a reason fits in the `target` column before the rest is left to
/// the detail popup. Reasons are written to be read (they name files and say
/// what to do), so they can run long.
const TARGET_CHARS: usize = 160;

fn headline(s: &str) -> String {
    if s.chars().count() <= TARGET_CHARS {
        return s.to_string();
    }
    let cut: String = s.chars().take(TARGET_CHARS).collect();
    format!("{cut}…")
}

fn row(source: String, tool: &str, target: String, ok: bool, ms: u64, detail: String) -> ActivityRecord {
    ActivityRecord {
        entry: ActivityEntry::new(
            ActivityKind::Plugin,
            crate::activity::now_ms(),
            String::new(),
            source,
            tool.to_string(),
            target,
            0,
            ms,
            ok,
            Attribution::Headless,
            None,
            // V37 added `server`/`category`: MCP-row columns. A plugin
            // discovery row is about a manifest file, not about a server.
            None,
            None,
        ),
        request: String::new(),
        response: detail,
    }
}

/// The row one rejected manifest produces.
pub fn error_row(e: &PluginError) -> ActivityRecord {
    let tool = match e.kind {
        PluginErrorKind::Conflict => CONFLICT,
        _ => REJECTED,
    };
    // The identity when the file had one, else the file name — a plugin whose
    // JSON did not parse has no trustworthy name, and guessing one would put a
    // fabricated identity in the audit trail.
    let source = e.key.clone().unwrap_or_else(|| e.file_names());
    let detail = format!("{}\n\n{}", e.paths.join("\n"), e.reason);
    row(source, tool, headline(&e.reason), false, 0, detail)
}

/// The one summary row a scan produces, whatever else it produced.
///
/// Minted even when the folder is empty and clean: an empty lane cannot
/// distinguish "the scan found nothing" from "no scan ever ran", and that
/// ambiguity is what the V33 sandbox lane added its positive rows to remove.
pub fn summary_row(set: &PluginSet) -> ActivityRecord {
    let loaded = set.plugins.len();
    let rejected = set.errors.len();
    let target = format!("loaded {loaded} · rejected {rejected}");
    let detail = if set.dir.is_empty() {
        "no plugins directory could be resolved (cImp could not determine its own directory)"
            .to_string()
    } else {
        format!("scanned {}", set.dir)
    };
    row(
        SCAN_SOURCE.to_string(),
        RESCAN,
        target,
        rejected == 0,
        set.scan_ms,
        detail,
    )
}

/// Every row one scan produces: the rejections first (they are what a reader
/// came for), then the summary.
pub fn scan_rows(set: &PluginSet) -> Vec<ActivityRecord> {
    let mut rows: Vec<ActivityRecord> = set.errors.iter().map(error_row).collect();
    rows.push(summary_row(set));
    rows
}

/// Record what a scan found. Called by `PluginStore::rescan` — startup and the
/// manual Rescan go through the same path, so the two cannot disagree.
pub fn record_scan(set: &PluginSet) {
    for r in scan_rows(set) {
        crate::activity::record_bg(r);
    }
}

/// The row for a registered `command`-kind tool that was passed over because it
/// has no binary path, while `run_command` ran the same program name through
/// the `command_allowlist` and PATH instead (V38 Phase D review, A-D1).
///
/// **The interesting half is that the run SUCCEEDED.** A refusal explains
/// itself in its own error text; this case does not — the model got its output,
/// and the only thing that went wrong is that the binary the user meant to
/// register was not the binary that ran. Without this row the supersession the
/// user configured evaporates wordlessly, which is the same class of silence
/// the `plugin` lane exists to break.
///
/// `ok: false` for the same reason the rejection rows carry it: nothing failed,
/// but the configuration did not do what it looks like it does.
pub fn command_skipped_row(tool_key: &str, label: &str, command: &str) -> ActivityRecord {
    let reason = format!(
        "`{command}` ran through the command allowlist (resolved on PATH) because the \
         registered tool `{label}` has no binary path configured — a registered tool only \
         supersedes the allowlist once you point it at a file. Set its path in \
         Settings -> Tool Plugins."
    );
    row(
        tool_key.to_string(),
        SKIPPED,
        headline(&reason),
        false,
        0,
        reason,
    )
}

/// Record [`command_skipped_row`] **once per (tool, command) per process**.
///
/// `run_command` is model-driven and repeats freely, so a row per call would
/// crowd this lane out of its own retention window with one fact restated. The
/// dedup key is the pair rather than the tool alone: two allowlisted names
/// shadowing the same inert tool are two distinct configuration mistakes.
/// (`sandbox::record_skip`'s rule, and its `Mutex<Option<HashSet>>` shape.)
pub fn record_command_skipped(tool_key: &str, label: &str, command: &str) {
    use std::collections::HashSet;
    use std::sync::Mutex;
    static EMITTED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    if !crate::sandbox::once_per_session(
        &EMITTED,
        format!("{tool_key}|{}", command.to_ascii_lowercase()),
    ) {
        return;
    }
    crate::activity::record_bg(command_skipped_row(tool_key, label, command));
}

/// The row for machine-scope tool-plugin fields found in a project overlay and
/// dropped (`settings::persistence`'s structured strip).
///
/// **ONE row per load, listing every offending field** — not one row per field.
/// A hand-edited (or stale) `.cimp/config.json` typically carries a whole block
/// at once, and a row per key would bury the fact that they were all ignored
/// under the noise of saying so eight times. The names go in `target` up to the
/// headline width, and all of them in the detail.
///
/// `ok: false` because this IS a discrepancy the user should resolve: the file
/// says something cImp will not honour, so what they see in that file is not
/// what the machine does.
pub fn overlay_strip_row(overlay_path: &str, dropped: &[String]) -> ActivityRecord {
    let target = format!(
        "ignored {} machine-scope field{} in the project config: {}",
        dropped.len(),
        if dropped.len() == 1 { "" } else { "s" },
        dropped.join(", ")
    );
    let detail = format!(
        "{overlay_path}\n\n{}\n\nBinary paths, enables and timeouts for tool plugins are \
         machine scope: they describe this machine, and a project's config file lives inside \
         the sandbox boundary a confined tool can write to. Only declared variable \
         values and extra parameters ride a project's config; set the rest in \
         Settings → Tool Plugins.",
        dropped.join("\n")
    );
    row(
        SCAN_SOURCE.to_string(),
        OVERLAY,
        headline(&target),
        false,
        0,
        detail,
    )
}

/// Mint [`overlay_strip_row`] — no-op when the overlay carried nothing to drop,
/// so a clean project never speaks.
pub fn record_overlay_strip(overlay_path: &str, dropped: &[String]) {
    if dropped.is_empty() {
        return;
    }
    crate::activity::record_bg(overlay_strip_row(overlay_path, dropped));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::loader::PluginError;

    /// A plugin file path spelled the way the RUNNING OS spells one.
    ///
    /// The behaviour these fixtures drive — sourcing a row by its file NAME —
    /// is cross-platform, so the fixture has to be too: `C:\cimp\plugins\a.json`
    /// is a single opaque file name on Linux (a backslash is an ordinary
    /// character there), so `file_name()` answers the whole string and the
    /// assertion would be about nothing. Built with `join` from the temp
    /// directory instead, it is a real, native, absolute path on both.
    fn plugin_file(name: &str) -> String {
        std::env::temp_dir()
            .join("cimp")
            .join("plugins")
            .join(name)
            .to_string_lossy()
            .into_owned()
    }

    fn err(kind: PluginErrorKind, paths: &[&str], key: Option<&str>, reason: &str) -> PluginError {
        PluginError {
            kind,
            paths: paths.iter().map(|p| p.to_string()).collect(),
            key: key.map(str::to_string),
            reason: reason.to_string(),
        }
    }

    fn set_with(errors: Vec<PluginError>, loaded: usize) -> PluginSet {
        let mut s = PluginSet {
            dir: "C:\\cimp\\plugins".to_string(),
            scan_ms: 7,
            ..PluginSet::default()
        };
        s.errors = errors;
        // `plugins` needs a length only; the row builders read nothing else.
        for i in 0..loaded {
            let text = format!(
                r#"{{"manifest_version":1,"name":"p{i}","version":"1",
                    "categories":[{{"id":"c","label":"C","tools":["t"]}}],
                    "tools":[{{"id":"t","label":"T","kind":"command"}}]}}"#
            );
            let m = crate::plugins::manifest::parse(&text, crate::plugins::manifest::Provenance::User)
                .expect("fixture manifest");
            s.plugins.push(crate::plugins::loader::LoadedPlugin {
                path: format!("C:\\cimp\\plugins\\p{i}.json"),
                provenance: crate::plugins::manifest::Provenance::User,
                key: m.key(),
                manifest: m,
            });
        }
        s
    }

    /// The documented column conventions, asserted rather than described.
    #[test]
    fn plugin_rows_are_headless_rootless_and_payload_free() {
        let s = set_with(
            vec![err(
                PluginErrorKind::Invalid,
                ["C:\\cimp\\plugins\\bad.json"].as_slice(),
                None,
                "not a valid manifest: expected value at line 1",
            )],
            1,
        );
        for r in scan_rows(&s) {
            assert_eq!(r.entry.kind, ActivityKind::Plugin.as_str());
            assert_eq!(
                r.entry.root, "",
                "a plugin folder is not about a project — see ActivityEntry::root"
            );
            assert!(matches!(r.entry.tab, Attribution::Headless));
            assert!(r.entry.session.is_none());
            assert_eq!(r.entry.chars, 0, "a plugin row carries no payload");
            assert!(r.request.is_empty());
        }
    }

    /// V38 Phase B: ONE row per load naming every ignored field, and it is not
    /// green — a config file that says something cImp will not honour is a
    /// discrepancy, not a normal event. Silence for a clean project.
    #[test]
    fn an_overlay_strip_is_one_row_naming_every_dropped_field() {
        let dropped = vec![
            "tool_plugins.global_paths".to_string(),
            "tool_plugins.plugins.acme@1.0.0.tools.scan.enabled".to_string(),
        ];
        let r = overlay_strip_row("C:\\repo\\.cimp\\config.json", &dropped);
        assert_eq!(r.entry.tool, OVERLAY);
        assert!(!r.entry.ok);
        assert!(r.entry.target.contains("2 machine-scope fields"), "{}", r.entry.target);
        for name in &dropped {
            assert!(
                r.entry.target.contains(name) || r.response.contains(name),
                "`{name}` was dropped but the row never says so"
            );
        }
        // The detail names the file to edit and where the setting really lives.
        assert!(r.response.contains("C:\\repo\\.cimp\\config.json"), "{}", r.response);
        assert!(r.response.contains("Settings"), "{}", r.response);
        // Singular reads as singular — a row that says "1 fields" is a row
        // nobody trusts to have counted.
        let one = overlay_strip_row("x", &["tool_plugins.global_paths".to_string()]);
        assert!(one.entry.target.contains("1 machine-scope field in"), "{}", one.entry.target);
    }

    /// A rejection is never green, and the reason must reach the popup intact
    /// even when the column truncates it.
    #[test]
    fn a_rejection_is_not_ok_and_keeps_its_full_reason_in_the_detail() {
        let long = format!("something went wrong: {}", "x".repeat(400));
        let e = err(
            PluginErrorKind::Invalid,
            ["C:\\cimp\\plugins\\bad.json"].as_slice(),
            Some("bad@1.0.0"),
            &long,
        );
        let r = error_row(&e);
        assert!(!r.entry.ok);
        assert_eq!(r.entry.tool, REJECTED);
        assert_eq!(
            r.entry.source, "bad@1.0.0",
            "an identified plugin is sourced by identity"
        );
        assert!(r.entry.target.chars().count() <= TARGET_CHARS + 1);
        assert!(
            r.response.contains(&long),
            "the full reason must survive into the detail popup"
        );
        assert!(r.response.contains("bad.json"), "and so must the file path");
        assert_eq!(r.entry.ms, 0, "a per-file row is not a duration");
    }

    /// A file that did not parse has no trustworthy identity, so the row is
    /// sourced by file name rather than by a guessed one.
    #[test]
    fn an_unidentified_file_is_sourced_by_its_file_name() {
        let broken = plugin_file("broken.json");
        let e = err(
            PluginErrorKind::Invalid,
            [broken.as_str()].as_slice(),
            None,
            "not a valid manifest",
        );
        assert_eq!(error_row(&e).entry.source, "broken.json");
    }

    /// A conflict's whole value is naming both files — in the detail, where the
    /// full paths are, not just the truncated headline.
    #[test]
    fn a_conflict_row_names_every_offending_file() {
        let (a, b) = (plugin_file("a.json"), plugin_file("b.json"));
        let e = err(
            PluginErrorKind::Conflict,
            [a.as_str(), b.as_str()].as_slice(),
            Some("acme@1.0.0"),
            "2 files declare the plugin `acme@1.0.0`",
        );
        let r = error_row(&e);
        assert_eq!(r.entry.tool, CONFLICT);
        assert!(!r.entry.ok);
        assert!(r.response.contains("a.json") && r.response.contains("b.json"));
        assert_eq!(e.file_names(), "a.json + b.json");
    }

    /// The summary's `ok` is the FOLDER's health — see the module docs.
    #[test]
    fn the_summary_row_reports_the_folder_not_the_scan() {
        let clean = summary_row(&set_with(Vec::new(), 2));
        assert!(clean.entry.ok);
        assert_eq!(clean.entry.target, "loaded 2 · rejected 0");
        assert_eq!(clean.entry.tool, RESCAN);
        assert_eq!(clean.entry.source, SCAN_SOURCE);
        assert_eq!(clean.entry.ms, 7, "the summary carries the scan duration");

        let dirty = summary_row(&set_with(
            vec![err(PluginErrorKind::Invalid, ["x.json"].as_slice(), None, "nope")],
            1,
        ));
        assert!(
            !dirty.entry.ok,
            "a folder with a rejected plugin must not read as clean at a glance"
        );
        assert_eq!(dirty.entry.target, "loaded 1 · rejected 1");
    }

    /// An empty, clean folder still mints one row: an empty lane cannot say
    /// whether a scan ever ran.
    #[test]
    fn even_an_empty_scan_leaves_a_trace() {
        let rows = scan_rows(&set_with(Vec::new(), 0));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].entry.target, "loaded 0 · rejected 0");
        assert!(rows[0].response.contains("C:\\cimp\\plugins"));
    }

    /// Rejections come first: a reader scanning the feed sees the problems
    /// before the count that summarizes them.
    #[test]
    fn rejections_precede_the_summary() {
        let rows = scan_rows(&set_with(
            vec![err(PluginErrorKind::Invalid, ["x.json"].as_slice(), None, "nope")],
            0,
        ));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].entry.tool, REJECTED);
        assert_eq!(rows[1].entry.tool, RESCAN);
    }
}
