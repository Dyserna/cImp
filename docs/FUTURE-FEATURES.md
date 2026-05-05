# Future Features

This document tracks features that are deferred pending external dependencies, design clarification, or accumulated real-world signal from cctts use. Each entry describes what the feature is, what's blocking it, what should happen when the blocker resolves, and how to verify upstream changes.

---

## Aider TTS Markup Injection

### What it is

Inject TTS markup instructions (the `[[TTS]]...[[/TTS]]` convention) into aider's system prompt at launch, the same way the Claude tab does via `claude --append-system-prompt`. This would enable spoken TTS for aider tab output, bringing it to parity with the Claude Code tab.

### Status

**Deferred pending upstream support in aider.**

As of the v2 design phase (Q2 2026), aider does not provide a CLI flag for appending content to the system prompt. The closest available mechanism is `--read <file>`, which adds files as read-only context (chat-level user messages), not as system prompt content. Per aider's own community discussions, instructions delivered as user messages are treated differently by many LLMs and may be ignored or deprioritized — which is why direct system prompt injection is what we want.

### What's blocking it

An open aider feature request exists for a `--system-prompt-extras <file>` flag (or similar):

- aider issue #4817: "Option: --system-prompt-extras to always append file content to system prompt for coder mode"
- The request describes exactly the use case cctts has: append a small instructions file to the system prompt at every request, optionally re-reading the file each turn so users can edit instructions live

If aider adds this flag (or any equivalent mechanism — e.g., `--append-system-prompt`, `--system-prompt-file`, or a config-key override settable via CLI like `-c system_prompt_extras=...`), we should adopt it.

### What v2 ships with

- Aider tab spawns aider as a subprocess in a PTY, exactly like the Claude tab spawns claude
- No system prompt injection — aider runs with its default behavior
- TTS markup tags will not appear in aider's output, so no speech will play for the aider tab (the cctts fallback-silent behavior handles this naturally)
- Tab status indicators, notifications, avatar state, and permission detection (when aider patterns are added) all still function — these are independent of TTS markup
- Documentation in the README explicitly notes that aider tab TTS coverage depends on upstream aider support

### What to do when the blocker resolves

When aider releases a CLI flag for system prompt injection:

1. **Verify the mechanism.** Check aider's release notes and CLI reference for the new flag's exact name, semantics (replace vs. append), and file vs. inline-string semantics. Match against what cctts needs (append, file-based or inline-string, applied at every turn).

2. **Update the aider tab launch logic.** The PTY spawn for the aider tab should pass the new flag with cctts's TTS markup instructions, similar to how the Claude tab passes `--append-system-prompt` today. The implementation should be isolated to a single function (the aider-spawn helper) so this is a one-place change.

3. **Decide on file vs. inline string.**
   - If aider's flag is inline-string-based (`--append-system-prompt "..."`), pass the markup instructions directly. Simplest. Same approach as Claude.
   - If aider's flag is file-based only (`--system-prompt-extras <file>`), cctts writes a small temp file at app launch with the markup instructions, passes the path to aider, and cleans up the temp file when the aider subprocess exits. Use the OS temp directory (`/tmp` on Linux, `%TEMP%` on Windows).
   - If both are available, prefer the inline-string form to avoid temp-file management.

4. **Add a per-tab "TTS injection enabled" toggle to settings.** Default on. Lets the user disable injection if a particular model performs poorly with the markup convention or if it interferes with their workflow. The toggle is per-tab so the Claude tab and aider tab can be controlled independently.

5. **Update the README and design documentation.** Remove the "aider TTS not supported" caveat. Note that TTS markup compliance still depends on the local model's instruction-following ability — smaller local models may not wrap content in tags reliably even when instructed.

6. **Test with both cloud and local models.** The Claude tab uses Claude (good at following system prompts). The aider tab might be configured with anything from GPT-4 to a 7B local model. Verify that markup tags appear in output for at least one capable model (e.g., Claude via API, or GPT-4o), then accept that smaller local models may be inconsistent.

### How to monitor for upstream resolution

- **Periodic check**: every few months, run `aider --help` and look for a new system-prompt-related flag, or visit the aider releases page on GitHub
- **Issue subscription**: subscribe to the GitHub issue (#4817 and related), so notifications fire when the feature lands
- **Search**: a quick search for `aider --append-system-prompt` or similar would surface release notes and tutorials if the flag is added under any name

---

## Adding new entries to this document

Entries in this file are for deferred features with concrete external dependencies, not general wishlist items. The format is:

- **What it is**: one-paragraph description of the feature
- **Status**: a clear blocker statement
- **What's blocking it**: specific external thing that must change
- **What v2 (or current version) ships with**: the workaround or omission
- **What to do when the blocker resolves**: actionable steps for future implementation
- **How to monitor for upstream resolution**: where to check

General wishlist items, polish ideas, and "nice to have" features without external blockers should not go here — those belong in a separate `WISHLIST.md` if you want to track them, or just live in your head until they crystallize.
