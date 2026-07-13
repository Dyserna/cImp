//! Minimal OpenAI-compatible chat-completions wire types for talking to
//! `llama-server`'s `/v1/chat/completions` endpoint with tool-calling
//! (`--jinja`). Only the fields the agent loop reads/writes are modeled;
//! `#[serde(default)]` + `skip_serializing_if` keep requests lean and
//! tolerate llama.cpp's extra response fields.

use serde::{Deserialize, Serialize};

/// One chat message. `role` is `system` | `user` | `assistant` |
/// `tool`. Assistant turns may carry `tool_calls`; `tool` turns carry
/// the `tool_call_id` they answer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self::text("system", content)
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self::text("user", content)
    }
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
        }
    }
    fn text(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
}

/// A model-requested tool call. `function.arguments` is a JSON *string*
/// (OpenAI encodes the args object as a string), parsed by the executor.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    #[serde(default)]
    pub id: String,
    #[serde(rename = "type", default = "default_tool_type")]
    pub kind: String,
    pub function: FunctionCall,
}

fn default_tool_type() -> String {
    "function".into()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// JSON-encoded arguments object (a string per the OpenAI spec).
    #[serde(default)]
    pub arguments: String,
}

/// A tool advertised to the model. `parameters` is a JSON Schema object.
#[derive(Clone, Debug, Serialize)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: FunctionDef,
    /// Whether this tool **observes changing state** — reads the live
    /// filesystem or runs a process — as opposed to being a pure lookup over
    /// data that can't change within a single run. Declared here, beside the
    /// tool definition, so the per-run call cache (`agent::CallCache`) can
    /// decide whether an identical repeat may be served from cache (pure
    /// lookup) or must re-execute (stateful) without keeping a hardcoded name
    /// list — a future stateful tool can't be forgotten. Not sent to the
    /// model. Defaults to `true` (fail toward fresh execution); pure lookups
    /// opt out via [`ToolDef::pure`].
    #[serde(skip)]
    pub stateful: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct FunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

impl ToolDef {
    pub fn function(name: impl Into<String>, description: impl Into<String>, parameters: serde_json::Value) -> Self {
        Self {
            kind: "function",
            function: FunctionDef {
                name: name.into(),
                description: description.into(),
                parameters,
            },
            // Fail-safe default: treat a tool as stateful unless it explicitly
            // declares itself a pure lookup. MCP-server tools (also built here)
            // therefore default to stateful — the offload worker can't know a
            // third-party server won't touch mutable state, so it re-executes.
            stateful: true,
        }
    }

    /// Mark this tool as a **pure lookup** over data that is immutable within a
    /// single run (e.g. the `graph_*` queries against the code-graph snapshot,
    /// which isn't rebuilt mid-run). Such a tool's identical repeats may be
    /// served from the call cache; see [`ToolDef::stateful`].
    pub fn pure(mut self) -> Self {
        self.stateful = false;
        self
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ChatRequest {
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Per-request thinking switch (Qwen/llama.cpp). `Some(false)` maps
    /// to `{"enable_thinking": false}` to suppress `<think>` blocks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_template_kwargs: Option<serde_json::Value>,
    /// `Some(true)` requests an SSE token stream. We stream so that dropping
    /// the request mid-generation makes llama-server detect the disconnect
    /// and abort — the basis for cancellation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// `{"include_usage": true}` asks the server to emit a final usage chunk
    /// when streaming, so we still get `prompt_tokens` for budget policing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<serde_json::Value>,
    /// Hard ceiling on generated tokens (`n_predict`). Set from the slot's free
    /// context so `prompt + generation` can't exceed `n_ctx` — the server stops
    /// cleanly at the cap instead of running into the context wall and dropping
    /// the stream. `None` leaves generation unbounded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// V21 F9: grammar-enforced structured output. When set, carries
    /// llama-server's `{"type": "json_schema", "json_schema": {…}}` so the
    /// sampler constrains generation to JSON matching the caller's schema. Only
    /// ever set on the final-synthesis request (tool-call turns stay free-form).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ChatResponse {
    #[serde(default)]
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub usage: Option<Usage>,
    /// Measured generation throughput (tokens/sec) from the server's `timings`,
    /// used to size the next request's `max_tokens` to its time budget. `None`
    /// if the server didn't report it. Not a wire field on requests.
    #[serde(default)]
    pub gen_tps: Option<f32>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Choice {
    pub message: ChatMessage,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u32,
    /// Generated (output) tokens for the request. Reported on the streamed
    /// final usage chunk; surfaced per-call in the run log. `0` if absent.
    #[serde(default)]
    pub completion_tokens: u32,
}

// ── Streaming (SSE) wire types ────────────────────────────────────────────
// With `stream:true` the server emits `data: {ChatChunk}` lines and a final
// `data: [DONE]`. Each chunk carries incremental `delta`s; tool calls arrive
// fragmented (name in the first fragment, `arguments` concatenated across
// many), keyed by `index`. `StreamAccumulator` folds them back into the same
// `ChatResponse` the non-streaming path produced, so the agent loop is
// agnostic to the transport.

/// One streamed chunk.
#[derive(Debug, Deserialize)]
pub struct ChatChunk {
    #[serde(default)]
    pub choices: Vec<ChunkChoice>,
    /// Present only on the final usage chunk (`stream_options.include_usage`).
    #[serde(default)]
    pub usage: Option<Usage>,
    /// llama.cpp's per-request `timings`, carried on the final chunk. We read
    /// `predicted_per_second` to size the next request's output budget.
    #[serde(default)]
    pub timings: Option<Timings>,
}

/// llama.cpp generation timings (final chunk only). Only the generation rate
/// is modeled.
#[derive(Clone, Copy, Debug, Deserialize)]
pub struct Timings {
    /// Tokens generated per second for this request.
    #[serde(default)]
    pub predicted_per_second: Option<f32>,
}

#[derive(Debug, Deserialize)]
pub struct ChunkChoice {
    #[serde(default)]
    pub delta: Delta,
}

#[derive(Debug, Default, Deserialize)]
pub struct Delta {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<DeltaToolCall>,
}

#[derive(Debug, Deserialize)]
pub struct DeltaToolCall {
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<DeltaFunction>,
}

#[derive(Debug, Deserialize)]
pub struct DeltaFunction {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

/// Folds streamed `ChatChunk`s back into a single `ChatResponse`.
#[derive(Default)]
pub struct StreamAccumulator {
    content: String,
    tool_calls: Vec<ToolCall>,
    usage: Option<Usage>,
    gen_tps: Option<f32>,
}

impl StreamAccumulator {
    pub fn push_chunk(&mut self, chunk: ChatChunk) {
        if let Some(u) = chunk.usage {
            self.usage = Some(u);
        }
        if let Some(t) = chunk.timings.and_then(|t| t.predicted_per_second) {
            self.gen_tps = Some(t);
        }
        for choice in chunk.choices {
            if let Some(c) = choice.delta.content {
                self.content.push_str(&c);
            }
            for tc in choice.delta.tool_calls {
                // Grow to cover `index` (fragments can arrive sparsely).
                while self.tool_calls.len() <= tc.index {
                    self.tool_calls.push(ToolCall {
                        id: String::new(),
                        kind: default_tool_type(),
                        function: FunctionCall {
                            name: String::new(),
                            arguments: String::new(),
                        },
                    });
                }
                let slot = &mut self.tool_calls[tc.index];
                if let Some(id) = tc.id.filter(|s| !s.is_empty()) {
                    slot.id = id;
                }
                if let Some(f) = tc.function {
                    if let Some(n) = f.name.filter(|s| !s.is_empty()) {
                        slot.function.name = n;
                    }
                    if let Some(a) = f.arguments {
                        slot.function.arguments.push_str(&a);
                    }
                }
            }
        }
    }

    /// Build the assembled response. Mirrors the non-streaming shape: one
    /// assistant choice carrying the merged content + tool calls.
    pub fn into_response(self) -> ChatResponse {
        ChatResponse {
            choices: vec![Choice {
                message: ChatMessage {
                    role: "assistant".into(),
                    content: if self.content.is_empty() {
                        None
                    } else {
                        Some(self.content)
                    },
                    tool_calls: self.tool_calls,
                    tool_call_id: None,
                },
            }],
            usage: self.usage,
            gen_tps: self.gen_tps,
        }
    }
}

/// Strip Qwen `<think>…</think>` reasoning blocks from a final answer
/// before returning it to Opus. Tolerates an unterminated trailing
/// `<think>` (truncated reasoning) by dropping from the tag to the end.
pub fn strip_think(text: &str) -> String {
    const OPEN: &str = "<think>";
    const CLOSE: &str = "</think>";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        // Walk from just after this opener, counting NESTED opens, until the
        // matching close brings depth back to 0. A naive "find the first
        // </think>" would mismatch `<think>a<think>b</think>c</think>` and leak
        // the stray trailing `</think>` (and the text after it) into the answer.
        let mut i = start + OPEN.len();
        let mut depth = 1usize;
        loop {
            let tail = &rest[i..];
            let next_open = tail.find(OPEN);
            let next_close = tail.find(CLOSE);
            match (next_open, next_close) {
                (Some(o), Some(c)) if o < c => {
                    depth += 1;
                    i += o + OPEN.len();
                }
                (_, Some(c)) => {
                    depth -= 1;
                    i += c + CLOSE.len();
                    if depth == 0 {
                        break;
                    }
                }
                _ => {
                    // No matching close (unterminated) — drop to the end.
                    i = rest.len();
                    break;
                }
            }
        }
        rest = &rest[i..];
    }
    out.push_str(rest);
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_think_blocks() {
        assert_eq!(strip_think("<think>reason</think>answer"), "answer");
        assert_eq!(strip_think("before<think>x</think>after"), "beforeafter");
        assert_eq!(strip_think("plain"), "plain");
        // Nested blocks must balance — no stray </think> in the output.
        assert_eq!(strip_think("<think>a<think>b</think>c</think>answer"), "answer");
        assert_eq!(
            strip_think("x<think>a<think>b</think>c</think>y"),
            "xy"
        );
    }

    #[test]
    fn strips_unterminated_think() {
        assert_eq!(strip_think("answer<think>cut off"), "answer");
    }

    #[test]
    fn stream_accumulator_merges_content_tool_calls_and_usage() {
        let mut acc = StreamAccumulator::default();
        let feed = |acc: &mut StreamAccumulator, s: &str| {
            acc.push_chunk(serde_json::from_str::<ChatChunk>(s).unwrap());
        };
        // Content arrives in two deltas.
        feed(&mut acc, r#"{"choices":[{"delta":{"content":"Hel"}}]}"#);
        feed(&mut acc, r#"{"choices":[{"delta":{"content":"lo"}}]}"#);
        // A tool call: id+name in the first fragment, arguments across two more.
        feed(
            &mut acc,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file"}}]}}]}"#,
        );
        feed(
            &mut acc,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}]}"#,
        );
        feed(
            &mut acc,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"x\"}"}}]}}]}"#,
        );
        // A second tool call at index 1.
        feed(
            &mut acc,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_2","function":{"name":"code_search","arguments":"{}"}}]}}]}"#,
        );
        // Final usage chunk (empty choices) carrying usage + timings.
        feed(
            &mut acc,
            r#"{"choices":[],"usage":{"prompt_tokens":42},"timings":{"predicted_per_second":172.3}}"#,
        );

        let resp = acc.into_response();
        assert_eq!(resp.gen_tps, Some(172.3));
        let msg = &resp.choices[0].message;
        assert_eq!(msg.content.as_deref(), Some("Hello"));
        assert_eq!(msg.tool_calls.len(), 2);
        assert_eq!(msg.tool_calls[0].id, "call_1");
        assert_eq!(msg.tool_calls[0].function.name, "read_file");
        assert_eq!(msg.tool_calls[0].function.arguments, r#"{"path":"x"}"#);
        assert_eq!(msg.tool_calls[1].id, "call_2");
        assert_eq!(msg.tool_calls[1].function.name, "code_search");
        assert_eq!(resp.usage.unwrap().prompt_tokens, 42);
    }

    #[test]
    fn tool_call_roundtrips() {
        let json = r#"{"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"x\"}"}}"#;
        let tc: ToolCall = serde_json::from_str(json).unwrap();
        assert_eq!(tc.function.name, "read_file");
        assert_eq!(tc.id, "call_1");
    }

    #[test]
    fn response_format_serializes_only_when_present() {
        // V21 F9: `response_format` follows its `Option` siblings —
        // `skip_serializing_if` means it never appears on the wire unless set.
        let base = ChatRequest {
            messages: vec![ChatMessage::user("hi")],
            tools: Vec::new(),
            tool_choice: None,
            model: None,
            temperature: None,
            chat_template_kwargs: None,
            stream: None,
            stream_options: None,
            max_tokens: None,
            response_format: None,
        };
        let v = serde_json::to_value(&base).unwrap();
        assert!(v.get("response_format").is_none(), "None must be omitted from the request body");

        let mut with = base.clone();
        with.response_format = Some(serde_json::json!({
            "type": "json_schema",
            "json_schema": { "schema": { "type": "object" } }
        }));
        let v = serde_json::to_value(&with).unwrap();
        assert_eq!(v["response_format"]["type"], "json_schema");
        assert_eq!(v["response_format"]["json_schema"]["schema"]["type"], "object");
    }
}
