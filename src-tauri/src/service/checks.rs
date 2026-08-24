//! The `run_check` editor's use cases: detect, apply, nudge, dry-run,
//! validate (V22 Phases D and E).
//!
//! ## What the wrap found here
//!
//! One real coupling and one false one. The false one is the settings handle:
//! four of these six commands took `State<'_, AppState>` to reach it and
//! nothing else, so they were WebView-only for no reason at all.
//!
//! The real one is the warm code-graph index, reached for one thing — the
//! per-language file counts that turn a marker file into evidence
//! (`checks_lang_stats`). That is [`ChecksLangStats`], a second narrow host
//! trait beside [`GraphIndexHost`](crate::service::sink::GraphIndexHost)
//! rather than another method on it: the two have different callers and
//! nothing in common but their implementor, and a shared trait would make
//! every settings save name a capability it never uses.
//!
//! **It is a trait and not a passed-in value** because of one early return.
//! [`ChecksService::suggestion`] answers `count: 0` for a project that already
//! has checks configured, BEFORE it resolves the root or asks for stats;
//! computing the stats at the wire boundary and handing them in would have run
//! that work — and `resolve_graph_root`'s fallible `current_dir()` — on a path
//! that used to return early. A lazily-called trait keeps the order the command
//! had.
//!
//! ## What did NOT change
//!
//! [`ChecksService::apply_proposals`] still validates EVERY incoming
//! [`CheckDef`] before the settings write, not after and not only in the
//! editor: a bad `regex-custom` pattern or a `cwd` escaping the project root is
//! refused at this boundary because the frontend's copy of the rule is a
//! courtesy and this one is the enforcement. And the dry run still builds its
//! sandbox from the LIVE settings (locked decision L2 — the OS boundary belongs
//! to the seam, not to who clicked), so "Test" is sandboxed exactly as a real
//! `run_check` is.

use std::path::Path;

use crate::error::{AppError, AppResult};
use crate::service::project_root;
use crate::settings::SettingsHandle;

/// V22 Phase D: the passive-nudge payload for the Code Intelligence chip.
/// `count` is the number of VALID detection proposals for a project whose
/// `checks` is empty; the chip renders only when `count > 0 && !dismissed`.
#[derive(serde::Serialize)]
pub struct ChecksSuggestion {
    pub count: usize,
    pub dismissed: bool,
    pub auto_configure: bool,
}

/// V22 Phase D: [`ChecksService::apply_proposals`]' result — the check names
/// actually written (added or refreshed) after the `auto`-ownership merge.
#[derive(serde::Serialize)]
pub struct ApplySummary {
    pub applied: Vec<String>,
}

/// The warm code-graph index's per-language file counts, as language detection
/// needs them. See the module docs for why this is its own trait, and why it is
/// a trait rather than a computed value handed in.
pub trait ChecksLangStats: Send + Sync {
    fn checks_lang_stats(&self, root: &Path) -> Vec<crate::checks::detect::LangStat>;
}

impl ChecksLangStats for std::sync::Arc<crate::graph::GraphService> {
    fn checks_lang_stats(&self, root: &Path) -> Vec<crate::checks::detect::LangStat> {
        crate::graph::GraphService::checks_lang_stats(self, root)
    }
}

/// The checks-editor use cases, over one borrowed handle — same shape and
/// rationale as [`crate::service::tabs::TabService`].
pub struct ChecksService<'a> {
    settings: &'a SettingsHandle,
}

impl<'a> ChecksService<'a> {
    pub fn new(settings: &'a SettingsHandle) -> Self {
        Self { settings }
    }

    /// V22 Phase D: merge selected proposal checks into the project's `checks`
    /// setting (by `name`, honoring the `auto`-ownership rule — a user-owned
    /// `auto == false` entry is never overwritten) through the normal settings
    /// path, which lands the change as a per-project `.cimp/config.json`
    /// overlay diff. Returns the names actually written.
    pub fn apply_proposals(&self, checks: Vec<crate::checks::CheckDef>) -> AppResult<ApplySummary> {
        // Defense in depth: reject a malformed selection (bad `regex-custom`,
        // escaping `cwd`/`report_file`) before it lands in settings.
        for def in &checks {
            def.validate()?;
        }
        let mut applied = Vec::new();
        self.settings.mutate(|s| {
            applied = crate::checks::detect::merge_auto(&mut s.checks, checks);
        });
        Ok(ApplySummary { applied })
    }

    /// V22 Phase D: the passive nudge. When the project's `checks` is empty and
    /// the user hasn't dismissed it, returns how many VALID proposals detection
    /// finds so the Code Intelligence chip can show "N suggested checks".
    /// Chosen as a queryable use case over wiring the count into the
    /// `graph-status` event — far less invasive. Runs the scan off-thread.
    pub async fn suggestion(
        &self,
        root: Option<String>,
        langs: &dyn ChecksLangStats,
    ) -> AppResult<ChecksSuggestion> {
        let snap = self.settings.current();
        let dismissed = snap.checks_suggestion_dismissed;
        let auto_configure = snap.checks_auto_configure;
        // Only offer for a project with no checks configured yet.
        if !snap.checks.is_empty() {
            return Ok(ChecksSuggestion {
                count: 0,
                dismissed,
                auto_configure,
            });
        }
        let root = project_root(root)?;
        let stats = langs.checks_lang_stats(&root);
        let count = tokio::task::spawn_blocking(move || {
            crate::checks::detect::detect(&root, &stats)
                .into_iter()
                .filter(|p| p.valid)
                .count()
        })
        .await
        .map_err(|e| AppError::Checks(format!("detection task failed: {e}")))?;
        Ok(ChecksSuggestion {
            count,
            dismissed,
            auto_configure,
        })
    }

    /// V22 Phase D: remember that the user dismissed the suggestion nudge for
    /// this project (persists via the per-project overlay). Idempotent.
    pub fn dismiss_suggestion(&self) -> AppResult<()> {
        self.settings
            .mutate(|s| s.checks_suggestion_dismissed = true);
        Ok(())
    }

    /// V22 Phase E: dry-run one (possibly unsaved) [`CheckDef`](crate::checks::CheckDef)
    /// through the ordinary `checks::run` path (`changed_only = false`) so the
    /// Settings "Test" button can show exit status, the parsed diagnostic
    /// count, the first few diagnostics, and the captured output sizes — the
    /// last of which lets the UI flag a wrong-parser config (output produced,
    /// zero diagnostics). A validation/spawn failure is folded into the
    /// result's `error` field, not returned as an `Err`, so the editor renders
    /// every outcome inline. `root` defaults to the launch directory.
    ///
    /// V33: the Test button's dry run gets the SAME OS sandbox a real
    /// `run_check` gets (locked decision L2 — the boundary belongs to the seam,
    /// not to who clicked), built from the live settings.
    pub async fn test(
        &self,
        root: Option<String>,
        def: crate::checks::CheckDef,
    ) -> AppResult<crate::checks::ChecksTestResult> {
        let root = project_root(root)?;
        let sandbox = crate::sandbox::SandboxCfg::from_settings(&self.settings.current());
        Ok(crate::checks::test_check(&root, &def, &sandbox).await)
    }
}

/// V22 Phase D: detect the project's languages/tooling and return `run_check`
/// proposals (marker + code-graph evidence, PATH-validated — invalid ones carry
/// a `reason` for greying). Read-only; the bounded filesystem + PATH scan runs
/// on a blocking thread so the async reactor (and the UI) stays responsive. No
/// network. `root` defaults to the launch directory, mirroring `graph_rebuild`.
///
/// Free rather than a [`ChecksService`] method: it reads no settings, so a
/// service would be a handle it never touches.
pub async fn detect(
    root: Option<String>,
    langs: &dyn ChecksLangStats,
) -> AppResult<Vec<crate::checks::detect::Proposal>> {
    let root = project_root(root)?;
    let stats = langs.checks_lang_stats(&root);
    tokio::task::spawn_blocking(move || crate::checks::detect::detect(&root, &stats))
        .await
        .map_err(|e| AppError::Checks(format!("detection task failed: {e}")))
}

/// V22 Phase C/E: validate a `regex-custom` pattern for the ChecksEditor's live
/// (debounced) feedback — the exact same check the save path
/// (`CheckDef::validate` → `parsers::validate_pattern`) applies, so the UI error
/// matches what a save would reject. `Ok(())` when the pattern compiles and
/// declares the mandatory `file`/`line`/`message` named groups; the `Err` string
/// is ready to display.
pub fn validate_pattern(pattern: &str) -> Result<(), String> {
    crate::checks::parsers::validate_pattern(pattern)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::{CheckDef, ParserKind};
    use crate::settings::Settings;
    use std::path::PathBuf;
    use uuid::Uuid;

    /// A throwaway directory to point [`SettingsHandle`] at, so the debounced
    /// saver writes its `.cimp/config.json` somewhere disposable.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("cimp-chksvc-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&path).expect("scratch dir");
            Self(path)
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct Fixture {
        settings: SettingsHandle,
        scratch: ScratchDir,
    }

    impl Fixture {
        fn new() -> Self {
            let scratch = ScratchDir::new();
            let defaults = Settings::default();
            let settings = SettingsHandle::new(defaults.clone(), defaults, scratch.0.clone());
            Self { settings, scratch }
        }

        fn service(&self) -> ChecksService<'_> {
            ChecksService::new(&self.settings)
        }

        fn check_names(&self) -> Vec<String> {
            self.settings
                .current()
                .checks
                .iter()
                .map(|c| c.name.clone())
                .collect()
        }
    }

    /// An index that has never seen a file. Detection then rests on marker
    /// files alone, which is what a cold project looks like — and it keeps this
    /// test off the real `GraphService`, which cannot be built without a Tauri
    /// app.
    struct NoLangStats;

    impl ChecksLangStats for NoLangStats {
        fn checks_lang_stats(&self, _root: &Path) -> Vec<crate::checks::detect::LangStat> {
            Vec::new()
        }
    }

    fn a_check(name: &str, auto: bool) -> CheckDef {
        CheckDef {
            name: name.to_string(),
            cmd: "cargo check".to_string(),
            auto,
            ..CheckDef::default()
        }
    }

    /// **Previously "user clicks in the app".** The checks editor's apply flow:
    /// the Detect panel proposes, the user ticks some rows, and Apply writes
    /// them. The two rules that make it safe had no test — a malformed row must
    /// be refused BEFORE the settings write (the editor's own validation is a
    /// courtesy; this is the enforcement), and a re-apply must never overwrite a
    /// check the user has taken ownership of.
    #[tokio::test]
    async fn apply_proposals_validates_first_and_respects_user_ownership() {
        let fixture = Fixture::new();
        let svc = fixture.service();

        // A `regex-custom` row whose pattern declares none of the mandatory
        // groups is refused, and nothing lands — not even the valid row beside
        // it, because validation runs over the whole selection first.
        let bad = CheckDef {
            parser: ParserKind::RegexCustom,
            pattern: Some("no groups here".to_string()),
            ..a_check("custom", true)
        };
        assert!(svc
            .apply_proposals(vec![a_check("cargo", true), bad])
            .is_err());
        assert!(
            fixture.check_names().is_empty(),
            "a refused selection must write nothing at all"
        );

        // A clean selection lands and reports what it wrote.
        let summary = svc
            .apply_proposals(vec![a_check("cargo", true), a_check("eslint", true)])
            .expect("apply");
        assert_eq!(summary.applied, vec!["cargo", "eslint"]);
        assert_eq!(fixture.check_names(), vec!["cargo", "eslint"]);

        // The user edits `cargo` in the editor, which clears `auto`…
        fixture.settings.mutate(|s| {
            let owned = s.checks.iter_mut().find(|c| c.name == "cargo").unwrap();
            owned.auto = false;
            owned.cmd = "cargo clippy".to_string();
        });
        // …and a re-detection must not fight that edit.
        svc.apply_proposals(vec![a_check("cargo", true)])
            .expect("re-apply");
        let after = fixture.settings.current();
        let cargo = after.checks.iter().find(|c| c.name == "cargo").unwrap();
        assert_eq!(
            cargo.cmd, "cargo clippy",
            "a user-owned check owns its own name"
        );
    }

    /// **Previously "user clicks in the app".** The Code Intelligence chip's
    /// nudge: it offers only for a project with no checks configured, and the
    /// Dismiss button has to survive as a per-project setting.
    #[tokio::test]
    async fn the_suggestion_nudge_only_offers_for_an_unconfigured_project() {
        let fixture = Fixture::new();
        let root = Some(fixture.scratch.0.to_string_lossy().into_owned());

        let fresh = fixture
            .service()
            .suggestion(root.clone(), &NoLangStats)
            .await
            .expect("suggestion");
        assert!(!fresh.dismissed);

        fixture.service().dismiss_suggestion().expect("dismiss");
        let after = fixture
            .service()
            .suggestion(root.clone(), &NoLangStats)
            .await
            .expect("suggestion");
        assert!(after.dismissed, "Dismiss persists into the project overlay");

        // Once the project HAS checks, the count is zero regardless of what
        // detection would have found — and the early return happens before the
        // scan, so an unresolvable root cannot make this fail.
        fixture
            .service()
            .apply_proposals(vec![a_check("cargo", true)])
            .expect("apply");
        let configured = fixture
            .service()
            .suggestion(Some(String::new()), &NoLangStats)
            .await
            .expect("suggestion");
        assert_eq!(configured.count, 0);
    }

    /// The editor's live pattern feedback is the same rule the save path
    /// applies, so a pattern the editor calls good cannot be refused on save.
    #[test]
    fn pattern_validation_matches_the_save_path() {
        let good = r"(?P<file>[^:]+):(?P<line>\d+): (?P<message>.*)";
        assert!(validate_pattern(good).is_ok());
        assert!(validate_pattern("no named groups").is_err());

        let as_a_check = CheckDef {
            parser: ParserKind::RegexCustom,
            pattern: Some(good.to_string()),
            ..a_check("custom", false)
        };
        assert!(as_a_check.validate().is_ok());
    }
}
