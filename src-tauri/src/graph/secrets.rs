//! V32 Phase C2 (user decision 2026-08-07) — the **memory secret screen**.
//!
//! # Why a write-time screen and not a read-time latch
//!
//! `context_recall` / `context_notes` are TRUSTED: they never latch and are
//! never blocked, and they return **every pinned note for the project**. Locked
//! decision 10 covers the write side for *injected* content — a note written
//! under an EXTERNAL latch is quarantined — but it says nothing about a note the
//! user themselves pinned. A user who pinned a credential pinned it into a class
//! a contaminated tab can read back and exfiltrate.
//!
//! The alternative — latching the memory reads — was rejected for the reason
//! decision 10 already gives for rejecting a hard write block: it costs a
//! contaminated tab its own memory, *"a block that silently drops legitimate
//! research conclusions"*. So the screen runs on the way IN, once, where exactly
//! one short string has to be looked at.
//!
//! # The action on a hit: quarantine, not refuse and not redact
//!
//! A hit stores the note **with the same `tainted` flag decision 10 already
//! uses**: it is saved, withheld from every recall path, and surfaced in the
//! Memory view for promote-or-discard. Nothing new was built to hold it.
//!
//! The two alternatives both fail the "keep a legitimate note recoverable" test
//! the decision names:
//! - **Refuse** throws the note away. A false positive then costs the user the
//!   research conclusion the session existed to produce, unrecoverably, and the
//!   model is told to try again — which it cannot usefully do.
//! - **Strip / redact** silently rewrites the user's own memory. The user finds
//!   out only when they read a note with a hole in it, and there is no copy of
//!   what was removed.
//!
//! Quarantine costs a false positive one click in a queue the user already has,
//! and a true positive never reaches a recall path in the meantime. It is also
//! the only one of the three that is honest about uncertainty: the screen is a
//! pattern match, and a pattern match is a *suspicion*, which is precisely what
//! a review queue is for.
//!
//! # Reusing the detection machinery
//!
//! The engine is the one already compiled into this process: yara-x, through
//! [`signature::compile_sources`] and [`signature::scan_with`], the same two
//! functions the C3 updater validates a staged bundle with. No new dependency,
//! no new scanner, the same timeout and prefix discipline.
//!
//! Two things that were considered and are **not** reused, recorded so they are
//! not re-proposed:
//!
//! - **gitleaks** (the audit runner's secret scanner) is an out-of-process
//!   child, optional on every install, transported through a SARIF report file,
//!   and takes seconds. A `context_note` call cannot spawn it, and a screen that
//!   silently no-ops on the majority of installs is not a screen.
//! - **The live `rules.d` bundle** (`signature::scan`) is the wrong home for
//!   these patterns even though it is the same engine. It is replaced wholesale
//!   by the C3 updater, it is switched off by the injection-detection toggle,
//!   and a broken file in `rules.d/local/` can drop rules out of it. A screen
//!   over the user's own credentials must not be removable by a bundle update or
//!   by a toggle about untrusted *web* content. So the rules are compiled from
//!   [`SOURCE`], baked into the binary.
//!
//!   The cost of that choice, stated: these patterns do not get the daily
//!   update channel, and a user cannot extend them from `rules.d/local/`.
//!   Promoting them into the updatable bundle *in addition* is a legitimate
//!   follow-up; removing the baked copy is not.

use std::sync::{Arc, OnceLock};

use crate::offload::detection::signature;

/// The rule source, compiled into the binary. See the file's own header for the
/// curation rules that apply to anything added to it.
const SOURCE: &str = include_str!("secrets.yar");

/// The display name this file carries into [`signature::compile_sources`]'s
/// failure list. It never reaches a user (a compile failure here is a build-time
/// mistake, caught by [`tests::the_baked_ruleset_compiles`]), but the compiler
/// blames a name and an anonymous one would be unreadable in a log.
const SOURCE_NAME: &str = "graph/secrets.yar";

/// The compiled screen, built once per process on first use.
///
/// `None` when the baked source does not compile — which a test makes
/// impossible to ship, but the runtime must still degrade to "nothing to say"
/// rather than panic inside a tool call. That degradation is fail-OPEN: a note
/// that would have been quarantined is stored clean. It is the same posture
/// [`signature::scan`] takes for the same reason (a screen that cannot run must
/// not become a refusal path), and it is why the compile is pinned by a test
/// rather than trusted.
fn rules() -> Option<Arc<yara_x::Rules>> {
    static COMPILED: OnceLock<Option<Arc<yara_x::Rules>>> = OnceLock::new();
    COMPILED
        .get_or_init(|| compile(SOURCE))
        .as_ref()
        .map(Arc::clone)
}

/// Compile one rule source with the detection layer's own compiler, so the
/// screen and the signature layer cannot diverge on what a rule file may
/// contain. Split out for the test, which compiles the baked source explicitly
/// rather than through the process-wide cell.
fn compile(source: &str) -> Option<Arc<yara_x::Rules>> {
    let (rules, failed) =
        signature::compile_sources(&[(SOURCE_NAME.to_string(), source.to_string())]);
    if !failed.is_empty() {
        tracing::error!(
            target: "graph",
            failed = %failed.join(", "),
            "memory secret screen: the baked ruleset did not compile — notes will not be screened"
        );
    }
    rules
}

/// The identifiers of every secret rule matching `text`, or an empty vec for a
/// clean note (and for a screen that could not run — see [`rules`]).
///
/// Sorted, so the model-facing message and the activity row are stable for the
/// same input rather than dependent on yara-x's match order.
///
/// This deliberately uses [`signature::scan_with`]'s hits-only shape rather than
/// `scan_outcome_with`'s three-way one. The distinction that shape exists for —
/// *"unscreened is not clean"* (#48, D-1) — is about a multi-megabyte fetched
/// page against a 256 KiB prefix cap and a 750 ms deadline. The input here is
/// one `context_note` string; it cannot reach either bound, so the only way to
/// get `DidNotComplete` is a scanner fault, and for that the honest answer is
/// the same fail-open every other screen takes: a screen that cannot run must
/// not become a refusal path.
pub fn screen(text: &str) -> Vec<String> {
    let Some(rules) = rules() else {
        return Vec::new();
    };
    let mut hits = signature::scan_with(&rules, text);
    hits.sort();
    hits.dedup();
    hits
}

/// The fixed suffix appended to a `context_note` result held by the secret
/// screen, with the matched rule identifiers folded in.
///
/// Content-free about the note itself, exactly like
/// [`QUARANTINE_WRITE_NOTICE`](crate::offload::toolclass::QUARANTINE_WRITE_NOTICE):
/// it names the RULES that matched, never the matched text. Echoing the match
/// back would make the screen a way to confirm a credential's exact shape.
///
/// It deliberately does **not** tell the model to rewrite the note without the
/// secret. A note whose value depends on a credential is one the user should
/// decide about; a model that strips it to get past the screen has produced a
/// note that no longer says what it found, and taught itself that the boundary
/// is negotiable.
pub fn write_notice(hits: &[String]) -> String {
    format!(
        " ⚠ HELD FOR REVIEW (secret screen): this note matched cImp's credential patterns \
         ({}), so it was saved but held instead of entering project memory — pinned notes are \
         readable by any later session, including one reading untrusted content, so a credential \
         in one is a credential that can be exfiltrated. It will NOT be recalled or auto-injected \
         until the user releases it in cImp's Memory view. Nothing further can be done from here; \
         do not rewrite or re-save it without the secret, and tell the user what you recorded and \
         where the value actually belongs.",
        hits.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The baked source must compile — the whole screen is silently absent
    /// otherwise (`rules()` degrades to `None` on purpose, so nothing else in
    /// the suite would notice).
    #[test]
    fn the_baked_ruleset_compiles() {
        assert!(
            compile(SOURCE).is_some(),
            "graph/secrets.yar did not compile — the memory secret screen would be inert"
        );
    }

    /// Every rule identifier carries the `secret_` prefix the module docs
    /// promise, and the set is non-empty.
    #[test]
    fn every_rule_is_named_secret() {
        let rules = compile(SOURCE).expect("compiles");
        let names: Vec<String> = rules.iter().map(|r| r.identifier().to_string()).collect();
        assert!(names.len() >= 10, "thin ruleset: {names:?}");
        for n in &names {
            assert!(
                n.starts_with("secret_"),
                "rule `{n}` breaks the prefix rule"
            );
        }
    }

    /// The positive control: one sample per rule, each matching the rule it is
    /// paired with. Values are synthetic — no real credential is committed.
    #[test]
    fn each_rule_matches_its_own_sample() {
        for (rule, sample) in [
            (
                "secret_private_key_block",
                "-----BEGIN OPENSSH PRIVATE KEY----- then base64",
            ),
            (
                "secret_aws_access_key_id",
                "creds are AKIAIOSFODNN7EXAMPLE for the staging bucket",
            ),
            (
                "secret_github_token",
                "ghp_0123456789abcdefghijklmnopqrstuvwxyzAB",
            ),
            (
                "secret_slack_token",
                "xoxb-1234567890-0987654321-abcdefghijklmnop",
            ),
            (
                "secret_anthropic_api_key",
                "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            ),
            (
                "secret_openai_style_api_key",
                "sk-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            ),
            (
                // `AIza` + exactly 35 — the rule is length-exact on purpose,
                // which is also why this sample is spelled out rather than
                // eyeballed.
                "secret_google_api_key",
                "AIzaSyB1234567890abcdefghijklmnopqrstuv",
            ),
            ("secret_stripe_key", "sk_live_0123456789abcdefghijklmnop"),
            (
                "secret_json_web_token",
                "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r",
            ),
            (
                "secret_assigned_credential",
                "config line: password = \"hunter2-hunter2-hunter2\"",
            ),
            (
                "secret_bearer_credential",
                "header was Authorization: Bearer abcdefghijklmnopqrstuvwxyz012345",
            ),
            (
                "secret_url_with_password",
                "the dsn is postgres://svc:s3cr3tpw@db.internal:5432/app",
            ),
        ] {
            let hits = screen(sample);
            assert!(
                hits.iter().any(|h| h == rule),
                "`{rule}` did not match its own sample; hits were {hits:?}"
            );
        }
    }

    /// The negative control, and the one that matters: ordinary research
    /// conclusions — including ones that TALK about credentials — must pass
    /// clean. A false positive here is the failure locked decision 10 cares
    /// about, held one review click away instead of dropped, but still a cost.
    #[test]
    fn benign_notes_do_not_match() {
        for note in [
            "we chose FNV hashing for stability across releases",
            "the API key lives in .env and must never be committed — see .gitleaks.toml",
            "auth is a bearer token issued by the gateway; rotate it monthly",
            "password reset flow is broken when the user has no email on file",
            "run `cargo test -p cimp` before pushing; the graph tests need a temp dir",
            "secret: the offload server is single-slot, so never parallelize calls",
            "GraphIndex::mem_notes is the one quarantine filter — do not add a second query",
            "https://github.com/anthropics/claude-code/issues/123 has the repro",
            "postgres://localhost:5432/app is the dev dsn (no password, trust auth)",
        ] {
            assert!(
                screen(note).is_empty(),
                "benign note flagged by the secret screen: {note:?} → {:?}",
                screen(note)
            );
        }
    }

    /// The notice names the rules and never the matched text.
    #[test]
    fn the_notice_names_rules_not_content() {
        let n = write_notice(&["secret_aws_access_key_id".to_string()]);
        assert!(n.contains("secret_aws_access_key_id"));
        assert!(n.contains("HELD FOR REVIEW (secret screen)"));
        assert!(n.contains("Memory view"));
        // No format placeholders survived, and no advice to launder the note
        // past the screen.
        assert!(!n.contains('{'));
    }

    /// An empty note is not a hit — `str::contains("")`-shaped traps are why
    /// this is asserted rather than assumed.
    #[test]
    fn an_empty_note_is_clean() {
        assert!(screen("").is_empty());
    }
}
