//! Anthropic Messages upstream client (`MA_UPSTREAM_TYPE=anthropic-messages`).

use super::{Result, Upstream};
use crate::config::Config;
use crate::types::{
    ContentBlock, Message, Role, StreamEvent, StreamOutcome, ThinkingBlock, ToolCall, ToolDef,
};
use anyhow::{bail, Context};
use serde_json::{json, Value};
use std::collections::HashMap;

pub struct AnthropicClient {
    url: String,
    api_key: String,
    model: String,
    max_tokens: usize,
    /// Resolved `budget_tokens` for the `MA_THINKING_EFFORT` thinking block,
    /// already clamped below `max_tokens`; `0` when thinking is disabled.
    thinking_budget: usize,
    client: reqwest::Client,
}

impl AnthropicClient {
    pub fn new(cfg: &Config, client: reqwest::Client) -> Self {
        let base = cfg.url.trim_end_matches('/');
        // Accept any of: `.../v1/messages`, `.../v1`, or a bare host root.
        let url = if cfg.url.ends_with("/messages") {
            cfg.url.clone()
        } else if base.ends_with("/v1") {
            format!("{base}/messages")
        } else {
            format!("{base}/v1/messages")
        };
        // Anthropic requires `budget_tokens` in [1024, max_tokens): clamp to keep
        // the request valid even at the default `MA_MAX_TOKENS=4096`, and disable
        // thinking entirely when `max_tokens` is too small to hold a budget.
        let thinking_budget = cfg
            .thinking_effort
            .anthropic_budget()
            .filter(|_| cfg.max_tokens > 1024)
            .map(|b| b.min(cfg.max_tokens - 1024).max(1024))
            .unwrap_or(0);
        AnthropicClient {
            url,
            api_key: cfg.api_key.clone(),
            model: cfg.model.clone(),
            max_tokens: cfg.max_tokens,
            thinking_budget,
            client,
        }
    }

    /// Build the Messages request JSON (pure, testable).
    fn build_body(&self, system: &str, messages: &[Message], tools: &[ToolDef]) -> Value {
        let mut body = json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "messages": self.convert_messages(messages),
        });
        if self.thinking_budget > 0 {
            body["thinking"] = json!({"type": "enabled", "budget_tokens": self.thinking_budget});
        }
        if !system.is_empty() {
            body["system"] = json!(system);
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(self.wire_tools(tools));
        }
        body
    }

    fn convert_messages(&self, messages: &[Message]) -> Vec<Value> {
        messages
            .iter()
            .filter_map(|m| {
                if m.blocks.is_empty() {
                    return None;
                }
                let content: Vec<Value> = m
                    .blocks
                    .iter()
                    .map(|b| match b {
                        ContentBlock::Text(t) => json!({"type": "text", "text": t}),
                        ContentBlock::Thinking {
                            thinking,
                            signature,
                        } => {
                            // Replay the hidden reasoning verbatim, carrying the
                            // handoff signature when the upstream issued one so
                            // Anthropic keeps accepting the conversation.
                            let mut blk = json!({
                                "type": "thinking",
                                "thinking": thinking,
                            });
                            if let Some(sig) = signature {
                                blk["signature"] = json!(sig);
                            }
                            blk
                        }
                        ContentBlock::ToolUse { id, name, input } => json!({
                            "type": "tool_use", "id": id, "name": name, "input": input
                        }),
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => json!({
                            "type": "tool_result", "tool_use_id": tool_use_id,
                            "content": content, "is_error": is_error
                        }),
                    })
                    .collect();
                let role = match m.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                };
                Some(json!({"role": role, "content": content}))
            })
            .collect()
    }

    async fn request(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
        emitter: tokio::sync::mpsc::UnboundedSender<StreamEvent>,
    ) -> Result<StreamOutcome> {
        let body = self.build_body(system, messages, tools);

        tracing::debug!(url = %self.url, model = %self.model, "anthropic request");
        tracing::debug!(body = %body, "anthropic request body");

        let resp = self
            .client
            .post(&self.url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .with_context(|| "anthropic request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body_txt = super::sse::error_body(resp).await;
            bail!("anthropic API error {status}: {body_txt}");
        }

        let mut tool_uses: HashMap<usize, Accum> = HashMap::new();
        let mut thinking: HashMap<usize, ThinkingAccum> = HashMap::new();
        let mut out = StreamOutcome::default();

        super::sse::for_each_event(resp, |ev| {
            if ev.event.as_deref() == Some("error") {
                tracing::error!(data = %ev.data, "anthropic SSE error event");
                return;
            }
            let Ok(v) = serde_json::from_str::<Value>(&ev.data) else {
                return;
            };
            match v.get("type").and_then(|t| t.as_str()) {
                Some("content_block_start") => {
                    let idx = v["index"].as_u64().unwrap_or(0) as usize;
                    let cb = &v["content_block"];
                    match cb.get("type").and_then(|t| t.as_str()) {
                        Some("tool_use") => {
                            tool_uses.insert(
                                idx,
                                Accum {
                                    id: cb["id"].as_str().unwrap_or_default().to_string(),
                                    name: cb["name"].as_str().unwrap_or_default().to_string(),
                                    partial_json: String::new(),
                                },
                            );
                        }
                        Some("thinking") => {
                            thinking.insert(idx, ThinkingAccum::default());
                        }
                        _ => {}
                    }
                }
                Some("content_block_delta") => {
                    let idx = v["index"].as_u64().unwrap_or(0) as usize;
                    let delta = &v["delta"];
                    match delta.get("type").and_then(|t| t.as_str()) {
                        Some("thinking_delta") => {
                            let t = delta["thinking"].as_str().unwrap_or_default().to_string();
                            if !t.is_empty() {
                                let _ = emitter.send(StreamEvent::Think(t.clone()));
                                thinking.entry(idx).or_default().text.push_str(&t);
                            }
                        }
                        Some("signature_delta") => {
                            // Anthropic closes a thinking block with the opaque
                            // handoff token the continuation requires.
                            if let Some(sig) = delta.get("signature").and_then(|s| s.as_str()) {
                                thinking.entry(idx).or_default().signature =
                                    Some(sig.to_string());
                            }
                        }
                        Some("text_delta") => {
                            let t = delta["text"].as_str().unwrap_or_default().to_string();
                            if !t.is_empty() {
                                let _ = emitter.send(StreamEvent::Answer(t.clone()));
                                out.assistant_text.push_str(&t);
                            }
                        }
                        Some("input_json_delta") => {
                            if let Some(a) = tool_uses.get_mut(&idx) {
                                a.partial_json.push_str(
                                    delta["partial_json"].as_str().unwrap_or_default(),
                                );
                            }
                        }
                        _ => {}
                    }
                }
                Some("content_block_stop") => {
                    let idx = v["index"].as_u64().unwrap_or(0) as usize;
                    finalize_tool(&mut tool_uses.remove(&idx), &mut out);
                    finalize_thinking(&mut thinking.remove(&idx), &mut out);
                }
                _ => {}
            }
        })
        .await?;

        // Any tool_use / thinking block never stopped still needs finalizing.
        let remaining_tools: Vec<Accum> = tool_uses.drain().map(|(_, a)| a).collect();
        for a in remaining_tools {
            finalize_tool(&mut Some(a), &mut out);
        }
        let remaining_thinking: Vec<ThinkingAccum> = thinking.drain().map(|(_, a)| a).collect();
        for a in remaining_thinking {
            finalize_thinking(&mut Some(a), &mut out);
        }

        Ok(out)
    }
}

struct Accum {
    id: String,
    name: String,
    partial_json: String,
}

#[derive(Default)]
struct ThinkingAccum {
    text: String,
    signature: Option<String>,
}

fn finalize_tool(acc: &mut Option<Accum>, out: &mut StreamOutcome) {
    if let Some(a) = acc.take() {
        if a.name.is_empty() {
            return;
        }
        let parsed = serde_json::from_str(&a.partial_json).unwrap_or(Value::Null);
        if !parsed.is_object() {
            tracing::debug!(name = %a.name, partial = %a.partial_json, "tool_use input not an object");
        }
        out.tool_calls.push(ToolCall {
            id: a.id,
            name: a.name,
            arguments: if parsed.is_object() { parsed } else { json!({}) },
        });
    }
}

fn finalize_thinking(acc: &mut Option<ThinkingAccum>, out: &mut StreamOutcome) {
    if let Some(a) = acc.take() {
        if a.text.is_empty() {
            return;
        }
        out.assistant_thinking = Some(ThinkingBlock {
            thinking: a.text,
            signature: a.signature,
        });
    }
}

#[async_trait::async_trait]
impl Upstream for AnthropicClient {
    fn wire_tools(&self, tools: &[ToolDef]) -> Vec<Value> {
        tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
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
    use super::AnthropicClient;
    use crate::types::{ContentBlock, Message, Role};
    use serde_json::json;

    fn client() -> AnthropicClient {
        AnthropicClient {
            url: "http://x/v1/messages".into(),
            api_key: "k".into(),
            model: "m".into(),
            max_tokens: 100,
            thinking_budget: 0,
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
                    id: "t1".into(),
                    name: "read_file".into(),
                    input: json!({"path": "a"}),
                }],
            },
            Message {
                role: Role::User,
                blocks: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "abc".into(),
                    is_error: false,
                }],
            },
        ];
        let out = client().convert_messages(&msgs);
        assert_eq!(out[0]["role"], "user");
        assert_eq!(out[0]["content"][0]["type"], "text");
        assert_eq!(out[1]["role"], "assistant");
        assert_eq!(out[1]["content"][0]["type"], "tool_use");
        assert_eq!(out[1]["content"][0]["name"], "read_file");
        assert_eq!(out[2]["role"], "user");
        assert_eq!(out[2]["content"][0]["type"], "tool_result");
        assert_eq!(out[2]["content"][0]["tool_use_id"], "t1");
    }

    #[test]
    fn build_body_omits_thinking_when_disabled() {
        let body = client().build_body("sys", &[], &[]);
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn builds_client_with_clamped_thinking_budget() {
        // max_tokens=4096, Max effort → budget clamps to max_tokens-1024.
        let big = crate::config::Config {
            max_tokens: 4096,
            thinking_effort: crate::config::ThinkingEffort::Max,
            ..cfg("http://x")
        };
        let c = AnthropicClient::new(&big, reqwest::Client::new());
        assert!(c.thinking_budget > 0, "thinking should be enabled for Max effort");
        assert!(c.thinking_budget < c.max_tokens);
    }

    #[test]
    fn build_body_includes_thinking_block_when_budget_set() {
        let c = AnthropicClient {
            max_tokens: 4096,
            thinking_budget: 3072,
            ..client()
        };
        let body = c.build_body("sys", &[], &[]);
        let thinking = &body["thinking"];
        assert_eq!(thinking["type"], "enabled");
        assert_eq!(thinking["budget_tokens"], 3072);
    }

    #[test]
    fn build_body_omits_thinking_when_too_small_max_tokens() {
        // max_tokens=100 ≤ 1024 with Max effort → thinking fully disabled (never
        // emit a budget ≥ max_tokens, which Anthropic would reject).
        let small = crate::config::Config {
            max_tokens: 100,
            thinking_effort: crate::config::ThinkingEffort::Max,
            ..cfg("http://x")
        };
        let c = AnthropicClient::new(&small, reqwest::Client::new());
        assert_eq!(c.thinking_budget, 0);
        let body = c.build_body("sys", &[], &[]);
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn convert_messages_preserves_thinking_signature() {
        let msgs = vec![Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Thinking {
                thinking: "let me think".into(),
                signature: Some("sig-abc".into()),
            }],
        }];
        let out = client().convert_messages(&msgs);
        assert_eq!(out[0]["content"][0]["type"], "thinking");
        assert_eq!(out[0]["content"][0]["thinking"], "let me think");
        // The handoff token must be replayed verbatim.
        assert_eq!(out[0]["content"][0]["signature"], "sig-abc");
    }

    fn cfg(url: &str) -> crate::config::Config {
        crate::config::Config {
            upstream_type: crate::config::UpstreamType::AnthropicMessages,
            url: url.into(),
            api_key: "k".into(),
            model: "m".into(),
            max_tokens: 100,
            thinking_effort: crate::config::ThinkingEffort::None,
            extra_headers: vec![],
            max_turns: 5,
            task_max_turns: None,
            deny_tools: vec![],
            gate_enabled: true,
            max_tool_result_bytes: 1000,
            mcp_servers: vec![],
            mcp_list_tools_timeout_ms: 1000,
            system_prefix: None,
            system_suffix: None,
            persona: None,
            system_prompt: None,
            log_dir: None,
            log_level: "info".into(),
        }
    }

    #[test]
    fn joins_messages_url() {
        let cases = [
            ("https://api.anthropic.com", "https://api.anthropic.com/v1/messages"),
            ("https://api.anthropic.com/", "https://api.anthropic.com/v1/messages"),
            ("https://api.anthropic.com/v1", "https://api.anthropic.com/v1/messages"),
            (
                "https://api.anthropic.com/v1/messages",
                "https://api.anthropic.com/v1/messages",
            ),
        ];
        for (input, expected) in cases {
            let got = AnthropicClient::new(&cfg(input), reqwest::Client::new()).url;
            assert_eq!(got, expected, "for input {input}");
        }
    }
}
