# Feature: Aider Integration Parity

## Purpose

Bring the aider tab to parity with the Claude Code tab on two fronts:

1. **TTS markup injection** — inject the `[[TTS]]...[[/TTS]]` instructions into aider's system prompt at launch, the same way the Claude tab does via `claude --append-system-prompt`. Today the aider tab spawns aider with no markup-aware system prompt, so no segments are spoken.
2. **Permission prompt detection** — detect aider's confirmation prompts (e.g., "Apply this edit? (Y)es/(N)o/(D)on't ask again") and trigger the same `AwaitingPermission` avatar state and notification that the Claude tab gets via the patterns landed in V2-03.

Both are blocked today, but on different things. **TTS injection is blocked on aider upstream** (no CLI flag for system prompt injection yet); **permission detection is blocked on pattern enumeration** (someone has to sit down and produce robust regexes from aider's source / live runs). Different blockers, but the implementation surfaces overlap enough — both touch the aider-tab spawn and processing path — that they belong in one feature doc.

See `FUTURE-FEATURES.md` § "Aider TTS markup injection" and § "Aider permission detection patterns" for the full per-item rationale and monitoring guidance; this doc captures the implementation strategy.

## Items in this group

1. **Aider TTS markup injection** — externally blocked.
2. **Aider permission detection patterns** — pattern-enumeration blocked, not externally.

## Per-item implementation

### 1. TTS markup injection

#### Pre-pickup: monitor for upstream

`FUTURE-FEATURES.md` lists three monitoring signals (periodic `aider --help` check, GitHub issue subscription on aider#4817 and related, search for `aider --append-system-prompt` or similar). Re-read the upstream issue when triggering this work to confirm the flag's exact name and semantics — flag may have changed since FUTURE-FEATURES.md was written.

#### When upstream lands

1. **Verify the mechanism.** Check aider's release notes and CLI reference. Note the flag's name, semantics (replace vs. append), and file vs. inline-string semantics. We need: append, file-based or inline-string, applied at every turn.
2. **Update the aider tab spawn.** Today the aider tab is launched as a PTY subprocess from a single helper (find in `src-tauri/src/...` near the Claude tab spawn path; both are PTY launches with arg-list construction). Pass the new flag with cctts's TTS markup instructions. The change should be isolated to that one helper.
3. **File vs. inline string:**
   - If aider's flag is inline-string-based (e.g., `--append-system-prompt "..."`), pass markup directly. Simplest. Same approach as Claude.
   - If file-based only (e.g., `--system-prompt-extras <file>`), write a temp file at app launch with markup instructions, pass the path, clean up on subprocess exit. Use OS temp dir (`%TEMP%` on Windows, `/tmp` on Linux).
   - If both available, prefer inline-string to avoid temp-file management.
4. **Per-tab "TTS injection enabled" toggle.** Default on. Lets the user disable injection if a particular model performs poorly with the markup convention. **Per-tab** so the Claude tab and aider tab can be controlled independently. Add to `ConfigureTabDialog.svelte` for AI-tool tabs.
5. **README updates.** Remove the "aider TTS not supported" caveat. Note that markup compliance still depends on the local model's instruction-following ability — small local models may not wrap content in tags reliably even when instructed.
6. **Test with both cloud and local models.** Verify markup tags appear in output for at least one capable model (e.g., Claude via aider's API integration, GPT-4o). Accept that smaller local models may be inconsistent.

#### TTS markup instructions reuse

The instructions string (or file content) cctts already injects into Claude's system prompt is in `docs/CLAUDE.md` (the `# TTS-Aware Output` section). Reuse the exact same content for aider. The instructions are tool-agnostic; they only describe the markup convention. Centralize in a single source-of-truth string at runtime — don't fork the wording per tab kind.

### 2. Permission detection patterns

This is **not externally blocked** — aider's prompts exist and are observable. The blocker is the time investment to enumerate them robustly. When picked up:

1. **Enumerate prompt sites.** Two complementary approaches:
   - Read aider's source (GitHub) for confirmation-prompt strings. Search for `input(` and `input_y_n(` (or whatever aider's prompt helpers are named at the time).
   - Run aider through its main flows (apply edit, create file, run command, model command, /quit confirmation if applicable) and capture the exact prompt strings from a PTY transcript.
2. **Build the regex set.** One regex per prompt type, all alternated together into the existing permission-detection processing layer. Reuse the V2-03 architecture — the Claude permission patterns live in `src-tauri/src/...` (find via the V2-03 milestone notes); add an `aider` pattern set adjacent.

   **Example shape** (illustrative; verify wording at implementation):
   ```
   r"^Apply this edit\? \(\(Y\)es/\(N\)o/\(D\)on't ask again\)\s*$"
   r"^Create new file [^?]+\? \(\(Y\)es/\(N\)o\)\s*$"
   r"^Run shell command\? \(\(Y\)es/\(N\)o/\(A\)lways\)\s*$"
   ```
   Embed file paths and other variable text as `.+?` or character-class ranges, not literal — make the patterns survive aider's wording variations across versions.

3. **Tests with captured fixtures.** Snapshot the captured prompt strings as test fixtures; verify each regex matches its target and doesn't false-positive on adjacent normal output. Critical given aider's chat-style stdout — a regex too loose will fire on prose mentioning "apply" or "yes/no."

4. **Per-tab "Aider permission detection enabled" setting.** Default on. Lets the user disable if false positives bite. Add to `ConfigureTabDialog.svelte` for aider tabs.

5. **State machine integration.** When a pattern matches, set the tab's `AwaitingPermission` flag (existing v2 state machine) and trigger the existing notification. Clear the flag on the next non-prompt output line — same logic as the Claude path. The pattern recognizer is the only new code; downstream state handling is unchanged.

6. **Document maintenance burden.** Permission patterns are version-coupled to aider. Add a note in `docs/MAINTENANCE.md` saying "aider permission patterns may need re-validation when aider updates; symptom is the `AwaitingPermission` state failing to fire on a prompt that worked previously, or false-positive notifications on normal prose."

## Shared design

### Per-tab settings shape

Both items add a per-tab setting (`tts_injection_enabled`, `aider_permission_detection_enabled`). Add them to the tab schema together in one PR even if only one item is shipping at the time, so the schema surface for AI-tool tabs is settled in one place. The settings are scoped by tab kind:

- `tts_injection_enabled: bool` — applies to Claude and aider tabs (and any future AI-tool tab kind). Defaults: `true` for tabs that have a CLI mechanism for injection (Claude today, aider once item 1 lands), absent/`null` otherwise.
- `aider_permission_detection_enabled: bool` — applies only to aider tabs.

A future symmetric `claude_permission_detection_enabled` could exist for parity, but the V2-03 patterns are stable enough that disabling them isn't a needed escape hatch yet — defer.

### Configure Tab dialog placement

Add an "AI Tool Integration" section to `ConfigureTabDialog.svelte`, visible only for Claude and aider tab kinds. Holds the toggles above. Reuse the existing per-tab-kind conditional rendering pattern.

## Open questions

- **What if aider ships a flag with non-append semantics** (e.g., `--system-prompt <file>` that *replaces* the system prompt)? Don't use it — replacing aider's default prompt would break aider's own behavior (file edit conventions, repo-map context, etc.). Wait for an *append* mechanism specifically.
- **Aider config file mechanism**: aider has a `.aider.conf.yml` at the project root that *can* set `system-prompt-extras`-style keys if the upstream feature lands as a config-key rather than a CLI flag. cctts could write that file at launch. Cleaner than CLI flags in some ways. Decide at implementation time based on what aider actually exposes.
- **Pattern enumeration scope**: enumerate *all* aider confirmation prompts initially, or just the most common (apply edit, create file)? Recommend all-at-once. Partial coverage is the worst case — users learn that some prompts trigger the indicator and others don't, lose trust in it.
- **Permission detection in the v1.3+ multi-pane world**: V2-03's patterns matched line-buffered PTY output; the v1.3 architecture didn't change PTY processing. No new concerns expected. Verify at implementation.

## Milestone recommendation

**No milestone docs needed.** Both items are individually one-PR-sized:

- Item 1, when it unblocks, is small (one helper, one toggle, one README update).
- Item 2 is medium (regex enumeration, tests, settings toggle), but bounded — comparable in size to V2-03 itself, which was a single milestone. If item 2 grows during implementation (e.g., aider's prompt structure turns out to be substantially different from Claude's), promote it to a milestone at that point.

Both items can be done independently and in either order. Item 2 specifically does **not** need to wait for item 1 — pattern detection works regardless of whether TTS is injected.

## Files most likely touched

- `src-tauri/src/...` — aider tab spawn helper (item 1), permission pattern module (item 2), tab schema (per-tab toggles)
- `src-tauri/src/settings/{schema,migration}.rs` — new toggle fields
- `src/lib/dialog/ConfigureTabDialog.svelte` — "AI Tool Integration" section
- `docs/CLAUDE.md` — re-source-of-truth for the markup instructions (already exists; reused, not duplicated)
- `README.md` — remove aider TTS caveat (item 1), document aider permission patterns and maintenance burden (item 2)
- `docs/MAINTENANCE.md` — aider pattern re-validation note (item 2)
