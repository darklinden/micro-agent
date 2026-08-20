//! OpenAI Chat Completions upstream client (`UPSTREAM_TYPE=oai-chat`).
//!
//! Also covers DeepSeek / Ollama / vLLM and other OpenAI-compatible endpoints.

use super::{Result, Upstream};
use crate::config::Config;
use crate::types::{ContentBlock, Message, Role, StreamOutcome, ToolCall, ToolDef};
use anyhow::{bail, Context};
use serde_json::{json, Value};
use std::collections::HashMap;

pub struct OaiChatClient {
    url: String,
    api_key: String,
    model: String,
    max_tokens: usize,
    client: reqwest::Client,
}

impl OaiChatClient {
    pub fn new(cfg: &Config, client: reqwest::Client) -> Self {
        let base = cfg.url.trim_end_matches('/');
        let url = if cfg.url.ends_with("/chat/completions") {
            cfg.url.clone()
        } else {
            format!("{base}/chat/completions")
        };
        OaiChatClient {
            url,
            api_key: cfg.api_key.clone(),
            model: cfg.model.clone(),
            max_tokens: cfg.max_tokens,
            client,
        }
    }

    /// Expand neutral messages into flat OpenAI wire messages.
    fn convert_messages(&self, messages: &[Message]) -> Vec<Value> {
        let mut out: Vec<Value> = Vec::new();
        for m in messages {
            match m.role {
                Role::User => {
                    let mut text_parts: Vec<String> = Vec::new();
                    for b in &m.blocks {
                        match b {
                            ContentBlock::Text(t) => text_parts.push(t.clone()),
                            ContentBlock::ToolResult {
                                tool_use_id,
                                content,
                                ..
                            } => {
                                if !text_parts.is_empty() {
                                    out.push(json!({"role": "user", "content": text_parts.join("\n")}));
                                    text_parts.clear();
                                }
                                out.push(json!({
                                    "role": "tool",
                                    "tool_call_id": tool_use_id,
                                    "content": content
                                }));
                            }
                            ContentBlock::ToolUse { .. } => {
                                // tool_use inside user messages is invalid for oai-chat; ignore.
                            }
                        }
                    }
                    if !text_parts.is_empty() {
                        out.push(json!({"role": "user", "content": text_parts.join("\n")}));
                    }
                }
                Role::Assistant => {
                    let mut text: Option<String> = None;
                    let mut calls: Vec<Value> = Vec::new();
                    for b in &m.blocks {
                        match b {
                            ContentBlock::Text(t) => {
                                text = Some(text.map(|x| x + t).unwrap_or_else(|| t.clone()));
                            }
                            ContentBlock::ToolUse { id, name, input } => {
                                calls.push(json!({
                                    "id": id,
                                    "type": "function",
                                    "function": {
                                        "name": name,
                                        "arguments": input.to_string(),
                                    }
                                }));
                            }
                            ContentBlock::ToolResult { .. } => {}
                        }
                    }
                    let mut msg = json!({"role": "assistant"});
                    msg["content"] = text.map(Value::String).unwrap_or(Value::Null);
                    if !calls.is_empty() {
                        msg["tool_calls"] = Value::Array(calls);
                    }
                    out.push(msg);
                }
            }
        }
        out
    }

    async fn request(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
        emitter: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> Result<StreamOutcome> {
        let mut wire = Vec::new();
        if !system.is_empty() {
            wire.push(json!({"role": "system", "content": system}));
        }
        wire.extend(self.convert_messages(messages));

        let mut body = json!({
            "model": self.model,
            "stream": true,
            "max_tokens": self.max_tokens,
            "messages": wire,
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(self.wire_tools(tools));
        }

        tracing::debug!(url = %self.url, model = %self.model, "oai-chat request");
        tracing::debug!(body = %body, "oai-chat request body");

        let resp = self
            .client
            .post(&self.url)
            .header("authorization", format!("Bearer {}", self.api_key))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .with_context(|| "oai-chat request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body_txt = super::sse::error_body(resp).await;
            bail!("oai-chat API error {status}: {body_txt}");
        }

        let mut acc: HashMap<usize, OaiAccum> = HashMap::new();
        let mut out = StreamOutcome::default();

        super::sse::for_each_event(resp, |ev| {
            if ev.data == "[DONE]" {
                return;
            }
            let Ok(v) = serde_json::from_str::<Value>(&ev.data) else {
                return;
            };
            let Some(choice) = v["choices"].as_array().and_then(|c| c.first()) else {
                return;
            };
            let delta = &choice["delta"];
            if let Some(content) = delta.get("content") {
                // May be a plain string or an array of parts.
                let chunk = match content {
                    Value::String(s) => s.clone(),
                    Value::Array(parts) => parts
                        .iter()
                        .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                        .collect::<String>(),
                    _ => String::new(),
                };
                if !chunk.is_empty() {
                    let _ = emitter.send(chunk.clone());
                    out.assistant_text.push_str(&chunk);
                }
            }
            if let Some(calls) = delta.get("tool_calls").and_then(|c| c.as_array()) {
                for c in calls {
                    let idx = c["index"].as_u64().unwrap_or(0) as usize;
                    let e = acc.entry(idx).or_insert_with(|| OaiAccum {
                        id: String::new(),
                        name: String::new(),
                        args: String::new(),
                    });
                    if let Some(id) = c.get("id").and_then(|i| i.as_str()) {
                        e.id = id.to_string();
                    }
                    if let Some(f) = c.get("function") {
                        if let Some(n) = f.get("name").and_then(|n| n.as_str()) {
                            e.name = n.to_string();
                        }
                        if let Some(a) = f.get("arguments").and_then(|a| a.as_str()) {
                            e.args.push_str(a);
                        }
                    }
                }
            }
        })
        .await?;

        // Flush accumulated tool calls, keyed by index -> deterministic order.
        let mut keys: Vec<usize> = acc.keys().copied().collect();
        keys.sort_unstable();
        for k in keys {
            let a = &acc[&k];
            if a.name.is_empty() {
                continue;
            }
            let parsed = serde_json::from_str(&a.args).unwrap_or(Value::Null);
            out.tool_calls.push(ToolCall {
                id: if a.id.is_empty() {
                    format!("call_{}", k)
                } else {
                    a.id.clone()
                },
                name: a.name.clone(),
                arguments: if parsed.is_object() { parsed } else { json!({}) },
            });
        }

        Ok(out)
    }
}

struct OaiAccum {
    id: String,
    name: String,
    args: String,
}

#[async_trait::async_trait]
impl Upstream for OaiChatClient {
    fn wire_tools(&self, tools: &[ToolDef]) -> Vec<Value> {
        tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    }
                })
            })
            .collect()
    }

    async fn chat(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
        emitter: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> Result<StreamOutcome> {
        self.request(system, messages, tools, emitter).await
    }
}

#[cfg(test)]
mod tests {
    use super::OaiChatClient;
    use crate::types::{ContentBlock, Message, Role};
    use serde_json::json;

    fn client() -> OaiChatClient {
        OaiChatClient {
            url: "http://x/v1/chat/completions".into(),
            api_key: "k".into(),
            model: "m".into(),
            max_tokens: 100,
            client: reqwest::Client::new(),
        }
    }

    #[test]
    fn converts_tool_round_trip() {
        let msgs = vec![
            Message::user_text("hi"),
            Message {
                role: Role::Assistant,
                blocks: vec![ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "bash".into(),
                    input: json!({"command": "echo hi"}),
                }],
            },
            Message {
                role: Role::User,
                blocks: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "greeting".into(),
                    is_error: false,
                }],
            },
        ];
        let out = client().convert_messages(&msgs);
        assert_eq!(out[0]["role"], "user");
        assert_eq!(out[1]["role"], "assistant");
        assert_eq!(out[1]["tool_calls"][0]["function"]["name"], "bash");
        assert_eq!(out[1]["tool_calls"][0]["function"]["arguments"], r#"{"command":"echo hi"}"#);
        assert_eq!(out[2]["role"], "tool");
        assert_eq!(out[2]["tool_call_id"], "call_1");
        assert_eq!(out[2]["content"], "greeting");
    }
}
