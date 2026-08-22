//! OpenAI Chat Completions upstream client (`MA_UPSTREAM_TYPE=oai-chat`).
//!
//! Also covers DeepSeek / Ollama / vLLM and other OpenAI-compatible endpoints.

use super::{Result, Upstream};
use crate::config::Config;
use crate::types::{
    ContentBlock, Message, Role, StreamEvent, StreamOutcome, ToolCall, ToolDef,
};
use anyhow::{bail, Context};
use serde_json::{json, Value};
use std::collections::HashMap;

pub struct OaiChatClient {
    url: String,
    api_key: String,
    model: String,
    max_tokens: usize,
    /// OpenAI `reasoning_effort` from `MA_THINKING_EFFORT`; `None` omits it.
    reasoning_effort: Option<&'static str>,
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
            reasoning_effort: cfg.thinking_effort.as_audience_effort(),
            client,
        }
    }

    /// Build the Chat Completions request JSON (pure, testable).
    fn build_body(&self, system: &str, messages: &[Message], tools: &[ToolDef]) -> Value {
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
        if let Some(effort) = self.reasoning_effort {
            body["reasoning_effort"] = json!(effort);
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(self.wire_tools(tools));
        }
        body
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
                            ContentBlock::Thinking { .. } => {
                                // thinking never appears in user messages; ignore.
                            }
                        }
                    }
                    if !text_parts.is_empty() {
                        out.push(json!({"role": "user", "content": text_parts.join("\n")}));
                    }
                }
                Role::Assistant => {
                    let mut text: Option<String> = None;
                    let mut thinking: Option<String> = None;
                    let mut calls: Vec<Value> = Vec::new();
                    for b in &m.blocks {
                        match b {
                            ContentBlock::Text(t) => {
                                text = Some(text.map(|x| x + t).unwrap_or_else(|| t.clone()));
                            }
                            ContentBlock::Thinking { thinking: t, .. } => {
                                // Re-emit for DeepSeek-compatible models that
                                // read `reasoning_content` on assistant turns
                                // (mirrors ai-bridge `preserve_reasoning_content`).
                                thinking = Some(
                                    thinking.map(|x| x + t).unwrap_or_else(|| t.clone()),
                                );
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
                    if let Some(rc) = thinking {
                        msg["reasoning_content"] = json!(rc);
                    }
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
        emitter: tokio::sync::mpsc::UnboundedSender<StreamEvent>,
    ) -> Result<StreamOutcome> {
        let body = self.build_body(system, messages, tools);

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
        let mut thinking_text = String::new();

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
            // Reasoning / thinking content (e.g. DeepSeek `reasoning_content`).
            // Accumulated so the assistant turn can re-emit it on replay.
            if let Some(reasoning) = delta.get("reasoning_content").and_then(Value::as_str)
                && !reasoning.is_empty()
            {
                let _ = emitter.send(StreamEvent::Think(reasoning.to_string()));
                thinking_text.push_str(reasoning);
            }
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
                    let _ = emitter.send(StreamEvent::Answer(chunk.clone()));
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

        if !thinking_text.is_empty() {
            out.assistant_thinking = Some(crate::types::ThinkingBlock {
                thinking: thinking_text,
                signature: None,
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
        emitter: tokio::sync::mpsc::UnboundedSender<StreamEvent>,
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
            reasoning_effort: None,
            client: reqwest::Client::new(),
        }
    }

    fn client_with_effort(effort: &'static str) -> OaiChatClient {
        OaiChatClient {
            reasoning_effort: Some(effort),
            ..client()
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

    #[test]
    fn build_body_omits_reasoning_effort_when_none() {
        let body = client().build_body("sys", &[], &[]);
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn build_body_includes_reasoning_effort() {
        let body = client_with_effort("high").build_body("sys", &[], &[]);
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn convert_messages_emits_reasoning_content_from_thinking() {
        let msgs = vec![Message {
            role: Role::Assistant,
            blocks: vec![
                ContentBlock::Thinking {
                    thinking: "step A ".into(),
                    signature: None,
                },
                ContentBlock::Text("answer".into()),
            ],
        }];
        let out = client().convert_messages(&msgs);
        assert_eq!(out[0]["reasoning_content"], "step A ");
        assert_eq!(out[0]["content"], "answer");
    }
}
