//! Upstream API clients.
//!
//! Each `UpstreamType` has a client that (a) converts neutral `Message`s and
//! `ToolDef`s to its wire format and (b) streams a chat completion, surfacing
//! text deltas via a callback and returning tool calls. The turn loop never
//! touches provider-specific JSON.

pub mod anthropic;
pub mod oai_chat;
pub mod sse;

use crate::config::{Config, UpstreamType};
use crate::types::{Message, StreamEvent, StreamOutcome, ToolDef};
use anyhow::Result;
use serde_json::Value;

/// Common behaviour of an upstream chat client.
#[async_trait::async_trait]
pub trait Upstream: Send + Sync {
    /// Convert neutral `ToolDef`s into the upstream's wire tool array.
    fn wire_tools(&self, tools: &[ToolDef]) -> Vec<Value>;

    /// Run one streaming chat turn. Text deltas are pushed onto `emitter` as
    /// they arrive, tagged by kind ([`StreamEvent::Think`] for reasoning,
    /// [`StreamEvent::Answer`] for the visible reply); the caller drains them
    /// to stdout. Returns the tool calls (if any) and the full assistant text
    /// for logging.
    async fn chat(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
        emitter: tokio::sync::mpsc::UnboundedSender<StreamEvent>,
    ) -> Result<StreamOutcome>;
}

/// Build the shared HTTP client used by all upstream requests.
fn http_client(cfg: &Config) -> Result<reqwest::Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    for (k, v) in &cfg.extra_headers {
        let name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
            .map_err(|e| anyhow::anyhow!("invalid header name {k:?}: {e}"))?;
        headers.insert(name, v.parse()?);
    }
    Ok(reqwest::Client::builder()
        .default_headers(headers)
        .build()?)
}

/// Construct the upstream client for this configuration.
pub fn build(cfg: &Config) -> Result<Box<dyn Upstream>> {
    let client = http_client(cfg)?;
    match cfg.upstream_type {
        UpstreamType::AnthropicMessages => {
            Ok(Box::new(anthropic::AnthropicClient::new(cfg, client)))
        }
        UpstreamType::OaiChat => Ok(Box::new(oai_chat::OaiChatClient::new(cfg, client))),
    }
}

/// Truncate a string to `max` bytes, appending a marker if truncated.
pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        // `max` may land mid-character for multi-byte UTF-8; floor to the
        // nearest char boundary before slicing to avoid a panic.
        let bound = s.floor_char_boundary(max);
        let mut t = s[..bound].to_string();
        t.push_str("\n…[truncated]");
        t
    }
}

#[cfg(test)]
mod tests {
    use super::truncate;

    #[test]
    fn truncate_multibyte_does_not_panic() {
        // `é` is 2 bytes; byte 1 is exactly its start boundary, so we keep "h".
        assert_eq!(truncate("héllo", 1), "h\n…[truncated]");
        // Full-width '（' is 3 bytes; max that lands inside it must not panic.
        let s = "a".repeat(6) + "（b";
        let out = truncate(&s, 7);
        assert!(!out.is_empty());
        assert!(out.starts_with("aaaaaa"));
        assert!(out.contains("[truncated]"));
    }

    #[test]
    fn truncate_zero_limit() {
        assert_eq!(truncate("hello", 0), "\n…[truncated]");
    }

    #[test]
    fn truncate_passthrough_within_limit() {
        assert_eq!(truncate("hello", 5), "hello");
    }
}
