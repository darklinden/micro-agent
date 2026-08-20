//! Minimal Server-Sent-Events (SSE) parser over a streaming reqwest body.
//!
//! Handles the common shape both Anthropic Messages and OpenAI Chat
//! Completions use: `data:` lines, an optional `event:` line, events separated
//! by blank lines. Uses `Response::chunk()` so no external stream adapter is
//! needed.

use anyhow::Result;

/// A single SSE event: optional event type plus the decoded `data` payload.
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

enum LineKind {
    Data(String),
    Event(String),
    Ignore,
}

const SSE_MAX_EVENT_BYTES: usize = 4 * 1024 * 1024;

/// Consume the streaming body of `resp` and call `f` for each complete event.
/// Events are surfaced incrementally so callers can stream text to stdout.
pub async fn for_each_event(mut resp: reqwest::Response, mut f: impl FnMut(SseEvent)) -> Result<()> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut pending_data: Vec<String> = Vec::new();
    let mut pending_event: Option<String> = None;
    let mut total: usize = 0;

    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| anyhow::anyhow!("stream read failed: {e}"))?
    {
        buf.extend_from_slice(&chunk);

        // Drain complete lines (split on '\n', tolerate '\r').
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.strip_suffix('\n').unwrap_or(&line);
            let line = line.strip_suffix('\r').unwrap_or(line);

            if line.is_empty() {
                // Event boundary -> emit if we have data.
                if !pending_data.is_empty() {
                    total += pending_data.iter().map(|s| s.len()).sum::<usize>();
                    if total > SSE_MAX_EVENT_BYTES {
                        anyhow::bail!("SSE event exceeded size limit");
                    }
                    let data = pending_data.join("\n");
                    f(SseEvent {
                        event: pending_event.take(),
                        data,
                    });
                    pending_data.clear();
                } else {
                    pending_event = None;
                }
                continue;
            }

            match parse_line(line) {
                LineKind::Data(d) => pending_data.push(d),
                LineKind::Event(e) => pending_event = Some(e),
                LineKind::Ignore => {}
            }
        }
    }

    // Flush a trailing event without a closing blank line.
    if !pending_data.is_empty() {
        let data = pending_data.join("\n");
        f(SseEvent {
            event: pending_event.take(),
            data,
        });
    }

    Ok(())
}

fn parse_line(line: &str) -> LineKind {
    if let Some(rest) = line.strip_prefix("data:") {
        LineKind::Data(rest.trim_start().to_string())
    } else if let Some(rest) = line.strip_prefix("event:") {
        LineKind::Event(rest.trim().to_string())
    } else {
        LineKind::Ignore
    }
}

/// Extract a verbose API error message from a non-2xx response body (a wide
/// superset of the JSON `{"error": {...}}` shapes both providers use).
pub async fn error_body(mut resp: reqwest::Response) -> String {
    let mut bytes = Vec::new();
    while let Ok(Some(chunk)) = resp.chunk().await {
        bytes.extend_from_slice(&chunk);
        if bytes.len() > 64 * 1024 {
            break;
        }
    }
    String::from_utf8_lossy(&bytes).to_string()
}

#[cfg(test)]
mod tests {
    use super::{parse_line, LineKind};

    #[test]
    fn parses_data_event_ignore() {
        assert!(matches!(parse_line("data: {}"), LineKind::Data(d) if d == "{}"));
        assert!(matches!(parse_line("data:{\"a\":1}"), LineKind::Data(d) if d == "{\"a\":1}"));
        assert!(matches!(parse_line("event: error"), LineKind::Event(e) if e == "error"));
        assert!(matches!(parse_line(":comment"), LineKind::Ignore));
        assert!(matches!(parse_line(""), LineKind::Ignore));
    }
}
