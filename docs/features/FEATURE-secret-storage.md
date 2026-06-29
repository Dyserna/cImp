# Feature: Secret storage for `claude_local.auth_token` and per-tab env credentials

Status: **Open — decide before v1**

This file scopes the work to move secrets out of the plaintext settings JSON and into an OS-level credential store. Created as part of the Slice 1 audit refactor; the immediate fix shipped with that slice (custom `Debug` redaction + Unix `0600` chmod) is a stopgap, not the final shape.

## Background

`claude_local.auth_token` lives in:

- `<exe-dir>/settings.json` (portable global baseline)
- `<launch_cwd>/.cimp.custom.config.json` (per-folder overlay)

Per-tab `env` entries (`AiToolTabConfig.env`, `ShellTabConfig.env`) can also carry credentials — anything the user typed into the *Environment* table in **Configure tab → Environment**. Those values may include real `ANTHROPIC_API_KEY`, OAuth bearer tokens, AWS keys, Slack webhooks, etc.

The current state, after the Slice 1 fix:

- Atomic writes (write-tmp-then-rename) eliminate the truncate-on-crash data-loss path.
- `ClaudeLocalSettings::Debug` redacts `auth_token` so a future `?settings` log line cannot leak it to the rolling log file.
- Unix permission bits on the file are set to `0600` after each save.
- Windows ACLs are inherited from the parent directory — not programmatically restricted.

What is *still* on disk in plaintext:

- `auth_token` itself, in `settings.json` and the overlay.
- Any per-tab `env` value the user added.
- Both files round-trip through the diff/overlay pipeline; deletions and edits create temp files (also `0600` on Unix, mode-from-create on Windows) which are renamed over the live file. No copy of the prior plaintext lives in `<file>.tmp` after the rename, but pre-Slice-1 backups (`settings.json.v1.*.bak`) on existing installs do contain plaintext.

## Why this matters

- Anyone with read access to the user's home / launch directory can read the token. On a single-user Windows workstation this is mostly the user themselves; on a shared Mac/Linux box it's any process running under the same UID (or another user with `chmod`-permitting ACLs).
- Backup tools (Time Machine, OneDrive sync, GitHub gitignore mistakes) can replicate the plaintext to off-machine storage without ceremony.
- A future telemetry / crash-report path that snapshots settings to upload would leak the token unless every export site remembers to redact it.

The field is documented in the schema as "local proxies typically accept dummy tokens, so this is acceptable" — true for the LM Studio / Ollama / vLLM happy path with `sk-dummy`. **Untrue if anyone routes a real Anthropic key through the same field**, and the Configure UI doesn't currently warn against that.

## Options

### Option A — `keyring` crate (OS keychain integration)

Cross-platform Rust binding to the OS credential store: macOS Keychain, Windows Credential Manager, Linux Secret Service / gnome-keyring / KWallet.

- **Pros:** Standard pattern. Plaintext leaves the file. The keychain is attached to the user's login session; a stolen disk image is no longer enough to extract the token. Survives backup.
- **Cons:**
  - One more dep (`keyring = "3"` brings ~5 transitive crates on Linux for Secret Service / dbus support).
  - Linux fallback: when neither gnome-keyring nor KWallet is running (headless WSL, fresh Docker, server installs), the keyring crate either errors out or silently writes plaintext to a fallback location depending on backend choice. We need to decide the failure mode: refuse to save the token, or fall back to plaintext with a one-time warning.
  - Migration: existing users have plaintext tokens on disk. On first launch after the upgrade, read the value, write it to keyring, blank the disk field, re-save settings. The `.bak` files written for the migration must NOT contain the plaintext — special-case the migration backup to redact the field before backing up.
  - The disk schema still needs a placeholder for "this token lives in the keyring" — likely an enum like `AuthToken::Keyring | AuthToken::Plain(String)` so downgrade from a future version doesn't lose the user's intent.

### Option B — Encrypt-at-rest with a machine-bound key

Encrypt `auth_token` (and per-tab env values matching a credential heuristic) with a key derived from the OS user — DPAPI on Windows, the user's login keychain on macOS, libsecret on Linux.

- **Pros:** No keyring popup / consent prompt. Works offline. Linux fallback is the same as keyring (libsecret).
- **Cons:** Equivalent to Option A's complexity but with a custom serialization layer on top. Net negative vs. just using `keyring`.

### Option C — Status quo, plus louder UX warnings

Leave plaintext on disk. Add a one-line warning in the Configure dialog when the user sets `auth_token` to anything other than `sk-dummy` / empty: *"Stored in plaintext. Don't paste a real Anthropic key here."*

- **Pros:** Zero additional dependency. Zero migration. Cleanest semantics if everyone really does use dummy tokens.
- **Cons:** Doesn't help the per-tab env case. Doesn't help users who paste real tokens despite the warning.

### Option D — Hybrid: Option C *and* Option A behind a feature flag

Default-off keyring integration shipped in v1, opt-in via a Settings toggle, default-on at v2. Lets us validate the keyring path on a small set of users before forcing the migration.

## Recommendation

For v1: **Option C** with the existing Slice 1 redaction + Unix-mode hardening. The cost of A is real (Linux dep tree, fallback policy, schema migration with redacted-field backup) and the benefit is small for the dummy-token happy path the docs describe.

For v1.x: revisit if anyone reports a leaked-token incident or if cImp grows a feature where the local-LLM tab routes through a paid third-party proxy that requires a real key (in which case Option A becomes table stakes).

## Decision needed before merging

- [ ] Confirm v1 ships Option C only.
- [ ] Add a Configure-dialog warning (TBD copy) on non-dummy `auth_token` values.
- [ ] (If shipping A) decide Linux fallback policy.
- [ ] (If shipping A) write the migration: read plaintext → write keyring → blank disk → save → redacted backup.

## Test scaffolding (for whichever option ships)

- Round-trip a real `auth_token` through save → reload → spawn → assert the value reaches `compose_ai_env` correctly.
- Verify `?settings` / `?cfg` debug formatting prints the redacted form (Slice 1 already covers this).
- (Option A) Verify behavior when the keyring is unavailable (mock the backend to return error) — settings still loads, the token is treated as missing, the user sees a clear error in the Local LLM section.
- (Option A) Verify the migration runs once and only once across multiple launches.
