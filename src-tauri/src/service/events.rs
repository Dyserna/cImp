//! V42 F6 (#131) — the app's Tauri event names, in ONE table.
//!
//! Every `emit`/`emit_to_window` name used to be a string literal at its call
//! site, matched by hand to a `listen('…')` literal in the frontend. Twenty
//! events, forty spellings, and a typo on either side fails **silently**: the
//! emitter emits into the void, the listener never fires, and nothing anywhere
//! says so. That is the same drift class Phase E deleted from the settings
//! mirror, one wire over.
//!
//! So: one const per event, one [`EventDef`] row per const, and three tests
//! that keep the halves joined —
//!
//!  1. `every_const_is_a_row` — a const added without a row (or a row without a
//!     const) is a name the generator would not emit;
//!  2. `no_emit_site_spells_a_literal` — a source scan over the whole crate: an
//!     `emit(...)`/`emit_to_window(...)` whose event-name argument is a string
//!     literal is refused, so a new event cannot be added the old way;
//!  3. `service::codegen` writes `src/lib/generated/events.ts` from [`ALL`],
//!     committed and CI-diff-gated, so the frontend's constants are these
//!     constants and its `listen()` calls cannot spell anything else
//!     (`src/lib/eventNames.test.ts` refuses a literal on that side).
//!
//! **The wire does not change.** Every `name` below is byte-identical to the
//! literal it replaced, and no payload was touched: this is naming and
//! generation only.
//!
//! ## The payload column
//!
//! [`EventDef::payload`] is a TypeScript **type expression**, written as an
//! inline `import('…')` so the generated file needs no import statements (the
//! same trick Phase E's `#[ts(type = …)]` seams use). It names a type the
//! frontend ALREADY declares — a ts-rs-generated one, or a hand-kept wire
//! interface. Nothing here invents new payload codegen: an event whose payload
//! the frontend does not model is spelled `unknown`, deliberately, and the
//! reason is on its row.
//!
//! ## The five alias consts
//!
//! `audit::runner::AUDIT_STATUS_EVENT`, `graph::service::GRAPH_STATUS_EVENT`,
//! `workbench::FS_BATCH_EVENT`, `service::window::DEEP_LINK_EVENT` and
//! `delegation::engine::EVENT_DELEGATION_CHANGED` predate this table and are
//! referenced by name across their modules. They stay where they are and are
//! now DEFINED from the row below, so the string is still spelled exactly once.
//! A new event gets no alias.

/// How an event reaches the frontend — the half that decides which windows see
/// it, and the reason `emit_to_window` exists at all.
///
/// Test-cfg, with [`EventDef`] and [`ALL`]: the table's only consumer is
/// `service::codegen`, so carrying it in the shipped binary would be three
/// items of dead weight and three `dead_code` warnings. The CONSTANTS below are
/// production — they are what the emitters use.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// `AppHandle::emit` — every webview.
    Broadcast,
    /// `AppHandle::emit_to` — one window by label.
    Window,
}

/// One event: the wire name, the TypeScript constant the frontend imports, the
/// TS type of its payload, and how it is delivered. Test-cfg; see [`Delivery`].
#[cfg(test)]
pub struct EventDef {
    /// The wire name. Byte-identical to the literal it replaced.
    pub name: &'static str,
    /// The generated TypeScript constant's identifier.
    pub ts: &'static str,
    /// The payload's TypeScript type expression (see the module docs).
    pub payload: &'static str,
    /// Broadcast to every webview, or targeted at one window.
    pub delivery: Delivery,
    /// Rendered as the constant's doc comment in the generated file.
    pub doc: &'static str,
}

pub const AI_TAB_RESTART_HINT: &str = "ai-tab-restart-hint";
pub const AUDIO_AMPLITUDE: &str = "audio-amplitude";
pub const AUDIT_STATUS: &str = "audit-status";
pub const AVATAR_ERROR: &str = "avatar-error";
pub const AVATAR_STATE: &str = "avatar-state";
pub const DELEGATION_CHANGED: &str = "delegation-changed";
pub const FS_BATCH: &str = "fs-batch";
pub const GRAPH_ANALYSES: &str = "graph-analyses";
pub const GRAPH_STATUS: &str = "graph-status";
pub const LLM_PRICING_CHANGED: &str = "llm-pricing-changed";
pub const MIC_AMPLITUDE: &str = "mic-amplitude";
pub const OFFLOAD_SERVER_METRICS: &str = "offload-server-metrics";
pub const OFFLOAD_SERVER_OUTPUT: &str = "offload-server-output";
pub const OFFLOAD_STATE: &str = "offload-state";
pub const PTY_EXIT: &str = "pty-exit";
pub const SETTINGS_CHANGED: &str = "settings-changed";
pub const SETTINGS_DEEP_LINK: &str = "settings-deep-link";
pub const STT_STATE: &str = "stt-state";
pub const STT_TRANSCRIPTION: &str = "stt-transcription";
pub const TAB_RESTART_REQUESTED: &str = "tab-restart-requested";

/// Every event the backend emits, sorted by wire name.
///
/// Sorted so the generated file is byte-stable without the generator having to
/// sort anything at emission time — the property the byte-stability test rests
/// on, and the one a `HashMap` would have taken away. Test-cfg; see
/// [`Delivery`].
#[cfg(test)]
pub const ALL: &[EventDef] = &[
    EventDef {
        name: AI_TAB_RESTART_HINT,
        ts: "AI_TAB_RESTART_HINT",
        payload: "string[]",
        delivery: Delivery::Broadcast,
        doc: "The spawn-injection edge hint (#48): the consumer ids whose AI tabs were spawned \
              with settings this change has invalidated. Rendered as the per-tab restart hint.",
    },
    EventDef {
        name: AUDIO_AMPLITUDE,
        ts: "AUDIO_AMPLITUDE",
        payload: "number[]",
        delivery: Delivery::Broadcast,
        doc: "TTS playback amplitude samples, ~50/s, for the avatar's mouth. Hot path — the \
              payload is a bare sample buffer on purpose.",
    },
    EventDef {
        name: AUDIT_STATUS,
        ts: "AUDIT_STATUS",
        payload: "import('../codeAudit/types').AuditSnapshot",
        delivery: Delivery::Broadcast,
        doc: "A Code Audit run's state transition, carrying the whole snapshot (also fetchable \
              with `audit_snapshot`).",
    },
    EventDef {
        name: AVATAR_ERROR,
        ts: "AVATAR_ERROR",
        payload: "import('../avatarState').AvatarErrorInfo",
        delivery: Delivery::Broadcast,
        doc: "An avatar/TTS subsystem error, surfaced in the avatar overlay.",
    },
    EventDef {
        name: AVATAR_STATE,
        ts: "AVATAR_STATE",
        payload: "unknown",
        delivery: Delivery::Broadcast,
        doc: "The tagged state/tab-lifecycle stream (`state::manager::StateEvent`). PAYLOAD IS \
              `unknown` BY DESIGN: two listeners narrow this union to different, partial shapes \
              — `avatarState.ts`'s `StateEvent` omits the `tts-selection-progress` and \
              `turn-ended` variants that `selectionTts.ts` reads — so there is no single \
              frontend type to name here, and inventing one would be a new hand-written mirror \
              of exactly the kind this table exists to delete. Each listener keeps its own \
              explicit `listen<…>` generic.",
    },
    EventDef {
        name: DELEGATION_CHANGED,
        ts: "DELEGATION_CHANGED",
        payload: "import('../delegation').DelegationChanged",
        delivery: Delivery::Broadcast,
        doc: "V39: a cross-harness delegation's state moved. A dedicated event rather than a \
              `settings-changed` piggyback — nothing about a delegation is in settings.",
    },
    EventDef {
        name: FS_BATCH,
        ts: "FS_BATCH",
        payload: "{ root: string; paths: string[]; truncated: boolean }",
        delivery: Delivery::Broadcast,
        doc: "A debounced batch of filesystem paths the graph watcher saw change. The payload is \
              spelled inline: `workbench::FsBatch` has no frontend counterpart, because every \
              listener treats the event as a bare signal and re-queries.",
    },
    EventDef {
        name: GRAPH_ANALYSES,
        ts: "GRAPH_ANALYSES",
        payload: "import('../graph').GraphAnalyses",
        delivery: Delivery::Broadcast,
        doc: "The post-build analyses (dead exports, import cycles) for the current project.",
    },
    EventDef {
        name: GRAPH_STATUS,
        ts: "GRAPH_STATUS",
        payload: "import('../graph').GraphStatus",
        delivery: Delivery::Broadcast,
        doc: "The graph indexer's status — building/idle, counts, last error.",
    },
    EventDef {
        name: LLM_PRICING_CHANGED,
        ts: "LLM_PRICING_CHANGED",
        payload: "null",
        delivery: Delivery::Broadcast,
        doc: "The model price table was refreshed. A bare signal: the emitter sends `()`, and the \
              listener re-fetches.",
    },
    EventDef {
        name: MIC_AMPLITUDE,
        ts: "MIC_AMPLITUDE",
        payload: "number[]",
        delivery: Delivery::Broadcast,
        doc: "Microphone amplitude samples while dictation is recording. Same hot path as \
              `audio-amplitude`.",
    },
    EventDef {
        name: OFFLOAD_SERVER_METRICS,
        ts: "OFFLOAD_SERVER_METRICS",
        payload: "import('../offload').BackendDashboard[]",
        delivery: Delivery::Broadcast,
        doc: "One dashboard row per configured offload backend, polled by the supervisor.",
    },
    EventDef {
        name: OFFLOAD_SERVER_OUTPUT,
        ts: "OFFLOAD_SERVER_OUTPUT",
        payload: "import('../offload').ServerLogLine",
        delivery: Delivery::Broadcast,
        doc: "One line of a supervised offload server's stdout/stderr, live-tailed into the \
              read-only log panel.",
    },
    EventDef {
        name: OFFLOAD_STATE,
        ts: "OFFLOAD_STATE",
        payload: "import('../offload').OffloadState",
        delivery: Delivery::Broadcast,
        doc: "The offload supervisor's lifecycle state (stopped/starting/ready/failed).",
    },
    EventDef {
        name: PTY_EXIT,
        ts: "PTY_EXIT",
        payload: "import('../ipc').PtyExitPayload",
        delivery: Delivery::Broadcast,
        doc: "A PTY session's child process exited, with the rendered exit description.",
    },
    EventDef {
        name: SETTINGS_CHANGED,
        ts: "SETTINGS_CHANGED",
        payload: "import('../settings/types').Settings",
        delivery: Delivery::Broadcast,
        doc: "The whole settings tree, broadcast once at startup and again after every persisted \
              change. The frontend store's only writer.",
    },
    EventDef {
        name: SETTINGS_DEEP_LINK,
        ts: "SETTINGS_DEEP_LINK",
        payload: "{ kind: string; tab_id?: string; section?: string }",
        delivery: Delivery::Window,
        doc: "V1.4-07: the hot half of the Settings deep link — sent to the Settings window when \
              it is ALREADY open (the cold half is drained from an armed slot on mount). The \
              payload is spelled inline: it is built ad hoc with `serde_json::json!` on the Rust \
              side and has no named struct to mirror.",
    },
    EventDef {
        name: STT_STATE,
        ts: "STT_STATE",
        payload: "{ state: import('../stt').SttState }",
        delivery: Delivery::Broadcast,
        doc: "The dictation engine's state. Wrapped in an object by the emitter, not a bare \
              string.",
    },
    EventDef {
        name: STT_TRANSCRIPTION,
        ts: "STT_TRANSCRIPTION",
        payload: "{ text: string }",
        delivery: Delivery::Broadcast,
        doc: "A finished dictation transcript, ready to be typed into the focused tab.",
    },
    EventDef {
        name: TAB_RESTART_REQUESTED,
        ts: "TAB_RESTART_REQUESTED",
        payload: "import('../tabs/types').TabId",
        delivery: Delivery::Window,
        doc: "Ask the MAIN window to restart one tab's child. Window-targeted: the Settings \
              window has no terminals and must not act on it.",
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rustsrc;

    /// This module's own source, for the const-to-row scan below.
    const SELF_SRC: &str = include_str!("events.rs");

    /// Every `pub const … : &str = "…";` declared here has a row in [`ALL`],
    /// and every row's name is one of those consts.
    ///
    /// Both directions, because the two failures are different: a const with no
    /// row is an event the generator never tells the frontend about, and a row
    /// with no const is a name the Rust side cannot emit without re-spelling a
    /// literal — the thing this table exists to make impossible.
    #[test]
    fn every_const_is_a_row() {
        let code = rustsrc::uncommented("service/events.rs", SELF_SRC);
        let mut declared: Vec<String> = Vec::new();
        for line in code.lines() {
            let line = line.trim();
            let Some(rest) = line.strip_prefix("pub const ") else {
                continue;
            };
            let Some((_, value)) = rest.split_once(": &str = ") else {
                continue;
            };
            let value = value.trim_end_matches(';').trim();
            let unquoted = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or_else(|| panic!("`{line}` does not declare a literal event name"));
            declared.push(unquoted.to_string());
        }
        assert!(
            declared.len() >= 15,
            "the const scan found only {} names — a broken scan finds no drift while \
             reporting ok",
            declared.len()
        );

        let mut rows: Vec<String> = ALL.iter().map(|d| d.name.to_string()).collect();
        declared.sort();
        rows.sort();
        assert_eq!(
            declared, rows,
            "a `pub const` here has no `ALL` row (the generator would never emit it), or an \
             `ALL` row has no const (the emitters would have to re-spell the literal)"
        );
    }

    /// `ALL` is sorted by wire name and free of duplicates — the byte-stability
    /// the generated file's CI diff gate rests on.
    #[test]
    fn the_table_is_sorted_and_unique() {
        let names: Vec<&str> = ALL.iter().map(|d| d.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            names, sorted,
            "`ALL` must stay sorted by name and hold each name once — the generated \
             `events.ts` is emitted in this order and diffed byte-exactly"
        );

        let ts: Vec<&str> = ALL.iter().map(|d| d.ts).collect();
        let mut uniq = ts.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(
            uniq.len(),
            ts.len(),
            "two rows claim the same TypeScript constant name"
        );
        for d in ALL {
            assert!(
                !d.ts.is_empty()
                    && d.ts
                        .bytes()
                        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'),
                "`{}` is not a SCREAMING_SNAKE TypeScript identifier",
                d.ts
            );
            assert!(
                !d.payload.is_empty(),
                "`{}` declares no payload type",
                d.name
            );
            assert!(!d.doc.is_empty(), "`{}` has no doc", d.name);
        }
    }

    /// The files a source scan is allowed to find an emit-with-a-literal in,
    /// each with the reason.
    ///
    /// Deliberately tiny. Every entry is a place where the "event name" is not
    /// an app event at all, so pointing it at [`ALL`] would be a lie rather
    /// than a fix.
    const LITERAL_EMIT_ALLOWED: &[(&str, &str)] = &[
        // The `EventSink` trait, its Tauri implementation, and its unit tests.
        // The production code here emits the `event: &str` it was HANDED (no
        // literal); the tests emit synthetic names ("tab-created") into a
        // recording fake to prove the trait plumbing, and those are not app
        // events — an `ALL` row for them would claim the app emits something it
        // does not. This exclusion is also the positive control for the scan:
        // see `the_emit_scan_finds_what_it_claims_to_find`.
        (
            "service/sink.rs",
            "the sink trait itself + fake-sink unit tests using synthetic names",
        ),
    ];

    /// Step from just after an `emit`-family `(` to the first byte of the
    /// EVENT-NAME argument.
    ///
    /// `emit_to`/`emit_to_window` take the target first, so the name is the
    /// second argument; `emit` takes it first. Shared by the scan and its
    /// control so the two cannot disagree about where they are looking.
    fn name_arg_at(bytes: &[u8], mut i: usize, second_arg: bool) -> usize {
        let skip_ws = |b: &[u8], mut i: usize| {
            while i < b.len() && (b[i] as char).is_whitespace() {
                i += 1;
            }
            i
        };
        i = skip_ws(bytes, i);
        if second_arg {
            let mut depth = 0i32;
            while i < bytes.len() {
                match bytes[i] {
                    b'(' | b'[' => depth += 1,
                    b')' | b']' => {
                        if depth == 0 {
                            break;
                        }
                        depth -= 1;
                    }
                    b',' if depth == 0 => {
                        i += 1;
                        break;
                    }
                    _ => {}
                }
                i += 1;
            }
            i = skip_ws(bytes, i);
        }
        i
    }

    /// No `emit`/`emit_to`/`emit_to_window` call site anywhere in the crate
    /// spells its event name as a string literal.
    ///
    /// This is the half that makes the table binding. `every_const_is_a_row`
    /// keeps the table internally consistent; without this scan a new event
    /// could still be added the old way — one literal at one call site, matched
    /// by hand to one literal in the frontend — and the whole apparatus would
    /// sit beside it saying nothing.
    ///
    /// Searches `code_of`-blanked source (comments and literals blanked, byte
    /// offsets preserved) for the call, then reads the ORIGINAL bytes at that
    /// offset to see whether the argument really is a literal. Blanking first is
    /// what keeps the module-doc examples in `service/pty.rs` and
    /// `service/settings.rs` — which spell `app.emit("pty-exit", …)` in prose —
    /// from being reported as call sites.
    #[test]
    fn no_emit_site_spells_a_literal() {
        let files = rustsrc::source_files();
        let mut scanned = 0usize;
        let mut offenders: Vec<String> = Vec::new();

        for (rel, src) in files {
            if LITERAL_EMIT_ALLOWED.iter().any(|(f, _)| f == rel) {
                continue;
            }
            let code = rustsrc::code_of(rel, src);
            let bytes = src.as_bytes();
            for needle in [".emit(", ".emit_to(", ".emit_to_window("] {
                for (at, _) in code.match_indices(needle) {
                    scanned += 1;
                    let i = name_arg_at(bytes, at + needle.len(), needle != ".emit(");
                    if bytes.get(i) == Some(&b'"') {
                        let line = src[..i].lines().count();
                        let shown: String =
                            src[i..].chars().take_while(|c| *c != '\n').take(48).collect();
                        offenders.push(format!("{rel}:{line}: {shown}"));
                    }
                }
            }
        }

        assert!(
            scanned >= 20,
            "the emit scan found only {scanned} call sites — a scan that walks nothing \
             reports clean"
        );
        assert!(
            offenders.is_empty(),
            "these emit sites spell their event name as a literal instead of using a \
             `service::events` constant — a typo here fails silently (the listener never \
             fires):\n  {}",
            offenders.join("\n  ")
        );
    }

    /// The control for the scan above: it really does see a literal when one is
    /// there.
    ///
    /// `no_emit_site_spells_a_literal` passes by finding nothing, which is also
    /// how a scan that reads the wrong bytes passes. The allowlist supplies a
    /// free positive control — `service/sink.rs` is excluded precisely BECAUSE
    /// it contains literal emits, so running the same predicate over it must
    /// report them.
    #[test]
    fn the_emit_scan_finds_what_it_claims_to_find() {
        let files = rustsrc::source_files();
        let (rel, src) = files
            .iter()
            .find(|(rel, _)| rel == "service/sink.rs")
            .expect("service/sink.rs is in the walk");
        let code = rustsrc::code_of(rel, src);
        let bytes = src.as_bytes();
        let mut found = 0usize;
        for needle in [".emit(", ".emit_to_window("] {
            for (at, _) in code.match_indices(needle) {
                let i = name_arg_at(bytes, at + needle.len(), needle != ".emit(");
                if bytes.get(i) == Some(&b'"') {
                    found += 1;
                }
            }
        }
        assert!(
            found >= 2,
            "the scan found {found} literal emits in service/sink.rs, which has several — \
             the predicate is reading the wrong bytes, and the tree-wide scan built on it is \
             vacuous"
        );
    }
}
