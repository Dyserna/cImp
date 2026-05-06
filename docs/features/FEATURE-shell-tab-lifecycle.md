# Feature: Shell Tab Lifecycle & Configuration

## Purpose

Five items that round out the shell-tab experience: filling in UI for a schema field that already exists (env vars), adding lifecycle automation (auto-restart, exit history), enriching configurability (profiles), and small UX polish (notification text variables). They cluster because they all touch the same code surfaces — `src/lib/dialog/{NewShellTabDialog,ShellTabFields,ConfigureTabDialog}.svelte` on the frontend, `src-tauri/src/shell/{detect,mod}.rs` and the shell-tab spawn/exit path on the backend, and `settings/schema.rs` for any schema changes.

See `FUTURE-FEATURES.md` § "v1.2 deferrals still pending" for per-item rationale; this doc captures the implementation strategy.

## Items in this group

1. **Per-shell-tab environment variable UI** — UI for the `env: HashMap<String, String>` field that already exists in the schema. Hand-editing settings.json works today; the field just lacks a frontend.
2. **Profiles/templates for shell tabs** — saved shell-tab configurations (name, command, args, cwd, env). Pick "WSL Ubuntu" or "Python venv" from a menu instead of filling in the New Shell Tab dialog every time.
3. **Shell auto-restart on crash** — when a shell subprocess exits unexpectedly, automatically restart with backoff and a max-retry count. Per-tab opt-in setting.
4. **Notification text variables beyond `{code}`** — `{name}`, `{tab_position}`, `{cwd}`, `{timestamp}`, `{pid}` placeholders in notification text editing. Currently only `{code}` is interpolated.
5. **History/log of subprocess exits** — append-to-ring-buffer in memory, persist last N entries. Settings dialog gains a "Subprocess log" tab.

## Shared design

### UI surface

Items 1, 2, 3 expand the existing **Configure Tab dialog** (`ConfigureTabDialog.svelte`) and the **New Shell Tab dialog** (`NewShellTabDialog.svelte`). Both already share a `ShellTabFields.svelte` subcomponent for the common shell-tab inputs (command, args, cwd). Add new fields to that shared component so both dialogs pick them up:

- Env-var editor row (item 1)
- Auto-restart checkbox + max-retries field (item 3)

Item 2 (profiles) adds a *new* affordance to `NewShellTabDialog` — a profile picker at the top, plus a "Save as profile..." button on `ConfigureTabDialog`.

Item 4 (notification text variables) is small — extend the existing `NotificationEditor.svelte` placeholder list and the interpolation function it calls.

Item 5 (exit history) is its own Settings tab (or sub-section of an existing tab — General is fine).

### Backend wiring

Items 1, 2 are pure schema/UI; the shell spawn already reads `env`, `command`, `args`, `cwd` from the tab settings.

Item 3 (auto-restart) needs logic in the shell-exit handler in `src-tauri/src/shell/mod.rs` (or wherever the subprocess-exit event is handled today). When a shell tab exits with a non-zero code *and* `auto_restart` is enabled *and* the retry budget isn't exhausted, schedule a restart with exponential backoff (1s / 2s / 4s / cap at 30s, max 5 attempts). On retry budget exhaustion, fall through to the existing closed-state overlay.

Items 4, 5 share the subprocess-exit event surface. The exit handler:
1. Records the exit in the history buffer (item 5).
2. Renders the user-configured notification text with all interpolated variables (items 4 + existing `{code}`).
3. Triggers the user's notification preference.

These steps are sequential in the same handler. Implementing items 4 and 5 together is natural; doing one without the other doesn't share much code, but they share the *event surface* which is the expensive part to find and modify.

### Schema additions

Item 1: no schema change (`env` already exists on shell tabs).

Item 2: new top-level `shell_profiles: Vec<ShellProfile>` field. `ShellProfile { id, name, command, args, cwd, env, auto_restart, max_retries }`. UI to manage list (Settings tab), and the New Shell Tab dialog gains a profile picker that copies profile fields into the new tab's settings (a profile is a *template*, not a live binding — editing a profile later doesn't retroactively update tabs created from it).

Item 3: new per-shell-tab fields:
```rust
pub struct ShellTabConfig {
    // ... existing fields ...
    pub auto_restart: bool,            // default false
    pub max_retries: u32,              // default 5
}
```

Item 4: no schema change. Just extend the interpolation function.

Item 5: no settings schema change for the buffer itself (it's runtime ring-buffer state). Optional: a `shell_exit_history_size: u32` (default 100) global setting if users want to tune it. Defer until asked.

### Migration

Items 2, 3 add new fields. Migration:
- `shell_profiles` defaults to `[]`.
- For each existing shell tab: `auto_restart = false`, `max_retries = 5`.

Idempotent and additive. Bump settings version, follow v1.2/v1.3 backup pattern.

## Per-item implementation notes

### 1. Per-shell-tab environment variable UI

- Extend `ShellTabFields.svelte` with an env-var editor.
- The existing `src/lib/settings/ArrayEditor.svelte` may be close enough to reuse — check at implementation time. If it's keyed by index, an env-var key/value editor is similar but keyed by string. Either reuse or a small new `EnvVarEditor.svelte` that wraps `ArrayEditor`.
- Validate: keys are non-empty, no `=` in keys, values are arbitrary strings.
- Expose in both `NewShellTabDialog.svelte` and `ConfigureTabDialog.svelte` via the shared subcomponent.

### 2. Profiles/templates for shell tabs

- New "Shell Profiles" Settings tab. List of profiles with edit/delete buttons. "New profile" copies the current shell-tab field set as a starting point.
- `NewShellTabDialog.svelte`: dropdown at top — "Start from profile..." → fills in fields from the selected profile. "Custom" / "Blank" stays the default for users who don't use profiles.
- `ConfigureTabDialog.svelte`: "Save as new profile..." button captures current tab config into a new profile. Doesn't bind the tab to the profile (no live update).
- **Overlap with v1.3 layout presets**: the patterns are similar (named templates, save current state, restore by name, manage via dialog). Don't try to unify them in this milestone — they serve different audiences. Reuse `src/lib/dialog/SaveLayoutDialog.svelte`'s patterns as a reference for the "Save as profile..." dialog if useful.

### 3. Shell auto-restart on crash

- Per-tab setting in `ShellTabFields.svelte`: "Auto-restart on crash" checkbox + "Max retries" numeric input (visible only when checkbox is on).
- Backend: in the shell-exit event handler in `src-tauri/src/shell/mod.rs`, branch on `auto_restart`:
  - If on and retry budget remaining: schedule restart via `tokio::time::sleep(backoff).await` then re-spawn through the existing tab spawn path.
  - Track retries in a per-tab in-memory counter that resets on user-initiated restart (clicking the closed-state overlay) and on graceful exit (code 0).
  - Backoff: 1s, 2s, 4s, 8s, 16s, then cap at 30s. Cap retries at the configured `max_retries`.
- Visible state during retries: show a small "Restarting in 4s... (attempt 3/5)" banner in the closed-state overlay area, or in the tab bar if the tab is unfocused. Reuse `Toast.svelte` or `ClosedShellOverlay.svelte` if either fits.
- After exhausting retries: fall through to the existing closed-state UI ("Press Enter to restart").

### 4. Notification text variables beyond `{code}`

- Extend the interpolation function (find in `src-tauri/src/notifications/...` or wherever `{code}` is currently substituted; possibly `src/lib/status/...`).
- New placeholders:
  - `{name}` — tab's user-visible name.
  - `{tab_position}` — index in `settings.tabs`.
  - `{cwd}` — tab's working directory.
  - `{timestamp}` — ISO 8601 local time of the exit event.
  - `{pid}` — subprocess PID (still available at exit-event time on most platforms; Linux exposes it; Windows ConPTY does too).
- Update `NotificationEditor.svelte` placeholder helper text to list all available variables.
- Document in README.

### 5. History/log of subprocess exits

- New backend module `src-tauri/src/shell/exit_history.rs` (or similar) holding a `RingBuffer<ExitEntry>`.
  - `ExitEntry { tab_id, tab_name_at_exit, command, exit_code, timestamp, retry_count_at_this_exit }`.
  - Default capacity 100.
- Append on every subprocess exit (whether user-initiated, crash, or auto-restart-triggered).
- Persistence: in-memory only by default. Optional follow-on: persist to a sidecar file (`<config_dir>/cctts/shell_exits.jsonl`) so history survives restarts. Defer the persistence; in-memory is enough to debug "why does my shell keep dying" within a single session.
- Frontend: new "Subprocess Log" sub-section in Settings General tab. Read via a new Tauri command `get_shell_exit_history()`. Render as a simple table with columns Timestamp, Tab, Command, Exit Code, Retry Count.
- "Clear history" button. No filters in the initial ship.

## Open questions

- **Profiles vs. layouts**: should a layout preset *contain* shell-tab profile references? A user saving a layout preset with three shells might want "the same three profiles," not "the same three baked-in shell configs." This is a v2 question — defer until both items have shipped and the answer is clearer from real use.
- **Auto-restart and TTS**: a shell tab restarting automatically could spam the notification system. Suppress notifications during the auto-restart window (or coalesce: one notification per *exhaustion of retries*, not per *retry*). Decide at implementation time; recommend coalesced behavior.
- **Notification placeholder failure**: what happens if a user types `{unknown}`? Recommend leaving the literal `{unknown}` in the rendered text (don't error, don't remove it). This matches v1.x's tolerant behavior.
- **Exit history scope**: include AI-tool tab exits (Claude, aider) too, or only Shell tabs? Recommend all tab kinds — they're all subprocesses and the user might want to debug Claude crashes the same way. Header column "Tab Kind" (Shell / Claude / Aider).

## Milestone recommendation

**Milestones needed**, but split is flexible:

- `MILESTONE-V1.4-XX-shell-config-ui.md` — items 1 + 2. Both are pure schema/UI work; they share the dialog surfaces; one PR is reasonable but two milestones is fine if calendar dictates.
- `MILESTONE-V1.4-XX-shell-lifecycle.md` — items 3 + 5. Both touch the shell-exit event handler in Rust. Restart logic (item 3) + exit history (item 5) are natural co-implementations.
- Item 4 (notification text variables) is trivial — fold into either of the above as a 30-minute task. Doesn't need its own milestone.

**When implementation starts, write the milestones in detail then.** Pick whichever item's trigger fires first (typically item 1, since it's the lowest-friction win and `FUTURE-FEATURES.md` notes "the moment you hand-edit settings.json to add an env var" as the trigger).

## Files most likely touched

- `src-tauri/src/settings/{schema,migration}.rs` (items 2, 3)
- `src-tauri/src/shell/mod.rs` (items 3, 4, 5)
- `src-tauri/src/shell/exit_history.rs` (new file, item 5)
- `src/lib/dialog/{NewShellTabDialog,ConfigureTabDialog,ShellTabFields}.svelte` (items 1, 2, 3)
- `src/lib/settings/{ArrayEditor.svelte,NotificationEditor.svelte}` (items 1, 4)
- New: `src/lib/settings/EnvVarEditor.svelte` (item 1, if needed)
- New: a "Shell Profiles" Settings tab and a "Subprocess Log" sub-section
