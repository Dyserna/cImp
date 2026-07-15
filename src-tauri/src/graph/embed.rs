//! V9-01 Phase G — the embedding client. Talks to an OpenAI-compatible
//! `/v1/embeddings` endpoint (e.g. a `llama-server --embedding` on a spare GPU
//! box) to turn doc chunks and queries into vectors for semantic search.
//!
//! Kept deliberately thin: the endpoint is remote and optional, so every call
//! is fallible and the caller degrades to full-text search when it's
//! unreachable. The vector store, epoch bookkeeping, and k-NN live in `index`;
//! the backfill loop lives in `service`.

use std::sync::OnceLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// A reachable, configured embedder. Cheap to clone (just config + a client).
#[derive(Clone)]
pub struct Embedder {
    client: reqwest::Client,
    endpoint: String,
    model: String,
    /// The dimension the vector store was sized to, once known. `embed`
    /// rejects any response whose vectors don't match it — so a remote
    /// `llama-server` silently restarted with a different-dimension model
    /// can't feed wrong-length vectors into a store built for the old size
    /// (which later panics or produces NaN scores in the HNSW k-NN). `None`
    /// during the initial `probe_dim` (when the dimension isn't known yet).
    expected_dim: Option<usize>,
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

impl Embedder {
    /// Build an embedder for `endpoint` (the base, e.g. `http://host:8081` or a
    /// full `.../v1/embeddings`) + `model`. Returns `None` when unconfigured.
    pub fn new(endpoint: &str, model: &str) -> Option<Embedder> {
        let endpoint = endpoint.trim();
        if endpoint.is_empty() {
            return None;
        }
        Some(Embedder {
            client: shared_client(),
            endpoint: normalize_endpoint(endpoint),
            model: model.trim().to_string(),
            expected_dim: None,
        })
    }

    /// Pin the dimension every subsequent `embed`/`embed_one` must return —
    /// the size the vector store was built for. Call once the store dimension
    /// is resolved (configured or probed) and before persisting any vectors.
    pub fn expect_dim(&mut self, dim: usize) {
        self.expected_dim = Some(dim);
    }

    /// Embed a batch of texts, preserving order. `Err` on any transport/decode
    /// failure so the caller can mark the embedder down and fall back.
    pub async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let req = EmbedRequest {
            model: &self.model,
            input: texts,
        };
        let resp = self
            .client
            .post(&self.endpoint)
            .json(&req)
            .send()
            .await
            .map_err(|e| format!("embeddings request failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("embeddings endpoint returned {}", resp.status()));
        }
        let body: EmbedResponse = resp
            .json()
            .await
            .map_err(|e| format!("embeddings decode failed: {e}"))?;
        if body.data.len() != texts.len() {
            return Err(format!(
                "embeddings count mismatch: asked {}, got {}",
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
            return Err("embeddings endpoint returned zero-length vectors".to_string());
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
        assert!(Embedder::new("", "m").is_none());
        assert!(Embedder::new("   ", "m").is_none());
        assert!(Embedder::new("http://x", "m").is_some());
    }

    fn datum(index: usize, tag: f32) -> EmbedDatum {
        EmbedDatum {
            embedding: vec![tag],
            index,
        }
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
