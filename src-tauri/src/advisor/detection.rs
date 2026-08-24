//! The detection/updater advisor (V32 Phase C3, #46, #48).
//!
//! Six warn-only rules about the injection-detection layer: the update
//! channel (`update_available`, `update_failed`, `update_stalled`), and the
//! rules on disk (`signature_down`, `local_rules_broken`, `rules_incomplete`).
//! They read only [`crate::offload::detection`] state and share nothing with
//! the drift canaries in [`super::drift`] beyond the dismissal list — which is
//! why V42 R7 could lift them out of `drift_rules` whole.
//!
//! None of them proposes a settings write: the fix is a button in
//! Settings → Injection protection → Injection detection, so every proposal
//! here is `warn_only` with an empty `setting`.
//!
//! **Card order.** [`rules`] is called from [`super::evaluate`] BETWEEN the two
//! halves of [`super::drift`], because that is where the V32 block was
//! appended inside the old `drift_rules` — see `evaluate`'s comment. The
//! panel renders the vector in order, so the position is observable.

use super::*;

/// The six detection/updater canaries, in the order the panel shows them.
/// Each carries its own trigger — a fact, not a statistic — so none sits
/// behind the global [`MIN_SESSIONS`] floor.
pub(super) fn rules(sig: &Signals) -> Vec<Proposal> {
    let mut out = Vec::new();

    // V32 C3 — detection.update_available.v1: a newer detection bundle was
    // found and not taken. This is the consumer for check-only mode: without
    // it, "check-only" would mean "silently record a version nobody reads".
    // Warn-only — Apply lives in
    // Settings → Injection protection → Injection detection, next to the
    // versions and the revert button, because taking an update is not a
    // settings write.
    for u in &sig.detection_updates {
        let signature = format!("{}:{}", u.component, u.available);
        if is_dismissed(&sig.dismissed, RULE_DETECTION_UPDATE_AVAILABLE, &signature) {
            continue;
        }
        out.push(Proposal {
            setting: String::new(),
            current: if u.installed.is_empty() {
                "(shipped)".to_string()
            } else {
                u.installed.clone()
            },
            proposed: u.available.clone(),
            rationale: format!(
                "A newer injection-detection {} bundle ({}) is available and was not applied — \
                 this component is set to check-only, or the bundle needs a newer cImp. Detection \
                 data decays without updates, so a component that keeps declining them slowly \
                 stops matching what attackers actually send.{} Review and take it from \
                 Settings → Injection protection → Injection detection (Apply), or switch the \
                 component to auto.",
                u.component,
                u.available,
                if u.notes.trim().is_empty() {
                    String::new()
                } else {
                    format!(" Curator's note: {}.", u.notes.trim())
                }
            ),
            rule_id: RULE_DETECTION_UPDATE_AVAILABLE,
            signature,
            warn_only: true,
            action: None,
            capability: None,
            harness: None,
        });
    }

    // V32 C3 — detection.update_failed.v1: a bundle was refused. Nothing
    // broke (the old data is still live and the updater never degrades to
    // no-detection), but a component whose every candidate is rejected is
    // frozen, and freezing quietly is the failure this rule exists to prevent.
    for f in &sig.detection_update_failures {
        // `f.signature`, not `f.version`: a manifest-level refusal has no
        // version, and signing those `component:` made one dismissal a
        // permanent mute on every later refusal (#46).
        let signature = format!("{}:{}", f.component, f.signature);
        if is_dismissed(&sig.dismissed, RULE_DETECTION_UPDATE_FAILED, &signature) {
            continue;
        }
        out.push(Proposal {
            setting: String::new(),
            current: format!("{} update rejected", f.component),
            proposed: "a bundle that passes validation".to_string(),
            rationale: format!(
                "The injection-detection {} update{} was REJECTED before activation, and the \
                 previous data is still live: {}. Nothing is degraded right now, but this \
                 component is not getting fresher either — check the manifest/bundle per \
                 MAINTENANCE.md → \"Detection updater\", then re-run Check now.",
                f.component,
                if f.version.is_empty() {
                    String::new()
                } else {
                    format!(" to `{}`", f.version)
                },
                f.reason
            ),
            rule_id: RULE_DETECTION_UPDATE_FAILED,
            signature,
            warn_only: true,
            action: None,
            capability: None,
            harness: None,
        });
    }

    // V32 C3 / #46, #48 — detection.update_stalled.v1: a week of checks has
    // left this component no fresher. Nothing is broken, so this is NOT a
    // rejection card; it is the freshness canary decision 13 asks for, firing
    // at the point where "one bad check" has become "this component has stopped
    // getting fresher". It says nothing about the CAUSE — an outage and a
    // channel that refuses everything it serves are the same event from here —
    // because the cause is in the last outcome, quoted verbatim, and because
    // the two rules that do speak to cause can both be dismissed away.
    // Any check that comes back current resets the streak, so this cannot fire
    // on a working install.
    for s in &sig.detection_update_stalled {
        // Bucketed by the threshold: dismissing holds for roughly another
        // week's worth of failures rather than forever, and a recovery resets
        // the streak so a fresh outage starts the count (and the signature)
        // over.
        let signature = format!(
            "{}:{}",
            s.component,
            s.streak / crate::offload::detection::updater::STALLED_AFTER_CHECKS
        );
        if is_dismissed(&sig.dismissed, RULE_DETECTION_UPDATE_STALLED, &signature) {
            continue;
        }
        out.push(Proposal {
            setting: String::new(),
            current: format!("{} updates have stalled", s.component),
            proposed: "a component that is getting fresher".to_string(),
            rationale: format!(
                "The injection-detection {} component has not taken an update for {} checks in a \
                 row, so it is no longer getting fresher. Nothing is degraded — the data you have \
                 is still live and still scanning — but signatures and weights decay, so a \
                 component that stays frozen eventually means missing what attackers have started \
                 sending. The last check said: {}. If that is a network or proxy problem, check \
                 access to the manifest URL shown in \
                 Settings → Injection protection → Injection detection; if a bundle is \
                 being refused, see MAINTENANCE.md → \"Detection updater\"; or set the component \
                 to `off` if this machine is deliberately offline.",
                s.component, s.streak, s.reason
            ),
            rule_id: RULE_DETECTION_UPDATE_STALLED,
            signature,
            warn_only: true,
            action: None,
            capability: None,
            harness: None,
        });
    }

    // V32 Phase C / #48 — detection.signature_down.v1: the signature layer is
    // ON and has nothing to match with. The one detection card that is about
    // the DATA rather than the channel, and the consumer decision 13's
    // "never silently degrades to no-detection" was missing: without it a
    // rules directory that compiles to nothing reports every page clean while
    // the badge, the activity feed and the three cards above all say the layer
    // is fine. Warn-only — the fix is a file on disk (or Reload rules), not a
    // settings write.
    if let Some(d) = &sig.detection_signature_down {
        // Signed by what the user would look at: the directory plus the counts.
        // A dismissal holds for that state and re-raises when it changes, which
        // includes the case where a partial recovery leaves the layer still
        // disarmed for a different reason.
        let signature = format!("{}:{}:{}", d.dir, d.files_loaded, d.files_failed);
        if !is_dismissed(&sig.dismissed, RULE_DETECTION_SIGNATURE_DOWN, &signature) {
            out.push(Proposal {
                setting: String::new(),
                current: format!(
                    "{} file(s) loaded, {} rule(s){}",
                    d.files_loaded,
                    d.rules,
                    if d.failed.is_empty() {
                        String::new()
                    } else {
                        format!(" — {} rejected: {}", d.files_failed, d.failed.join(", "))
                    }
                ),
                proposed: "a rules directory that compiles".to_string(),
                rationale: format!(
                    "The injection-detection SIGNATURE layer is switched on and has no rules to \
                     match against, so every external result it screens comes back clean because \
                     there is nothing to compare it with — not because it is. cImp looked in {}. \
                     If a previously loaded set is still in memory it keeps screening with that, \
                     but it will not survive a restart. Fix or remove the rejected file(s) — a \
                     broken rule in `rules.d/local/` is the usual cause — then press Reload rules \
                     in Settings → Injection protection → Injection detection.",
                    d.dir
                ),
                rule_id: RULE_DETECTION_SIGNATURE_DOWN,
                signature,
                warn_only: true,
                action: None,
                capability: None,
                harness: None,
            });
        }
    }

    // V32 Phase C3 / #48 U-4 — detection.local_rules_broken.v1: a rule file the
    // USER wrote is on disk and does not compile, so it is skipped while the
    // rest of the layer runs. Suppressed while the card above is up (same
    // folder, louder problem) — the updater's `broken_local_rules` applies that
    // suppression at the source, so the two can never both fire.
    if let Some(b) = &sig.detection_local_rules_broken {
        // The signature covers BOTH lists (#48, M-13): a dismissal of "your
        // file does not compile" must not also silence "and this other rule of
        // yours is now live under a different name", and a rename appearing
        // later must re-raise a card the user already dismissed.
        let renamed_sig: Vec<String> = b.renamed.iter().map(|r| r.describe()).collect();
        // Unchanged when there are no renames, so an existing dismissal of the
        // broken-file card survives this change rather than re-firing once for
        // everyone.
        let signature = if renamed_sig.is_empty() {
            b.failed.join(",")
        } else {
            format!("{}|{}", b.failed.join(","), renamed_sig.join(","))
        };
        if !is_dismissed(
            &sig.dismissed,
            RULE_DETECTION_LOCAL_RULES_BROKEN,
            &signature,
        ) {
            // Two conditions, described in their own words. Folding them into
            // one sentence is how a rule that IS matching ends up reported as
            // "not screening anything" — the family of bug this milestone keeps
            // finding, in miniature.
            let mut current = Vec::new();
            if !b.failed.is_empty() {
                current.push(format!(
                    "{} rejected: {}",
                    b.failed.len(),
                    b.failed.join(", ")
                ));
            }
            if !b.renamed.is_empty() {
                current.push(format!(
                    "{} renamed: {}",
                    b.renamed.len(),
                    renamed_sig.join(", ")
                ));
            }
            // The counts ride the `current` line in both cases, because without
            // them the card reads as an outage.
            current.push(format!(
                "{} file(s), {} rule(s) still live",
                b.files_loaded, b.rules
            ));
            let mut rationale = String::new();
            if !b.failed.is_empty() {
                rationale.push_str(
                    "Your own detection rule file(s) in `rules.d/local/` do not compile, so they \
                     are being skipped: the patterns you wrote are not screening anything, while \
                     the rest of the layer runs normally and reports nothing wrong. A file is also \
                     rejected when it collides on a rule IDENTIFIER that cImp could not rename \
                     around — another compile error in the same file, or every renamed form taken \
                     too. ",
                );
            }
            if !b.renamed.is_empty() {
                rationale.push_str(
                    "A rule you wrote in `rules.d/local/` declares an IDENTIFIER the shipped \
                     bundle now also uses, and YARA identifiers must be unique across the set. \
                     Rather than drop your rule or refuse the update, cImp loaded your rule under \
                     a `custom_` identifier — it is live and still matching, but a hit reports the \
                     renamed identifier, so anything of yours keyed on the old name (a saved \
                     search, a log filter) will not see it. Your file on disk was NOT modified. \
                     Renaming the rule in your own file takes the name back. ",
                );
            }
            rationale.push_str(&format!(
                "cImp looked in {}. After editing, press Reload rules in \
                 Settings → Injection protection → Injection detection.",
                b.dir
            ));
            out.push(Proposal {
                setting: String::new(),
                current: current.join("; "),
                proposed: "a `rules.d/local/` that compiles under its own names".to_string(),
                rationale,
                rule_id: RULE_DETECTION_LOCAL_RULES_BROKEN,
                signature,
                warn_only: true,
                action: None,
                capability: None,
                harness: None,
            });
        }
    }

    // V32 Phase C3 / #48 M-11 — detection.rules_incomplete.v1: a rollback could
    // not put every file back, so the live set is genuinely short. NOT
    // suppressed by any of the cards above: this one is the only signal in the
    // module that says something is degraded RIGHT NOW, and the refusal card it
    // most often accompanies says the opposite in so many words.
    for r in &sig.detection_rules_incomplete {
        let signature = format!("{}:{}", r.component, r.files.join(","));
        if is_dismissed(&sig.dismissed, RULE_DETECTION_RULES_INCOMPLETE, &signature) {
            continue;
        }
        out.push(Proposal {
            setting: String::new(),
            current: format!("{} missing: {}", r.component, r.files.join(", ")),
            proposed: "a complete rule set".to_string(),
            rationale: format!(
                "An interrupted or failed detection update could not put {} rule file(s) back \
                 into {} ({}), so the signature layer is running with FEWER rules than it should \
                 — a real, current reduction in coverage, not a stale warning. The files are not \
                 lost: they are still in the retained copy under `detection-updates/previous/`, \
                 and cImp retries the restore on every update check and every launch. The usual \
                 cause is something holding the file open — antivirus real-time scanning, an \
                 editor, or a file manager sitting in that folder. Close it and restart cImp; if \
                 that does not clear it, Revert from \
                 Settings → Injection protection → Injection detection restores the \
                 retained copy whole.",
                r.files.len(),
                r.dir,
                r.files.join(", ")
            ),
            rule_id: RULE_DETECTION_RULES_INCOMPLETE,
            signature,
            warn_only: true,
            action: None,
            capability: None,
            harness: None,
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── V32 Phase C3 — detection-updater canaries ───────────────────────

    use crate::offload::detection::updater::{
        AvailableUpdate, FailedUpdate, StalledUpdate, STALLED_AFTER_CHECKS,
    };

    /// The consumer for check-only mode: a recorded offer becomes a card that
    /// names both versions and points at where to take it.
    #[test]
    fn an_available_detection_update_raises_a_warn_only_card() {
        let sig = Signals {
            detection_updates: vec![AvailableUpdate {
                component: "rules".into(),
                installed: String::new(),
                available: "2026.09.01".into(),
                notes: "multilingual variant".into(),
            }],
            ..Signals::default()
        };
        let p = evaluate(&sig)
            .into_iter()
            .find(|p| p.rule_id == RULE_DETECTION_UPDATE_AVAILABLE)
            .expect("fires");
        assert!(p.warn_only, "there is nothing safe to auto-apply");
        assert!(p.setting.is_empty());
        assert_eq!(p.proposed, "2026.09.01");
        assert_eq!(p.current, "(shipped)", "no installed version yet");
        assert!(p.rationale.contains("rules"));
        assert!(p.rationale.contains("multilingual variant"), "{}", p.rationale);
        assert_eq!(p.signature, "rules:2026.09.01");
    }

    /// A rejected bundle is a card too — nothing is degraded, but a component
    /// that refuses every candidate is freezing, and freezing quietly is the
    /// failure the rule exists to catch.
    #[test]
    fn a_rejected_detection_update_raises_a_card_that_says_nothing_broke() {
        let sig = Signals {
            detection_update_failures: vec![FailedUpdate {
                component: "rules".into(),
                version: "2026.08.07".into(),
                signature: "2026.08.07".into(),
                reason: "false-positive smoke failed: readme.txt matched Foo".into(),
            }],
            ..Signals::default()
        };
        let p = evaluate(&sig)
            .into_iter()
            .find(|p| p.rule_id == RULE_DETECTION_UPDATE_FAILED)
            .expect("fires");
        assert!(p.warn_only);
        assert!(p.rationale.contains("previous data is still live"));
        assert!(p.rationale.contains("false-positive smoke failed"));
        assert_eq!(p.signature, "rules:2026.08.07");
    }

    /// #46's compounding defect: two DIFFERENT manifest-level refusals used to
    /// share the signature `rules:` (both versionless), so dismissing a 404
    /// permanently muted a containment violation. The updater now hands the
    /// rule a per-reason signature, and the rule must key on it.
    #[test]
    fn dismissing_one_versionless_refusal_does_not_mute_a_different_one() {
        let refusal = |sig_key: &str, reason: &str| FailedUpdate {
            component: "rules".into(),
            version: String::new(),
            signature: sig_key.into(),
            reason: reason.into(),
        };
        let dismissed = vec![DismissedRule {
            rule_id: RULE_DETECTION_UPDATE_FAILED.to_string(),
            signature: "rules:reason:aaaaaaaaaaaaaaaa".to_string(),
        }];
        let same = Signals {
            detection_update_failures: vec![refusal(
                "reason:aaaaaaaaaaaaaaaa",
                "manifest schema 2 is not supported",
            )],
            dismissed: dismissed.clone(),
            ..Signals::default()
        };
        assert!(
            !evaluate(&same)
                .iter()
                .any(|p| p.rule_id == RULE_DETECTION_UPDATE_FAILED),
            "the dismissed refusal stays dismissed"
        );
        let other = Signals {
            detection_update_failures: vec![refusal(
                "reason:bbbbbbbbbbbbbbbb",
                "file points outside the manifest's own directory",
            )],
            dismissed,
            ..Signals::default()
        };
        let p = evaluate(&other)
            .into_iter()
            .find(|p| p.rule_id == RULE_DETECTION_UPDATE_FAILED)
            .expect("a containment violation still fires");
        assert!(p.rationale.contains("outside the manifest"), "{}", p.rationale);
    }

    /// A week of unreachable checks is a card — and it says the channel is
    /// unreachable, NOT that a bundle was rejected. That distinction is the
    /// whole of #46.
    #[test]
    fn a_stalled_update_channel_raises_its_own_card_not_a_rejection() {
        let sig = Signals {
            detection_update_stalled: vec![StalledUpdate {
                component: "rules".into(),
                streak: STALLED_AFTER_CHECKS,
                reason: "could not reach the update channel: GET https://…: HTTP 404".into(),
            }],
            ..Signals::default()
        };
        let ids: Vec<&str> = evaluate(&sig).iter().map(|p| p.rule_id).collect();
        assert!(
            !ids.contains(&RULE_DETECTION_UPDATE_FAILED),
            "an outage must never render as a refusal: {ids:?}"
        );
        let p = evaluate(&sig)
            .into_iter()
            .find(|p| p.rule_id == RULE_DETECTION_UPDATE_STALLED)
            .expect("fires");
        assert!(p.warn_only);
        assert!(p.setting.is_empty(), "nothing here is a settings write");
        assert!(
            !p.rationale.to_ascii_lowercase().contains("rejected"),
            "{}",
            p.rationale
        );
        assert!(p.rationale.contains("not getting fresher") || p.rationale.contains("fresher"));
        assert!(p.rationale.contains("HTTP 404"), "the reason is diagnosable");
        assert_eq!(p.signature, "rules:1");
    }

    /// #48/A1-5: the same card carries a channel that is perfectly REACHABLE
    /// and refuses everything it serves. The rule must not claim an outage it
    /// knows nothing about — the cause is whatever the last outcome says, and
    /// this signal's job is only "nothing is landing".
    #[test]
    fn the_stall_card_does_not_diagnose_the_cause_it_quotes_the_last_outcome() {
        let sig = Signals {
            detection_update_stalled: vec![StalledUpdate {
                component: "rules".into(),
                streak: STALLED_AFTER_CHECKS,
                reason: "checksum mismatch on `core.yar` (expected ab…, got cd…)".into(),
            }],
            ..Signals::default()
        };
        let p = evaluate(&sig)
            .into_iter()
            .find(|p| p.rule_id == RULE_DETECTION_UPDATE_STALLED)
            .expect("fires for a refusing channel too");
        let lower = p.rationale.to_ascii_lowercase();
        assert!(
            !lower.contains("unreachable") && !lower.contains("could not reach"),
            "nothing here knows the channel is unreachable: {}",
            p.rationale
        );
        assert!(p.rationale.contains("checksum mismatch"), "{}", p.rationale);
    }

    /// The dismissal re-fires after another threshold's worth of failures, so a
    /// permanently dead channel is raised roughly weekly rather than once.
    #[test]
    fn a_dismissed_stall_card_refires_after_another_threshold_of_failures() {
        let stalled = |streak: u32| Signals {
            detection_update_stalled: vec![StalledUpdate {
                component: "rules".into(),
                streak,
                reason: "offline".into(),
            }],
            dismissed: vec![DismissedRule {
                rule_id: RULE_DETECTION_UPDATE_STALLED.to_string(),
                signature: "rules:1".to_string(),
            }],
            ..Signals::default()
        };
        let fires = |s: &Signals| {
            evaluate(s)
                .iter()
                .any(|p| p.rule_id == RULE_DETECTION_UPDATE_STALLED)
        };
        assert!(!fires(&stalled(STALLED_AFTER_CHECKS)), "dismissed");
        assert!(
            !fires(&stalled(STALLED_AFTER_CHECKS * 2 - 1)),
            "still inside the dismissed bucket"
        );
        assert!(
            fires(&stalled(STALLED_AFTER_CHECKS * 2)),
            "another threshold's worth of silence re-raises it"
        );
    }

    /// Dismissal is keyed to the VERSION, so declining one bundle does not
    /// silence the next — the whole point of a freshness canary.
    #[test]
    fn a_dismissed_detection_card_refires_on_the_next_version() {
        let update = |v: &str| AvailableUpdate {
            component: "rules".into(),
            installed: "2026.07.01".into(),
            available: v.into(),
            notes: String::new(),
        };
        let dismissed = vec![DismissedRule {
            rule_id: RULE_DETECTION_UPDATE_AVAILABLE.to_string(),
            signature: "rules:2026.08.07".to_string(),
        }];
        let same = Signals {
            detection_updates: vec![update("2026.08.07")],
            dismissed: dismissed.clone(),
            ..Signals::default()
        };
        assert!(!evaluate(&same)
            .iter()
            .any(|p| p.rule_id == RULE_DETECTION_UPDATE_AVAILABLE));
        let next = Signals {
            detection_updates: vec![update("2026.09.01")],
            dismissed,
            ..Signals::default()
        };
        assert!(evaluate(&next)
            .iter()
            .any(|p| p.rule_id == RULE_DETECTION_UPDATE_AVAILABLE));
    }

    /// The healthy steady state — auto mode applying updates and clearing both
    /// records — says nothing at all.
    #[test]
    fn a_healthy_updater_raises_no_detection_card() {
        let ids: Vec<&str> = evaluate(&Signals::default())
            .iter()
            .map(|p| p.rule_id)
            .collect();
        assert!(!ids.contains(&RULE_DETECTION_UPDATE_AVAILABLE));
        assert!(!ids.contains(&RULE_DETECTION_UPDATE_FAILED));
        assert!(!ids.contains(&RULE_DETECTION_UPDATE_STALLED));
        assert!(!ids.contains(&RULE_DETECTION_SIGNATURE_DOWN));
    }

    // ── V32 Phase C / #48 D-2 — the disarmed signature layer ────────────

    use crate::offload::detection::signature::SignatureDown;

    /// The consumer half of D-2. Before this rule existed, a rules directory
    /// that compiled to nothing had NO consumer: `scan` returned empty for the
    /// rest of the process's life, every page reported clean, and all four
    /// signal channels said the opposite — the badge is derived from settings
    /// toggles, no activity row is written on reload, and the stall card
    /// literally says "Nothing is degraded".
    #[test]
    fn a_disarmed_signature_layer_raises_a_warn_only_card() {
        let sig = Signals {
            detection_signature_down: Some(SignatureDown {
                dir: r"C:\cimp\detection\rules.d".into(),
                files_loaded: 0,
                files_failed: 3,
                rules: 0,
                failed: vec!["injection_core.yar".into(), "local/mine.yar".into()],
            }),
            ..Signals::default()
        };
        let p = evaluate(&sig)
            .into_iter()
            .find(|p| p.rule_id == RULE_DETECTION_SIGNATURE_DOWN)
            .expect("a layer with no rules must say so");
        assert!(
            p.warn_only,
            "the fix is a file on disk, not a settings write"
        );
        assert!(p.setting.is_empty());
        assert!(p.action.is_none());
        // The card must not read as reassurance: the whole defect was surfaces
        // claiming a disarmed layer was fine.
        assert!(
            p.rationale.contains("no rules to match against"),
            "{}",
            p.rationale
        );
        assert!(
            p.rationale.contains(r"C:\cimp\detection\rules.d"),
            "names where to look: {}",
            p.rationale
        );
        assert!(p.current.contains("injection_core.yar"), "{}", p.current);
        assert_eq!(p.signature, r"C:\cimp\detection\rules.d:0:3");
    }

    /// The signature is the state the user looked at, so a dismissal holds for
    /// that state and re-raises when the directory changes underneath it — a
    /// half-repaired directory that is still disarmed is a new fact.
    #[test]
    fn a_dismissed_signature_down_card_refires_when_the_directory_changes() {
        let down = |loaded: usize, failed: usize| Signals {
            detection_signature_down: Some(SignatureDown {
                dir: "/rules.d".into(),
                files_loaded: loaded,
                files_failed: failed,
                rules: 0,
                failed: vec!["a.yar".into()],
            }),
            dismissed: vec![DismissedRule {
                rule_id: RULE_DETECTION_SIGNATURE_DOWN.to_string(),
                signature: "/rules.d:0:3".to_string(),
            }],
            ..Signals::default()
        };
        let fires = |s: &Signals| {
            evaluate(s)
                .iter()
                .any(|p| p.rule_id == RULE_DETECTION_SIGNATURE_DOWN)
        };
        assert!(!fires(&down(0, 3)), "dismissed for the state it described");
        assert!(
            fires(&down(1, 2)),
            "one file repaired and the layer is STILL disarmed — a new fact"
        );
    }

    /// `None` is both healthy states — rules are live, or the user switched the
    /// layer off — and the producer resolves the switch, so nothing here has to
    /// know about settings.
    #[test]
    fn no_signature_signal_means_no_signature_card() {
        assert!(!evaluate(&Signals {
            detection_signature_down: None,
            ..Signals::default()
        })
        .iter()
        .any(|p| p.rule_id == RULE_DETECTION_SIGNATURE_DOWN));
    }

    // ── #48 U-4 — a user rule that does not compile ─────────────────────

    use crate::offload::detection::signature::RenamedRule;
    use crate::offload::detection::updater::BrokenLocalRules;

    fn broken_local(files: &[&str]) -> Signals {
        local_rules(files, &[])
    }

    fn local_rules(files: &[&str], renamed: &[(&str, &str, &str)]) -> Signals {
        Signals {
            detection_local_rules_broken: Some(BrokenLocalRules {
                dir: r"C:\cimp\detection\rules.d".into(),
                failed: files.iter().map(|f| (*f).to_string()).collect(),
                renamed: renamed
                    .iter()
                    .map(|(file, from, to)| RenamedRule {
                        file: (*file).to_string(),
                        from: (*from).to_string(),
                        to: (*to).to_string(),
                    })
                    .collect(),
                files_loaded: 3,
                rules: 12,
            }),
            ..Signals::default()
        }
    }

    /// The consumer for U-4's other half. Stopping a broken `local/` file from
    /// vetoing the update channel is only half a fix: without a card the user's
    /// own rules are silently not protecting them, and the only trace is a
    /// `warn!` line. The card must say what is skipped AND that the rest is
    /// live, or it reads as "detection is off".
    #[test]
    fn a_broken_user_rule_raises_a_warn_only_card() {
        let p = evaluate(&broken_local(&["local/mine.yar"]))
            .into_iter()
            .find(|p| p.rule_id == RULE_DETECTION_LOCAL_RULES_BROKEN)
            .expect("a skipped user rule must say so");
        assert!(p.warn_only, "the fix is a file on disk");
        assert!(p.setting.is_empty());
        assert!(p.action.is_none());
        assert!(p.current.contains("local/mine.yar"), "{}", p.current);
        assert!(
            p.current.contains("12 rule(s) still live"),
            "the card must not read as an outage: {}",
            p.current
        );
        assert!(p.rationale.contains("do not compile"), "{}", p.rationale);
        // The identifier-collision cause is named, because it is the one an
        // update can introduce without the user's file changing at all.
        assert!(p.rationale.contains("IDENTIFIER"), "{}", p.rationale);
        assert!(
            p.rationale.contains(r"C:\cimp\detection\rules.d"),
            "{}",
            p.rationale
        );
    }

    /// Signed by the failing file NAMES, so a dismissal holds for the files the
    /// user looked at and re-raises when a different file breaks.
    #[test]
    fn a_dismissed_broken_rule_card_refires_for_a_different_file() {
        let dismissed = vec![DismissedRule {
            rule_id: RULE_DETECTION_LOCAL_RULES_BROKEN.to_string(),
            signature: "local/mine.yar".to_string(),
        }];
        let fires = |files: &[&str]| {
            let mut s = broken_local(files);
            s.dismissed = dismissed.clone();
            evaluate(&s)
                .iter()
                .any(|p| p.rule_id == RULE_DETECTION_LOCAL_RULES_BROKEN)
        };
        assert!(!fires(&["local/mine.yar"]), "dismissed for what it named");
        assert!(
            fires(&["local/other.yar"]),
            "a different file is a new fact"
        );
        assert!(
            fires(&["local/mine.yar", "local/other.yar"]),
            "a widened set is a new fact"
        );
    }

    /// **#48/M-13 — a renamed rule is a NOTICE, not an outage, and it must
    /// still reach the user.**
    ///
    /// The collision that used to freeze the update channel now resolves by
    /// loading the user's rule under a `custom_` identifier. That is a silent
    /// success unless something says so, and a silent success is what every
    /// finding in this milestone turned out to be. So the card fires with no
    /// broken file at all, and it must describe the rename in the rename's own
    /// words — never in the broken file's.
    ///
    /// What would this still pass with? Not a card that merely mentions the
    /// file: it pins the OLD and NEW identifiers (the actionable half — the old
    /// name is what a user's saved search keys on), the "not modified" promise
    /// that justifies not touching their file, and the absence of the
    /// "do not compile" sentence, which would be false about a rule that is
    /// matching.
    #[test]
    fn a_renamed_user_rule_raises_a_card_that_does_not_call_it_broken() {
        let p = evaluate(&local_rules(
            &[],
            &[("local/mine.yar", "Dup_Rule", "custom_Dup_Rule")],
        ))
        .into_iter()
        .find(|p| p.rule_id == RULE_DETECTION_LOCAL_RULES_BROKEN)
        .expect("a renamed user rule must say so");
        assert!(p.warn_only, "the fix is a file on disk");
        assert!(p.current.contains("Dup_Rule"), "{}", p.current);
        assert!(p.current.contains("custom_Dup_Rule"), "{}", p.current);
        assert!(p.current.contains("local/mine.yar"), "{}", p.current);
        assert!(
            p.current.contains("12 rule(s) still live"),
            "the card must not read as an outage: {}",
            p.current
        );
        assert!(
            !p.rationale.contains("do not compile"),
            "a renamed rule IS matching; describing it as broken is the exact lie this \
             milestone keeps finding: {}",
            p.rationale
        );
        assert!(p.rationale.contains("still matching"), "{}", p.rationale);
        assert!(
            p.rationale.contains("NOT modified"),
            "the promise that justifies renaming at load time must be stated: {}",
            p.rationale
        );
    }

    /// A dismissal of the broken-file card must not also silence a rename that
    /// shows up later — the two are different facts on one card, so the
    /// signature carries both.
    #[test]
    fn a_dismissed_broken_rule_card_refires_when_a_rule_is_renamed() {
        let dismissed = vec![DismissedRule {
            rule_id: RULE_DETECTION_LOCAL_RULES_BROKEN.to_string(),
            signature: "local/mine.yar".to_string(),
        }];
        let fires = |renamed: &[(&str, &str, &str)]| {
            let mut s = local_rules(&["local/mine.yar"], renamed);
            s.dismissed = dismissed.clone();
            evaluate(&s)
                .iter()
                .any(|p| p.rule_id == RULE_DETECTION_LOCAL_RULES_BROKEN)
        };
        assert!(!fires(&[]), "dismissed for exactly what it named");
        assert!(
            fires(&[("local/other.yar", "Dup", "custom_Dup")]),
            "a rename is a new fact the dismissal never covered"
        );
    }

    /// `None` is the healthy steady state, and the producer owns every reason to
    /// stay quiet (layer off, everything compiles with no collision, the failure
    /// is in a bundle file, or the layer is disarmed and the louder card is
    /// already up).
    #[test]
    fn no_broken_rule_signal_means_no_card() {
        assert!(!evaluate(&Signals::default())
            .iter()
            .any(|p| p.rule_id == RULE_DETECTION_LOCAL_RULES_BROKEN));
    }

    // ── #48 M-11 — the live rule set is SHORT of files ──────────────────

    use crate::offload::detection::updater::RulesIncomplete;

    fn incomplete(files: &[&str]) -> Signals {
        Signals {
            detection_rules_incomplete: vec![RulesIncomplete {
                component: "rules".into(),
                files: files.iter().map(|f| (*f).to_string()).collect(),
                dir: r"C:\cimp\detection\rules.d".into(),
            }],
            ..Signals::default()
        }
    }

    /// The consumer M-11 needs, and the reason it could not be folded into
    /// `detection.update_failed.v1`: that card's whole reassurance is "nothing
    /// is degraded right now, the previous data is still live", and this one
    /// exists because that sentence is false.
    #[test]
    fn an_incomplete_rule_set_raises_its_own_card_naming_the_missing_files() {
        let p = evaluate(&incomplete(&["core.yar"]))
            .into_iter()
            .find(|p| p.rule_id == RULE_DETECTION_RULES_INCOMPLETE)
            .expect("a short rule set must say so");
        assert!(p.warn_only, "the fix is a file handle, not a setting");
        assert!(p.setting.is_empty());
        assert!(p.action.is_none());
        assert!(p.current.contains("core.yar"), "{}", p.current);
        assert!(
            p.rationale.contains("FEWER rules"),
            "the card must say it is degraded NOW: {}",
            p.rationale
        );
        assert!(
            p.rationale.contains("not lost") && p.rationale.contains("retries"),
            "…and that it is recoverable, or the user will not wait for it: {}",
            p.rationale
        );
        assert!(
            p.rationale.contains(r"C:\cimp\detection\rules.d"),
            "{}",
            p.rationale
        );
    }

    /// Signed by the missing file names, so a dismissal holds for the set the
    /// user looked at and re-raises if it grows.
    #[test]
    fn a_dismissed_incomplete_card_refires_when_more_files_go_missing() {
        let dismissed = vec![DismissedRule {
            rule_id: RULE_DETECTION_RULES_INCOMPLETE.to_string(),
            signature: "rules:core.yar".to_string(),
        }];
        let fires = |files: &[&str]| {
            let mut s = incomplete(files);
            s.dismissed = dismissed.clone();
            evaluate(&s)
                .iter()
                .any(|p| p.rule_id == RULE_DETECTION_RULES_INCOMPLETE)
        };
        assert!(!fires(&["core.yar"]), "dismissed for what it named");
        assert!(
            fires(&["core.yar", "extra.yar"]),
            "a wider gap is a new fact"
        );
    }

    /// Empty is the healthy steady state, and — unlike the two cards above —
    /// this one is NOT suppressed by any of them: it is the only signal in the
    /// detection family that reports a present, ongoing loss of coverage, so it
    /// has to be able to fire alongside a refusal card that says the opposite.
    #[test]
    fn an_incomplete_card_fires_even_beside_a_refusal_card() {
        assert!(!evaluate(&Signals::default())
            .iter()
            .any(|p| p.rule_id == RULE_DETECTION_RULES_INCOMPLETE));

        let mut s = incomplete(&["core.yar"]);
        s.detection_update_failures = vec![crate::offload::detection::updater::FailedUpdate {
            component: "rules".into(),
            version: "2026.08.09".into(),
            signature: "2026.08.09".into(),
            reason: "the activated bundle did not load cleanly".into(),
        }];
        let ids: Vec<&str> = evaluate(&s).iter().map(|p| p.rule_id).collect();
        assert!(ids.contains(&RULE_DETECTION_RULES_INCOMPLETE), "{ids:?}");
        assert!(ids.contains(&RULE_DETECTION_UPDATE_FAILED), "{ids:?}");
    }
}
