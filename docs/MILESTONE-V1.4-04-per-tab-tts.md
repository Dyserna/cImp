# Milestone V1.4-04: Per-Tab TTS Settings (Skeleton)

## Purpose

Item 4 from `FEATURE-per-tab-overrides.md`. Per-tab voice / speed / volume override so each tab can have its own TTS identity (e.g., a distinct voice for the Claude tab vs. aider tab). Read V1.4-01 first — V1.4-04 follows the same pattern, but the *resolver* lives in Rust because TTS happens in the backend.

## What This Milestone Delivers

1. `tts_override: Option<TtsOverride>` on both tab variants, where `TtsOverride` is a struct of *individually nullable* fields:

   ```rust
   pub struct TtsOverride {
       pub voice:  Option<String>,
       pub speed:  Option<f32>,
       pub volume: Option<f32>,
   }
   ```

   This is a deliberate divergence from themes/background/avatar. TTS settings are atomic primitives users tune individually ("I want a different voice on the aider tab but keep my global speed and volume"); themes are not. Per-field nullability inside the override matches that mental model.
2. `effective_tts(tab, global) -> ResolvedTts` resolver in Rust, in whichever module the TTS pipeline currently consumes voice/speed/volume from `settings.tts.*`. Per-field merge: `o?.voice ?? global.voice`, etc.
3. TTS pipeline reads per-tab settings *at synthesis-request time*, not at session start. TTS segments already carry their origin tab id; the worker looks up the tab + global TTS config at the moment it asks for "what voice should this segment use." Both data sources are already in scope at the resolution site — no architectural surgery.
4. Settings file migration v1.X → v1.X+1: stamp `tts_override: null` on every existing tab. Backup follows the established pattern.
5. **Per-tab UI**: `ConfigureTabDialog.svelte` Appearance (or new "Audio") section gains three rows — voice dropdown, speed slider, volume slider — each with a "Use global default (current: <value>)" first entry that maps to that field being `None` inside the override. Setting any field to a real value flips override from `None` to `Some(TtsOverride { ..nulls.., field: Some(...) })`. Setting all three back to "Use global" collapses the override to `None` again.
6. README updates the TTS section to note per-tab override.

## Key Deltas vs V1.4-01 (Themes)

- **Resolver in Rust, not TS.** TTS synthesis is backend work. Frontend never resolves the effective TTS config — it only reads/writes the settings. The resolver function lives next to the synthesis worker and runs on every TTS request.
- **Per-field nullability inside the override.** Themes/background/avatar override the *whole structure*; TTS overrides individual fields. Two layers of optionality (`Option<TtsOverride>` and `Option<String>` inside) — a tab with no override is `tts_override: null`; a tab overriding only voice is `tts_override: { voice: "alloy", speed: null, volume: null }`. The UI collapses an all-`null` override back to `null` to keep the file clean.
- **Audio target tab gate still applies.** v1+'s "audio_target_tab" gate decides whose audio plays at all. A non-target tab's volume override is moot — its synthesis is dropped before audio output. Per-tab volume affects only the target-tab playback path.
- **No frontend live-update plumbing.** Themes need `term.options.theme = next` on every settings change. TTS doesn't render anywhere — the resolver is invoked on each synthesis call, picks up changes naturally, no subscription wiring.
- **Voice dropdown source-of-truth.** The available voice list is whatever `settings.tts.voice` is selected from today (probably a hard-coded constant or a TTS-engine probe result). Reuse the existing source for the per-tab dropdown — don't fork or duplicate.

## What This Milestone Does NOT Do

- **Per-tab TTS injection toggle.** That's a separate per-tab setting, listed in `FEATURE-per-tab-overrides.md`'s sibling features and `FEATURE-aider-parity.md`. Out of scope here.
- **Audio mixing / multi-tab simultaneous playback.** Listed as deferred-indefinitely in `FUTURE-FEATURES.md`. The audio target tab gate stays single-tab.
- **Per-tab audio output device.** Different speakers per tab. Out of scope; not in the feature doc.

## Files Most Likely Touched

- `src-tauri/src/settings/schema.rs` — `TtsOverride` struct, `tts_override` on tab variants
- `src-tauri/src/settings/migration.rs` — v1.X → v1.X+1 transform + backup
- `src-tauri/src/tts/...` — exact module varies; whichever currently reads `settings.tts.{voice,speed,volume}`. Replace with `effective_tts(tab, global)` lookup keyed on segment origin tab id.
- `src/lib/dialog/ConfigureTabDialog.svelte` — three TTS override rows
- `src/lib/settings/types.ts` — `TtsOverride` mirror
- README.md — per-tab TTS mention

## Risks and Open Questions

- **Resolver call site identification.** "Wherever the TTS worker reads voice/speed/volume" assumes a single call site. Verify by grepping `settings.tts.voice` / `settings.tts.speed` / `settings.tts.volume` in `src-tauri/src/tts/` and confirm they're all reachable from a single resolver-injection point. If resolution is currently split across multiple call sites, factor first or feed the resolver through to each.
- **Settings race condition.** If the user changes a per-tab override mid-synthesis, the resolver might pick up the new value mid-stream (one segment uses the old voice, the next uses the new). Probably fine for end users; document if it bites. Stronger consistency would require pinning the config at segment-creation time.
- **Per-tab volume vs. global mute semantics.** If global volume is 0, does a per-tab volume of 0.5 still play? Per the resolver, yes — per-field merge means `volume: 0.5` overrides the global `volume: 0`. Confirm this matches user mental model; if not, the override semantics may need a "respect global mute" carve-out.
- **Voice dropdown stability.** Voice names are user-visible strings. If the underlying TTS engine swaps/renames voices, per-tab overrides could point at dead names. Validate at resolve time; fall back to global voice with a one-line warning in the logs.
- **Override-collapse heuristic in the UI.** The "set all three to global → drop the override entirely" heuristic keeps settings.json clean. Ensure the UI always writes `tts_override: null` when all three fields are `None`, never `tts_override: { voice: null, speed: null, volume: null }` — they're behaviorally equivalent but the former is the canonical form.
