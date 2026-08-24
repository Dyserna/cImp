//! `POST /delegate` — V39 Phase B cross-harness delegation.
//!
//! One of the route families V42 R4 (#115) split out of `loopback.rs`. The
//! app owns the tabs, so this is the only way in; the gate
//! ([`super::delegate_admit`]) runs before anything is driven.

use super::*;

/// A `POST /delegate` body — one cross-harness delegation request (V39
/// Phase B, locked decision 3).
///
/// `harness` rather than a tab id, and that is the whole shape of decision 3:
/// at most one tab per harness holds the Manual role, so the driver names a
/// harness and cImp resolves the tab. A tab argument would let a model drive
/// any tab it could guess the id of.
#[derive(Deserialize)]
pub(super) struct DelegateBody {
    #[serde(default)]
    pub(super) harness: String,
    #[serde(default)]
    pub(super) task: String,
    #[serde(default)]
    pub(super) context: Option<String>,
    #[serde(default)]
    pub(super) timeout_s: Option<u64>,
    /// Which consumer this child serves — cImp-authored argv on the child side.
    #[serde(default)]
    pub(super) consumer: Option<String>,
    /// The calling tab. Unforgeable in practice (`--tab` is composed by cImp at
    /// spawn), and REQUIRED: the acyclic check and the Events row both need it.
    #[serde(default)]
    pub(super) tab: Option<String>,
}

/// A `POST /delegate` response — [`RunResult`]'s three fields plus the meta the
/// child renders as the result footer, so a delegation result reads like an
/// `offload_task` one (worker, duration, screening verdict).
#[derive(Serialize)]
pub(super) struct DelegateResult {
    pub(super) ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) worker: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) screened: Option<bool>,
}

impl DelegateResult {
    fn failed(msg: String) -> Self {
        Self {
            ok: false,
            text: None,
            error: Some(msg),
            worker: None,
            duration_ms: None,
            screened: None,
        }
    }
}

/// `POST /delegate` — drive another harness's tab and return its answer.
///
/// The app owns the tabs, so this route is the only way in; the child has no
/// self-contained fallback and says so rather than inventing one.
///
/// **Target resolution is a lookup, not a search** (locked decision 8): the
/// harness id names the one tab whose `delegation_role == Manual` for that
/// harness. If it moved or closed between `tools/list` and this call, the call
/// is refused naming the condition — never silently retargeted.
///
/// Every other condition is the engine's: `delegation::drive` runs the whole of
/// locked decision 12's preflight, so this handler deliberately re-checks
/// nothing it could get wrong on its own.
pub(super) async fn handle_delegate(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let body: DelegateBody = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => {
            let r = DelegateResult::failed(format!("bad request body: {e}"));
            return write_json(stream, 400, &r).await;
        }
    };
    if body.task.trim().is_empty() {
        let r = DelegateResult::failed("`task` must be non-empty".into());
        return write_json(stream, 400, &r).await;
    }

    let settings = live_settings(app);
    let agent = crate::graph::source_for_consumer(body.consumer.as_deref().unwrap_or(crate::harness::DEFAULT_HARNESS.token()));
    // The calling tab must be a CONFIGURED tab of this consumer. An anonymous
    // or unrecognized id is refused rather than fail-open: unlike a latch (where
    // "we do not know who this is" degrades to no containment), delegation has
    // nothing safe to do without a driver — the cycle check and the audit row
    // are both keyed by it.
    let driver = match tab_identity(&settings, agent, body.tab.as_deref()) {
        TabIdentity::Configured(t) => crate::state::TabId::from_str(t),
        TabIdentity::Anonymous | TabIdentity::Unknown(_) => {
            let r = DelegateResult::failed(
                "delegation needs to know which tab is asking, and this request names none that \
                 is configured for this harness"
                    .into(),
            );
            return write_json(stream, 200, &r).await;
        }
    };

    // V39 Phase B: the taint gate, and it sits HERE on purpose — after every
    // parse-boundary rejection (so a malformed request never engages a latch)
    // and before the worker is resolved, the slot is claimed, the read-only
    // lock is engaged or a single byte is typed. A refused delegation must
    // leave the worker tab exactly as it was, and must mint no `start` row for
    // a delegation that never began; both are true by this ordering, and
    // `the_delegate_gate_runs_before_anything_is_driven` pins it.
    if let Err(refusal) = delegate_admit(
        latches(),
        DELEGATE_TOOL,
        agent,
        body.tab.as_deref(),
        |a, t| latch_scope(app, &settings, a, t),
        |scope| GatePolicy::resolve(&settings, scope),
    ) {
        let r = DelegateResult::failed(refusal.to_string());
        return write_json(stream, 200, &r).await;
    }

    let harness = body.harness.trim();
    let Some(worker_id) = manual_tab_for(&settings, harness) else {
        let r = DelegateResult::failed(format!(
            "no tab currently holds the Manual delegation role for `{harness}` — it was set to \
             None, moved, or the tab was closed since this tool was listed"
        ));
        return write_json(stream, 200, &r).await;
    };

    // V39 review R-6: **watch the caller** while the delegation runs, the same
    // way `/run` does and with the same reasoning one step further. A worker
    // tab is single-slot and its keyboard is locked for the whole flight, so a
    // `delegate_task_*` caller that died — a closed session, a killed child —
    // used to hold BOTH for the full `delegation.default_timeout_s` (ten
    // minutes by default) waiting to hand over a reply nobody would read.
    //
    // After the request body a well-behaved client sends nothing and does not
    // half-close its write half until it has the response, so a probe read
    // returning 0 bytes (or erroring) means the connection went away. No
    // heartbeat half: unlike `/run` this route answers with one JSON object and
    // adding a keep-alive stream would change the wire shape for the child.
    //
    // What happens on cancel is `drive_watching`'s, shared with the facade: the
    // engine is TOLD (no key is ever sent — the worker finishes visibly), the
    // flight is awaited rather than dropped so the slot and lock are released
    // by their owner, and a pre-claim abandonment mark (R-8) is dropped after.
    let cancel = CancellationToken::new();
    let drive_req = crate::delegation::DriveRequest {
        worker: crate::state::TabId::from_str(&worker_id),
        driver: Some(driver),
        mode: crate::delegation::DelegationMode::Explicit,
        task: body.task,
        context: body.context,
        timeout_s: body.timeout_s,
        // The explicit tool adds NOTHING (locked decision 2a): what the user
        // asked for is what the worker reads. Only the Phase C facade passes a
        // note, and only because `offload_task`'s `schema` / `profile` have no
        // other way through a PTY.
        format_note: None,
    };
    let reply = {
        let (mut rd, _wr) = stream.split();
        let flight = crate::delegation::drive_watching(app, drive_req, &cancel);
        tokio::pin!(flight);
        loop {
            let mut probe = [0u8; 1];
            tokio::select! {
                biased;
                r = &mut flight => break r,
                read = rd.read(&mut probe) => match read {
                    Ok(0) | Err(_) => {
                        debug!("delegate loopback: caller disconnected mid-flight; cancelling");
                        cancel.cancel();
                        break (&mut flight).await;
                    }
                    // A stray byte before the response is unexpected on this
                    // one-shot protocol; ignore it and keep waiting.
                    Ok(_) => continue,
                },
            }
        }
    };

    let r = match reply {
        Ok(reply) => DelegateResult {
            ok: true,
            text: Some(reply.text),
            error: None,
            worker: Some(reply.worker),
            duration_ms: Some(reply.duration_ms),
            screened: Some(reply.screened),
        },
        // 200 with `ok:false`, like `/run`: a refusal, a timeout and a
        // take-over are all task-level outcomes the model should read and adapt
        // to, not transport errors.
        Err(e) => DelegateResult::failed(e.to_string()),
    };
    write_json(stream, 200, &r).await
}

/// The tab id currently holding the Manual delegation role for `harness`.
///
/// `None` when nothing does — which is a real state, not an error: the role may
/// have been cleared or moved between the `tools/list` that advertised the tool
/// and this call (locked decision 8's move rule makes that a normal event).
pub(super) fn manual_tab_for(settings: &crate::settings::Settings, harness: &str) -> Option<String> {
    settings.tabs.iter().find_map(|t| match t {
        crate::settings::TabConfig::AiTool(c)
            if c.delegation_role == crate::settings::DelegationRole::Manual
                && crate::tabs::tab_consumer(c) == Some(harness) =>
        {
            Some(c.id.clone())
        }
        _ => None,
    })
}
