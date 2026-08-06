//! V32 Phase C3 — the **update manifest**: its schema, its parse boundary, and
//! the fetch seam the rest of the updater is written against.
//!
//! # The channel, and why it is a manifest at all
//!
//! Locked decision 13: the updater pulls from **a cImp-curated manifest, never
//! from third-party repositories directly**. The rule corpora we derive from
//! (Vigil, garak) live in repositories we do not control, and the defense
//! layer's own update channel is attack surface — a compromise of any of those
//! upstreams would otherwise write rule content straight into every install.
//! The maintenance run curates upstream into a bundle; the bundle is published
//! as GitHub release assets on a fixed tag; this module reads the JSON index of
//! that release.
//!
//! # Schema (v1)
//!
//! ```json
//! {
//!   "schema": 1,
//!   "generated": "2026-08-07T12:00:00Z",
//!   "components": [
//!     {
//!       "component": "rules",
//!       "version": "2026.08.07",
//!       "min_app_version": "0.51.0",
//!       "notes": "Vigil refresh + three new exfiltration rules.",
//!       "files": [
//!         {
//!           "name": "injection_core.yar",
//!           "sha256": "6f…64 hex chars…",
//!           "size": 8421,
//!           "url": "https://github.com/Dyserna/cImp/releases/download/detection-v1/rules-2026.08.07-injection_core.yar"
//!         }
//!       ]
//!     }
//!   ]
//! }
//! ```
//!
//! - `schema` is an exact match, not a floor: an unknown schema is REJECTED
//!   rather than best-effort parsed. A future schema may move a field this
//!   version would read with the old meaning, and "misread a security artifact"
//!   is strictly worse than "did not update".
//! - `component` is a closed set ([`Component`]). An unknown component is
//!   skipped, not an error — that is the one forward-compatible affordance, so
//!   a manifest that adds a third component still updates the two this build
//!   knows.
//! - `min_app_version`, when present, gates the component: a bundle that needs
//!   a newer app is *available*, not *applicable*.
//! - `files[].name` is a BARE file name, and [`Manifest::parse`] enforces it —
//!   see [`is_safe_name`]. A manifest is remote input, so a `name` of
//!   `../../cimp.exe` must be impossible to express, not merely unlikely.
//! - `size` is the exact expected byte count. It is checked alongside the
//!   digest (a mismatch fails before hashing, which is what keeps a
//!   declared-2 KB / actually-2 GB entry from being downloaded in full).
//!
//! # No archive format
//!
//! Every file is listed and fetched individually. A zip would be one request
//! instead of five, but it would also mean a new dependency AND an unpacking
//! step that runs *before* the content can be validated — a decompressor
//! parsing attacker-controlled bytes, in the module whose entire purpose is to
//! not do that. Per-file downloads keep "verify the digest, then touch the
//! content" a straight line.
//!
//! # The asset-origin invariant
//!
//! **Every artifact URL must live under the manifest's own directory.** The
//! manifest is fetched from a pinned URL ([`DEFAULT_MANIFEST_URL`]); an asset
//! URL is accepted only if it starts with that URL minus its last path segment
//! ([`asset_prefix`]). So a manifest served from the `detection-v1` release can
//! only ever name assets on the `detection-v1` release. This is what makes the
//! curated channel actually curated: whoever can rewrite the manifest still
//! cannot redirect the download to a host of their choosing.
//!
//! It also composes with the settings override: pointing
//! `detection_update_manifest_url` at a local test bundle relocates the assets
//! with it, with no special case and no way to mix origins.

use std::collections::BTreeMap;

use serde::Deserialize;

/// The pinned manifest URL: a fixed release tag whose assets are replaced and
/// added over time, exactly like the `models-v1` release the model blobs ship
/// from. A fixed tag (rather than "latest") means the URL never has to be
/// discovered, so there is no API call and no redirect chain to trust.
pub const DEFAULT_MANIFEST_URL: &str =
    "https://github.com/Dyserna/cImp/releases/download/detection-v1/manifest.json";

/// The only manifest schema this build understands.
pub const SCHEMA_VERSION: u32 = 1;

/// Ceiling on the manifest document itself. It is an index, not a payload;
/// anything approaching this is a sign the URL is pointed at the wrong thing.
pub const MAX_MANIFEST_BYTES: u64 = 256 * 1024;

/// Ceiling on one rule file. The whole shipped bundle is ~20 KiB of text today;
/// 4 MiB is three orders of magnitude of headroom and still small enough that a
/// mis-sized entry cannot fill a disk.
pub const MAX_RULE_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// Ceiling on one classifier artifact. The 22M ONNX export is ~90 MB and the
/// documented 86M upgrade path is ~350 MB, so this bounds the *known* upgrade
/// path with room to spare rather than being an arbitrary large number.
pub const MAX_CLASSIFIER_FILE_BYTES: u64 = 512 * 1024 * 1024;

/// Ceiling on files per component — a bundle is a handful of rule files or two
/// model artifacts, never hundreds.
pub const MAX_FILES_PER_COMPONENT: usize = 64;

/// The two updatable components. A closed set: each has its own validation
/// pipeline, its own on-disk destination and its own default mode, so "some
/// other component" has nowhere to go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub enum Component {
    /// The YARA signature bundle under `<exe-dir>/detection/rules.d/`.
    Rules,
    /// The Prompt Guard 2 weights + tokenizer under `models/promptguard2-22m/`.
    Classifier,
}

impl Component {
    pub const ALL: [Component; 2] = [Component::Rules, Component::Classifier];

    pub const fn as_str(self) -> &'static str {
        match self {
            Component::Rules => "rules",
            Component::Classifier => "classifier",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "rules" => Some(Component::Rules),
            "classifier" => Some(Component::Classifier),
            _ => None,
        }
    }

    /// Per-file size ceiling for this component's artifacts.
    pub const fn max_file_bytes(self) -> u64 {
        match self {
            Component::Rules => MAX_RULE_FILE_BYTES,
            Component::Classifier => MAX_CLASSIFIER_FILE_BYTES,
        }
    }
}

/// One artifact: what to fetch, how big it must be, and what it must hash to.
#[derive(Debug, Clone, PartialEq)]
pub struct FileEntry {
    /// Bare destination file name (validated by [`is_safe_name`]).
    pub name: String,
    /// Lowercase 64-hex SHA-256 of the file's bytes.
    pub sha256: String,
    /// Exact expected size in bytes.
    pub size: u64,
    /// Absolute https URL under the manifest's own directory.
    pub url: String,
}

/// One component's release: a version plus the complete file list that
/// constitutes it. A component update is always **whole-bundle** — the files
/// listed here replace every updater-managed file at the destination — because
/// a per-file merge would let a partial fetch produce a rule set that no
/// curation step ever validated as a set.
#[derive(Debug, Clone, PartialEq)]
pub struct ComponentEntry {
    pub component: Component,
    pub version: String,
    /// App version required to use this bundle, when the curator set one.
    pub min_app_version: Option<String>,
    /// Free-text changelog line, shown in Settings. Rendered as text, never
    /// interpreted — it is remote content.
    pub notes: Option<String>,
    pub files: Vec<FileEntry>,
}

/// A parsed, fully validated manifest. Constructing one is the parse boundary:
/// every invariant in this module's header holds for any value of this type, so
/// no consumer re-checks them.
#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    pub generated: Option<String>,
    /// Components keyed for lookup. A duplicate `component` entry is a parse
    /// error rather than a last-one-wins — two versions of the same bundle in
    /// one manifest means the curation step produced something nobody meant.
    pub components: BTreeMap<Component, ComponentEntry>,
}

// ── The wire shapes ────────────────────────────────────────────────────────
//
// Separate from the validated types above on purpose: these mirror the JSON
// exactly and are permissive; `Manifest::parse` is where permissive becomes
// strict. `deny_unknown_fields` is deliberately NOT set — a manifest that gains
// a field for a future build must still parse here.

#[derive(Deserialize)]
struct RawManifest {
    schema: u32,
    #[serde(default)]
    generated: Option<String>,
    #[serde(default)]
    components: Vec<RawComponent>,
}

#[derive(Deserialize)]
struct RawComponent {
    component: String,
    version: String,
    #[serde(default)]
    min_app_version: Option<String>,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    files: Vec<RawFile>,
}

#[derive(Deserialize)]
struct RawFile {
    name: String,
    sha256: String,
    size: u64,
    url: String,
}

impl Manifest {
    /// Parse and validate `json`, rejecting anything that does not satisfy
    /// every invariant in the module header. `manifest_url` supplies the
    /// asset-origin prefix (see [`asset_prefix`]).
    ///
    /// Errors are one-line, human-readable and land verbatim in an Advisor card
    /// and an activity row — they are read by the person curating the bundle,
    /// so "which entry, which field" matters more than brevity.
    pub fn parse(json: &str, manifest_url: &str) -> Result<Self, String> {
        let raw: RawManifest =
            serde_json::from_str(json).map_err(|e| format!("manifest is not valid JSON: {e}"))?;
        if raw.schema != SCHEMA_VERSION {
            return Err(format!(
                "manifest schema {} is not supported (this build understands {SCHEMA_VERSION}) — \
                 update cImp rather than reinterpreting an unknown schema",
                raw.schema
            ));
        }
        let prefix = asset_prefix(manifest_url)?;
        let mut components: BTreeMap<Component, ComponentEntry> = BTreeMap::new();
        for rc in raw.components {
            // Unknown component: the one forward-compatible skip. A newer
            // manifest listing a third component must still update the two this
            // build knows about.
            let Some(component) = Component::parse(&rc.component) else {
                continue;
            };
            if components.contains_key(&component) {
                return Err(format!(
                    "manifest lists component `{}` twice — a bundle has exactly one current version",
                    rc.component
                ));
            }
            if rc.version.trim().is_empty() {
                return Err(format!("component `{}` has an empty version", rc.component));
            }
            if rc.files.is_empty() {
                return Err(format!(
                    "component `{}` lists no files — an empty bundle would deactivate the layer",
                    rc.component
                ));
            }
            if rc.files.len() > MAX_FILES_PER_COMPONENT {
                return Err(format!(
                    "component `{}` lists {} files (cap {MAX_FILES_PER_COMPONENT})",
                    rc.component,
                    rc.files.len()
                ));
            }
            let mut files = Vec::with_capacity(rc.files.len());
            let mut seen: Vec<String> = Vec::new();
            for rf in rc.files {
                let entry = parse_file(&rc.component, rf, component, &prefix)?;
                if seen.contains(&entry.name) {
                    return Err(format!(
                        "component `{}` lists `{}` twice",
                        rc.component, entry.name
                    ));
                }
                seen.push(entry.name.clone());
                files.push(entry);
            }
            components.insert(
                component,
                ComponentEntry {
                    component,
                    version: rc.version.trim().to_string(),
                    min_app_version: rc.min_app_version.filter(|s| !s.trim().is_empty()),
                    notes: rc.notes.filter(|s| !s.trim().is_empty()),
                    files,
                },
            );
        }
        if components.is_empty() {
            return Err(
                "manifest lists no component this build recognizes (expected `rules` and/or \
                 `classifier`)"
                    .to_string(),
            );
        }
        Ok(Manifest {
            generated: raw.generated,
            components,
        })
    }
}

/// Validate one file entry. Split out so every rejection reason names the
/// component and the file it belongs to.
fn parse_file(
    label: &str,
    rf: RawFile,
    component: Component,
    prefix: &str,
) -> Result<FileEntry, String> {
    // NOT trimmed: `is_safe_name` rejects trailing spaces and dots precisely
    // because the Win32 path layer strips them, so `evil.yar ` would land on
    // `evil.yar` — a rename the digest check cannot see. Trimming here first
    // would erase the very thing being checked for.
    let name = rf.name.clone();
    if !is_safe_name(&name) {
        return Err(format!(
            "component `{label}` has an unusable file name `{name}` — names must be a bare file \
             name with no path separators, no `..` and no drive letter"
        ));
    }
    if component == Component::Rules && !has_rule_extension(&name) {
        return Err(format!(
            "component `rules` file `{name}` is not a .yar/.yara file — the rules directory is \
             read by extension, so anything else would be downloaded and then ignored"
        ));
    }
    let sha256 = rf.sha256.trim().to_ascii_lowercase();
    if !is_sha256_hex(&sha256) {
        return Err(format!(
            "component `{label}` file `{name}` has no usable sha256 (expected 64 hex characters)"
        ));
    }
    if rf.size == 0 {
        return Err(format!(
            "component `{label}` file `{name}` declares size 0 — an empty artifact is never a \
             valid bundle member"
        ));
    }
    let cap = component.max_file_bytes();
    if rf.size > cap {
        return Err(format!(
            "component `{label}` file `{name}` declares {} bytes, over the {cap}-byte ceiling for \
             this component",
            rf.size
        ));
    }
    let url = rf.url.trim().to_string();
    if !url.starts_with(prefix) {
        return Err(format!(
            "component `{label}` file `{name}` points outside the manifest's own directory \
             (`{url}` is not under `{prefix}`) — artifacts may only come from the same curated \
             location as the manifest"
        ));
    }
    Ok(FileEntry {
        name,
        sha256,
        size: rf.size,
        url,
    })
}

/// The directory portion of `manifest_url`, which every artifact URL must start
/// with. Requires an `https` (or, for a local test server, `http`) URL with a
/// path — a bare origin has no directory to anchor to.
pub fn asset_prefix(manifest_url: &str) -> Result<String, String> {
    let scheme_end = manifest_url
        .find("://")
        .ok_or_else(|| format!("manifest URL `{manifest_url}` has no scheme"))?;
    let scheme = &manifest_url[..scheme_end];
    if scheme != "https" && scheme != "http" {
        return Err(format!(
            "manifest URL scheme `{scheme}` is not supported (expected https)"
        ));
    }
    // Everything up to and including the last `/`. Searching from `scheme_end +
    // 3` keeps the scheme's own slashes out of the answer for an origin-only
    // URL, which then fails the "no path" check below rather than yielding
    // `https:/` as a prefix.
    let after_scheme = scheme_end + 3;
    let cut = manifest_url[after_scheme..]
        .rfind('/')
        .ok_or_else(|| format!("manifest URL `{manifest_url}` has no path to anchor assets to"))?;
    Ok(manifest_url[..after_scheme + cut + 1].to_string())
}

/// A bare file name: no separators, no parent traversal, no drive prefix, not a
/// reserved dot entry. Checked at the parse boundary because a manifest is
/// remote input and every consumer of `FileEntry::name` joins it onto a
/// directory path.
pub fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name != "."
        && name != ".."
        && !name.starts_with('.')
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains(':')
        && !name.contains('\0')
        // Trailing dots/spaces are stripped by the Win32 path layer, so
        // `evil.yar.` and `evil.yar ` would land on `evil.yar` — a rename the
        // digest check cannot see.
        && name.trim_end_matches([' ', '.']) == name
}

fn has_rule_extension(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".yar") || lower.ends_with(".yara")
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

// ── Version comparison ─────────────────────────────────────────────────────

/// Whether `candidate` is strictly newer than `installed`.
///
/// Deliberately NOT a semver parse. The manifest's own contract (decision 13)
/// is "monotonic or semver", and the rule bundle's natural version is a date
/// (`2026.08.07`) — which is not semver and must not be forced into it. So:
/// split both on non-alphanumeric boundaries, compare segment by segment,
/// numerically where both segments are numeric and lexicographically otherwise.
/// `2026.08.07` > `2026.7.30` and `1.10.0` > `1.9.0` both come out right, and a
/// nonsense pair falls back to a stable string order rather than a panic.
///
/// An empty `installed` means "nothing installed": anything is newer.
/// **Equality is not newer** — the daily check must be a no-op when it finds
/// the version already on disk.
pub fn is_newer(candidate: &str, installed: &str) -> bool {
    if installed.trim().is_empty() {
        return !candidate.trim().is_empty();
    }
    compare_versions(candidate, installed) == std::cmp::Ordering::Greater
}

/// Segment-wise ordering behind [`is_newer`], exposed for tests.
pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let seg = |s: &str| -> Vec<String> {
        s.split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|p| !p.is_empty())
            .map(|p| p.to_ascii_lowercase())
            .collect()
    };
    let (av, bv) = (seg(a), seg(b));
    for i in 0..av.len().max(bv.len()) {
        // A missing segment sorts below a present one: `1.2` < `1.2.1`.
        let (x, y) = (av.get(i), bv.get(i));
        let ord = match (x, y) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Less,
            (Some(_), None) => Ordering::Greater,
            (Some(x), Some(y)) => match (x.parse::<u64>(), y.parse::<u64>()) {
                (Ok(x), Ok(y)) => x.cmp(&y),
                _ => x.cmp(y),
            },
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// Whether this build satisfies a component's `min_app_version`. An absent or
/// unparseable requirement is satisfied — a curator typo must not silently
/// freeze updates for everyone.
pub fn app_version_satisfies(min: Option<&str>) -> bool {
    let Some(min) = min else { return true };
    compare_versions(env!("CARGO_PKG_VERSION"), min) != std::cmp::Ordering::Less
}

// ── The fetch seam ─────────────────────────────────────────────────────────

/// How the updater gets bytes. A trait so every test in this milestone runs
/// with **no network at all**: the whole validate-activate-rollback pipeline is
/// exercised against an in-memory map of URL → bytes, which is also the only
/// way to test a checksum mismatch or a truncated download deterministically.
#[async_trait::async_trait]
pub trait Fetcher: Send + Sync {
    /// GET `url`, refusing to buffer more than `max_bytes`.
    ///
    /// The cap is enforced by the implementation, not by the caller checking
    /// afterwards: "download it all, then notice it was too big" is not a cap.
    async fn get(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>, String>;
}

/// The real fetcher: HTTPS via the same `reqwest` + rustls stack the rest of
/// the app uses, with its own client so a long model download cannot share a
/// timeout with anything interactive.
pub struct HttpFetcher;

/// Per-request ceiling. Generous because the classifier artifact is ~90 MB on
/// whatever connection the user has; the updater runs in the background, so a
/// slow download costs nothing but its own patience.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

impl HttpFetcher {
    fn client() -> &'static reqwest::Client {
        static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
        C.get_or_init(|| {
            reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .user_agent(concat!("cImp/", env!("CARGO_PKG_VERSION"), " detection-updater"))
                .build()
                .unwrap_or_else(|e| {
                    tracing::warn!(
                        target: "offload",
                        error = %e,
                        "detection updater: failed to build HTTP client; using the default"
                    );
                    reqwest::Client::new()
                })
        })
    }
}

#[async_trait::async_trait]
impl Fetcher for HttpFetcher {
    async fn get(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>, String> {
        let mut resp = Self::client()
            .get(url)
            .send()
            .await
            .map_err(|e| format!("GET {url}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("GET {url}: HTTP {}", resp.status()));
        }
        // `Content-Length` is used only to fail EARLY. The streaming cap below
        // is what actually bounds memory, because the header is remote input:
        // it may be absent (chunked) or simply a lie.
        if let Some(len) = resp.content_length() {
            if len > max_bytes {
                return Err(format!(
                    "GET {url}: response declares {len} bytes, over the {max_bytes}-byte ceiling"
                ));
            }
        }
        // `Response::chunk` rather than a `Stream`: it is the same loop without
        // pulling `futures`/`tokio-stream` in for one call site.
        let mut out: Vec<u8> = Vec::new();
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    if out.len() as u64 + chunk.len() as u64 > max_bytes {
                        return Err(format!(
                            "GET {url}: response exceeded the {max_bytes}-byte ceiling mid-stream"
                        ));
                    }
                    out.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(e) => return Err(format!("GET {url}: {e}")),
            }
        }
        Ok(out)
    }
}

/// In-memory [`Fetcher`] for tests: no network, and the only way to produce a
/// corrupted or truncated download deterministically. Lives here rather than in
/// a test module so the updater's own tests can drive the real pipeline with
/// it.
#[cfg(test)]
pub struct MapFetcher {
    files: std::collections::HashMap<String, Vec<u8>>,
    /// URLs requested, in order — lets a test assert that a component which was
    /// never applied was also never downloaded.
    pub seen: std::sync::Mutex<Vec<String>>,
}

#[cfg(test)]
impl MapFetcher {
    pub fn new(files: std::collections::HashMap<String, Vec<u8>>) -> Self {
        Self {
            files,
            seen: std::sync::Mutex::new(Vec::new()),
        }
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl Fetcher for MapFetcher {
    async fn get(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>, String> {
        self.seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(url.to_string());
        let body = self
            .files
            .get(url)
            .ok_or_else(|| format!("GET {url}: HTTP 404"))?;
        if body.len() as u64 > max_bytes {
            return Err(format!(
                "GET {url}: response exceeded the {max_bytes}-byte ceiling mid-stream"
            ));
        }
        Ok(body.clone())
    }
}

/// Lowercase hex SHA-256 of `data`.
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(data);
    let mut out = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        // Infallible into a String; the `let _` keeps clippy quiet without
        // pretending there is an error path to handle.
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const BASE: &str = "https://github.com/Dyserna/cImp/releases/download/detection-v1/";

    fn manifest_json(files: &str) -> String {
        format!(
            r#"{{"schema":1,"generated":"2026-08-07T00:00:00Z","components":[
                 {{"component":"rules","version":"2026.08.07","files":[{files}]}}]}}"#
        )
    }

    fn file_json(name: &str, sha: &str, size: u64, url: &str) -> String {
        format!(r#"{{"name":"{name}","sha256":"{sha}","size":{size},"url":"{url}"}}"#)
    }

    fn good_file() -> String {
        file_json(
            "injection_core.yar",
            &"a".repeat(64),
            1234,
            &format!("{BASE}rules-2026.08.07-injection_core.yar"),
        )
    }

    #[test]
    fn a_well_formed_manifest_parses_into_its_validated_shape() {
        let m = Manifest::parse(&manifest_json(&good_file()), DEFAULT_MANIFEST_URL)
            .expect("the happy path parses");
        let rules = m.components.get(&Component::Rules).expect("rules present");
        assert_eq!(rules.version, "2026.08.07");
        assert_eq!(rules.files.len(), 1);
        assert_eq!(rules.files[0].size, 1234);
        assert!(!m.components.contains_key(&Component::Classifier));
    }

    /// An unknown schema is refused outright. Best-effort parsing a security
    /// artifact whose field meanings may have moved is worse than not updating.
    #[test]
    fn an_unknown_schema_is_rejected_rather_than_best_effort_parsed() {
        let json = manifest_json(&good_file()).replace("\"schema\":1", "\"schema\":2");
        let e = Manifest::parse(&json, DEFAULT_MANIFEST_URL).expect_err("schema 2 is rejected");
        assert!(e.contains("schema 2"), "{e}");
    }

    /// The forward-compatible affordance: a component this build does not know
    /// is skipped, and the ones it does know still parse.
    #[test]
    fn an_unknown_component_is_skipped_and_the_known_ones_still_load() {
        let json = format!(
            r#"{{"schema":1,"components":[
                 {{"component":"judge","version":"9","files":[{}]}},
                 {{"component":"rules","version":"2026.08.07","files":[{}]}}]}}"#,
            good_file(),
            good_file()
        );
        let m = Manifest::parse(&json, DEFAULT_MANIFEST_URL).expect("known component survives");
        assert_eq!(m.components.len(), 1);
        assert!(m.components.contains_key(&Component::Rules));
    }

    /// A manifest with nothing this build can use is an error, not an empty
    /// success — "checked, found nothing" and "checked, understood nothing"
    /// are different outcomes and only one of them is healthy.
    #[test]
    fn a_manifest_with_no_recognized_component_is_an_error() {
        let json = r#"{"schema":1,"components":[{"component":"judge","version":"9","files":[]}]}"#;
        assert!(Manifest::parse(json, DEFAULT_MANIFEST_URL).is_err());
    }

    /// Path traversal in a file name is impossible to express. This is the one
    /// rejection that would be a remote-write primitive if it were missing.
    #[test]
    fn a_file_name_that_escapes_its_directory_is_rejected() {
        for bad in [
            "../evil.yar",
            "..\\evil.yar",
            "sub/dir.yar",
            "C:evil.yar",
            ".hidden.yar",
            "..",
            "",
            "trailing.yar ",
            "trailing.yar.",
        ] {
            let f = file_json(bad, &"a".repeat(64), 10, &format!("{BASE}x"));
            assert!(
                Manifest::parse(&manifest_json(&f), DEFAULT_MANIFEST_URL).is_err(),
                "name {bad:?} must be rejected"
            );
        }
        assert!(is_safe_name("injection_core.yar"));
        assert!(is_safe_name("model.onnx"));
    }

    /// A rules bundle may only carry files the rules loader will actually read
    /// — otherwise a "successful" update installs files that do nothing.
    #[test]
    fn a_rules_bundle_may_only_carry_yara_files() {
        let f = file_json(
            "readme.txt",
            &"a".repeat(64),
            10,
            &format!("{BASE}readme.txt"),
        );
        let e = Manifest::parse(&manifest_json(&f), DEFAULT_MANIFEST_URL).expect_err("rejected");
        assert!(e.contains(".yar"), "{e}");
    }

    #[test]
    fn a_missing_or_malformed_checksum_is_rejected() {
        for sha in ["", "abc", &"z".repeat(64), &"a".repeat(63)] {
            let f = file_json("a.yar", sha, 10, &format!("{BASE}a.yar"));
            assert!(
                Manifest::parse(&manifest_json(&f), DEFAULT_MANIFEST_URL).is_err(),
                "sha {sha:?} must be rejected"
            );
        }
    }

    #[test]
    fn an_oversize_or_empty_entry_is_rejected() {
        let big = file_json(
            "a.yar",
            &"a".repeat(64),
            MAX_RULE_FILE_BYTES + 1,
            &format!("{BASE}a.yar"),
        );
        assert!(Manifest::parse(&manifest_json(&big), DEFAULT_MANIFEST_URL).is_err());
        let empty = file_json("a.yar", &"a".repeat(64), 0, &format!("{BASE}a.yar"));
        assert!(Manifest::parse(&manifest_json(&empty), DEFAULT_MANIFEST_URL).is_err());
    }

    /// The asset-origin invariant: whoever can rewrite the manifest still
    /// cannot redirect a download to a host of their choosing.
    #[test]
    fn an_asset_url_outside_the_manifests_own_directory_is_rejected() {
        for bad in [
            "https://evil.example/rules.yar",
            "https://github.com/Dyserna/cImp/releases/download/other-tag/rules.yar",
            "http://github.com/Dyserna/cImp/releases/download/detection-v1/rules.yar",
            "file:///C:/rules.yar",
        ] {
            let f = file_json("a.yar", &"a".repeat(64), 10, bad);
            assert!(
                Manifest::parse(&manifest_json(&f), DEFAULT_MANIFEST_URL).is_err(),
                "url {bad} must be rejected"
            );
        }
    }

    /// The override composes with the same rule and needs no special case: a
    /// local test manifest relocates its own assets with it.
    #[test]
    fn the_prefix_rule_follows_a_manifest_url_override() {
        let local = "http://127.0.0.1:8099/bundle/manifest.json";
        assert_eq!(
            asset_prefix(local).unwrap(),
            "http://127.0.0.1:8099/bundle/"
        );
        let f = file_json(
            "a.yar",
            &"a".repeat(64),
            10,
            "http://127.0.0.1:8099/bundle/a.yar",
        );
        assert!(Manifest::parse(&manifest_json(&f), local).is_ok());
        // …and the pinned prefix is still refused from the local manifest.
        let f = file_json("a.yar", &"a".repeat(64), 10, &format!("{BASE}a.yar"));
        assert!(Manifest::parse(&manifest_json(&f), local).is_err());
    }

    #[test]
    fn asset_prefix_needs_a_supported_scheme_and_a_path() {
        assert!(asset_prefix("ftp://x/y/manifest.json").is_err());
        assert!(asset_prefix("github.com/manifest.json").is_err());
        assert!(asset_prefix("https://github.com").is_err());
        assert_eq!(
            asset_prefix(DEFAULT_MANIFEST_URL).unwrap(),
            "https://github.com/Dyserna/cImp/releases/download/detection-v1/"
        );
    }

    #[test]
    fn duplicate_components_and_duplicate_file_names_are_rejected() {
        let json = format!(
            r#"{{"schema":1,"components":[
                 {{"component":"rules","version":"1","files":[{}]}},
                 {{"component":"rules","version":"2","files":[{}]}}]}}"#,
            good_file(),
            good_file()
        );
        assert!(Manifest::parse(&json, DEFAULT_MANIFEST_URL).is_err());
        let two = format!("{},{}", good_file(), good_file());
        assert!(Manifest::parse(&manifest_json(&two), DEFAULT_MANIFEST_URL).is_err());
    }

    /// An empty file list would install "no rules", which is exactly the silent
    /// degradation to no-detection decision 13 forbids.
    #[test]
    fn a_component_with_no_files_is_rejected() {
        let json = r#"{"schema":1,"components":[{"component":"rules","version":"1","files":[]}]}"#;
        assert!(Manifest::parse(json, DEFAULT_MANIFEST_URL).is_err());
    }

    // ── Version ordering ────────────────────────────────────────────────

    #[test]
    fn version_ordering_handles_dates_and_semver_and_equality() {
        assert!(is_newer("2026.08.07", "2026.07.30"));
        assert!(is_newer("2026.08.07", "2026.8.6"));
        assert!(!is_newer("2026.08.07", "2026.08.07"), "equal is not newer");
        assert!(!is_newer("2026.07.30", "2026.08.07"));
        assert!(is_newer("1.10.0", "1.9.0"), "numeric, not lexicographic");
        assert!(is_newer("1.2.1", "1.2"));
        // Nothing installed: anything is newer. Nothing offered: never.
        assert!(is_newer("0.0.1", ""));
        assert!(!is_newer("", ""));
        assert!(!is_newer("", "1.0"));
    }

    #[test]
    fn min_app_version_gates_only_when_this_build_is_older() {
        assert!(app_version_satisfies(None));
        assert!(app_version_satisfies(Some("0.0.1")));
        assert!(app_version_satisfies(Some(env!("CARGO_PKG_VERSION"))));
        assert!(!app_version_satisfies(Some("999.0.0")));
    }

    // ── Digest ──────────────────────────────────────────────────────────

    /// Pinned against the published SHA-256 vectors — the digest is what every
    /// other guarantee in this milestone rests on.
    #[test]
    fn sha256_hex_matches_the_published_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    // ── The test fetcher ────────────────────────────────────────────────

    #[tokio::test]
    async fn the_test_fetcher_enforces_the_same_ceiling_as_the_real_one() {
        let mut m = HashMap::new();
        m.insert("u".to_string(), vec![0u8; 100]);
        let f = MapFetcher::new(m);
        assert!(f.get("u", 100).await.is_ok());
        assert!(f.get("u", 99).await.is_err());
        assert!(f.get("missing", 100).await.is_err());
    }
}
