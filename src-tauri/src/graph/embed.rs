//! V9-01 Phase G — the embedding client. Talks to an OpenAI-compatible
//! `/v1/embeddings` endpoint (e.g. a `llama-server --embedding` on a spare GPU
//! box) to turn doc chunks and queries into vectors for semantic search.
//!
//! Kept deliberately thin: the endpoint is remote and optional, so every call
//! is fallible and the caller degrades to full-text search when it's
//! unreachable. The vector store, epoch bookkeeping, and k-NN live in `index`;
//! the backfill loop lives in `service`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Error-message prefixes. Shared between the producers below and
/// [`is_item_level_error`] so the classification can never drift from the
/// strings actually produced (the backfill's degrade-vs-isolate decision
/// hinges on telling a dead endpoint apart from a rejected input).
const ERR_TRANSPORT: &str = "embeddings request failed";
const ERR_STATUS: &str = "embeddings endpoint returned";
const ERR_DECODE: &str = "embeddings decode failed";
const ERR_COUNT: &str = "embeddings count mismatch";

/// Tokens reserved out of the server's reported window for the BOS/EOS/special
/// tokens the server adds around our text. Small and fixed: the point is to
/// stop being *exactly* at the boundary, not to model any tokenizer.
const PROPS_TOKEN_MARGIN: usize = 16;

/// Floor for the adaptive shrink (see the backfill's per-item isolation). A
/// chunk that can't be embedded at 64 tokens isn't a sizing problem.
pub const MIN_TOKEN_LIMIT: usize = 64;

/// A reachable, configured embedder. Cheap to clone (just config + a client).
///
/// No `Debug`, derived or otherwise — it holds `auth_token`. If one is ever
/// wanted, hand-roll it and redact that field (the house pattern:
/// `settings::schema::ClaudeLocalSettings`).
#[derive(Clone)]
pub struct Embedder {
    client: reqwest::Client,
    endpoint: String,
    model: String,
    /// V33 Phase E: bearer token for the embedding endpoint, or empty for
    /// none. Set on ALL FOUR request sites — the embeddings POST plus the
    /// `/props`, `/tokenize` and `/detokenize` helpers — because they all hit
    /// the same server, and a `--api-key` llama-server gates them all.
    ///
    /// Deliberately part of the handle rather than a per-call argument: the
    /// `Embedder` is cloned and re-derived all over the backfill (adaptive
    /// shrink, per-item isolation), and a per-call token is a thing one of
    /// those paths eventually forgets to pass.
    auth_token: String,
    /// The dimension the vector store was sized to, once known. `embed`
    /// rejects any response whose vectors don't match it — so a remote
    /// `llama-server` silently restarted with a different-dimension model
    /// can't feed wrong-length vectors into a store built for the old size
    /// (which later panics or produces NaN scores in the HNSW k-NN). `None`
    /// during the initial `probe_dim` (when the dimension isn't known yet).
    expected_dim: Option<usize>,
    /// Effective per-input token budget. `Some(n)` ⇒ every text handed to
    /// `embed` is guaranteed to fit under `n` tokens (truncated if needed);
    /// `None` ⇒ no limit is known and texts go out unchanged (the pre-V31
    /// behavior, kept for non-llama servers that expose no `/props`).
    max_tokens: Option<usize>,
}

#[derive(Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    input: &'a [String],
}

#[derive(Deserialize)]
struct EmbedResponse {
    data: Vec<EmbedDatum>,
}

#[derive(Deserialize)]
struct EmbedDatum {
    embedding: Vec<f32>,
    #[serde(default)]
    index: usize,
}

#[derive(Serialize)]
struct TokenizeRequest<'a> {
    content: &'a str,
}

#[derive(Deserialize)]
struct TokenizeResponse {
    #[serde(default)]
    tokens: Vec<u32>,
}

#[derive(Serialize)]
struct DetokenizeRequest<'a> {
    tokens: &'a [u32],
}

#[derive(Deserialize)]
struct DetokenizeResponse {
    #[serde(default)]
    content: String,
}

impl Embedder {
    /// Build an embedder for `endpoint` (the base, e.g. `http://host:8081` or a
    /// full `.../v1/embeddings`) + `model` + `auth_token` (empty = no auth).
    /// Returns `None` when unconfigured.
    ///
    /// V33 Phase E: this is the single injection point for the bearer token —
    /// every one of the four request sites reads it off the handle, so a new
    /// endpoint call added later authenticates by construction.
    pub fn new(endpoint: &str, model: &str, auth_token: &str) -> Option<Embedder> {
        let endpoint = endpoint.trim();
        if endpoint.is_empty() {
            return None;
        }
        Some(Embedder {
            client: shared_client(),
            endpoint: normalize_endpoint(endpoint),
            model: model.trim().to_string(),
            auth_token: auth_token.trim().to_string(),
            expected_dim: None,
            max_tokens: None,
        })
    }

    /// Attach the bearer token when there is one. An empty token sends NO
    /// `Authorization` header — a bare `Bearer ` is worse than none, and every
    /// pre-V33 (unauthenticated) endpoint must keep working untouched.
    fn with_auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.auth_token.is_empty() {
            rb
        } else {
            rb.bearer_auth(&self.auth_token)
        }
    }

    /// Pin the dimension every subsequent `embed`/`embed_one` must return —
    /// the size the vector store was built for. Call once the store dimension
    /// is resolved (configured or probed) and before persisting any vectors.
    pub fn expect_dim(&mut self, dim: usize) {
        self.expected_dim = Some(dim);
    }

    /// The effective per-input token budget, if one is known.
    pub fn max_tokens(&self) -> Option<usize> {
        self.max_tokens
    }

    /// Set the effective budget for THIS handle only (no cache write). Used by
    /// the backfill's adaptive shrink to trial a smaller size on a clone.
    pub fn set_max_tokens(&mut self, limit: usize) {
        self.max_tokens = Some(limit.max(1));
    }

    /// Lower the effective budget for this handle **and** the process-wide
    /// cache for this endpoint. `/props` reports `n_ctx`, but a llama-server's
    /// real per-request bound for pooled embeddings can be the physical batch
    /// size (`n_ubatch`), which `/props` does not report — so `n_ctx` is only
    /// an upper estimate. When a shrunk retry succeeds we've *measured* the
    /// real bound; recording it stops every later item (and every later run in
    /// this process) from repeating the same discovery.
    ///
    /// Only ever lowers: a raise would undo a measurement with a guess.
    pub fn lower_max_tokens(&mut self, limit: usize) {
        let limit = limit.max(1);
        if self.max_tokens.is_none_or(|cur| limit < cur) {
            self.max_tokens = Some(limit);
        }
        let mut cache = detection_cache().lock().unwrap_or_else(|e| e.into_inner());
        let slot = cache.entry(self.endpoint.clone()).or_insert(limit);
        *slot = (*slot).min(limit);
    }

    /// Apply a token budget WITHOUT any network round-trip: the manual
    /// override when it's set, else whatever detection this process has
    /// already cached for this endpoint. The query paths use this — a search
    /// must stay low-latency, and it inherits the backfill's probe for free.
    pub fn apply_token_limit(&mut self, override_tokens: u32) {
        self.max_tokens = if override_tokens > 0 {
            Some(override_tokens as usize)
        } else {
            cached_max_tokens(&self.endpoint)
        };
    }

    /// Resolve the token budget, probing `/props` once per process per
    /// endpoint when nothing is cached and no override is set. Used by the
    /// backfill (the one caller that can afford a probe). Returns the
    /// resolved budget, or `None` when the server exposes no usable window
    /// (a non-llama endpoint) — in which case texts go out unchanged, exactly
    /// as before, and the manual override is the escape hatch.
    pub async fn ensure_max_tokens(&mut self, override_tokens: u32) -> Option<usize> {
        if override_tokens > 0 {
            self.max_tokens = Some(override_tokens as usize);
            return self.max_tokens;
        }
        if let Some(cached) = cached_max_tokens(&self.endpoint) {
            self.max_tokens = Some(cached);
            return self.max_tokens;
        }
        if let Some(detected) = self.detect_max_tokens().await {
            let mut cache = detection_cache().lock().unwrap_or_else(|e| e.into_inner());
            cache.insert(self.endpoint.clone(), detected);
            drop(cache);
            self.max_tokens = Some(detected);
        }
        self.max_tokens
    }

    /// GET `{base}/props` and derive a per-input token budget from the
    /// server's context window. Best-effort: any failure (not a llama-server,
    /// unreachable, unparsable) returns `None` and imposes no limit.
    ///
    /// The `default_generation_settings.n_ctx` llama-server reports is already
    /// PER SLOT — do not divide by `total_slots` (confirmed project knowledge;
    /// the offload prober's extra division exists only for `--kv-unified`).
    pub async fn detect_max_tokens(&self) -> Option<usize> {
        let base = server_base(&self.endpoint)?;
        // Short per-request timeout (the shared client's is 30s): detection is
        // best-effort and runs BEFORE the dimension probe, so an unreachable
        // host must not stack two 30s stalls onto a degrading backfill.
        let resp = self
            .with_auth(self.client.get(format!("{base}/props")))
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let body: serde_json::Value = resp.json().await.ok()?;
        let n_ctx = props_n_ctx(&body)?;
        let limit = (n_ctx as usize).checked_sub(PROPS_TOKEN_MARGIN)?;
        (limit > 0).then_some(limit)
    }

    /// Rewrite `texts` so each one provably fits the effective budget.
    /// `None` ⇒ nothing to do (no budget known, or every text already fits by
    /// the byte-length fast path), so the caller sends the originals unchanged
    /// and this whole feature costs one comparison per item.
    async fn fit_inputs(&self, texts: &[String]) -> Option<Vec<String>> {
        let limit = self.max_tokens?;
        // A BPE token is at least one byte, so `bytes <= limit` ⇒
        // `tokens <= limit`. This covers virtually every chunk with no I/O.
        if texts.iter().all(|t| t.len() <= limit) {
            return None;
        }
        let mut out = Vec::with_capacity(texts.len());
        for t in texts {
            if t.len() <= limit {
                out.push(t.clone());
            } else {
                out.push(self.fit_one(t, limit).await);
            }
        }
        Some(out)
    }

    /// Truncate one oversized text to `limit` tokens. Prefers the server's own
    /// tokenizer (`/tokenize` + `/detokenize`, exact); falls back to a
    /// char-boundary byte truncation, which over-truncates prose but can never
    /// exceed the budget.
    ///
    /// The head is kept and the tail dropped: the vector store maps one chunk
    /// to exactly one vector, so splitting is not an option here.
    async fn fit_one(&self, text: &str, limit: usize) -> String {
        match self.tokenize(text).await {
            Ok(tokens) if tokens.len() <= limit => text.to_string(),
            Ok(tokens) => match self.detokenize(&tokens[..limit]).await {
                Ok(s) if !s.is_empty() => s,
                _ => truncate_bytes(text, limit).to_string(),
            },
            Err(_) => truncate_bytes(text, limit).to_string(),
        }
    }

    async fn tokenize(&self, text: &str) -> Result<Vec<u32>, String> {
        let base = server_base(&self.endpoint).ok_or("no server base")?;
        let resp = self
            .with_auth(self.client.post(format!("{base}/tokenize")))
            .json(&TokenizeRequest { content: text })
            .send()
            .await
            .map_err(|e| format!("tokenize request failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("tokenize returned {}", resp.status()));
        }
        let body: TokenizeResponse = resp
            .json()
            .await
            .map_err(|e| format!("tokenize decode failed: {e}"))?;
        if body.tokens.is_empty() {
            return Err("tokenize returned no tokens".to_string());
        }
        Ok(body.tokens)
    }

    async fn detokenize(&self, tokens: &[u32]) -> Result<String, String> {
        let base = server_base(&self.endpoint).ok_or("no server base")?;
        let resp = self
            .with_auth(self.client.post(format!("{base}/detokenize")))
            .json(&DetokenizeRequest { tokens })
            .send()
            .await
            .map_err(|e| format!("detokenize request failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("detokenize returned {}", resp.status()));
        }
        let body: DetokenizeResponse = resp
            .json()
            .await
            .map_err(|e| format!("detokenize decode failed: {e}"))?;
        Ok(body.content)
    }

    /// Embed a batch of texts, preserving order. `Err` on any transport/decode
    /// failure so the caller can mark the embedder down and fall back.
    pub async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        // Guarantee every input fits the server's window before sending: one
        // oversized chunk otherwise fails the WHOLE batch with a non-2xx, and
        // the same chunk is re-selected on the next backfill pass forever.
        let fitted = self.fit_inputs(texts).await;
        let texts: &[String] = fitted.as_deref().unwrap_or(texts);
        let req = EmbedRequest {
            model: &self.model,
            input: texts,
        };
        let resp = self
            .with_auth(self.client.post(&self.endpoint))
            .json(&req)
            .send()
            .await
            .map_err(|e| format!("{ERR_TRANSPORT}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("{ERR_STATUS} {}", resp.status()));
        }
        let body: EmbedResponse = resp
            .json()
            .await
            .map_err(|e| format!("{ERR_DECODE}: {e}"))?;
        if body.data.len() != texts.len() {
            return Err(format!(
                "{ERR_COUNT}: asked {}, got {}",
                texts.len(),
                body.data.len()
            ));
        }
        // Every vector must share one non-zero dimension. A misbehaving server
        // that returns mixed-length vectors would otherwise be stored as-is and
        // later panic (or silently produce NaN scores) when the HNSW index
        // computes a distance between mismatched dimensions.
        let dim = body.data.first().map_or(0, |d| d.embedding.len());
        if dim == 0 {
            // Deliberately NOT phrased "…endpoint returned …": that prefix is
            // reserved for HTTP-status rejections, which `is_item_level_error`
            // treats as retry-per-item. A zero-length vector is a broken model,
            // so it must classify as degrade-and-stop.
            return Err("embeddings response had zero-length vectors".to_string());
        }
        if let Some(bad) = body.data.iter().find(|d| d.embedding.len() != dim) {
            return Err(format!(
                "embeddings dimension mismatch: expected {dim}, got {}",
                bad.embedding.len()
            ));
        }
        // Guard against the store's dimension too: a server restarted with a
        // different-dimension model returns internally-consistent vectors that
        // would still corrupt a store built for the old size.
        if let Some(want) = self.expected_dim {
            if dim != want {
                return Err(format!(
                    "embeddings dimension changed: store expects {want}, endpoint returned {dim} \
                     (embedding model likely changed — rebuild the vector store)"
                ));
            }
        }
        Ok(ordered_embeddings(body.data))
    }

    /// Embed a single text (the query path).
    pub async fn embed_one(&self, text: &str) -> Result<Vec<f32>, String> {
        let mut v = self.embed(&[text.to_string()]).await?;
        v.pop()
            .ok_or_else(|| "empty embedding response".to_string())
    }

    /// Probe reachability + the live vector dimension (one tiny embed). Used to
    /// auto-size the vector store when `embedding_dims` is 0.
    pub async fn probe_dim(&self) -> Result<usize, String> {
        let v = self.embed_one("probe").await?;
        if v.is_empty() {
            Err("embedder returned a zero-length vector".to_string())
        } else {
            Ok(v.len())
        }
    }
}

/// Put embeddings back in input order. Reorders by `index` ONLY when the
/// server returned a complete, unique set (`0..n`). Some OpenAI-compatible
/// servers omit `index`, which serde fills as 0 for every item; a stable sort
/// on all-zero keys is a silent no-op that would mis-pair vectors with inputs
/// if the server also batched out of order. When the indices aren't a valid
/// permutation we trust the response order (the spec returns data in input
/// order) rather than a meaningless sort.
fn ordered_embeddings(mut data: Vec<EmbedDatum>) -> Vec<Vec<f32>> {
    let n = data.len();
    let valid_indices = {
        let mut seen = vec![false; n];
        data.iter()
            .all(|d| d.index < n && !std::mem::replace(&mut seen[d.index], true))
    };
    if valid_indices {
        data.sort_by_key(|d| d.index);
    }
    data.into_iter().map(|d| d.embedding).collect()
}

/// One process-wide `reqwest::Client` (its connection pool is the whole point
/// of reuse). Building a fresh client per `Embedder::new` — which the backfill
/// does after every watcher batch — spins up a new pool each time and, under
/// heavy churn on Windows, can exhaust ephemeral ports before old pools drain.
/// `Client` is internally `Arc`, so cloning the shared one is cheap.
fn shared_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            // Don't `unwrap_or_default()`: the default client has *no* timeout,
            // so on the rare `build()` failure (a broken TLS backend) every
            // embed call would hang forever instead of degrading. A build
            // failure means the environment is broken — surface it loudly.
            reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("failed to build embedding HTTP client")
        })
        .clone()
}

/// Classify an [`Embedder::embed`] error.
///
/// `true` — the server answered and rejected *this input* (HTTP status, a
/// malformed body, a short response). The caller may retry the items one at a
/// time and skip the individual poison item; the endpoint itself is fine.
///
/// `false` — transport failure (endpoint gone) or a contract violation
/// (dimension changed / zero-length vectors, i.e. the model behind the
/// endpoint was swapped). Retrying per item would fail identically for every
/// item and mis-report a global outage as "N chunks skipped": degrade instead.
pub fn is_item_level_error(err: &str) -> bool {
    err.starts_with(ERR_STATUS) || err.starts_with(ERR_DECODE) || err.starts_with(ERR_COUNT)
}

/// Process-wide `/props` detection results, keyed by NORMALIZED endpoint. The
/// backfill populates it (it can afford a probe); the query paths read it, so
/// a search inherits the limit for free without adding a round-trip.
fn detection_cache() -> &'static Mutex<HashMap<String, usize>> {
    static CACHE: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Read the cached detection for a normalized endpoint, if any.
fn cached_max_tokens(endpoint: &str) -> Option<usize> {
    detection_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(endpoint)
        .copied()
}

/// Derive the llama-server base URL (the parent of `/v1/embeddings`) from a
/// NORMALIZED endpoint, so `/props`, `/tokenize` and `/detokenize` can be
/// reached. `None` when the endpoint isn't shaped like one we produced — a
/// custom path such as `http://h/embeddings/v2` has no derivable base, and
/// guessing one would POST project text at an unrelated URL.
fn server_base(endpoint: &str) -> Option<String> {
    let base = endpoint
        .strip_suffix("/v1/embeddings")
        .or_else(|| endpoint.strip_suffix("/embeddings"))?;
    // Must still be an absolute URL with something after the scheme.
    let after_scheme = base.split_once("://")?.1;
    (!after_scheme.is_empty()).then(|| base.to_string())
}

/// Pull `n_ctx` out of a llama-server `/props` body. Mirrors the offload
/// prober's dual-location parse (`src-tauri/src/offload/mcp.rs`): current
/// builds nest it under `default_generation_settings`, older ones expose it at
/// the top level.
fn props_n_ctx(v: &serde_json::Value) -> Option<u64> {
    v.get("default_generation_settings")
        .and_then(|g| g.get("n_ctx"))
        .and_then(|x| x.as_u64())
        .or_else(|| v.get("n_ctx").and_then(|x| x.as_u64()))
}

/// Truncate to at most `max_bytes`, backing up to the nearest char boundary.
/// Because a BPE token is ≥1 byte, the result is also ≤ `max_bytes` TOKENS —
/// which is what makes this a safe fallback when the server's tokenizer
/// endpoints aren't available.
fn truncate_bytes(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Append `/v1/embeddings` unless the user already pointed at a full path.
fn normalize_endpoint(endpoint: &str) -> String {
    let trimmed = endpoint.trim_end_matches('/');
    // Match the `/embeddings` path SEGMENT, not a substring — so a host path
    // like `.../embeddingsvc` isn't mistaken for a complete endpoint (which
    // would skip appending `/v1/embeddings` and POST to the wrong path).
    if trimmed.ends_with("/embeddings") || trimmed.contains("/embeddings/") {
        trimmed.to_string()
    } else if trimmed.ends_with("/v1") {
        format!("{trimmed}/embeddings")
    } else {
        format!("{trimmed}/v1/embeddings")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_normalization() {
        assert_eq!(
            normalize_endpoint("http://h:8081"),
            "http://h:8081/v1/embeddings"
        );
        assert_eq!(
            normalize_endpoint("http://h:8081/"),
            "http://h:8081/v1/embeddings"
        );
        assert_eq!(
            normalize_endpoint("http://h:8081/v1"),
            "http://h:8081/v1/embeddings"
        );
        assert_eq!(
            normalize_endpoint("http://h:8081/v1/embeddings"),
            "http://h:8081/v1/embeddings"
        );
        // A path that merely *contains* "embeddings" is not a complete endpoint:
        // the `/v1/embeddings` suffix must still be appended.
        assert_eq!(
            normalize_endpoint("http://h:8081/embeddingsvc"),
            "http://h:8081/embeddingsvc/v1/embeddings"
        );
    }

    #[test]
    fn unconfigured_endpoint_is_none() {
        assert!(Embedder::new("", "m", "").is_none());
        assert!(Embedder::new("   ", "m", "").is_none());
        assert!(Embedder::new("http://x", "m", "").is_some());
    }

    /// V33 Phase E: the token is optional and its ABSENCE must send no header
    /// at all — an `Authorization: Bearer ` with an empty value is worse than
    /// none, and every existing unauthenticated endpoint depends on this.
    #[test]
    fn an_absent_or_blank_embedding_token_sends_no_authorization_header() {
        let client = reqwest::Client::new();
        let header_of = |token: &str| -> Option<String> {
            let e = Embedder::new("http://auth-test.invalid", "m", token).expect("configured");
            e.with_auth(client.get("http://auth-test.invalid"))
                .build()
                .expect("a buildable request")
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .map(|v| v.to_str().expect("ascii").to_string())
        };
        assert_eq!(header_of(""), None, "no token ⇒ no header");
        assert_eq!(header_of("   "), None, "a blank token is not a token");
        assert_eq!(header_of("sk-x"), Some("Bearer sk-x".to_string()));
    }

    fn datum(index: usize, tag: f32) -> EmbedDatum {
        EmbedDatum {
            embedding: vec![tag],
            index,
        }
    }

    #[test]
    fn server_base_derives_from_normalized_endpoints() {
        // Every shape `normalize_endpoint` can produce must round-trip back to
        // the base the /props, /tokenize and /detokenize routes hang off.
        assert_eq!(
            server_base(&normalize_endpoint("http://h:8081")).as_deref(),
            Some("http://h:8081")
        );
        assert_eq!(
            server_base(&normalize_endpoint("http://h:8081/v1")).as_deref(),
            Some("http://h:8081")
        );
        // The custom-path case: the base keeps the mount prefix.
        assert_eq!(
            server_base(&normalize_endpoint("http://h:8081/embeddingsvc")).as_deref(),
            Some("http://h:8081/embeddingsvc")
        );
        // A user-supplied full path that ends in `/embeddings` (no `/v1`).
        assert_eq!(
            server_base("http://h:8081/custom/embeddings").as_deref(),
            Some("http://h:8081/custom")
        );
        // Not derivable: nothing to strip, or no host left after stripping.
        assert_eq!(server_base("http://h:8081/embeddings/v2"), None);
        assert_eq!(server_base("http://h:8081/v1/chat"), None);
        assert_eq!(server_base("http:///embeddings"), None);
    }

    #[test]
    fn props_n_ctx_reads_both_locations_and_rejects_garbage() {
        let nested = serde_json::json!({
            "default_generation_settings": { "n_ctx": 7192, "n_predict": -1 },
            "total_slots": 2
        });
        assert_eq!(props_n_ctx(&nested), Some(7192));

        // Older builds expose it at the top level only.
        let top = serde_json::json!({ "n_ctx": 4096 });
        assert_eq!(props_n_ctx(&top), Some(4096));

        // Nested wins when both are present (it's the authoritative per-slot one).
        let both = serde_json::json!({
            "default_generation_settings": { "n_ctx": 2048 },
            "n_ctx": 8192
        });
        assert_eq!(props_n_ctx(&both), Some(2048));

        // Garbage / wrong types / absent → no detection (no limit imposed).
        assert_eq!(props_n_ctx(&serde_json::json!({})), None);
        assert_eq!(props_n_ctx(&serde_json::json!({ "n_ctx": "lots" })), None);
        assert_eq!(
            props_n_ctx(&serde_json::json!({ "default_generation_settings": 5 })),
            None
        );
        assert_eq!(props_n_ctx(&serde_json::json!("not an object")), None);
    }

    #[test]
    fn byte_length_fast_path_is_a_real_token_guarantee() {
        // The fast path's whole claim: a BPE token is at least one byte, so a
        // text whose BYTE length is within budget can never exceed the budget
        // in tokens. Encode that as the invariant `chars <= bytes` (a token is
        // at least one char, and a char is at least one byte).
        for s in ["", "hello world", "héllo wörld", "日本語のテキスト", "🎉🎉🎉"] {
            assert!(
                s.chars().count() <= s.len(),
                "char count must never exceed byte length for {s:?}"
            );
        }
    }

    #[test]
    fn byte_truncation_lands_on_a_char_boundary() {
        // 3 bytes per char: cutting at 5 must back up to 3, not split a char.
        let s = "日本語";
        assert_eq!(s.len(), 9);
        let cut = truncate_bytes(s, 5);
        assert_eq!(cut, "日");
        assert!(cut.len() <= 5);

        // 4-byte chars (emoji) — cutting mid-sequence backs up to 0.
        let e = "🎉🎉";
        assert_eq!(truncate_bytes(e, 3), "");
        assert_eq!(truncate_bytes(e, 4), "🎉");
        assert_eq!(truncate_bytes(e, 7), "🎉");
        assert_eq!(truncate_bytes(e, 8), "🎉🎉");

        // Within budget → untouched (no allocation, no boundary walk).
        assert_eq!(truncate_bytes("abc", 10), "abc");
        assert_eq!(truncate_bytes("abc", 3), "abc");
        assert_eq!(truncate_bytes("", 0), "");
    }

    #[test]
    fn error_classification_splits_isolate_from_degrade() {
        // Produced by `embed` — these mean "the server rejected this input".
        assert!(is_item_level_error(&format!("{ERR_STATUS} 500 Internal Server Error")));
        assert!(is_item_level_error(&format!("{ERR_DECODE}: expected value")));
        assert!(is_item_level_error(&format!("{ERR_COUNT}: asked 4, got 3")));

        // Transport + contract violations must degrade the whole run instead.
        assert!(!is_item_level_error(&format!(
            "{ERR_TRANSPORT}: connection refused"
        )));
        assert!(!is_item_level_error(
            "embeddings dimension changed: store expects 2560, endpoint returned 768"
        ));
        assert!(!is_item_level_error("embeddings dimension mismatch: expected 4, got 8"));
        assert!(!is_item_level_error(
            "embeddings response had zero-length vectors"
        ));
        // Regression guard for the collision this test originally caught: the
        // zero-length-vector message must NOT be phrased with the HTTP-status
        // prefix, or a broken model would be misread as a per-item rejection
        // and every chunk would land in the skip set instead of degrading.
        assert!(!"embeddings response had zero-length vectors".starts_with(ERR_STATUS));
    }

    #[test]
    fn token_limit_helpers_only_ever_lower() {
        let mut e = Embedder::new("http://limit-test.invalid", "m", "").unwrap();
        assert_eq!(e.max_tokens(), None);

        // Manual override wins and needs no probe.
        e.apply_token_limit(1024);
        assert_eq!(e.max_tokens(), Some(1024));

        // A measured smaller bound lowers it (and seeds the process cache).
        e.lower_max_tokens(256);
        assert_eq!(e.max_tokens(), Some(256));
        // A larger "measurement" can't undo it.
        e.lower_max_tokens(4096);
        assert_eq!(e.max_tokens(), Some(256));

        // A fresh handle on the same endpoint with NO override inherits the
        // cached bound without touching the network.
        let mut fresh = Embedder::new("http://limit-test.invalid", "m", "").unwrap();
        fresh.apply_token_limit(0);
        assert_eq!(fresh.max_tokens(), Some(256));

        // …but an explicit override still beats the cache.
        fresh.apply_token_limit(2048);
        assert_eq!(fresh.max_tokens(), Some(2048));
    }

    #[tokio::test]
    async fn fit_inputs_is_a_no_op_without_a_limit_or_within_budget() {
        let mut e = Embedder::new("http://fit-test.invalid", "m", "").unwrap();
        let texts = vec!["short".to_string(), "also short".to_string()];
        // No limit known → nothing rewritten (pre-V31 behavior preserved).
        assert!(e.fit_inputs(&texts).await.is_none());
        // Limit known and every text fits by byte length → still no I/O.
        e.apply_token_limit(64);
        assert!(e.fit_inputs(&texts).await.is_none());
    }

    #[tokio::test]
    async fn fit_inputs_falls_back_to_byte_truncation_when_tokenize_is_unreachable() {
        // No server behind this endpoint: /tokenize fails, so the guaranteed
        // byte-truncation fallback must still bring the text under budget.
        let mut e = Embedder::new("http://127.0.0.1:1/fit", "m", "").unwrap();
        e.apply_token_limit(8);
        let texts = vec!["x".repeat(64), "fits".to_string()];
        let out = e.fit_inputs(&texts).await.expect("oversized input rewritten");
        assert_eq!(out.len(), 2);
        assert!(out[0].len() <= 8, "{:?} must fit the 8-byte budget", out[0]);
        assert_eq!(out[1], "fits", "in-budget items pass through untouched");
    }

    #[test]
    fn ordered_embeddings_handles_index_present_absent_and_invalid() {
        // Valid permutation (server returned out of order): reorder by index.
        let got = ordered_embeddings(vec![datum(1, 1.0), datum(0, 0.0), datum(2, 2.0)]);
        assert_eq!(got, vec![vec![0.0], vec![1.0], vec![2.0]]);

        // `index` omitted → all default to 0 (not a 0..n permutation): trust
        // the response order rather than collapsing everything onto key 0.
        let got = ordered_embeddings(vec![datum(0, 10.0), datum(0, 11.0), datum(0, 12.0)]);
        assert_eq!(got, vec![vec![10.0], vec![11.0], vec![12.0]]);

        // Single element is trivially a valid permutation.
        assert_eq!(ordered_embeddings(vec![datum(0, 5.0)]), vec![vec![5.0]]);
    }
}
