# Feature: Rebrand `cctts` → `ccImp`

## Why

`cctts` = "Claude Code **TTS**" named the app after a single *feature*. The project
grew into a full desktop front-end for Claude Code — voice (TTS), dictation (STT),
an animated avatar, multi-pane tabs, a compose overlay, themes. `ccImp` = "Claude
Code **Imp**" renames it after its *character* instead of a feature, which mirrors
that evolution. Keeping the `cc` prefix preserves continuity with `cctts` and is
honest: Claude Code is the main character (Aider is vestigial and can be dropped
later without affecting the name). An *imp* is a small mischievous helper spirit —
semantically apt — and gives the app its own mascot alongside Clawd.

## Casing / identity map

| Thing | `cctts` (old) | `ccImp` (new) |
|---|---|---|
| Brand / display name | cctts | **ccImp** |
| Binary | `cctts.exe` | `ccimp.exe` |
| Cargo crate + `[[bin]]` name | `cctts` | `ccimp` |
| npm package name | `cctts` | `ccimp` |
| Tauri `productName` | `cctts` | `ccImp` |
| Tauri `mainBinaryName` | (unset) | `ccimp` (force lowercase exe despite display caps) |
| Tauri `identifier` | `com.cctts.app` | `com.ccimp.app` *(open decision)* |
| GPU env var | `CCTTS_GPU` | `CCIMP_GPU` |
| Log target / `RUST_LOG` | `cctts` | `ccimp` |
| Window titles | `"<proj> - cctts"`, `"cctts — Settings"` | `"<proj> - ccImp"`, `"ccImp — Settings"` |
| Per-folder overlay file | `.cctts.custom.config.json` | `.ccimp.custom.config.json` |
| Log filename prefix | `cctts.log.*` | `ccimp.log.*` |
| Global settings file | `settings.json` | `settings.json` (name is already generic) |
| Default mascot | `claudeSprites` (Clawd) | `impSprites` (the imp); Clawd stays selectable |

## Principles (non-negotiable)

1. **Stay 100% portable.** The app already writes *only* relative to
   `current_exe()` — `settings.json`, `logs/`, scrollback, themes. There is **no
   `%APPDATA%`, no `dirs`/`ProjectDirs`, no Tauri `app_data_dir()` usage, and no
   single-instance plugin**. The rebrand must NOT introduce any. Verify this still
   holds at the end (grep for `APPDATA|dirs::|app_data_dir|app_config_dir`).
2. **No backward compatibility.** A clean rename — no aliases, no old-name
   fallbacks. Renamed env var / overlay file simply take effect; any pre-rename
   `.cctts.*` overlay or `CCTTS_GPU` usage is abandoned (re-set them under the new
   names). `settings.json` keeps its name only because it's already generic, not
   for compat.

## Work checklist

### 1. Crate & app identity
- [ ] `src-tauri/Cargo.toml`: `[package] name = "ccimp"`, `[[bin]] name = "ccimp"`, refresh `description`.
- [ ] `src-tauri/tauri.conf.json`: `productName = "ccImp"`, add `mainBinaryName = "ccimp"`, `identifier` (decide), window `title`.
- [ ] `package.json` / `package-lock.json`: `name = "ccimp"`.
- [ ] `bump-version.mjs`: the `cargo update -p cctts` → `-p ccimp`.
- [ ] Smoke-test doc comment in `tts/engine.rs` (`--bin cctts` → `--bin ccimp`).

### 2. GPU env var
- [ ] `tts/engine.rs` + `stt/engine.rs`: `CCTTS_GPU` → `CCIMP_GPU` (straight rename,
      no alias). Optionally centralize in one shared helper.
- [ ] Docs/comments referencing `CCTTS_GPU`.

### 3. Writable-state literals (preserve portability)
- [ ] `settings/persistence.rs`: `CUSTOM_FILE_NAME` → `.ccimp.custom.config.json`
      (straight rename, no old-name fallback). `GLOBAL_FILE_NAME` stays `settings.json`.
- [ ] `logging.rs`: appender prefix `cctts.log` → `ccimp.log`; the prune/“active
      file” matcher (`starts_with("cctts.log")`) → new prefix.
- [ ] `content.rs` / `pty/scrollback.rs`: any `cctts`-named files/dirs.
- [ ] `settings/migration.rs`: backup-suffix logic references `.cctts.custom.config.json` in comments/paths.

### 4. Display strings (cosmetic, user-visible)
- [ ] `main.rs`: window title `"{} - cctts"`, the `"cctts starting"` log, the
      `"cctts"` fallback project label.
- [ ] `ipc/windows.rs`: `"cctts — Settings"`.
- [ ] `audio/playback.rs`: thread name `cctts-audio` (cosmetic).
- [ ] Frontend: any `cctts` in Settings → About, headers, etc.

### 5. Mascot swap
- [ ] `sprites/impSprites/` — scaffolded by this change (manifest cloned from
      Clawd; art dropped in later). See "Imp sprite set" below.
- [ ] `src/lib/avatarConfig.ts`: add `'impSprites'` to `KNOWN_SPRITE_SETS`; once art
      exists, set `FALLBACK_SPRITE_SET`/default → `impSprites`.
- [ ] Default avatar/sprite setting in `defaultSettings()` → `impSprites` (gate on
      art being present; until then keep `claudeSprites` default so the overlay is
      never blank).
- [ ] `release.yml` already stages `sprites/*` recursively → `impSprites` ships with
      no workflow change.

### 6. Build / release pipeline
- [ ] `.github/workflows/release.yml`: every `cctts.exe` → `ccimp.exe` (build-output
      path + both staging blocks); stage-dir names (`cctts-portable-win-x64-*` →
      `ccimp-portable-win-x64-*`); any display text.
- [ ] `scripts/portable-readme.txt` / `portable-readme-no-models.txt`.
- [ ] `docs/RELEASE.md`, `docs/PACKAGING.md`: binary name, zip names, layout.

### 7. Docs & repo
- [ ] Sweep `README.md`, `NOTICE`, `CHANGELOG.md`, `docs/**` for `cctts` → `ccImp`
      (display) / `ccimp` (technical). Keep historical milestone docs as-is where
      they describe shipped history; update living docs.
- [ ] `README.md:436` hardcoded `github.com/Dyserna/cctts/releases` → new URL.
- [ ] **GitHub repo rename** `Dyserna/cctts` → `Dyserna/ccimp` *after* the v0.11.0
      release CI is green (renaming mid-run could disrupt `gh release create`).
      GitHub auto-redirects old URLs; then `git remote set-url origin <new>`.
- [ ] Optional: rename the local clone folder. NOTE this breaks the
      `src-tauri/target/models` → repo `models/` symlink — recreate it after.

### 8. Verify
- [ ] `cargo build` (default) + `--features tts-webgpu` compile; `tauri dev`/build runs.
- [ ] Output binary is `ccimp.exe`.
- [ ] `settings.json` is read/written next to the exe; `CCIMP_GPU=cpu` forces CPU.
- [ ] Window title shows `ccImp`; logs are `ccimp.log.*`.
- [ ] **Portability regression check:** grep confirms no new `%APPDATA%`/path-API usage.
- [ ] `impSprites` loads in the avatar overlay (once art is present).

## Imp sprite set (the mascot)

`impSprites` is a second sprite set parallel to `claudeSprites` — same 20px pixel-art
style and the **same 13 animations** so it's a drop-in. Concept: same silhouette as
Clawd, recolored **red + horns + an impish grin** (its own creature, family
resemblance preserved). Orange Clawd (Claude) + red imp (ccImp, the app that voices
him) read as a duo; Clawd stays as a selectable alternate.

Animation checklist the art must cover (cloned into the scaffolded manifest):

| Category | Animation (frames) |
|---|---|
| Idle | breathe (16), blink (16), look-around (17) |
| Expressions | wink (12), surprise (12), sleep (24) |
| Work | coding (23), think (24) |
| Dance | bounce (16), sway (12), bounce-dj (16), sway-dj (12), djmix (16) |

The scaffold creates `sprites/impSprites/manifest.json` (frame list + `hold_ms`
cloned from Clawd so timing matches) and one folder per animation with a `.gitkeep`.
Drop `NN.png` frames into each folder to bring the imp to life; then flip the
default in `avatarConfig.ts` / `defaultSettings()`.

**Behaviour is manifest-driven** (refactored out of the old hardcoded
`SPRITE_STATE_ANIMS`): each manifest's `groups` array maps avatar **state →
animation list** (`{ "state": "Idle", "animations": [...] }`, one per the 5
states `Idle/Listening/Thinking/Speaking/Error`), and the player rotates a list
when it has >1 entry. `SpritePlayer.groupFor(state)` reads it; a state with no
group falls back to the set's `Idle` group. So a new set fully defines its own
behaviour in its manifest — the imp can mirror Clawd (same groups, already
cloned) or diverge per state without touching app code.

## Open decisions

- **Bundle identifier:** `com.ccimp.app` vs `com.dyserna.ccimp`. Cosmetic on a
  portable build (no installer/registry), but it's the OS-facing app id — pick one
  and keep it stable.
- **Local clone folder rename:** optional; do last and recreate the models symlink.
- **Default mascot timing:** switch the default to `impSprites` only once the art is
  drawn (otherwise the overlay would be blank); ship the scaffold + registry entry
  first.

## Sequencing

Do this as **v0.12.0**, after v0.11.0 ships. The code/doc rename is one focused
change; the GitHub repo rename is a separate manual step gated on the v0.11.0 CI
finishing. The imp art can land incrementally — scaffold now, draw frames later,
flip the default when ready.
