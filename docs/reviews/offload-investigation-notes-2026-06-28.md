# Offload server — issues to investigate

Observed during the 2026-06-28 Qwen-vs-Sonnet comparison run (local model `Qwen3.6-35B-A3B-UD-Q4_K_M`, llama-server on 127.0.0.1:12333).

1. **Concurrent requests fail.** Firing 2+ offload_task calls so 2 run concurrently → the queued/2nd request returns `error sending request for url (http://127.0.0.1:12333/v1/chat/completions)` (connection-refused) and/or an empty result. Single (serial) calls are reliable. Both llama-server slots were live (~120 tok/s each, 75k ctx each) at the time, so the refusal looks like a **host-side queueing/dispatch bug**, not server capacity. The offload tool advertises "up to 2 concurrent" — that path is broken against this server.

2. **Empty return — NOT context overflow (corrected).** A unit sometimes returns `completed with no output` (empty). Originally assumed to be context overflow, but **disproven**: the full-scope `processing` unit (7 files) returned empty at **180k** ctx, where the prompt cannot overflow. Real cause is `agent.rs:295-297`:
   ```rust
   if msg.tool_calls.is_empty() {
       let content = msg.content.unwrap_or_default();
       return Ok(strip_think(&content));   // empty if content was None, or entirely a <think> block
   }
   ```
   The model returned a final turn with no tool call and either empty content or **only a `<think>…</think>` block** (which `strip_think` removes entirely) → the loop returns `""` and reports it as success. This is the empty-answer path Qwen itself flagged. **Fix direction:** treat an empty post-strip final answer as an error (or one forced-final retry) instead of silently returning "".
   - Note on the dashboard: the history `tokens` / per-slot `n_decoded` is **generated (output) tokens only** — it excludes the prompt. True context fill = `usage.prompt_tokens` + generated; only the `--metrics` `kv_cache_usage_ratio` gauge shows it. So history token counts cannot confirm/deny overflow.

3. **Orphaned slot on client error (transport error, NOT a timeout).** When the host returns an error to the caller, the underlying llama-server job KEEPS RUNNING and holds the slot; its result is lost (the MCP call already errored). No cancel is propagated to the server.
   - **Exact error string:** `offload error: chat request failed: error sending request for url (http://127.0.0.1:12333/v1/chat/completions)`
   - This is a **reqwest transport-level error** (connect/send stage), **NOT a response timeout** (a timeout reads "operation timed out"/"elapsed", `reqwest::Error::is_timeout()`). The TCP connection failed/was dropped; the host did not wait out a deadline.
   - **Contradiction to resolve:** the slot is observably generating, so the request DID reach llama-server — not a true failure-to-send. Likely causes:
     a. **Pooled-connection reuse** — the host's reqwest `Client` reuses a keep-alive connection llama-server closed after the previous response; the next send on the stale socket fails. Fix: `.pool_max_idle_per_host(0)` or retry once on transport error.
     b. **Header-stall during prompt processing** — at high ctx, llama-server accepts the socket and begins prompt eval before sending headers; a short read/overall timeout surfaces as a send/transport error.
   - **Actions:** (i) audit the host's reqwest `Client` builder — connect vs read/overall timeout, pool settings; (ii) on transport error, send an explicit cancel to llama-server (slot-stop) so the slot frees; (iii) log `is_timeout()` vs `is_connect()` vs `is_request()` so it's diagnosable.

3b. **Client interrupt/cancel does NOT stop the server job (confirmed live).** Issuing an offload_task and then interrupting/rejecting the tool call client-side still left llama-server running the job (slot active). The request is dispatched to the server at/around when the tool is issued, and aborting on the client (interrupt, dropped future, errored MCP call) leaves the server generating an orphaned job that holds the only slot — same end state as #3. **No cancel/abort propagation host→llama-server when the caller goes away.** Fix: tie the host's server-side request to the caller's cancellation token / dropped future, and issue a slot-stop on cancel. This makes every failed/aborted attempt cost a full generation and block the slot — the main thing making a serial review run fragile.

4. **No slot availability/status check.** Need a way to query slot busy/free/queue depth before dispatching (poll llama-server `/slots` or `/health`), so the host can queue gracefully, report "busy", or pick a free slot instead of erroring. Surface in the Offload Server tab too.

5. **Verbosity/format drift (model-side, minor).** Without a strict output cap, Qwen over-produced (one unit = 1863 lines / 81k chars, overflowed the tool's max-tokens). A tight "JSON only, <3500 tokens" instruction fixed it. Qwen also emitted confidence as floats (0.75) instead of high/med/low. Consider enforcing a response schema / max_tokens on the offload agent.

---

## ROOT CAUSE (confirmed by code trace, 2026-06-28)

The "error sending request" failures (#1, #3) trace to three concrete spots:

- **`service.rs:469` — no same-backend retry.** The connection-failure fail-over only re-routes when `views.len() > 1`:
  ```rust
  Err(e) if is_connection_error(&e.to_string()) && views.len() > 1 => { ...reroute... }
  Err(e) => Err(e),
  ```
  With a **single local backend** (the common setup), a connection-class send failure has **no retry** and propagates straight to the caller. A stale-pooled-connection blip is therefore fatal on the first try.

- **`service.rs:191` — chat client config.** Built as `reqwest::Client::builder().timeout(offload_timeout_secs.max(30)).build()` with **no `connect_timeout` and default keep-alive pooling on**. reqwest reuses idle pooled connections; if llama-server closed the keep-alive socket after the previous response (or between concurrent requests), the next `.send()` on the dead socket fails as "error sending request for url" — exactly the observed error. (Contrast `mcp_host.rs:778-784`, which DOES set `connect_timeout`.)

- **`agent.rs:381-384` — single send, no transport retry.** `builder.send().await.map_err(... "chat request failed: {e}")?` — one shot; any transport error is terminal.

**Why concurrency made it worse:** two simultaneous requests are more likely to race a stale/!idle pooled connection; combined with "no retry on a single backend", one of the two reliably fell over. The global semaphore (`service.rs:407`) queues correctly — it is NOT the cause. The "empty return" is the separate context-overflow case (#2).

### Recommended fix (low-risk, high-value), in priority order
1. **Retry the same backend once on a *transport* connection error** (not on timeouts — `is_connection_error` currently lumps `request timed out` in at `service.rs:1143`, and retrying a timeout risks double-running a long generation). Either split `is_connection_error` into `is_transport_error` vs `is_timeout`, or add a dedicated one-shot retry inside `run_on`/`post_chat` for connect/send failures. This alone fixes both the solo transient failures and the concurrency failures.
2. **Harden the chat client** (`service.rs:191`): add `.connect_timeout(Duration::from_secs(5))` and `.pool_max_idle_per_host(0)` for the local single-slot case (no idle reuse → no stale-socket sends). Pooling buys little against one local server.
3. **Propagate cancel** (#3b): on caller drop / our own error, POST llama-server's slot-stop so the slot frees instead of running an orphan to completion.
4. **Slot-status precheck** (#4): poll `/slots`/`/health` before dispatch; queue or report "busy" instead of erroring.
