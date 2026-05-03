# Milestone 7: Polish

## Goal

Bring the application from "functional" to "finished." Address error states, edge cases, cross-platform inconsistencies, performance issues, and any quality-of-life improvements that surfaced during earlier milestones. After this milestone, the application is ready for daily use.

## Why This Milestone Last

Polish is best done after all functionality is in place. Trying to polish an in-progress feature is wasted effort because the feature may change. By Milestone 6 the architecture is complete; this milestone tightens the screws.

## Scope

This milestone is intentionally less scripted than the previous ones. The exact work depends on what surfaced during development. The categories below are the framework; specific tasks within each are filled in based on what the project actually needs.

### In Scope

- **Error state handling**: every error path produces clear logging, user-visible feedback where appropriate, and a path to recovery
- **Edge case fixes**: anything noticed during earlier milestones that was deferred as "polish later"
- **Cross-platform validation**: thorough testing on both Windows and Linux, fixing any platform-specific issues
- **Performance review**: profiling and addressing any noticeable lag or jank
- **First-run experience**: what happens when the user launches the app for the first time, missing dependencies, missing model files, etc.
- **Logging hygiene**: appropriate levels, useful messages, no noise at INFO level during normal operation
- **README and minimal documentation**: how to install, what's needed, how to configure
- **Application packaging considerations**: notes on what's needed for distribution (not necessarily creating installers, but identifying what would be needed)

### Out of Scope

- New features beyond what was scoped in Milestones 1–6
- Items deferred in DESIGN.md to post-v1 (read-everything override, audio device selection in UI, conversation/session UI, voice mixing, STT, mobile, etc.)
- Application packaging and code signing (separate concern)
- Automated CI/CD setup (separate concern)

## Specific Polish Tasks

### Error State Surfacing

The State Manager has an Error state, but for it to be useful, the user needs to know *what* went wrong. Add:

- A small status banner or icon in the main window when in Error state, showing a brief description of the error
- The Error state image (configured in settings) is displayed
- A "Recover" or "Retry" action that:
  - For subprocess exit: respawns Claude Code
  - For TTS errors: clears the error flag and continues (next synthesis will retry)
  - For audio errors: same — clear and continue, next playback will attempt to use the audio device again

Specific error types worth distinguishing:

- Claude Code subprocess died → "Claude Code stopped. Restart?"
- TTS synthesis failed → log only, no user notification (transient, single-segment failures shouldn't interrupt UX)
- Audio device error → "Audio output unavailable" (more disruptive)
- Settings file write failed → "Settings could not be saved" (rare but user should know)
- Model file missing → "Kokoro model not found at <path>. Configure in settings or place at <expected location>."

Errors that don't fit the AvatarState::Error model (e.g., a transient TTS failure) are logged but don't transition state.

### Interrupt-on-Input Refinement

This was added in Milestone 6 as a setting but the behavior may need tuning:

- When enabled, typing during TTS playback should stop the current playback and clear the queue
- Should it stop on the *first* keystroke or only after a few characters? Current keypresses might be accidental
- Recommended: stop on *any* input, but make it configurable later if it feels too aggressive
- Verify the audio actually stops within a frame or two — no audible tail

### State Transition Tuning

The avatar state transitions are heuristic:

- "User stopped typing" timeout: 2 seconds is a guess. Tune based on feel.
- "Claude finished generating" stability window: 1 second is a guess. Watch for cases where Claude pauses mid-response (e.g., during slow tool calls) and transitions to Idle prematurely.
- The Listening → Thinking transition: ensure it feels natural — should fire reasonably soon after the user submits, not stick at Listening for a noticeable period

If any transition feels wrong, fix it. Document the chosen values in DESIGN.md if they materially differ from the original design.

### Cross-Platform Issues

Things that may differ between Windows and Linux:

- **Font rendering in xterm.js**: monospace fonts vary by platform. Ensure the default is something sensible on each (e.g., Consolas / Cascadia Code on Windows, monospace fallback on Linux). Document in settings notes.
- **Animated WebP support**: very old WebKitGTK versions don't support it. If targeting older Linux distros, document a minimum WebKitGTK version.
- **Audio glitches under PulseAudio vs PipeWire**: cpal handles both but behavior may differ subtly. Test on both if possible.
- **Path separators in settings**: settings might be edited by hand. JSON-escape or normalize paths so manually-edited Windows paths work.
- **UNC paths and special characters**: test with unusual launch directories.
- **High-DPI scaling**: verify the UI scales correctly on 4K and high-DPI displays. Tauri inherits browser DPI handling, but verify.

Run through the entire feature set on the second platform if it hasn't been the primary development environment.

### Performance Review

Profile under realistic conditions:

- **Long-running session**: leave the app running for an hour with periodic activity. Watch memory growth. Hunt down leaks if any.
- **Large terminal output**: have Claude generate something extremely long (paste a big file). Verify scrollback doesn't lag.
- **Rapid TTS requests**: stress the synthesis pipeline. Verify queue handles backpressure without deadlocking.
- **CPU usage when idle**: the app should idle near 0% CPU. The amplitude streaming task should pause when not playing audio. Verify.
- **GPU usage**: ONNX with CUDA EP can leave the GPU at high utilization if not careful with session management. Verify it idles when not synthesizing.

Fix anything that's noticeably slow or wasteful. Don't optimize prematurely — only fix observed problems.

### Logging Hygiene

By this point, logging may be noisy from development. Clean up:

- **TRACE**: per-byte processing details (only enabled when debugging the processing layer)
- **DEBUG**: state transitions, settings changes, individual TTS segments, individual audio buffers
- **INFO**: app lifecycle events (startup, shutdown), subprocess events (spawn, exit), avatar state changes (maybe), settings load/save
- **WARN**: recoverable errors (single TTS failure, malformed tag, transient PTY error)
- **ERROR**: unrecoverable errors (subprocess crashed, audio device unavailable, settings file unwriteable)

INFO during normal operation should be quiet enough to read. If running with default log level produces a wall of text, the levels are wrong.

Ensure logs include enough context to debug issues — module names via `tracing` targets, request IDs, etc.

### First-Run Experience

What does the user see the first time they launch the app on a fresh machine?

- **Missing model files**: Kokoro ONNX model and voice files need to be present. Decide: bundle with the app, download on first run, or require user to provide. For v1 simplicity, assume user provides; document clearly in README.
- **Missing CUDA**: app should fall back to CPU and continue working. Log clearly that GPU acceleration is unavailable.
- **Missing WebView2 (Windows)**: WebView2 is preinstalled on updated Windows 10/11. Older systems may need it manually. Tauri's installer can include it; document.
- **Missing WebKitGTK (Linux)**: required dependency. Document.
- **No CLAUDE.md present**: the wrapper still works without it, just no TTS markup. Or, optionally, install a default CLAUDE.md to the user's global Claude Code config directory on first run if they don't have one. The latter is more user-friendly but more invasive — let the user opt in.

Decide on the right behavior for each case and implement.

### README and Documentation

Write a README covering:

- What the app is and what it does
- System requirements (OS, GPU, dependencies)
- Where to obtain Kokoro model files
- How to install and run
- How to install / configure CLAUDE.md so Claude Code uses TTS markup
- Settings overview
- Troubleshooting common issues
- Known limitations (the things in DESIGN.md's "Out of Scope" section)

Keep it terse and information-dense. The user prefers that.

### Minor UX Improvements

A non-exhaustive list of things that may be worth doing if encountered:

- Smooth fade transitions between avatar states (instead of abrupt swaps), if rapid state changes are visible
- Loading indicator while Kokoro model loads at startup (it can take a few seconds)
- Window title shows current state or activity (e.g., "Claude — Speaking")
- Sensible default window size and position; remember last size on close
- Keyboard shortcut to open settings (e.g., Ctrl+,)
- Keyboard shortcut to mute (e.g., Ctrl+M) — useful when listening to audio elsewhere
- Drag-to-reorder isn't relevant here; skip
- Right-click menu on the avatar image: "Change image..." as a quick way to open settings to that section. Optional convenience.

Pick what feels valuable; skip what doesn't.

### Application Packaging Notes

This milestone doesn't ship installers but documents what would be needed:

- **Tauri's bundling**: `cargo tauri build` produces platform-specific bundles (`.msi` / `.exe` on Windows, `.deb` / `.AppImage` on Linux)
- **Code signing**: required for Windows distribution to avoid SmartScreen warnings; not required for personal use
- **Bundling Kokoro model files**: large (hundreds of MB). May be impractical to bundle directly; consider download-on-first-run or user-provided
- **CUDA dependency**: dynamically linked; user must have CUDA runtime installed. Document version requirements.
- **Updates**: Tauri has a built-in updater; not required for v1

Capture these in a `PACKAGING.md` so future-you (or future-Claude-Code) knows what to address when distribution becomes a goal.

## Acceptance Criteria

This milestone is done when:

1. The application runs reliably on both Windows and Linux without platform-specific bugs
2. Errors that occur during normal use are surfaced to the user appropriately and don't leave the app in a broken state
3. The app can be left running for extended periods without memory growth or performance degradation
4. First-time launch on a fresh machine has a clear path to working state, even if it requires user intervention (model files, etc.)
5. Logging is clean — INFO-level output during normal use is readable, not overwhelming
6. README documents installation, configuration, and known limitations
7. The user can comfortably use this as their primary Claude Code interface for daily work

## Validation Approach

The best validation for this milestone is sustained use. Run the app for several work sessions across multiple days. Note anything that feels broken, slow, or annoying. Fix what's worth fixing; document what isn't.

A more structured pass:

1. **Fresh-install test**: on a clean machine (or VM), install dependencies as documented, launch the app, attempt to use it. Note every friction point.
2. **Feature regression test**: walk through the validation steps from Milestones 1–6. Verify nothing has regressed.
3. **Stress test**: run the app under load (long sessions, rapid messages, large outputs) for an hour. Profile resource usage.
4. **Error injection test**: deliberately cause errors (kill subprocess, unplug audio device mid-playback if possible, corrupt settings file, etc.). Verify recovery paths.
5. **Cross-platform test**: full feature pass on the secondary platform.

## What "Done" Looks Like

You stop noticing the app's seams. Errors are visible when they happen and recoverable. Performance is fine. The app feels like a coherent, finished tool rather than a collection of working components. You can recommend it to someone else with reasonable confidence they'll get it working.

If after this milestone there are still things that bother you about the experience, file them as future work — but be honest about whether they're shipping blockers or wishlist items. Most are wishlist.

---

## After Milestone 7

The application is feature-complete for v1. Future work falls into the parking lot from DESIGN.md:

- Read-everything override
- Audio device selection in settings
- Conversation/session UI
- Voice mixing
- STT input
- Improved packaging and distribution
- Mobile or web deployment (if ever)

Each of these is a fresh design conversation when the time comes. v1 is done.
