//! V8-03 Offload Server metrics — a structured, live view of the local
//! `llama-server` for the read-only Offload Server tab's dashboard.
//!
//! Rather than parse the verbose server log, we poll the server's HTTP
//! endpoints (robust, structured):
//! - `GET /slots` — per-slot `is_processing` + `n_decoded` (tokens generated)
//!   + `n_ctx`. Always available. We **compute tokens/sec ourselves** from the
//!   `n_decoded` delta between polls, and derive request **history** from
//!   slots flipping busy→idle.
//! - `GET /metrics` — Prometheus gauges (only when the server was launched
//!   with `--metrics`): true context fill (`kv_cache_usage_ratio`), the
//!   server-side queue (`requests_deferred`), busy count
//!   (`requests_processing`), and server-computed throughput
//!   (`predicted_tokens_seconds`). Absent fields degrade to `None`.
//!
//! [`MetricsPoller`] holds the per-slot tracking + capped history across
//! polls; [`OffloadService`](super::OffloadService) drives it and emits each
//! [`ServerMetrics`] snapshot to the frontend.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

/// Cap on the retained request history (newest first).
const HISTORY_CAP: usize = 50;

/// One per-slot live row.
#[derive(Clone, Debug, Serialize)]
pub struct SlotMetric {
    pub id: u32,
    pub processing: bool,
    /// Tokens generated so far in the current (or last) request.
    pub n_decoded: u32,
    /// Per-slot context window (tokens).
    pub n_ctx: u32,
    /// Tokens/sec, computed from the `n_decoded` delta between polls. `None`
    /// when idle or just started (no delta yet).
    pub tps: Option<f32>,
}

/// One completed request, for the history view.
#[derive(Clone, Debug, Serialize)]
pub struct RequestRecord {
    pub slot: u32,
    /// Wall-clock start/end as epoch millis (formatted on the frontend).
    pub start_ms: u64,
    pub end_ms: u64,
    pub duration_s: f32,
    pub tokens: u32,
    pub avg_tps: f32,
}

/// A full dashboard snapshot.
#[derive(Clone, Debug, Serialize)]
pub struct ServerMetrics {
    /// A ready llama-server is being polled. When false the dashboard shows
    /// "not running" and the other fields are placeholders.
    pub running: bool,
    pub total_slots: u32,
    pub n_ctx_per_slot: Option<u32>,
    /// Slots currently processing (from `/metrics` when present, else counted
    /// from `/slots`).
    pub busy_slots: u32,
    pub slots: Vec<SlotMetric>,
    /// True context fill 0–100 (`/metrics` `kv_cache_usage_ratio`). `None`
    /// when `/metrics` is off *or* the server build doesn't expose that gauge
    /// (many don't) — distinguish via `metrics_available`.
    pub kv_cache_pct: Option<f32>,
    /// Server-computed generation throughput (`/metrics`); `None` without it.
    pub predicted_tps: Option<f32>,
    /// Server-computed prompt/prefill throughput (`/metrics`
    /// `prompt_tokens_seconds`); `None` without it.
    pub prompt_tps: Option<f32>,
    /// Server-side queued requests (`/metrics` `requests_deferred`); `None`
    /// without `--metrics`.
    pub requests_deferred: Option<u32>,
    /// Sum of per-slot computed tokens/sec — an always-available throughput
    /// figure even when `--metrics` is off.
    pub aggregate_tps: f32,
    /// ccImp's global concurrency gate: offloads holding a permit / the cap.
    pub global_in_flight: u32,
    pub global_cap: u32,
    /// Whether `/metrics` answered (so the UI can hint to add `--metrics`).
    pub metrics_available: bool,
    /// Completed requests, newest first.
    pub history: Vec<RequestRecord>,
}

/// One backend's dashboard card for the Offload Server tab. Wraps the live
/// [`ServerMetrics`] with the backend's identity and a coarse lifecycle
/// `state` so the frontend can group rows (Local vs Remote) and render an
/// accurate header even when the backend isn't being polled (stopped, cloud,
/// unreachable).
#[derive(Clone, Debug, Serialize)]
pub struct BackendDashboard {
    pub name: String,
    /// `"local"` | `"lan"` | `"cloud"` — drives grouping + the kind badge.
    pub kind: String,
    /// `"ready"` | `"stopped"` | `"starting"` | `"unreachable"` | `"blocked"`
    /// | `"disabled"`. Mirrors the Settings status vocabulary.
    pub state: String,
    pub metrics: ServerMetrics,
}

impl ServerMetrics {
    /// A "server not running" snapshot (offload disabled / backend stopped).
    pub fn offline(global_in_flight: u32, global_cap: u32) -> Self {
        Self {
            running: false,
            total_slots: 0,
            n_ctx_per_slot: None,
            busy_slots: 0,
            slots: Vec::new(),
            kv_cache_pct: None,
            predicted_tps: None,
            prompt_tps: None,
            requests_deferred: None,
            aggregate_tps: 0.0,
            global_in_flight,
            global_cap,
            metrics_available: false,
            history: Vec::new(),
        }
    }

    /// A reachable-but-not-polled snapshot for a backend whose `/slots` we
    /// don't poll (a cloud endpoint). `running` stays false — there's no live
    /// per-slot dashboard — but it carries the context/slot headline so the
    /// card header reads like a Settings status line.
    pub fn status_only(
        total_slots: u32,
        n_ctx: Option<u32>,
        global_in_flight: u32,
        global_cap: u32,
    ) -> Self {
        Self {
            total_slots,
            n_ctx_per_slot: n_ctx,
            ..Self::offline(global_in_flight, global_cap)
        }
    }
}

/// Per-slot tracking carried across polls (for tps + history).
struct SlotTrack {
    was_processing: bool,
    last_decoded: u32,
    last_instant: Instant,
    start_ms: u64,
    last_task: i64,
}

/// Stateful poller: one per Offload Server dashboard. Reused across ticks so
/// tokens/sec and request history accumulate.
pub struct MetricsPoller {
    client: reqwest::Client,
    tracks: HashMap<u32, SlotTrack>,
    history: VecDeque<RequestRecord>,
}

impl MetricsPoller {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(4))
            .build()
            .unwrap_or_default();
        Self {
            client,
            tracks: HashMap::new(),
            history: VecDeque::new(),
        }
    }

    /// Poll the server once and fold the result into a snapshot. `base_url`
    /// is the server origin (no trailing slash); `total_slots`/`n_ctx` come
    /// from the live backend handle; the global gate figures come from the
    /// service. `auth` is the bearer token for a remote endpoint that needs
    /// one (LAN llama-server usually doesn't; cloud APIs do).
    pub async fn poll(
        &mut self,
        base_url: &str,
        auth: Option<&str>,
        total_slots: u32,
        n_ctx: Option<u32>,
        global_in_flight: u32,
        global_cap: u32,
    ) -> ServerMetrics {
        let now = now_ms();
        let now_inst = Instant::now();

        // /slots — the always-available per-slot source.
        let slots_json = self.get_json(base_url, "/slots", auth).await;
        let mut slots: Vec<SlotMetric> = Vec::new();
        let mut aggregate_tps = 0.0f32;
        let mut busy = 0u32;

        if let Some(arr) = slots_json.as_ref().and_then(|v| v.as_array()) {
            for s in arr {
                let id = s.get("id").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
                let processing = s
                    .get("is_processing")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false);
                let slot_n_ctx = s
                    .get("n_ctx")
                    .and_then(|x| x.as_u64())
                    .map(|n| n as u32)
                    .or(n_ctx)
                    .unwrap_or(0);
                let n_decoded = slot_decoded(s);
                let task = s.get("id_task").and_then(|x| x.as_i64()).unwrap_or(-1);

                if processing {
                    busy += 1;
                }
                let tps = self.fold_slot(id, processing, n_decoded, task, now, now_inst);
                if let Some(t) = tps {
                    aggregate_tps += t;
                }
                slots.push(SlotMetric {
                    id,
                    processing,
                    n_decoded,
                    n_ctx: slot_n_ctx,
                    tps,
                });
            }
        }
        slots.sort_by_key(|s| s.id);

        // /metrics — richer, only with --metrics.
        let metrics_text = self.get_text(base_url, "/metrics", auth).await;
        let metrics_available = metrics_text.is_some();
        let parsed = parse_metrics(metrics_text.as_deref());

        ServerMetrics {
            running: true,
            total_slots,
            n_ctx_per_slot: n_ctx,
            busy_slots: parsed.requests_processing.unwrap_or(busy),
            slots,
            kv_cache_pct: parsed.kv_cache_pct,
            predicted_tps: parsed.predicted_tps,
            prompt_tps: parsed.prompt_tps,
            requests_deferred: parsed.requests_deferred,
            aggregate_tps,
            global_in_flight,
            global_cap,
            metrics_available,
            history: self.history.iter().cloned().collect(),
        }
    }

    /// Update one slot's tracking; returns the computed tokens/sec (if any)
    /// and records a history entry on a busy→idle (or task-change) transition.
    fn fold_slot(
        &mut self,
        id: u32,
        processing: bool,
        n_decoded: u32,
        task: i64,
        now_ms: u64,
        now_inst: Instant,
    ) -> Option<f32> {
        let track = self.tracks.entry(id).or_insert_with(|| SlotTrack {
            was_processing: false,
            last_decoded: n_decoded,
            last_instant: now_inst,
            start_ms: now_ms,
            last_task: task,
        });

        let mut tps = None;

        // New request starting (idle→busy, or a new task id mid-stream).
        let new_request = processing && (!track.was_processing || task != track.last_task);
        if new_request {
            track.start_ms = now_ms;
            track.last_decoded = n_decoded;
            track.last_instant = now_inst;
        } else if processing {
            let dt = now_inst.duration_since(track.last_instant).as_secs_f32();
            if dt > 0.05 && n_decoded >= track.last_decoded {
                tps = Some((n_decoded - track.last_decoded) as f32 / dt);
            }
        }

        // Request finished (busy→idle): record history. `n_decoded` still
        // holds the final count at this tick.
        if !processing && track.was_processing {
            let duration_s = (now_ms.saturating_sub(track.start_ms)) as f32 / 1000.0;
            let avg_tps = if duration_s > 0.0 {
                n_decoded as f32 / duration_s
            } else {
                0.0
            };
            self.history.push_front(RequestRecord {
                slot: id,
                start_ms: track.start_ms,
                end_ms: now_ms,
                duration_s,
                tokens: n_decoded,
                avg_tps,
            });
            while self.history.len() > HISTORY_CAP {
                self.history.pop_back();
            }
        }

        if processing {
            track.last_decoded = n_decoded;
            track.last_instant = now_inst;
        }
        track.last_task = task;
        track.was_processing = processing;
        tps
    }

    async fn get_json(&self, base_url: &str, path: &str, auth: Option<&str>) -> Option<Value> {
        let req = self.client.get(format!("{base_url}{path}"));
        let req = match auth {
            Some(t) => req.bearer_auth(t),
            None => req,
        };
        let resp = req.send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.json::<Value>().await.ok()
    }

    async fn get_text(&self, base_url: &str, path: &str, auth: Option<&str>) -> Option<String> {
        let req = self.client.get(format!("{base_url}{path}"));
        let req = match auth {
            Some(t) => req.bearer_auth(t),
            None => req,
        };
        let resp = req.send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.text().await.ok()
    }
}

/// Pull `n_decoded` from a `/slots` entry. Recent llama.cpp nests it under
/// `next_token[0].n_decoded`; older/other builds expose it top-level.
fn slot_decoded(s: &Value) -> u32 {
    s.get("next_token")
        .and_then(|n| n.get(0))
        .and_then(|n| n.get("n_decoded"))
        .and_then(|x| x.as_u64())
        .or_else(|| s.get("n_decoded").and_then(|x| x.as_u64()))
        .unwrap_or(0) as u32
}

/// The gauges we surface from `/metrics`. All optional — `--metrics` may be
/// off, and not every llama.cpp build exposes every gauge (notably
/// `kv_cache_usage_ratio` is absent in several builds).
#[derive(Default)]
struct ParsedMetrics {
    kv_cache_pct: Option<f32>,
    predicted_tps: Option<f32>,
    prompt_tps: Option<f32>,
    requests_deferred: Option<u32>,
    requests_processing: Option<u32>,
}

/// Parse the Prometheus `/metrics` text for the gauges we surface.
fn parse_metrics(text: Option<&str>) -> ParsedMetrics {
    let mut out = ParsedMetrics::default();
    let Some(text) = text else {
        return out;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // `llamacpp:metric{labels} value` — key is up to the first space or `{`.
        let mut parts = line.split_whitespace();
        let Some(raw_key) = parts.next() else { continue };
        let key = raw_key.split('{').next().unwrap_or(raw_key);
        let Some(val) = parts.last().and_then(|v| v.parse::<f64>().ok()) else {
            continue;
        };
        match key {
            "llamacpp:kv_cache_usage_ratio" => out.kv_cache_pct = Some((val * 100.0) as f32),
            "llamacpp:predicted_tokens_seconds" => out.predicted_tps = Some(val as f32),
            "llamacpp:prompt_tokens_seconds" => out.prompt_tps = Some(val as f32),
            "llamacpp:requests_deferred" => out.requests_deferred = Some(val.max(0.0) as u32),
            "llamacpp:requests_processing" => out.requests_processing = Some(val.max(0.0) as u32),
            _ => {}
        }
    }
    out
}

/// Current wall-clock as epoch millis (formatted on the frontend).
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn slot_decoded_reads_nested_then_top_level() {
        let nested = json!({ "next_token": [ { "n_decoded": 1541 } ] });
        assert_eq!(slot_decoded(&nested), 1541);
        let top = json!({ "n_decoded": 42 });
        assert_eq!(slot_decoded(&top), 42);
        assert_eq!(slot_decoded(&json!({})), 0);
    }

    #[test]
    fn parse_metrics_extracts_gauges() {
        let text = "# HELP llamacpp:kv_cache_usage_ratio KV-cache usage\n\
                    # TYPE llamacpp:kv_cache_usage_ratio gauge\n\
                    llamacpp:kv_cache_usage_ratio 0.31\n\
                    llamacpp:predicted_tokens_seconds 82.5\n\
                    llamacpp:requests_processing 1\n\
                    llamacpp:requests_deferred 3\n";
        let m = parse_metrics(Some(text));
        assert_eq!(m.kv_cache_pct, Some(31.0));
        assert_eq!(m.predicted_tps, Some(82.5));
        assert_eq!(m.requests_deferred, Some(3));
        assert_eq!(m.requests_processing, Some(1));
    }

    #[test]
    fn parse_metrics_handles_labels_and_absence() {
        let none = parse_metrics(None);
        assert!(none.kv_cache_pct.is_none() && none.requests_processing.is_none());
        // A build without kv_cache_usage_ratio (e.g. the user's) still yields
        // the other gauges; kv stays None.
        let real = "llamacpp:predicted_tokens_seconds 156.6\n\
                    llamacpp:prompt_tokens_seconds 5188.2\n\
                    llamacpp:requests_processing{model=\"x\"} 2\n";
        let m = parse_metrics(Some(real));
        assert_eq!(m.requests_processing, Some(2));
        assert_eq!(m.predicted_tps, Some(156.6));
        assert_eq!(m.prompt_tps, Some(5188.2));
        assert!(m.kv_cache_pct.is_none());
    }

    #[test]
    fn fold_slot_computes_tps_and_records_history() {
        let mut p = MetricsPoller::new();
        let t0 = Instant::now();
        // Start a request: idle→busy at decoded 0.
        let tps0 = p.fold_slot(0, true, 0, 7, 1_000, t0);
        assert!(tps0.is_none()); // no delta yet
        // 1s later, 100 tokens decoded → ~100 tps.
        let tps1 = p.fold_slot(0, true, 100, 7, 2_000, t0 + Duration::from_secs(1));
        assert!(tps1.unwrap() > 90.0 && tps1.unwrap() < 110.0);
        // Finish: busy→idle → one history record of 100 tokens.
        let _ = p.fold_slot(0, false, 100, 7, 3_000, t0 + Duration::from_secs(2));
        assert_eq!(p.history.len(), 1);
        let rec = &p.history[0];
        assert_eq!(rec.tokens, 100);
        assert_eq!(rec.slot, 0);
        assert!((rec.duration_s - 2.0).abs() < 0.01);
    }
}
