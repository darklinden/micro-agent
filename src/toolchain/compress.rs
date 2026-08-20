//! LLM compression of oversized tool results.
//!
//! Before a tool result is fed back to the model, `prepare_tool_result` checks
//! it against a configured byte threshold. Results at or below the threshold
//! pass through untouched; larger ones are re-sent to the LLM in a *separate*
//! call that compresses them "for the task objective", and the compressed
//! text is used as-is.
//!
//! Unlike [`super::gate::Gate`], this is deliberately *not* fail-safe: failure
//! (network error or empty answer) logs a warning and returns the raw content
//! unchanged, rather than recreating the byte-truncation this path removes.
//! Built-in tools already self-cap their outputs (~256 KiB), bounding how much
//! a raw fallback can blow up.
//!
//! Note: for very large payloads the compression prompt itself is large,
//! because it must carry the full raw text. That is an accepted trade-off.

use crate::types::{ContentBlock, Message, Role};
use crate::upstream::Upstream;

/// Return `raw` as-is if within `threshold` bytes; otherwise have the LLM
/// compress it against `objective` and return the compressed text ("as-is",
/// whether or not it still exceeds the threshold).
pub async fn prepare_tool_result(
    upstream: &dyn Upstream,
    objective: &str,
    threshold: usize,
    raw: &str,
) -> String {
    if raw.len() <= threshold {
        return raw.to_string();
    }

    let prompt = format!(
        "You are an internal summarizer for an autonomous CLI coding agent. \
Compress the following TOOL OUTPUT so it becomes compact yet lossless with respect to the TASK OBJECTIVE. \
Preserve every detail the task depends on: file paths, line numbers, exact error messages, identifiers, and data values. \
Omit verbosity, headers, and irrelevant noise.

TASK OBJECTIVE:
{objective}

TOOL OUTPUT (compress):
```
{raw}
```

Return ONLY the compressed output, no commentary."
    );

    let msg = Message {
        role: Role::User,
        blocks: vec![ContentBlock::Text(prompt)],
    };
    // Compression output is internal — discard the streamed deltas; we only
    // need the final text (same shape as the Gate's internal call).
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    drop(rx);
    let outcome = match upstream.chat("", &[msg], &[], tx).await {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(error = %e, "tool-result compression call failed; returning raw");
            return raw.to_string();
        }
    };
    if outcome.assistant_text.trim().is_empty() {
        tracing::warn!("tool-result compression returned empty; returning raw");
        return raw.to_string();
    }
    outcome.assistant_text
}

#[cfg(test)]
mod tests {
    use super::prepare_tool_result;
    use crate::types::{Message, StreamEvent, StreamOutcome, ToolDef};
    use crate::upstream::Upstream;
    use anyhow::Result;
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A canned upstream that records how many times `chat` was called.
    struct FakeUpstream {
        calls: AtomicUsize,
        /// `Ok(Some(text))` -> reply; `Ok(None)` -> empty reply; `Err` -> fail.
        reply: Arc<dyn Fn() -> Result<StreamOutcome> + Send + Sync>,
    }

    impl FakeUpstream {
        fn canned(text: &'static str) -> Self {
            let reply = Arc::new(move || {
                Ok(StreamOutcome {
                    assistant_text: text.to_string(),
                    ..StreamOutcome::default()
                })
            });
            FakeUpstream {
                calls: AtomicUsize::new(0),
                reply,
            }
        }
    }

    #[async_trait::async_trait]
    impl Upstream for FakeUpstream {
        fn wire_tools(&self, _tools: &[ToolDef]) -> Vec<Value> {
            vec![]
        }
        async fn chat(
            &self,
            _system: &str,
            _messages: &[Message],
            _tools: &[ToolDef],
            _emitter: tokio::sync::mpsc::UnboundedSender<StreamEvent>,
        ) -> Result<StreamOutcome> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            (self.reply)()
        }
    }

    #[tokio::test]
    async fn passthrough_when_within_threshold() {
        let fake = FakeUpstream::canned("should not be used");
        let raw = "short result".to_string();
        let out = prepare_tool_result(&fake, "objective", 1000, &raw).await;
        assert_eq!(out, raw);
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn compresses_when_over_threshold() {
        let fake = FakeUpstream::canned("compressed summary");
        let raw = "x".repeat(2000);
        let out = prepare_tool_result(&fake, "objective", 1000, &raw).await;
        assert_eq!(out, "compressed summary");
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn falls_back_to_raw_on_error() {
        let fake = FakeUpstream {
            calls: AtomicUsize::new(0),
            reply: Arc::new(|| Err(anyhow::anyhow!("boom"))),
        };
        let raw = "y".repeat(2000);
        let out = prepare_tool_result(&fake, "objective", 1000, &raw).await;
        assert_eq!(out, raw);
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn falls_back_to_raw_on_empty_reply() {
        let fake = FakeUpstream::canned("   ");
        let raw = "z".repeat(2000);
        let out = prepare_tool_result(&fake, "objective", 1000, &raw).await;
        assert_eq!(out, raw);
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    }
}
