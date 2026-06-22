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
        }
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
}

#[derive(Clone, Debug, Deserialize)]
pub struct ChatResponse {
    #[serde(default)]
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Choice {
    pub message: ChatMessage,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u32,
}

/// Strip Qwen `<think>…</think>` reasoning blocks from a final answer
/// before returning it to Opus. Tolerates an unterminated trailing
/// `<think>` (truncated reasoning) by dropping from the tag to the end.
pub fn strip_think(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<think>") {
        out.push_str(&rest[..start]);
        match rest[start..].find("</think>") {
            Some(end) => {
                rest = &rest[start + end + "</think>".len()..];
            }
            None => {
                // Unterminated — drop the rest.
                rest = "";
                break;
            }
        }
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
    }

    #[test]
    fn strips_unterminated_think() {
        assert_eq!(strip_think("answer<think>cut off"), "answer");
    }

    #[test]
    fn tool_call_roundtrips() {
        let json = r#"{"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"x\"}"}}"#;
        let tc: ToolCall = serde_json::from_str(json).unwrap();
        assert_eq!(tc.function.name, "read_file");
        assert_eq!(tc.id, "call_1");
    }
}
