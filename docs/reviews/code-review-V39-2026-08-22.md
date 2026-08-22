# V39 adversarial seam review — 2026-08-22

Branch `feat/v39-cross-harness-delegation` at `4fc007e` (25 commits, phases A/B/C).
Reviewer: Opus agent (fresh context, read-only). Orchestrator ruling: **every HIGH and
MEDIUM fixed before merge; LOWs fixed except L-8/L-9 (accepted residuals).** Fix
commits are recorded in `docs/MILESTONE-V39-cross-harness-delegation.md`.

| id | sev | finding | ruling |
|---|---|---|---|
| HIGH-1 | HIGH | Completion signal fires per assistant *message* (both fallback readers are TTS taps), so a mid-turn preamble is returned as the reply and the lock released mid-turn | FIX — fire only on the turn-over edge with the turn's last assistant text; Claude fallback requires a derivable turn boundary or CHP |
| HIGH-2 | HIGH | Model-authored task containing `ESC[201~` breaks out of the bracketed paste → raw keystrokes into a locked worker | FIX — preflight refuses ESC / C0 controls (except `\n`,`\t`), never sanitises; assert on written bytes |
| HIGH-3 | HIGH | `TabActivity.exited` never cleared on `ShellRestarted` → restarted worker refused forever | FIX — reset on restart |
| HIGH-4 | HIGH | Facade error strings name the worker/driver tab → the driver can tell it's a tab | FIX — backend-shaped messages naming only the facade name |
| M-5 | MED | User read-only + driven: prompt relaxation leaves `User` in force, radio disabled | FIX — `prompt_relaxed` honoured by `source()` |
| M-6 | MED | Fenced-code-only reply rejected as "no text"; raw text discarded | FIX — fences substantive unless scaffold; raw text kept in row |
| M-7 | MED | `has_live_reader` set: old reader's removal can delete the new registration | FIX — per-spawn epoch |
| M-8 | MED | Cycle/depth check not atomic with slot claim (A→B ∥ B→A both proceed) | FIX — `claim_checked` under one lock |
| M-9 | MED | Facade name collision dropped silently; Settings still shows it | FIX — mirror rule in UI, explain |
| M-10 | MED | Remote knobs use whole-document save; can revert an in-flight role change (`40d2b32` class) | FIX — narrow IPC `tab_set_delegation_backend` |
| M-11 | MED | Facade `schema` is a request; nothing re-validates | FIX — JSON-parse check + named error; doc states the downgrade |
| L-1 | LOW | Paste write emits `UserSubmit` early | FIX — explicit submit flag on the pipeline |
| L-2 | LOW | Default facade name = tab name leaks into prose | FIX — `worker-<hash4>` |
| L-3 | LOW | Late take-over toasts but cancels nothing | FIX — returns false |
| L-4 | LOW | `cancelled` scan hardcodes four files | FIX — repo-wide scan |
| L-5 | LOW | Banner eats pointer events on the terminal's first row | FIX — pass-through except button |
| L-6 | LOW | `delegation_sig` hashes tier as bool | FIX — serde string |
| L-7 | LOW | Facade ignores cancellation; holds the global permit up to 600 s | FIX — select on token, "driver gone" row |
| L-8 | LOW | Fast-tier facade answer can escalate to a full offload run | ACCEPTED (inert at default Quality) |
| L-9 | LOW | collision `SEEN` set unbounded | ACCEPTED (user-named keys) |

Verified-OK list (read-only enforcement + exemption fixtures, engine release on every
path, latch gating incl. the facade path via `offload_task`'s own gate, facade
synthesis never persisted, plugin-layer literals/gate/fail-closed, UI snapshot
semantics, spawn signature) is in the reviewer's report; the orchestrator spot-checked
the single write path, lock-before-write, and the attribution-line reach independently.
