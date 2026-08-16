//! V35 Phase H — the **capture-on-success corpus**: what the L2 probes saw,
//! scrubbed, stamped with the CLI version they saw it on, kept on disk.
//!
//! # The exit criterion, in one sentence
//!
//! *A breakage's first diagnostic is a diff between the last known-good capture
//! and today's, not an investigation.* Everything in this module exists to make
//! that sentence true, and nothing in it exists for any other reason: there is
//! no UI, no setting, no notice, no schema field. It is the cheapest part of the
//! milestone's design (`DESIGN-harness-drift-canaries.md` § 4.1) and probably
//! the highest-leverage during an actual breakage, which is precisely the
//! moment nobody has time to build it.
//!
//! # Known-good is the whole semantic
//!
//! The automatic trigger writes **only after a run with zero `Fail`**. That is
//! not caution, it is the definition: the corpus is a record of shapes that were
//! *working*, and a failing run overwriting it would destroy the one artifact
//! the diff needs. The manual trigger (`cimp --harness-capture`) exists for the
//! other half of the same moment — during a breakage you want today's *broken*
//! shapes too — so it writes them to a sibling `<version>-failing/` directory
//! where they can be diffed against the known-good one without becoming it.
//!
//! The gate is the **L2 probe verdict for that harness**, not the whole
//! auto-verify report. An L1 canary failure says a committed fixture no longer
//! satisfies *our* reader — a fact about this repo, not about the bytes the
//! installed CLI just produced. Those bytes are still known-good upstream
//! shapes, and they are exactly what a reader regression needs to be debugged
//! against.
//!
//! # Where it writes, and where it must never write
//!
//! `<app-data>/harness-captures/<harness>/<cli-version>/` — see
//! [`captures_root`]. Not the repo (nothing here can reach it, so no
//! `.gitignore` entry is needed or added), and deliberately **not the exe
//! directory** either, which is where every other cImp store lives. cImp is a
//! portable app: `settings.json`, `tool-activity.jsonl`, `detection/rules.d`
//! and `logs/` are all `<exe-dir>`-relative. A capture corpus must not join
//! them, because the exe is sometimes installed on a synced network path, and
//! this is the one store whose contents are derived from a user's real session.
//! Deployment convenience is worth less than not syncing that.
//!
//! # Scrubbing is not optional and not partial
//!
//! Every byte goes through [`crate::processing::sanitize::scrub_payload`]
//! before it touches disk (milestone locked decision 4). If the credential
//! screen cannot run, **nothing is written** — read that function's docs for
//! why this path fails closed where `graph::secrets` fails open.
//!
//! What the scrubber cannot do is make a transcript line stop being a
//! transcript line. A capture still holds whatever prose, paths and code the
//! session put in the fields the probes read. That is stated as a residual
//! rather than designed away: it is why the corpus lives outside the repo,
//! outside any synced directory, bounded to a handful of lines per capability
//! and swept to [`KEEP_VERSIONS`] versions — and why promotion to a committed
//! fixture stays a manual, reviewed step (design doc § 3.2).

use std::path::{Path, PathBuf};

use crate::harness::contract::Harness;
use crate::harness::probe::{self, harness_name, Driven};

// ── bounds ──────────────────────────────────────────────────────────────────

/// How many known-good version directories are kept per harness. Older ones are
/// swept at capture time.
///
/// **Eight.** The corpus answers "what did this look like before it broke", and
/// the useful window is the last few upstream releases — Claude Code ships
/// several a week, so eight is roughly a fortnight of history and a couple of
/// hundred KiB. The repo has a standing "unbounded growth is a smell"
/// principle; a diagnostic store nobody ever opens is exactly where an unbounded
/// lane hides, so this is a cap rather than a policy note.
pub const KEEP_VERSIONS: usize = 8;

/// How many `-failing` directories are kept, **swept independently** of
/// [`KEEP_VERSIONS`].
///
/// A separate, smaller cap and not a shared one, for a specific reason: a
/// breakage produces failing captures in bursts (the user runs
/// `--harness-capture`, changes something, runs it again), and a shared cap
/// would let that burst evict the known-good history it is meant to be diffed
/// against. The failing dirs are working notes; the known-good ones are the
/// evidence.
pub const KEEP_FAILING: usize = 3;

/// Suffix marking a directory as holding shapes from a run that FAILED. It is
/// in the directory name rather than only in the manifest so the distinction
/// survives a `ls`, a copy-paste into an issue, and a reader who never opens
/// the manifest.
pub const FAILING_SUFFIX: &str = "-failing";

/// How many transcript lines are captured per capability.
///
/// Three, not "all of them". The corpus records a *shape*; a fourth line of the
/// same shape adds nothing to a diff and adds a fourth line of a real user's
/// session to disk. The committed fixtures this corpus feeds are one line each.
pub const LINES_PER_CAPABILITY: usize = 3;

// ── what a probe hands over ─────────────────────────────────────────────────

/// One payload a probe observed, on its way to disk. Raw and unscrubbed —
/// scrubbing happens in [`write_into`], at the boundary, so no probe can
/// accidentally hand over a payload that skipped it.
#[derive(Debug, Clone)]
pub struct Observed {
    /// The registry capability id the payload belongs to. It **names the file**
    /// — the same join key the report, the Advisor notice, the gate and the
    /// health panel already speak, so a capture and the failure that sends you
    /// looking for it are matched by eye.
    pub capability: &'static str,
    /// File extension, chosen by the probe: `jsonl` for transcript lines,
    /// `json` for a single document, `txt` for CLI output.
    pub ext: &'static str,
    /// The payload text.
    pub text: String,
}

impl Observed {
    pub fn new(capability: &'static str, ext: &'static str, text: impl Into<String>) -> Self {
        Observed {
            capability,
            ext,
            text: text.into(),
        }
    }

    /// `<capability id>.<ext>` — the file name, derived in one place.
    fn file_name(&self) -> String {
        format!("{}.{}", self.capability, self.ext)
    }
}

/// Which directory a capture lands in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `<version>/` — the corpus. Written only by a run with zero `Fail`.
    KnownGood,
    /// `<version>-failing/` — today's broken shapes, for diffing against the
    /// known-good directory. Never overwrites it.
    Failing,
}

impl Mode {
    /// The mode a run with this many failures writes under. One function so the
    /// automatic and the manual trigger cannot disagree about what "success"
    /// means.
    pub fn of(failures: usize) -> Self {
        if failures == 0 {
            Mode::KnownGood
        } else {
            Mode::Failing
        }
    }

    fn suffix(self) -> &'static str {
        match self {
            Mode::KnownGood => "",
            Mode::Failing => FAILING_SUFFIX,
        }
    }
}

/// What one successful write produced.
#[derive(Debug, Clone)]
pub struct Written {
    pub dir: PathBuf,
    /// File names only — never contents, and never a path a caller could print
    /// a payload out of.
    pub files: Vec<String>,
    pub redactions: usize,
    pub omitted: usize,
}

// ── where it lives ──────────────────────────────────────────────────────────

/// `<app-data>/harness-captures`, or `None` when no data directory can be
/// resolved at all (in which case nothing is captured — a guessed path is worse
/// than no corpus).
///
/// See the module docs for why this is NOT `<exe-dir>`, which is where every
/// other cImp store lives.
pub fn captures_root() -> Option<PathBuf> {
    app_data_root().map(|p| p.join("harness-captures"))
}

/// The per-user, per-machine data directory: `%LOCALAPPDATA%\cimp` on Windows,
/// `$XDG_DATA_HOME/cimp` (or `~/.local/share/cimp`) elsewhere.
///
/// `LOCALAPPDATA` and not `APPDATA` on purpose — the roaming profile is
/// replicated between machines by domain policy, and a corpus derived from one
/// machine's sessions has no business travelling. Env vars rather than a new
/// `dirs` dependency, matching `oob::claude::home_dir`'s existing convention;
/// this also has to work from the `--harness-capture` early dispatch, where
/// there is no Tauri `AppHandle` to ask.
fn app_data_root() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(local) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
            return Some(PathBuf::from(local).join("cimp"));
        }
        crate::oob::claude::home_dir().map(|h| h.join("AppData").join("Local").join("cimp"))
    }
    #[cfg(not(windows))]
    {
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
            return Some(PathBuf::from(xdg).join("cimp"));
        }
        crate::oob::claude::home_dir().map(|h| h.join(".local").join("share").join("cimp"))
    }
}

// ── the automatic trigger ───────────────────────────────────────────────────

/// Capture-on-success, called from [`probe::run_for`] so **every** L2 run gets
/// it identically — the background auto-verify, the Harness health panel's
/// *Run checks now*, and `cimp --harness-canary` alike.
///
/// Silent and best-effort by construction (locked decision 8 of this phase: no
/// UI, no notice). Every reason to do nothing is a `debug!`, because a corpus
/// that started raising warnings would be a diagnostic aid demanding
/// maintenance of its own.
pub(crate) fn on_success(driven: &Driven) {
    let harness = harness_name(driven.harness);
    if driven.results.iter().any(|r| r.outcome.is_fail()) {
        tracing::debug!(
            harness,
            "harness capture skipped: the run FAILED, and a failing run must not overwrite the \
             last known-good capture — that capture is what it would be diffed against"
        );
        return;
    }
    if driven.observed.is_empty() {
        tracing::debug!(
            harness,
            "harness capture skipped: the run observed no payload (an absent CLI or a transcript \
             with nothing in the window), so there is nothing to record"
        );
        return;
    }
    let Some(root) = captures_root() else {
        tracing::debug!("harness capture skipped: no data directory could be resolved");
        return;
    };
    match write_into(&root, driven, Mode::KnownGood) {
        Ok(w) => tracing::debug!(
            harness,
            dir = %w.dir.display(),
            files = w.files.len(),
            redactions = w.redactions,
            omitted = w.omitted,
            "harness capture written"
        ),
        Err(why) => tracing::debug!(harness, why, "harness capture not written"),
    }
}

// ── the write ───────────────────────────────────────────────────────────────

/// Write one harness's observations under `root`. The seam the tests drive,
/// with an explicit root so no test can reach the real corpus.
///
/// `Err` is a *reason*, never a failure anything escalates: a capture is a
/// diagnostic convenience and losing one must not affect a probe run's verdict,
/// a version advance, or an exit code.
pub fn write_into(root: &Path, driven: &Driven, mode: Mode) -> Result<Written, String> {
    let version = driven.version.trim();
    if version.is_empty() {
        // Locked decision 6: no `unknown/` directories. A capture whose version
        // nobody knows cannot be diffed against anything, and a shared
        // `unknown/` bucket would silently mix releases into one directory —
        // which is the one thing that would make the corpus lie.
        return Err("the CLI version was not observed, so there is nothing to stamp a capture \
                    directory with"
            .to_string());
    }
    let harness_dir = root.join(harness_name(driven.harness));
    let dir = harness_dir.join(format!(
        "{}{}",
        // The version is harness-controlled text reaching a path join. One
        // definition of "safe as a single path segment", shared with the
        // updater rather than restated.
        crate::offload::detection::updater::store::sanitize_version(version),
        mode.suffix()
    ));

    let mut files: Vec<String> = Vec::new();
    let mut redactions = 0usize;
    let mut omitted = 0usize;
    for obs in &driven.observed {
        // The scrub is here and not at the probe, so there is no spelling of
        // "capture this payload" that skips it.
        let Some(scrubbed) = crate::processing::scrub_payload(&obs.text) else {
            return Err("the credential screen is unavailable (the baked rule set did not \
                        compile), so NOTHING was written — an unscreened capture is worse than \
                        no capture"
                .to_string());
        };
        redactions += scrubbed.redactions;
        omitted += scrubbed.omitted;
        let name = obs.file_name();
        crate::settings::write_atomic(&dir.join(&name), scrubbed.text.as_bytes())
            .map_err(|e| format!("{name}: {e}"))?;
        files.push(name);
    }
    if files.is_empty() {
        return Err("nothing was observed for this harness".to_string());
    }

    // The manifest describes the DIRECTORY, not just this run: a later capture
    // that could not observe a tool_result must not make the tool_result file
    // it did not overwrite look absent.
    let present = capability_files(&dir);
    crate::settings::write_atomic(
        &dir.join("MANIFEST.toml"),
        manifest(driven, mode, &present).as_bytes(),
    )
    .map_err(|e| format!("MANIFEST.toml: {e}"))?;

    sweep(&harness_dir, mode);

    Ok(Written {
        dir,
        files,
        redactions,
        omitted,
    })
}

/// Every capability file in a capture directory, sorted. The manifest's
/// `capabilities` list, and the reason it is read back from disk rather than
/// taken from the run.
fn capability_files(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "MANIFEST.toml")
        .collect();
    out.sort();
    out
}

/// The provenance file. Same discipline as the committed fixtures' manifests
/// (design doc § 3.1): **an anonymous capture is indistinguishable from a
/// guess**, and this one carries the four keys
/// `harness::canary::every_fixture_version_dir_has_a_manifest` requires, so a
/// directory promoted to `src-tauri/fixtures/harness/` passes that walker
/// without hand-editing.
fn manifest(driven: &Driven, mode: Mode, present: &[String]) -> String {
    let files = present
        .iter()
        .map(|f| format!("  \"{f}\",\n"))
        .collect::<String>();
    format!(
        "# V35 Phase H — provenance for a LIVE harness capture.\n\
         #\n\
         # Written by `cimp --harness-capture`, or automatically by a probe run\n\
         # ({}) that found no drift. Not a committed fixture: promotion to\n\
         # src-tauri/fixtures/harness/ is a manual, reviewed, hand-minimizing step\n\
         # (DESIGN-harness-drift-canaries.md 3.2). `capabilities` lists every file in\n\
         # THIS directory, including ones an earlier capture wrote.\n\
         \n\
         captured_from = \"{} {}\"\n\
         date = \"{}\"\n\
         method = \"live probe capture\"\n\
         redacted = true\n\
         redaction = \"scrubbed through processing/sanitize.rs::scrub_payload before any byte \
         reached disk: terminal escape sequences stripped, and every JSON string value (or, for \
         non-JSON text, every line) matching cImp's credential rule set (graph/secrets.yar) \
         replaced by a marker naming the rules that fired. Not anonymization: the payloads are \
         real, and still carry whatever prose, paths and code the observed session put in the \
         fields the probes read.\"\n\
         outcome = \"{}\"\n\
         cimp_version = \"{}\"\n\
         capabilities = [\n{}]\n",
        crate::harness::verify::EVIDENCE_PROBE,
        harness_name(driven.harness),
        driven.version,
        today(),
        match mode {
            Mode::KnownGood => "no capability FAILED (known-good)",
            Mode::Failing =>
                "at least one capability FAILED — these are the BROKEN shapes, kept for diffing \
                 against the sibling known-good directory",
        },
        env!("CARGO_PKG_VERSION"),
        files
    )
}

/// Today, as `YYYY-MM-DD`.
///
/// Derived from the epoch through `chrono::DateTime::from_timestamp` and
/// truncated out of the RFC 3339 form — the crate is built with
/// `default-features = false`, so `Utc::now()` (the `clock` feature) is not
/// available and this is the same route `usage::mod` already takes.
fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|t| t.to_rfc3339())
        .map(|s| s.chars().take(10).collect())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Keep the newest directories and delete the rest, **within the mode being
/// written**. Known-good and failing dirs are swept against their own caps so a
/// burst of one cannot evict the other (see [`KEEP_FAILING`]).
///
/// Newest by directory mtime rather than by version string: version strings do
/// not sort meaningfully across harnesses (or across a `2.1.9` / `2.1.10`
/// boundary lexically), and mtime is what "the last known-good capture" actually
/// means.
fn sweep(harness_dir: &Path, mode: Mode) {
    let keep = match mode {
        Mode::KnownGood => KEEP_VERSIONS,
        Mode::Failing => KEEP_FAILING,
    };
    let Ok(entries) = std::fs::read_dir(harness_dir) else {
        return;
    };
    let mut dirs: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter(|p| {
            let failing = p
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(FAILING_SUFFIX));
            failing == (mode == Mode::Failing)
        })
        .filter_map(|p| {
            let mtime = p.metadata().and_then(|m| m.modified()).ok()?;
            Some((mtime, p))
        })
        .collect();
    if dirs.len() <= keep {
        return;
    }
    dirs.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));
    for (_, path) in dirs.into_iter().skip(keep) {
        if let Err(e) = std::fs::remove_dir_all(&path) {
            tracing::debug!(dir = %path.display(), error = %e, "harness capture sweep failed");
        }
    }
}

// ── the manual trigger ──────────────────────────────────────────────────────

/// `cimp --harness-capture [--json]`.
///
/// Runs the same probes `--harness-canary` does and writes what they observed
/// **whatever the outcome** — a passing run refreshes the known-good corpus, a
/// failing one lands in `<version>-failing/`. That asymmetry is the point: the
/// moment you most want a capture is the moment the automatic trigger refuses
/// to take one.
///
/// Exit code is **0 unless nothing at all could be written**, deliberately
/// unlike `--harness-canary`: this command reports on the corpus, not on drift.
/// A user who wants the drift verdict has a command for it, and making this one
/// exit non-zero on drift would mean a maintenance script could not tell "the
/// capture failed" from "the capture worked and the harness is broken".
pub fn run(args: &[String]) -> i32 {
    let json = args.iter().any(|a| a == "--json");
    let Some(root) = captures_root() else {
        eprintln!(
            "cimp --harness-capture: no data directory could be resolved (neither LOCALAPPDATA \
             nor a home directory), so there is nowhere to write a capture."
        );
        return 1;
    };

    let mut records: Vec<(Driven, Result<Written, String>)> = Vec::new();
    // OpenCode first, for the same reason `probe::run` drives it first: one
    // `opencode serve` child answers both its probes.
    for harness in [Harness::OpenCode, Harness::Claude] {
        let driven = probe::drive(harness);
        let failures = driven.results.iter().filter(|r| r.outcome.is_fail()).count();
        let written = write_into(&root, &driven, Mode::of(failures));
        records.push((driven, written));
    }

    let any = records.iter().any(|(_, w)| w.is_ok());
    if json {
        print_json(&root, &records);
    } else {
        print_human(&root, &records);
    }
    i32::from(!any)
}

fn print_json(root: &Path, records: &[(Driven, Result<Written, String>)]) {
    let arr: Vec<serde_json::Value> = records
        .iter()
        .map(|(driven, written)| {
            let failures = driven.results.iter().filter(|r| r.outcome.is_fail()).count();
            match written {
                Ok(w) => serde_json::json!({
                    "harness": harness_name(driven.harness),
                    "version": driven.version,
                    "written": true,
                    "mode": if failures == 0 { "known-good" } else { "failing" },
                    "dir": w.dir.display().to_string(),
                    "files": w.files,
                    "redactions": w.redactions,
                    "omitted_lines": w.omitted,
                }),
                Err(why) => serde_json::json!({
                    "harness": harness_name(driven.harness),
                    "version": driven.version,
                    "written": false,
                    "why": why,
                }),
            }
        })
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "root": root.display().to_string(),
            "harnesses": arr,
        }))
        .unwrap_or_else(|_| "{}".to_string())
    );
}

fn print_human(root: &Path, records: &[(Driven, Result<Written, String>)]) {
    println!(
        "cimp {} — harness capture (L2). Writes what the probes observed, scrubbed.",
        env!("CARGO_PKG_VERSION")
    );
    println!();
    println!("  root: {}", root.display());
    for (driven, written) in records {
        println!();
        let failures = driven.results.iter().filter(|r| r.outcome.is_fail()).count();
        println!(
            "  {} {} — {} probe row(s), {} failed",
            harness_name(driven.harness),
            if driven.version.is_empty() {
                "(version not observed)"
            } else {
                driven.version.as_str()
            },
            driven.results.len(),
            failures
        );
        match written {
            Ok(w) => {
                println!("    {}", w.dir.display());
                for f in &w.files {
                    println!("      {f}");
                }
                println!("      MANIFEST.toml");
                println!(
                    "    {} container(s) redacted, {} line(s) omitted as too large to screen",
                    w.redactions, w.omitted
                );
                if failures > 0 {
                    println!(
                        "    NOTE: this run FAILED, so the capture landed in a `{FAILING_SUFFIX}` \
                         directory and the last known-good capture is untouched — diff them."
                    );
                }
            }
            Err(why) => println!("    not written: {why}"),
        }
    }
}

/// The capability ids this phase captures a payload for. Consumed by the test
/// that pins them against the registry — a capture file named for something the
/// matrix never declared would be a corpus drifting away from the join key
/// everything else uses.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const CAPTURED: &[&str] = &[
    "opencode.tool_registry",
    "claude.flag.session_id",
    "claude.flag.settings_overlay",
    "claude.transcript.usage",
    "claude.transcript.tool_result",
    "claude.transcript.identity",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::contract;
    use crate::harness::probe::{Outcome, ProbeResult};

    fn tmp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("cimp_capture_{tag}_{}", uuid::Uuid::new_v4()))
    }

    fn driven(harness: Harness, version: &str, observed: Vec<Observed>, fail: bool) -> Driven {
        Driven {
            harness,
            version: version.to_string(),
            observed,
            results: vec![ProbeResult::for_test(
                "opencode.tool_registry",
                if fail {
                    Outcome::Fail {
                        detail: "drifted".to_string(),
                    }
                } else {
                    Outcome::Pass {
                        detail: "fine".to_string(),
                    }
                },
            )],
        }
    }

    /// Every file name in `dir` and its subdirectories, relative to it — the
    /// "nothing was written anywhere else" assertion needs the whole tree, not
    /// the one directory it expects.
    fn tree(dir: &Path) -> Vec<String> {
        fn walk(base: &Path, at: &Path, out: &mut Vec<String>) {
            let Ok(entries) = std::fs::read_dir(at) else {
                return;
            };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(base, &p, out);
                } else if let Ok(rel) = p.strip_prefix(base) {
                    out.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, dir, &mut out);
        out.sort();
        out
    }

    /// **Milestone live-verify recipe 6, as a test.** A planted credential in a
    /// payload must not survive to disk — and the capture must land in exactly
    /// one place.
    ///
    /// Two secret shapes, because they exercise different halves of the
    /// scrubber: an `sk-` token inside a JSON string value (redacted in place,
    /// shape preserved) and an assigned password in plain non-JSON text
    /// (redacted by line). Both are synthetic and neither is a real credential.
    #[test]
    fn a_planted_secret_never_reaches_disk() {
        let root = tmp("redaction");
        // Assembled at runtime so the literal token never appears contiguously
        // in this file — it is synthetic, but shaped well enough that a repo
        // scanner would block the push. Same trick `graph::secrets`' own
        // samples use.
        let api_key = format!("sk-{}", "A".repeat(36));
        let password = format!("password = \"{}\"", "hunter2-hunter2-hunter2");

        let payload = serde_json::json!({
            "type": "assistant",
            "sessionId": "s-1",
            "message": { "id": "m1", "content": [{ "type": "text", "text":
                format!("the key is {api_key}, do not share it") }] }
        })
        .to_string();
        let help = format!("Options:\n  --settings <file>\n# config: {password}\n  --session-id");

        let d = driven(
            Harness::Claude,
            "2.1.232",
            vec![
                Observed::new("claude.transcript.usage", "jsonl", payload),
                Observed::new("claude.flag.settings_overlay", "txt", help),
            ],
            false,
        );
        let written = write_into(&root, &d, Mode::KnownGood).expect("the capture is written");

        // (a) the files exist where the version-stamped layout says they do
        let dir = root.join("claude").join("2.1.232");
        assert_eq!(written.dir, dir);
        assert!(dir.join("claude.transcript.usage.jsonl").is_file());
        assert!(dir.join("claude.flag.settings_overlay.txt").is_file());

        // (b) the secrets are gone — from every byte under the root, not merely
        // from the file we expected them in.
        for rel in tree(&root) {
            let body = std::fs::read_to_string(root.join(&rel)).unwrap_or_default();
            assert!(!body.contains(&api_key), "{rel} still holds the api key");
            assert!(
                !body.contains("hunter2-hunter2-hunter2"),
                "{rel} still holds the password"
            );
        }
        // …and the redaction is visible rather than a silent deletion.
        let usage = std::fs::read_to_string(dir.join("claude.transcript.usage.jsonl")).unwrap();
        assert!(usage.contains("[REDACTED"), "{usage}");
        // The SHAPE survives the redaction — that is the whole reason the unit
        // is a container and not a byte range.
        let parsed: serde_json::Value = serde_json::from_str(usage.trim()).expect("still JSON");
        assert_eq!(parsed["type"], "assistant");
        assert_eq!(parsed["message"]["id"], "m1");
        assert_eq!(parsed["message"]["content"][0]["type"], "text");
        assert!(written.redactions >= 2, "{written:?}");

        // (c) nothing was written anywhere else.
        assert_eq!(
            tree(&root),
            vec![
                "claude/2.1.232/MANIFEST.toml".to_string(),
                "claude/2.1.232/claude.flag.settings_overlay.txt".to_string(),
                "claude/2.1.232/claude.transcript.usage.jsonl".to_string(),
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The manifest carries the four keys the fixture walker requires plus the
    /// phase's own, and lists every file in the directory — including one an
    /// earlier capture wrote and this one did not.
    #[test]
    fn the_manifest_names_provenance_and_every_file_present() {
        let root = tmp("manifest");
        let first = driven(
            Harness::OpenCode,
            "1.18.13",
            vec![Observed::new("opencode.tool_registry", "json", "[\"bash\"]")],
            false,
        );
        write_into(&root, &first, Mode::KnownGood).expect("written");

        // A second capture of the same version that observed something else:
        // the file from the first must still be listed.
        let second = driven(
            Harness::OpenCode,
            "1.18.13",
            vec![Observed::new("opencode.route.noauth", "txt", "200")],
            false,
        );
        write_into(&root, &second, Mode::KnownGood).expect("written");

        let toml = std::fs::read_to_string(
            root.join("opencode").join("1.18.13").join("MANIFEST.toml"),
        )
        .unwrap();
        for key in [
            "captured_from",
            "date",
            "method",
            "redaction",
            "redacted = true",
            "capabilities",
        ] {
            assert!(toml.contains(key), "manifest lacks `{key}`:\n{toml}");
        }
        assert!(toml.contains("live probe capture"), "{toml}");
        assert!(toml.contains("opencode 1.18.13"), "{toml}");
        assert!(toml.contains("opencode.tool_registry.json"), "{toml}");
        assert!(toml.contains("opencode.route.noauth.txt"), "{toml}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The load-bearing rule of the phase: a failing run must never touch the
    /// known-good corpus, and its shapes go to a clearly-marked sibling.
    #[test]
    fn a_failing_run_cannot_overwrite_the_known_good_capture() {
        let root = tmp("failing");
        let good = driven(
            Harness::OpenCode,
            "1.18.13",
            vec![Observed::new(
                "opencode.tool_registry",
                "json",
                "[\"bash\",\"read\"]",
            )],
            false,
        );
        on_success_into(&root, &good);
        let known = root
            .join("opencode")
            .join("1.18.13")
            .join("opencode.tool_registry.json");
        assert_eq!(std::fs::read_to_string(&known).unwrap(), "[\"bash\",\"read\"]");

        // The same version, now drifting.
        let bad = driven(
            Harness::OpenCode,
            "1.18.13",
            vec![Observed::new(
                "opencode.tool_registry",
                "json",
                "[\"exfiltrate\"]",
            )],
            true,
        );
        on_success_into(&root, &bad);
        assert_eq!(
            std::fs::read_to_string(&known).unwrap(),
            "[\"bash\",\"read\"]",
            "the automatic trigger overwrote the last known-good capture with a failing run"
        );

        // …and the manual trigger keeps them apart rather than merging them.
        let w = write_into(&root, &bad, Mode::of(1)).expect("written");
        assert!(
            w.dir.ends_with(format!("1.18.13{FAILING_SUFFIX}")),
            "{:?}",
            w.dir
        );
        assert_eq!(
            std::fs::read_to_string(&known).unwrap(),
            "[\"bash\",\"read\"]"
        );
        assert_eq!(
            std::fs::read_to_string(w.dir.join("opencode.tool_registry.json")).unwrap(),
            "[\"exfiltrate\"]"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `on_success`'s decision logic against an explicit root — the real one
    /// resolves a per-user path no test may write to.
    fn on_success_into(root: &Path, driven: &Driven) {
        if driven.results.iter().any(|r| r.outcome.is_fail()) || driven.observed.is_empty() {
            return;
        }
        let _ = write_into(root, driven, Mode::KnownGood);
    }

    /// Retention is bounded, and the two lanes are swept against their own caps
    /// so a burst of failing captures cannot evict the known-good history it
    /// exists to be diffed against.
    #[test]
    fn retention_is_bounded_per_lane() {
        let root = tmp("sweep");
        let obs = || vec![Observed::new("opencode.tool_registry", "json", "[\"bash\"]")];

        for n in 0..KEEP_VERSIONS + 4 {
            let d = driven(Harness::OpenCode, &format!("1.0.{n}"), obs(), false);
            write_into(&root, &d, Mode::KnownGood).expect("written");
            // mtime granularity on Windows is coarse enough that same-tick
            // directories sort arbitrarily; the sweep only needs a strict
            // ordering to be meaningful, not the test to be fast.
            std::thread::sleep(std::time::Duration::from_millis(12));
        }
        for n in 0..KEEP_FAILING + 3 {
            let d = driven(Harness::OpenCode, &format!("2.0.{n}"), obs(), true);
            write_into(&root, &d, Mode::Failing).expect("written");
            std::thread::sleep(std::time::Duration::from_millis(12));
        }

        let dirs: Vec<String> = std::fs::read_dir(root.join("opencode"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        let good: Vec<&String> = dirs
            .iter()
            .filter(|d| !d.ends_with(FAILING_SUFFIX))
            .collect();
        let failing: Vec<&String> = dirs.iter().filter(|d| d.ends_with(FAILING_SUFFIX)).collect();
        assert_eq!(good.len(), KEEP_VERSIONS, "{dirs:?}");
        assert_eq!(failing.len(), KEEP_FAILING, "{dirs:?}");
        // The NEWEST survive, and the failing burst took nothing from the
        // known-good lane.
        assert!(
            good.iter()
                .any(|d| d.as_str() == format!("1.0.{}", KEEP_VERSIONS + 3)),
            "{dirs:?}"
        );
        assert!(
            !good.iter().any(|d| d.as_str() == "1.0.0"),
            "the oldest known-good capture should have been swept: {dirs:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An unversioned run writes NOTHING — locked decision 6's "no `unknown/`
    /// directories", which is what stops two releases sharing one directory and
    /// making every diff meaningless.
    #[test]
    fn an_unversioned_run_writes_nothing() {
        let root = tmp("noversion");
        let d = driven(
            Harness::Claude,
            "   ",
            vec![Observed::new("claude.flag.session_id", "txt", "--session-id")],
            false,
        );
        let err = write_into(&root, &d, Mode::KnownGood).expect_err("must refuse");
        assert!(err.contains("version"), "{err}");
        assert!(!root.exists(), "nothing may be created for an unstamped run");
    }

    /// The corpus speaks the registry's join key: every id a capture file is
    /// named for is a real capability, and every one of them is a row this
    /// phase's probes actually drive.
    #[test]
    fn captured_ids_are_registry_capabilities_the_probes_drive() {
        let driven_ids: std::collections::BTreeSet<&str> =
            probe::implemented_probes().iter().copied().collect();
        for id in CAPTURED {
            assert!(
                contract::get(id).is_some(),
                "{id} names no registry capability, so its capture file joins to nothing"
            );
            assert!(
                driven_ids.contains(id),
                "{id} is captured but not driven by an L2 probe — a payload nothing observes"
            );
        }
    }

    /// The corpus never lands in the repo, and never beside the exe (the
    /// standing finding: the exe is sometimes on a synced network path).
    #[test]
    fn the_root_is_a_data_dir_not_the_exe_dir_or_the_repo() {
        let Some(root) = captures_root() else {
            return; // no HOME/LOCALAPPDATA in this environment; nothing to pin
        };
        assert!(root.ends_with("harness-captures"), "{root:?}");
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                assert!(
                    !root.starts_with(dir),
                    "the capture corpus must not live beside the executable: {root:?}"
                );
            }
        }
        // The repo tree — a capture under it would need a .gitignore entry, and
        // the design is that no such entry is ever needed.
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        assert!(!root.starts_with(&manifest_dir), "{root:?}");
        assert!(
            manifest_dir
                .parent()
                .is_none_or(|repo| !root.starts_with(repo)),
            "{root:?}"
        );
    }
}
