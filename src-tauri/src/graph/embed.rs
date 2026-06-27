//! V9-01 Phase G — the embedding client. Talks to an OpenAI-compatible
//! `/v1/embeddings` endpoint (e.g. a `llama-server --embedding` on a spare GPU
//! box) to turn doc chunks and queries into vectors for semantic search.
//!
//! Kept deliberately thin: the endpoint is remote and optional, so every call
//! is fallible and the caller degrades to full-text search when it's
//! unreachable. The vector store, epoch bookkeeping, and k-NN live in `index`;
//! the backfill loop lives in `service`.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// A reachable, configured embedder. Cheap to clone (just config + a client).
#[derive(Clone)]
pub struct Embedder {
    client: reqwest::Client,
    endpoint: String,
    model: String,
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
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .ok()?;
        Some(Embedder {
            client,
            endpoint: normalize_endpoint(endpoint),
            model: model.trim().to_string(),
        })
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
        let mut body: EmbedResponse = resp
            .json()
            .await
            .map_err(|e| format!("embeddings decode failed: {e}"))?;
        // Defensive: the spec orders `data` by `index`, but don't assume it.
        body.data.sort_by_key(|d| d.index);
        if body.data.len() != texts.len() {
            return Err(format!(
                "embeddings count mismatch: asked {}, got {}",
                texts.len(),
                body.data.len()
            ));
        }
        Ok(body.data.into_iter().map(|d| d.embedding).collect())
    }

    /// Embed a single text (the query path).
    pub async fn embed_one(&self, text: &str) -> Result<Vec<f32>, String> {
        let mut v = self.embed(&[text.to_string()]).await?;
        v.pop().ok_or_else(|| "empty embedding response".to_string())
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

/// Append `/v1/embeddings` unless the user already pointed at a full path.
fn normalize_endpoint(endpoint: &str) -> String {
    let trimmed = endpoint.trim_end_matches('/');
    if trimmed.contains("/embeddings") {
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
        assert_eq!(normalize_endpoint("http://h:8081"), "http://h:8081/v1/embeddings");
        assert_eq!(normalize_endpoint("http://h:8081/"), "http://h:8081/v1/embeddings");
        assert_eq!(normalize_endpoint("http://h:8081/v1"), "http://h:8081/v1/embeddings");
        assert_eq!(
            normalize_endpoint("http://h:8081/v1/embeddings"),
            "http://h:8081/v1/embeddings"
        );
    }

    #[test]
    fn unconfigured_endpoint_is_none() {
        assert!(Embedder::new("", "m").is_none());
        assert!(Embedder::new("   ", "m").is_none());
        assert!(Embedder::new("http://x", "m").is_some());
    }
}
