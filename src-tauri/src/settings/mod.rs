//! Settings module: load/save JSON, in-memory store with broadcast updates.
//!
//! The store is the single source of truth for runtime configuration. The
//! broadcast channel propagates updates to subscribers (TTS engine, audio
//! output, processing layer, frontend). Saves are debounced (~500ms) so a
//! slider drag doesn't write the file on every frame.
//!
//! Storage is layered: the global baseline lives at `<exe-dir>/settings.json`
//! and a per-launch-directory overlay (`.cimp/config.json`, inside the
//! project's `.cimp` data dir) records only the fields that differ from
//! global. See `persistence` for details.

mod broadcaster;
/// V32 Phase G (locked decision 16): the three-level injection-protection
/// enable hierarchy and the ONE resolver every enforcement site calls. Public
/// because half the crate's V32 code depends on it — and because "no
/// enforcement site reads a raw settings field" only works if there is a
/// visible, obvious place for them to read instead.
pub mod injection;
mod migration;
mod persistence;
mod schema;

pub use broadcaster::SettingsHandle;
pub use persistence::{
    apply_portable_avatar_paths, load_readonly, mutate_global_harness, note_harness_version,
    read_global_harness_map, read_global_harness_settings, read_global_harness_versions,
    read_global_llm_pricing,
    read_global_prompt_templates, read_project_prompt_templates, reconcile_reserved_tabs,
    write_global_llm_pricing, write_global_prompt_templates,
};
pub use schema::*;

use std::fs;
use std::io::Write;
use std::path::Path;

use crate::error::{AppError, AppResult};
use crate::shell::ShellSpec;

/// Write `bytes` to `path` atomically: write to a sibling temp file, fsync,
/// then rename over the target. A crash or power loss between the temp
/// write and rename leaves the original file intact (the rename is the
/// commit point). This is the only way to write user-configuration files
/// without risking truncation on the 500 ms debounced save path. Used for
/// both the global baseline and the per-folder overlay.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(AppError::Io)?;
    }
    // Each write gets a UNIQUE temp name. Two writers can legitimately
    // target the same file concurrently — e.g. the 500 ms debounced saver
    // and `load()`'s repair-save both touch the per-folder overlay — and a
    // shared `<name>.tmp` lets them truncate each other's temp mid-write or
    // race on the rename (NotFound on the loser; sharing-violation on
    // Windows where the destination/temp may still be open). A per-write
    // suffix makes each writer's temp private; the rename is still the
    // atomic commit point and last-writer-wins on the final file.
    let tmp = match path.file_name() {
        Some(name) => {
            let mut tmp_name = name.to_os_string();
            tmp_name.push(format!(".{}.tmp", uuid::Uuid::new_v4()));
            path.with_file_name(tmp_name)
        }
        None => {
            return Err(AppError::Settings(format!(
                "path has no file name: {}",
                path.display()
            )))
        }
    };

    // Create/write/sync in a closure so any failure removes the temp before
    // returning. Each write uses a fresh UUID temp name, so without this a
    // repeated failure mode (full disk, transient I/O error) would leave a
    // growing pile of orphaned `<name>.<uuid>.tmp` files next to the target.
    let write_result = (|| -> AppResult<()> {
        let mut f = fs::File::create(&tmp).map_err(AppError::Io)?;
        f.write_all(bytes).map_err(AppError::Io)?;
        f.sync_all().map_err(AppError::Io)?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }

    // On Unix, restrict the file to owner read/write only — settings
    // contains the plaintext local-LLM auth_token and any per-tab env
    // values the user defined, which may include credentials. Windows
    // inherits ACLs from the parent directory; programmatic hardening
    // there is documented as a follow-up in
    // docs/FUTURE-FEATURES-keyring.md.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
        if let Err(e) = fs::set_permissions(&tmp, perms) {
            tracing::warn!(error = %e, path = %tmp.display(), "settings: chmod 0600 failed");
        }
    }

    if let Err(e) = fs::rename(&tmp, path) {
        // Rename failed — clean up the temp so it doesn't accumulate, then
        // surface the original error.
        let _ = fs::remove_file(&tmp);
        return Err(AppError::Io(e));
    }
    Ok(())
}

/// Bring up the settings store from disk (or defaults). Always succeeds —
/// missing/corrupt files are recovered with defaults; v1 / v1.1 files are
/// migrated to v1.2 and a backup is written. The default shell is needed
/// to fill in the platform-specific Shell-1 entry on fresh installs and
/// when migration consumes the legacy `_shell_1_tmp` interim key.
///
/// `launch_cwd` is the directory cimp was started in. The custom overlay,
/// if any, is read from and written to that directory.
pub fn init(default_shell: &ShellSpec, launch_cwd: &Path) -> SettingsHandle {
    let outcome = persistence::load(default_shell, launch_cwd);
    SettingsHandle::new(outcome.settings, outcome.global, launch_cwd.to_path_buf())
}

/// #48 findings **F-27** and **F-18** — the Rust side of three cross-language
/// mirrors, each of which has already shipped stale.
///
/// Every check here would be a compile error if the shape allowed one; none does.
/// F-27's two constants live in TypeScript with no link to Rust at all, and
/// F-18's pointers are prose in string literals and doc comments. So these are
/// source scans, deliberately, and each carries the three properties a source
/// scan needs to be worth anything:
///
/// 1. **It parses the CONSTRUCT, not lines.** `SPAWN_BAKED_INJECTION_FEATURES`
///    spans six lines and `backend_gate`'s refusal wraps across three; a
///    line-by-line scanner finds zero members in the first and an empty tail in
///    the second, and passes.
/// 2. **An empty parse FAILS.** Every parser asserts it found something before
///    anything is compared, because "the construct moved" and "the sets agree"
///    must not look the same.
/// 3. **Each has a positive control** that runs the same code over a synthetic
///    known-bad document and asserts it is rejected. A tripwire nobody has seen
///    fail is a tripwire nobody knows works.
#[cfg(test)]
mod frontend_mirrors {
    use super::injection::Feature;
    use super::LOCAL_DATA_TOOLS;
    use std::path::{Path, PathBuf};

    /// The frontend's hand-mirror of the Rust settings wire types (F-27's two
    /// constants live here), embedded at compile time.
    const TS_TYPES: &str = include_str!("../../../src/lib/settings/types.ts");

    /// The Settings window itself — the ONE declaration of the sidebar's
    /// sections. Parsed rather than hand-copied into Rust: a second list of
    /// labels in this file would be the very drift F-18 is about.
    const SETTINGS_APP: &str = include_str!("../../../src/SettingsApp.svelte");

    // ── Parsing TypeScript as constructs ────────────────────────────────────

    /// The `open … close` initialiser of `const <name>`, delimiter-matched so a
    /// multi-line construct — and a wrapped `as const satisfies …` tail — is one
    /// unit rather than a set of lines.
    ///
    /// Anchored on `= <open>` rather than on the first `open` after the name,
    /// because a TS type annotation can contain the delimiter: `SECTIONS` is
    /// declared `const SECTIONS: { … }[] = [`, and taking the first `[` would
    /// return the empty slice inside `[]` — an under-parse that would make every
    /// pointer look valid.
    fn ts_initialiser<'a>(src: &'a str, name: &str, open: char, close: char) -> &'a str {
        let decl = format!("const {name}");
        let at = src
            .find(&decl)
            .unwrap_or_else(|| panic!("`{decl}` is not declared in the mirrored TypeScript"));
        let assign = format!("= {open}");
        let start = at + src[at..]
            .find(&assign)
            .unwrap_or_else(|| panic!("`{decl}` has no `{assign}` initialiser"))
            + assign.len()
            - open.len_utf8();
        let mut depth = 0usize;
        for (i, c) in src[start..].char_indices() {
            if c == open {
                depth += 1;
            } else if c == close {
                depth -= 1;
                if depth == 0 {
                    return &src[start + open.len_utf8()..start + i];
                }
            }
        }
        panic!("`{decl}`'s initialiser is never closed — unbalanced `{open}`");
    }

    /// Every single-quoted run in `block`.
    fn single_quoted(block: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = block;
        while let Some(i) = rest.find('\'') {
            let after = &rest[i + 1..];
            let Some(j) = after.find('\'') else { break };
            out.push(after[..j].to_string());
            rest = &after[j + 1..];
        }
        out
    }

    /// The members of a TS string-array constant. **Panics on an empty parse**
    /// (property 2 above).
    fn ts_members(src: &str, name: &str) -> Vec<String> {
        let members = single_quoted(ts_initialiser(src, name, '[', ']'));
        assert!(
            !members.is_empty(),
            "parsed 0 members out of `{name}`: either the construct changed shape or this \
             parser is broken. An empty parse must FAIL — a vacuous pass is how a stale \
             mirror ships (#48 F-27)."
        );
        members
    }

    /// What one side has that the other does not, in both directions.
    fn set_diff(rust: &[&str], ts: &[String]) -> (Vec<String>, Vec<String>) {
        let missing_in_ts = rust
            .iter()
            .filter(|r| !ts.iter().any(|t| t == *r))
            .map(|r| (*r).to_string())
            .collect();
        let extra_in_ts = ts
            .iter()
            .filter(|t| !rust.iter().any(|r| r == t))
            .cloned()
            .collect();
        (missing_in_ts, extra_in_ts)
    }

    // ── Tripwire 1: LOCAL_DATA_TOOLS ────────────────────────────────────────

    /// #48 F-27, first instance. `run_check` joined Rust's `LOCAL_DATA_TOOLS`
    /// for finding F-12 and the TS array stayed at six entries, so the Settings
    /// window kept writing the OLD exclusion list into a backend's `tool_scope`
    /// — F-12's settings-side half had no production effect.
    ///
    /// Compared as a SET in both directions: an extra name on the TS side is
    /// just as wrong as a missing one (it would exclude a tool Rust considers
    /// cloud-safe), and neither side's order is part of the contract.
    #[test]
    fn local_data_tools_mirror_is_current() {
        let ts = ts_members(TS_TYPES, "LOCAL_DATA_TOOLS");
        let (missing, extra) = set_diff(LOCAL_DATA_TOOLS, &ts);
        assert!(
            missing.is_empty() && extra.is_empty(),
            "src/lib/settings/types.ts `LOCAL_DATA_TOOLS` has drifted from Rust's \
             (settings/schema.rs). Missing from TypeScript: {missing:?}; present only in \
             TypeScript: {extra:?}. The Settings window WRITES this list into a backend's \
             tool_scope, so a stale copy silently narrows the exclusion (#48 F-27)."
        );
    }

    /// The control: the same code over the exact document F-27 was — the
    /// six-entry list, `run_check` absent — must report the drift.
    #[test]
    fn local_data_tools_tripwire_catches_the_stale_six_entry_list() {
        const STALE: &str = "export const LOCAL_DATA_TOOLS = [\n  'read_file',\n  'list_dir',\n  \
                             'code_search',\n  'run_command',\n  'filesystem',\n  'git',\n];\n";
        let ts = ts_members(STALE, "LOCAL_DATA_TOOLS");
        assert_eq!(ts.len(), 6, "the control document is the pre-F-12 mirror");
        let (missing, extra) = set_diff(LOCAL_DATA_TOOLS, &ts);
        assert_eq!(
            missing,
            vec!["run_check".to_string()],
            "the tripwire must name the tool that went missing"
        );
        assert!(extra.is_empty());

        // …and an EMPTY construct is a failure, not a pass.
        let empty = std::panic::catch_unwind(|| {
            ts_members("export const LOCAL_DATA_TOOLS = [] as const;\n", "LOCAL_DATA_TOOLS")
        });
        assert!(empty.is_err(), "an empty parse must panic, not return []");
    }

    // ── Tripwire 2b: TAB_INJECTION_FEATURES ─────────────────────────────────

    /// **V39.** `TAB_INJECTION_FEATURES` is the frontend's list of per-tab L3
    /// cells, and three surfaces now depend on it being exactly
    /// `Feature::has_tab_scope`:
    ///
    /// * `allOffInjectionOverrides` builds the row a NEW tab ships with — a key
    ///   missing here means a control that silently stays inherited on every tab
    ///   the app creates, i.e. on rather than off;
    /// * `tabProtectionRows` filters the badge tint, the badge tooltip's
    ///   "n of m on" and the popover's toggle list by it — a missing key hides a
    ///   real control from the only surface that can reach it;
    /// * `setAllOverrides` / "Enable all" would silently skip it.
    ///
    /// Rust growing a tab-scoped feature is the direction TypeScript cannot
    /// catch, which is what this half is for. Set comparison both ways, like
    /// tripwire 2.
    #[test]
    fn tab_injection_feature_mirror_is_current_in_both_directions() {
        let rust: Vec<&'static str> = Feature::ALL
            .iter()
            .filter(|f| f.has_tab_scope())
            .map(|f| f.key())
            .collect();
        assert!(
            !rust.is_empty(),
            "no feature reports has_tab_scope() — the predicate, not the mirror, is broken"
        );
        let ts = ts_members(TS_TYPES, "TAB_INJECTION_FEATURES");
        let (missing, extra) = set_diff(&rust, &ts);
        assert!(
            missing.is_empty() && extra.is_empty(),
            "src/lib/settings/types.ts `TAB_INJECTION_FEATURES` has drifted from \
             `Feature::has_tab_scope`. Missing from TypeScript: {missing:?}; present only in \
             TypeScript: {extra:?}. A missing member ships a control ON for every newly \
             created tab and hides it from the tab badge popover."
        );
        // Order matters too: it is the order a new tab's row is written in and
        // the order the popover renders, and `Feature::ALL` is declared
        // cheapest-first / spawn-baked-last on purpose.
        assert_eq!(
            rust,
            ts.iter().map(String::as_str).collect::<Vec<_>>(),
            "`TAB_INJECTION_FEATURES` must be in `Feature::ALL` order"
        );
    }

    // ── Tripwire 2: SPAWN_BAKED_INJECTION_FEATURES ──────────────────────────

    /// The spawn-baked feature keys, from the predicate rather than a list:
    /// `Feature::ALL` filtered by [`Feature::spawn_baked`].
    fn rust_spawn_baked() -> Vec<&'static str> {
        Feature::ALL
            .iter()
            .filter(|f| f.spawn_baked())
            .map(|f| f.key())
            .collect()
    }

    /// #48 F-27, second instance — and the reason it is a *set* comparison in
    /// both directions rather than a `contains` check: the Settings window used
    /// to mirror this set twice, both copies went stale when `spotlighting`
    /// became spawn-baked (M-3), and flipping Spotlighting then raised no
    /// restart hint at all. Rust growing a fifth member is the direction
    /// nothing on the TypeScript side can catch — this is that half.
    #[test]
    fn spawn_baked_feature_mirror_is_current_in_both_directions() {
        let rust = rust_spawn_baked();
        assert!(
            !rust.is_empty(),
            "no feature reports spawn_baked() — the predicate, not the mirror, is broken"
        );
        let ts = ts_members(TS_TYPES, "SPAWN_BAKED_INJECTION_FEATURES");
        let (missing, extra) = set_diff(&rust, &ts);
        assert!(
            missing.is_empty() && extra.is_empty(),
            "src/lib/settings/types.ts `SPAWN_BAKED_INJECTION_FEATURES` has drifted from \
             `Feature::spawn_baked`. Missing from TypeScript: {missing:?}; present only in \
             TypeScript: {extra:?}. A missing member means no in-window restart hint for a \
             control that IS baked at spawn (#48 F-27 / M-3)."
        );

        // Every member must also carry an app-wide L2 cell, or the two readers
        // that fold this list into a restart hint read `undefined`.
        let l2 = ts_initialiser(TS_TYPES, "SPAWN_BAKED_L2", '{', '}');
        assert!(
            l2.contains("=>"),
            "parsed no SPAWN_BAKED_L2 body — an empty parse must fail"
        );
        for key in &ts {
            assert!(
                l2.contains(&format!("{key}:")),
                "`{key}` is in SPAWN_BAKED_INJECTION_FEATURES but names no cell in \
                 SPAWN_BAKED_L2, so `spawnBakedInjectionL2` would read undefined for it"
            );
        }
    }

    /// The control, one per trap: a six-line array whose members a per-line
    /// scanner cannot see is parsed correctly; a member Rust does not have is
    /// caught; and an `SPAWN_BAKED_L2` missing a cell is caught.
    #[test]
    fn spawn_baked_tripwire_catches_a_stale_list_and_a_missing_l2_cell() {
        // The real construct spans six lines and its tail wraps — proof the
        // parser reads the construct, not lines.
        let real = ts_members(TS_TYPES, "SPAWN_BAKED_INJECTION_FEATURES");
        assert!(
            real.len() >= 4,
            "the live construct must parse to its real members, not to nothing: {real:?}"
        );
        let decl_line = TS_TYPES
            .lines()
            .find(|l| l.contains("const SPAWN_BAKED_INJECTION_FEATURES"))
            .expect("the declaration exists");
        assert!(
            single_quoted(decl_line).is_empty(),
            "the declaration line itself carries no member — which is exactly why a \
             line-by-line scanner passes vacuously here"
        );

        // M-3's own defect: `spotlighting` dropped from the mirror.
        const STALE: &str = "export const SPAWN_BAKED_INJECTION_FEATURES = [\n  'native_web',\n  \
                             'consumer_hygiene',\n  'tool_steering',\n  \
                             'opencode_native_gate',\n] as const satisfies \
                             readonly (keyof TabInjectionOverrides)[];\n";
        let (missing, extra) = set_diff(&rust_spawn_baked(), &ts_members(STALE, "SPAWN"));
        assert_eq!(
            missing,
            vec!["spotlighting".to_string()],
            "the tripwire must name the member M-3 lost"
        );
        assert!(extra.is_empty());

        // A member with no L2 cell.
        const NO_CELL: &str = "const SPAWN_BAKED_L2: Record<\n  A,\n  B\n> = {\n  \
                               native_web: (o) => o.native_web_visibility,\n};\n";
        let l2 = ts_initialiser(NO_CELL, "SPAWN_BAKED_L2", '{', '}');
        assert!(l2.contains("native_web:"), "the control's one cell parses");
        assert!(
            !l2.contains("spotlighting:"),
            "…and the missing one is detected by the same containment check the live \
             assertion uses"
        );
    }

    // ── Tripwire 3: every settings-path pointer names a real section ─────────

    /// The sidebar labels, parsed out of `SettingsApp.svelte`'s own `SECTIONS`
    /// array — the list the window renders.
    fn sidebar_labels() -> Vec<String> {
        let block = ts_initialiser(SETTINGS_APP, "SECTIONS", '[', ']');
        let labels: Vec<String> = block
            .split("label: '")
            .skip(1)
            .filter_map(|s| s.split('\'').next().map(str::to_string))
            .collect();
        assert!(
            labels.len() >= 10,
            "parsed {} sidebar labels out of SECTIONS — an under-parse would make every \
             pointer look valid or every pointer look wrong: {labels:?}",
            labels.len()
        );
        assert!(
            labels.iter().any(|l| l == "Injection protection"),
            "the F-18 section itself must be in the parsed set: {labels:?}"
        );
        labels
    }

    /// One settings-path pointer found in this crate's source.
    #[derive(Debug)]
    struct Pointer {
        file: String,
        /// The text after the marker, up to the next arrow, quote or newline.
        run: String,
    }

    /// The marker every pointer starts with: the word `Settings`, U+2192, a
    /// space. The same arrow the UI strings use, so this scan sees exactly what a
    /// user would read.
    ///
    /// **Composed at runtime, never written as a literal** — this module's own
    /// source is one of the files the scan reads, so a literal here (or in the
    /// synthetic control documents below) would make the scanner find itself and
    /// report its own examples as defects.
    fn marker() -> String {
        format!("Settings {} ", '\u{2192}')
    }

    /// Pull every pointer out of one file's text.
    ///
    /// **Continued string literals are joined first.** A Rust literal broken
    /// with a trailing `\` continues on the next line with its leading
    /// whitespace stripped, so a refusal written as `… cImp Settings -> \` /
    /// `Checks -> …` is ONE string at runtime — while a line-by-line scanner sees
    /// the marker with a bare `\` after it, treats it as nothing to check, and
    /// skips it. That is not hypothetical: it is the shape of the refusal F-12's
    /// fix shipped (`offload/backend_gate.rs`), i.e. the single most important
    /// string for this tripwire to see.
    fn pointers_in(file: &str, text: &str) -> Vec<Pointer> {
        let marker = marker();
        let joined = join_continuations(text);
        let mut out = Vec::new();
        let mut rest = joined.as_str();
        while let Some(i) = rest.find(&marker) {
            let after = &rest[i + marker.len()..];
            let end = after
                .find(['\n', '"'])
                .into_iter()
                .chain(after.find('→'))
                .min()
                .unwrap_or(after.len());
            out.push(Pointer {
                file: file.to_string(),
                run: after[..end].trim().to_string(),
            });
            rest = after;
        }
        out
    }

    /// Join `\`-continued string literals: drop the backslash, the newline, and
    /// the next line's leading whitespace — exactly what rustc does.
    fn join_continuations(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut chars = text.char_indices().peekable();
        while let Some((_, c)) = chars.next() {
            if c == '\\' && matches!(chars.peek(), Some((_, '\n' | '\r'))) {
                while matches!(chars.peek(), Some((_, c)) if c.is_whitespace()) {
                    chars.next();
                }
                continue;
            }
            out.push(c);
        }
        out
    }

    /// Whether `run` begins with a real sidebar label at a word boundary.
    ///
    /// A **prefix** match rather than equality, because a pointer in prose runs
    /// on into the sentence around it (*"…enable it in Settings -> Code
    /// Intelligence if the server rejects long chunks"*), and guessing where the
    /// label ends is the ambiguity this check must not depend on.
    ///
    /// **Its limit, stated:** this validates the FIRST segment only. The
    /// backend_gate refusal that named `Code Intelligence -> Checks` — a real
    /// label followed by a wrong sub-path — would pass here; that half of F-18
    /// was fixed by hand. What this catches is a first segment naming a section
    /// that does not exist, which is every other F-18 sighting.
    fn names_a_section(run: &str, labels: &[String]) -> bool {
        labels.iter().any(|l| {
            run.strip_prefix(l.as_str())
                .is_some_and(|tail| !tail.starts_with(|c: char| c.is_alphanumeric()))
        })
    }

    /// Every `.rs` / `.css` file under `src-tauri/src`, skipping dot-directories
    /// (`.cimp` holds a graph db and a shadow git worktree, i.e. binaries).
    fn source_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read_dir under src-tauri/src") {
            let path = entry.expect("dir entry").path();
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            if path.is_dir() {
                source_files(&path, out);
            } else if matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("rs" | "css")
            ) {
                out.push(path);
            }
        }
    }

    fn crate_pointers() -> Vec<Pointer> {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        source_files(&root, &mut files);
        assert!(
            files.len() > 100,
            "scanned only {} source files — the walk is broken, and a broken walk finds no \
             bad pointer and passes",
            files.len()
        );
        let mut out = Vec::new();
        for path in files {
            let rel = path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{rel} is not readable as UTF-8: {e}"));
            out.extend(pointers_in(&rel, &text));
        }
        assert!(
            !out.is_empty(),
            "found no `{}` pointer anywhere in the crate — the matcher is broken (the arrow is \
             U+2192, not `->`)",
            marker()
        );
        out
    }

    /// #48 F-18 — every settings-path pointer in this crate names a section the
    /// sidebar actually has. Two of these shipped: `updates_allowed`'s refusal
    /// said *"Settings -> Tools -> Injection protection"* and `backend_gate`'s
    /// said *"Settings -> Code Intelligence -> Checks"*, and there is no `Tools`
    /// section while `Checks` is not a child of `Code Intelligence`. A pointer
    /// that resolves to nothing is worse than none: the reader concludes the
    /// switch does not exist.
    ///
    /// Comments are held to the same rule as strings, deliberately — a comment
    /// is a promise to the next maintainer, and every wrong pointer in this
    /// crate but two was one.
    #[test]
    fn every_settings_pointer_names_a_real_sidebar_section() {
        let labels = sidebar_labels();
        let pointers = crate_pointers();
        let bad: Vec<&Pointer> = pointers
            .iter()
            .filter(|p| !names_a_section(&p.run, &labels))
            .collect();
        assert!(
            bad.is_empty(),
            "{} of {} settings-path pointers name no sidebar section. Valid first segments \
             are {labels:?}. Offenders: {bad:#?}",
            bad.len(),
            pointers.len()
        );
    }

    /// The controls, and the reason the joining above is not decoration.
    #[test]
    fn the_settings_pointer_scan_sees_wrapped_literals_and_rejects_dead_paths() {
        let labels = sidebar_labels();

        // 1. The wrapped literal F-12's fix shipped, reconstructed byte for byte
        //    (composed, not written — see `marker`).
        let arrow = '\u{2192}';
        let wrapped_bad = format!(
            "                 remote offload backend is off — enable it for this project in cImp \
             {}\\\n                 Tools {arrow} Injection protection)\"\n",
            marker()
        );
        let found = pointers_in("control.rs", &wrapped_bad);
        assert_eq!(found.len(), 1, "the wrapped pointer must be found: {found:?}");
        assert_eq!(found[0].run, "Tools", "the tail comes from the NEXT line");
        assert!(
            !names_a_section(&found[0].run, &labels),
            "`Tools` is not a sidebar section and must be rejected"
        );
        // …and this is what the scanner it replaces saw: the marker followed by a
        // lone `\`. Anything that treats that as "no path here" skips exactly the
        // string F-12 shipped, which is a vacuous pass.
        let m = marker();
        let per_line: Vec<String> = wrapped_bad
            .lines()
            .filter_map(|l| l.split_once(m.as_str()))
            .map(|(_, tail)| tail.trim().to_string())
            .collect();
        assert_eq!(
            per_line,
            vec!["\\".to_string()],
            "a per-line scan sees only a continuation backslash: {per_line:?}"
        );

        // 2. The live crate's own wrapped refusal is really in the scan — the
        //    end-to-end version of control 1.
        let live = crate_pointers();
        let gate: Vec<&Pointer> = live
            .iter()
            .filter(|p| p.file == "offload/backend_gate.rs")
            .collect();
        assert!(
            gate.iter().any(|p| p.run == "Checks"),
            "backend_gate's wrapped refusal must be seen, and must name `Checks`: {gate:#?}"
        );

        // 3. Every other dead path F-18 reported is rejected, and the sections
        //    they should have named are accepted.
        for dead in [
            "Tools → Detection",
            "Code graph.",
            "Code Graph)",
            "Offload.)",
            "Waveform.",
            "Theme.",
        ] {
            assert!(
                !names_a_section(dead, &labels),
                "{dead:?} named no section when F-18 was raised and must not now"
            );
        }
        for live_path in [
            "Injection protection → Injection detection",
            "Code Intelligence.",
            "Offload task tools → Tools",
            "Avatar → Waveform.",
            "Appearance → Accent color",
            "Checks → Offload worker access)",
            "Bottom bar), otherwise",
        ] {
            assert!(
                names_a_section(live_path, &labels),
                "{live_path:?} is the corrected path and must be accepted"
            );
        }

        // 4. A label must match at a word boundary: `Tabsomething` is not `Tabs`.
        assert!(names_a_section("Tabs.", &labels));
        assert!(!names_a_section("Tabsomething", &labels));
    }
}
