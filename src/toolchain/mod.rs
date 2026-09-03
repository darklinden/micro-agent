//! Tool registry, dispatch, and the bash safety gate.
//!
//! The model sees one merged list: built-in tools plus MCP tools (MCP tools
//! carry an `mcp-` prefix). Dispatch routes a call to a built-in or to the
//! matching MCP server, applying the deny-list and the bash gate when needed.

pub mod builtin;
pub mod compress;
pub mod gate;
pub mod subagent;

use crate::config::Config;
use crate::mcp::McpPool;
use crate::toolchain::builtin::BASH;
use crate::toolchain::gate::Gate;
use crate::types::ToolDef;
use crate::upstream::Upstream;
use serde_json::Value;

/// The result of one tool invocation, fed back to the model as a tool result.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

/// Everything a tool execution needs access to.
pub struct ToolCtx<'a> {
    pub cfg: &'a Config,
    pub mcp: &'a McpPool,
    /// Shared upstream client (needed by the `task` sub-agent dispatcher).
    pub upstream: &'a dyn Upstream,
    pub gate: Gate<'a>,
    /// Nesting depth of the running agent (0 = top level). `task` refuses
    /// when depth > 0.
    pub depth: u32,
    /// This run's plan file path: `plan` records it on first write and
    /// overwrites it on updates, so one run produces one plan.
    pub plan_path: std::sync::Arc<std::sync::Mutex<Option<std::path::PathBuf>>>,
}

/// Merge built-in + MCP tool definitions into the list exposed to the model.
pub fn build_tools(mcp: &McpPool) -> Vec<ToolDef> {
    let mut tools = builtin::builtin_defs();
    tools.extend(mcp.tools_defs());
    tools
}

/// Execute a tool by its full name (built-in or `mcp-<server>--<tool>`).
pub async fn run_tool(name: &str, args: &Value, ctx: &ToolCtx<'_>) -> ToolOutput {
    // 1. Deny-list (immediate, no execution path).
    if ctx.cfg.deny_tools.iter().any(|d| d == name) {
        return ToolOutput {
            content: format!(
                "tool `{name}` is denied by configuration (deny_tools); use another approach"
            ),
            is_error: true,
        };
    }

    // 2. Bash safety gate.
    if name == BASH && ctx.cfg.gate_enabled {
        let cmd = args
            .get("command")
            .and_then(|c| c.as_str())
            .unwrap_or_default();
        let verdict = ctx.gate.check(cmd).await;
        if !verdict.is_allowed() {
            // A denied verdict always carries its kind (`GateVerdict::denied`
            // sets it); the allow path never reaches this branch.
            let kind = format!("{:?}", verdict.kind.expect("denied verdict has a kind"));
            // bash_refused carries reason + kind + depth so the denial class
            // (Judge vs Unparseable vs UpstreamError) is distinguishable in
            // the session log without guessing.
            crate::sesslog::emit(
                crate::sesslog::Level::Info,
                "bash_refused",
                serde_json::json!({
                    "depth": ctx.depth,
                    "command": cmd,
                    "reason": verdict.reason,
                    "kind": kind,
                }),
            );
            return ToolOutput {
                content: gate::build_denied_content(cmd, &verdict),
                is_error: true,
            };
        }
    }

    // 3. MCP tools.
    if let Some(rest) = name.strip_prefix("mcp-") {
        return ctx.mcp.call(rest, args).await;
    }

    // 4. Built-in tools.
    if let Some(out) = builtin::run(name, args, ctx).await {
        return out;
    }

    ToolOutput {
        content: format!("unknown tool `{name}`"),
        is_error: true,
    }
}

#[cfg(test)]
mod tests {
    use super::{run_tool, ToolCtx};
    use crate::config::{
        Config, ReasoningEffortOverride, ReasoningPolicy, UpstreamType,
    };
    use crate::mcp::McpPool;
    use crate::types::{Message, StreamOutcome, ToolDef};
    use crate::upstream::Upstream;
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A canned upstream that records how many times `chat` was called.
    /// Same shape as the fakes in compress.rs and subagent.rs tests.
    struct FakeUpstream {
        calls: AtomicUsize,
        reply: Arc<dyn Fn() -> anyhow::Result<StreamOutcome> + Send + Sync>,
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
        fn failing() -> Self {
            let reply = Arc::new(|| Err(anyhow::anyhow!("network down")));
            FakeUpstream {
                calls: AtomicUsize::new(0),
                reply,
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
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
            _emitter: tokio::sync::mpsc::UnboundedSender<crate::types::StreamEvent>,
        ) -> anyhow::Result<StreamOutcome> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            (self.reply)()
        }
    }

    fn cfg(gate_enabled: bool) -> Config {
        Config {
            upstream_type: UpstreamType::OaiChat,
            url: "http://x".into(),
            api_key: "k".into(),
            model: "m".into(),
            max_tokens: 100,
            reasoning: ReasoningPolicy {
                thinking_enabled: false,
                effort: ReasoningEffortOverride::Drop,
            },
            extra_headers: vec![],
            max_turns: 5,
            task_max_turns: None,
            deny_tools: vec![],
            gate_enabled,
            max_tool_result_bytes: 1_000_000,
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

    fn ctx<'a>(
        upstream: &'a dyn Upstream,
        cfg: &'a Config,
        pool: &'a McpPool,
        gate: super::gate::Gate<'a>,
    ) -> ToolCtx<'a> {
        ToolCtx {
            cfg,
            mcp: pool,
            upstream,
            gate,
            depth: 0,
            plan_path: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    #[tokio::test]
    async fn run_tool_denies_with_three_part_content() {
        let fake = FakeUpstream::canned(r#"{"allow": false, "reason": "rm -rf"}"#);
        let cfg = cfg(true);
        let pool = McpPool::empty();
        let gate = super::gate::Gate::new(&fake, "objective", 0);
        let out = run_tool(
            "bash",
            &json!({"command": "rm -rf x"}),
            &ctx(&fake, &cfg, &pool, gate),
        )
        .await;
        assert!(out.is_error);
        assert!(out.content.contains("The safety judge refused this bash command."));
        assert!(out.content.contains("rm -rf"));
        assert!(
            out.content.contains("split add/commit"),
            "refusal should carry the guard-rail sentence: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn run_tool_read_only_allow_passes_through() {
        // `cd /tmp && ls -la` is pre-allowed read-only, so the judge is never
        // reached — assert the allowance passes through as a normal result.
        let fake = FakeUpstream::canned("should not be called");
        let cfg = cfg(true);
        let pool = McpPool::empty();
        let gate = super::gate::Gate::new(&fake, "objective", 0);
        let out = run_tool(
            "bash",
            &json!({"command": "cd /tmp && ls -la"}),
            &ctx(&fake, &cfg, &pool, gate),
        )
        .await;
        assert!(!out.is_error);
        assert_eq!(fake.calls(), 0, "read-only command must not call the judge");
        assert!(!out.content.contains("safety judge refused"));
    }

    #[tokio::test]
    async fn run_tool_channel_failure_is_fail_safe_denial() {
        let fake = FakeUpstream::failing();
        let cfg = cfg(true);
        let pool = McpPool::empty();
        let gate = super::gate::Gate::new(&fake, "objective", 0);
        let out = run_tool(
            "bash",
            &json!({"command": "curl -sI https://example.com"}),
            &ctx(&fake, &cfg, &pool, gate),
        )
        .await;
        assert!(out.is_error);
        assert!(
            out.content.contains("NOT that the command is unsafe"),
            "channel failure must not imply the command is guilty: {}",
            out.content
        );
        assert!(out.content.contains("retry the command as-is once"));
    }

    #[tokio::test]
    async fn run_tool_gate_disabled_skips_check() {
        let fake = FakeUpstream::canned("should not be called");
        let cfg = cfg(false);
        let pool = McpPool::empty();
        let gate = super::gate::Gate::new(&fake, "objective", 0);
        let out = run_tool(
            "bash",
            &json!({"command": "echo hi"}),
            &ctx(&fake, &cfg, &pool, gate),
        )
        .await;
        assert!(!out.is_error);
        assert_eq!(fake.calls(), 0);
    }
}
