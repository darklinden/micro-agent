//! LLM safety gate for command execution.
//!
//! Before a `bash` command runs (when `MA_GATE` is on), a *separate* LLM call
//! asks a judge model whether the command serves the current task and is not
//! destructive. Any failure — network error, unparseable answer, or an
//! explicit refusal — defaults to a denial (fail-safe).

use crate::types::{Message, Role};
use crate::upstream::Upstream;

pub struct Gate<'a> {
    upstream: &'a dyn Upstream,
    objective: &'a str,
}

impl<'a> Gate<'a> {
    pub fn new(upstream: &'a dyn Upstream, objective: &'a str) -> Self {
        Gate { upstream, objective }
    }

    /// Ask whether `command` should run. Returns `Ok(true)` to allow.
    pub async fn check(&self, command: &str) -> anyhow::Result<bool> {
        let prompt = format!(
            "You are a safety judge for an autonomous CLI coding agent. \
Decide whether the following shell command should be EXECUTED, given the task the agent is trying to accomplish.

TASK CONTEXT:
{objective}

COMMAND TO EVALUATE:
```sh
{command}
```

Rules:
- allow = true only if the command directly and plausibly serves the task.
- allow = false if the command is destructive, downloads/executes untrusted code, exfiltrates data, makes network calls out of the blue, or is otherwise risky relative to the task.
- When in doubt, deny.

Respond with JSON only, no prose, in this exact shape:
{{\"allow\": true/false, \"reason\": \"short justification\"}}",
            objective = self.objective
        );

        let msg = Message {
            role: Role::User,
            blocks: vec![crate::types::ContentBlock::Text(prompt)],
        };
        // Gate output is discarded — we only need the final verdict text.
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(rx);
        let outcome = self
            .upstream
            .chat("", &[msg], &[], tx)
            .await
            .map_err(|e| anyhow::anyhow!("gate LLM call failed: {e}"))?;

        let verdict = parse_verdict(&outcome.assistant_text);
        match verdict {
            Some((allow, reason)) => {
                // A denial is more interesting than an allowance: surface it
                // at warn so a scan of the session log finds refusals fast.
                let level = if allow { crate::sesslog::Level::Info } else { crate::sesslog::Level::Warn };
                crate::sesslog::emit(
                    level,
                    "gate",
                    serde_json::json!({"command": command, "allow": allow, "reason": reason}),
                );
                Ok(allow)
            }
            None => {
                crate::sesslog::emit(
                    crate::sesslog::Level::Warn,
                    "gate",
                    serde_json::json!({
                        "command": command,
                        "allow": false,
                        "reason": "unparseable gate answer; denying (fail-safe)",
                    }),
                );
                Ok(false)
            }
        }
    }
}

fn parse_verdict(text: &str) -> Option<(bool, String)> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    let obj: serde_json::Value = serde_json::from_str(&text[start..=end]).ok()?;
    let allow = match obj.get("allow") {
        Some(serde_json::Value::Bool(b)) => *b,
        _ => return None,
    };
    let reason = obj
        .get("reason")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .to_string();
    Some((allow, reason))
}

#[cfg(test)]
mod tests {
    use super::parse_verdict;

    #[test]
    fn parses_plain_allow() {
        assert_eq!(
            parse_verdict(r#"{"allow": true, "reason": "fine"}"#),
            Some((true, "fine".to_string()))
        );
    }

    #[test]
    fn parses_deny() {
        assert_eq!(
            parse_verdict(r#"{"allow": false, "reason": "rm -rf"}"#),
            Some((false, "rm -rf".to_string()))
        );
    }

    #[test]
    fn parses_fenced_json() {
        assert_eq!(
            parse_verdict("Here you go:\n```json\n{\"allow\": true, \"reason\": \"ok\"}\n```\n"),
            Some((true, "ok".to_string()))
        );
    }

    #[test]
    fn rejects_non_json() {
        assert_eq!(parse_verdict("I do not know."), None);
        assert_eq!(parse_verdict(r#"{"allow": "yes"}"#), None);
    }
}
