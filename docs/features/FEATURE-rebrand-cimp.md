# Feature: Rebrand `ccImp` → `cImp`

## Why

`ccImp` = "**C**laude **C**ode **Imp**" named the app after one of its backends.
Since V19 the app hosts **both Claude Code and OpenCode** (and a local-LLM
Claude tab), so a Claude-Code-specific name is no longer honest. `cImp` =
"**c**ode **Imp**" generalizes it: an editor-/agent-agnostic imp that voices
whatever coding agent you run. The compact `cImp` is used because the fuller
"code imp" / "CodeImp" spelling is already taken elsewhere. The imp mascot and
character carry over unchanged — this is a name tightening, not a redesign.

This supersedes the earlier `cctts` → `ccImp` rebrand (see
[FEATURE-rebrand-ccimp.md](FEATURE-rebrand-ccimp.md)), kept as historical
record.

## Casing / identity map

| Thing | `ccImp` (old) | `cImp` (new) |
|---|---|---|
| Brand / display name | ccImp | **cImp** |
| Binary | `ccimp.exe` | `cimp.exe` |
| Cargo crate + `[[bin]]` name | `ccimp` | `cimp` |
| npm package name | `ccimp` | `cimp` |
| Tauri `productName` | `ccImp` | `cImp` |
| Tauri `mainBinaryName` | `ccimp` | `cimp` |
| Tauri `identifier` | `com.ccimp.app` | `com.cimp.app` |
| GPU env var | `CCIMP_GPU` | `CIMP_GPU` |
| Log target / `RUST_LOG` | `ccimp` | `cimp` |
| Window titles | `"<proj> - ccImp"`, `"ccImp"` | `"<proj> - cImp"`, `"cImp"` |
| Per-folder overlay file | `.ccimp.custom.config.json` | `.cimp.custom.config.json` |
| Per-project graph dir | `.ccimp/` | `.cimp/` |
| Log filename prefix | `ccimp.log.*` | `cimp.log.*` |
| Statusline subcommand | `ccimp --statusline` | `cimp --statusline` |
| Portable zip prefix | `ccimp-portable-*` | `cimp-portable-*` |
| Global settings file | `settings.json` | `settings.json` (already generic) |
| Mascot | impSprites (the imp) | impSprites (unchanged) |

## Principles (non-negotiable)

1. **Stay 100% portable.** The app still writes *only* relative to
   `current_exe()`. The rename introduces no `%APPDATA%`, `dirs`/`ProjectDirs`,
   `app_data_dir()`, or single-instance plugin.
2. **No backward compatibility.** A clean rename — no aliases, no old-name
   fallbacks. The renamed env var (`CIMP_GPU`), overlay file
   (`.cimp.custom.config.json`), graph dir (`.cimp/`), and log prefix
   (`cimp.log`) simply take effect; any pre-rename `.ccimp.*` / `.cctts.*`
   overlay or `CCIMP_GPU` usage is abandoned (re-set under the new names).
   `settings.json` keeps its name only because it's already generic.

## What changed

A global rewrite of every name token across code, config, scripts, the build
pipeline, and docs:

- `CCTTS`/`CCIMP` → `CIMP`, `ccImp` → `cImp`, `ccimp` → `cimp`, `cctts` →
  `cimp` (the grandparent name lingered only in older docs and binary/path
  examples).
- Identity files: `src-tauri/Cargo.toml`, `src-tauri/Cargo.lock`,
  `src-tauri/tauri.conf.json`, `package.json`, `package-lock.json`.
- `.gitignore`: active overlay → `.cimp.custom.config.json` and graph dir →
  `.cimp/`; the old `.ccimp.*` and `.cctts.*` entries are retained purely as
  stale-leftover ignores so pre-rename files on a dev machine never get
  committed.
- `.github/workflows/release.yml`, `scripts/portable-readme*.txt`,
  `scripts/bump-version.mjs` (`cargo update -p cimp`).
- README GitHub URLs `Dyserna/ccImp` → `Dyserna/cImp`.

### Excluded from the rewrite (deliberate)

- `docs/features/FEATURE-rebrand-ccimp.md` — historical narrative of the
  previous rename; left intact.
- Real local-clone paths that reference the on-disk folder name (still
  `…/cc-avatar/cctts`): `.claude/settings.local.json`, `.aider.chat.history.md`,
  build logs. The working-directory folder is not renamed.

## Verify

- [x] `cargo check` compiles (binary is `cimp`).
- [x] `npm run check` (svelte-check) passes.
- [ ] `cimp.exe` is the build output; window title shows `cImp`; logs are
      `cimp.log.*`; `CIMP_GPU=cpu` forces CPU; `.cimp.custom.config.json` is
      read/written next to the exe. *(verify on next `tauri build` / run)*
- [ ] Portability regression check: no new `%APPDATA%`/path-API usage.

## Follow-ups (manual, outside this change)

- **GitHub repo rename** `Dyserna/ccImp` → `Dyserna/cImp` *after* this
  release's CI is green (renaming mid-run could disrupt `gh release create`).
  GitHub auto-redirects old URLs; then `git remote set-url origin <new>`.
- Local clone folder rename is optional and intentionally skipped (it would
  break the `src-tauri/target/models` → repo `models/` symlink).
