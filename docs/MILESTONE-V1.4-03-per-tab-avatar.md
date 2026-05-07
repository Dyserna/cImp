# Milestone V1.4-03: Per-Tab Avatar Configuration (Skeleton)

## Purpose

Item 3 from `FEATURE-per-tab-overrides.md`. A per-tab avatar override so different tabs can show different sprite sets (e.g., distinct visuals for Claude vs. aider). Read V1.4-01 first — V1.4-03 follows the same pattern (schema → resolver → wiring → migration → UI).

## What This Milestone Delivers

1. `avatar_override: Option<AvatarConfig>` on both `AiToolTabConfig` and `ShellTabConfig`. Override is the *whole* `AvatarConfig` (idle / talking / thinking / awaiting / done sprite paths), not per-state — simpler shape, covers the headline use case ("different sprite per AI tab").
2. `effectiveAvatar(tab) = tab.avatar_override ?? globalAvatar` resolver, living next to `src/lib/avatarConfig.ts`.
3. `AvatarOverlay.svelte` reads the *focused tab's* effective avatar instead of the global one. The state machine (which sprite to *show* at any moment) is unchanged — `$avatarState` continues to derive from focused tab's processing state; only the asset paths change.
4. Settings file migration v1.5 → v1.6: stamp `avatar_override: null` on every existing tab. Backup `config.json.v1.5.bak.<ts>`. (Adjust version to whichever is current at impl time — V1.4-02 may or may not have shipped first.)
5. **Per-tab UI**: `ConfigureTabDialog.svelte` Appearance section gains an Avatar row with the same shape as themes/background: "Use global default (current: <name or path>)" first entry, then either path inputs or — if the optional "named avatar set" extension lands — a dropdown of bundled named sets.
6. README updates the existing Avatar section to mention per-tab override.

## Key Deltas vs V1.4-01 (Themes)

- **Whole-config override, not per-state.** The override replaces the entire `AvatarConfig` atomically. Per-state overrides ("same idle, different talking") are listed as an Open Question in the feature doc and deferred. Revisit only if real use shows the simpler shape pinches.
- **No live runtime loop.** Themes get a settings subscription that updates `term.options.theme` instantly. Avatars update on the next `$avatarState` tick anyway because `AvatarOverlay` is reactive to focused-tab and store changes. Probably no extra subscription wiring needed; verify at implementation time.
- **Bundled vs. user-supplied sprites.** v1+ ships a single bundled set; the per-tab UI accepts paths the same way the global config does today. Optional follow-on: ship 2-3 *named* bundled sets ("Claude", "aider", "default") and let the override pick a name instead of paths. Defer until users ask. The schema can stay path-based; "named set" would be a thin layer that resolves a name back to paths at load time.
- **Asset-path validation**: same gotcha as V1.4-02's image path. Invalid paths should surface a clear error in the Configure Tab dialog and fall back to the global avatar for rendering.

## What This Milestone Does NOT Do

- **Per-state avatar overrides.** Whole-config only. See Open Questions in `FEATURE-per-tab-overrides.md`.
- **Bundled named avatar sets.** Optional follow-on; not required for V1.4-03 to ship.
- **Avatar overlay multi-instance.** Still one overlay tied to the focused pane's active tab. Multi-pane visible avatars (one per pane) is a separate, larger UX question.

## Files Most Likely Touched

- `src-tauri/src/settings/schema.rs` — `avatar_override` on both tab variants
- `src-tauri/src/settings/migration.rs` — v1.X → v1.X+1 transform + backup
- `src/lib/avatarConfig.ts` — `effectiveAvatar` resolver
- `src/lib/AvatarOverlay.svelte` — read effective avatar instead of global
- `src/lib/dialog/ConfigureTabDialog.svelte` — Appearance section Avatar row
- README.md — per-tab avatar mention

## Risks and Open Questions

- **Focused-pane semantics.** Today `AvatarOverlay` reads `$focusedTabId` and derives the global avatar. Two tabs in two panes both visible — only the focused one's avatar shows. Confirm that's the desired behavior with per-tab overrides too (it is, per the feature doc, but worth a one-line confirmation in the milestone delivery doc).
- **AvatarConfig schema stability.** If `AvatarConfig` gains new sprite states later (e.g., "error"), the override-as-whole-config approach means tabs with overrides need a migration to add the new state — versus per-state overrides where missing states would inherit. Whole-config is still the right call (simpler today; migration cost is one-time), but record the trade-off.
- **Path-vs-name future.** If named bundled sets ship later, the schema needs a discriminator. Cleanest: change `AvatarConfig` to a tagged enum `Bundled(name) | Custom(paths)` rather than retrofitting a "is_bundled" flag. Plan for it but don't implement now.
