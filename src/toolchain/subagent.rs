//! `task` sub-agent dispatch: one focused sub-task, one nested agent loop.
//!
//! The `task` tool spawns a fresh, independent `loop_::Agent` run at nesting
//! depth +1. The sub-agent sees only its own objective (never the parent
//! conversation), runs the full tool loop with its own safety gate, and its
//! final plain-text reply is returned as the `task` tool result — which then
//! flows through the parent loop's normal tool-result compression.

use super::builtin::{arg, err, ok};
use super::{ToolCtx, ToolOutput};
use crate::loop_::{Agent, RunResult};
use serde_json::Value;

/// System prompt for sub-agents spawned by the `task` tool. Deliberately short:
/// the sub-agent cannot see the parent conversation, so its instructions must
/// be self-contained and its final message is the only thing the parent reads.
pub const SUBAGENT_PERSONA: &str = r#"You are a ma sub-agent. The parent agent dispatched you to complete ONE focused sub-task autonomously, with your own tool loop. You cannot see the parent conversation, and nobody observes your intermediate work — your FINAL message is the report the parent will read.

Guidelines:
- Do the work yourself with the available tools; `task` is unavailable to you (no nesting).
- Stay strictly within the scope of your sub-task.
- Finish with a concise report: what you did, exact file paths / commands / key outputs, and any failures or deviations the parent must know about."#;

/// Dispatch ONE sub-task as a nested agent run, returning its final report.
pub async fn dispatch(args: &Value, ctx: &ToolCtx<'_>) -> ToolOutput {
    // 1. Refuse nesting (hard depth limit of 1).
    if ctx.depth > 0 {
        return err(
            "`task` cannot be called from inside a sub-agent (nesting is disabled). \
             Do the work yourself with the other tools."
                .into(),
        );
    }

    // 2. Build the self-contained objective; cap a huge `context`.
    let task = arg(args, "task").trim().to_string();
    if task.is_empty() {
        return err("task must not be empty".into());
    }
    let context = crate::upstream::truncate(arg(args, "context").trim(), 16 * 1024);
    let objective = build_objective(&task, &context);

    // 3. Scoped budget: task_max_turns or inherited max_turns.
    let mut cfg = ctx.cfg.clone();
    cfg.max_turns = cfg.task_max_turns.unwrap_or(cfg.max_turns);

    // 4. Concise stdout markers; the nested run itself is quiet (depth > 0).
    crate::out::banner(&format!(
        "\n[task] sub-agent started: {}",
        first_line(&task)
    ));
    let start = std::time::Instant::now();
    crate::sesslog::emit(
        crate::sesslog::Level::Info,
        "subagent",
        serde_json::json!({
            "depth": ctx.depth + 1u32,
            "event": "started",
            "task": first_line(&task),
            "max_turns": cfg.max_turns,
        }),
    );
    let agent = Agent {
        cfg: &cfg,
        upstream: ctx.upstream,
        system: SUBAGENT_PERSONA.to_string(),
        objective,
        mcp: ctx.mcp,
        depth: ctx.depth + 1,
        max_turns: cfg.max_turns,
        plan_path: None,
        seed_messages: Vec::new(),
    };
    let elapsed = start.elapsed().as_secs_f64();
    // Box the recursive .run() future: Agent::run → run_tool → builtin::run →
    // dispatch → Agent::run is a cycle that must pass through a Box to avoid an
    // infinitely sized future.
    let outcome = Box::pin(agent.run()).await;
    match outcome {
        Ok(o) => {
            crate::out::banner(&format!(
                "[task] sub-agent {} after {} turn(s), {elapsed:.1}s",
                if o.result == RunResult::Done {
                    "finished"
                } else {
                    "stopped"
                },
                o.turns,
            ));
            crate::sesslog::emit(
                crate::sesslog::Level::Info,
                "subagent",
                serde_json::json!({
                    "depth": ctx.depth + 1u32,
                    "event": if o.result == RunResult::Done { "finished" } else { "max_turns" },
                    "turns": o.turns,
                    "elapsed_s": elapsed,
                }),
            );
            match (o.result, o.final_text.trim().is_empty()) {
                (RunResult::Done, false) => ok(o.final_text),
                (RunResult::Done, true) => ok("(sub-agent finished without a textual report)".into()),
                (RunResult::MaxTurns, _) => err(format!(
                    "sub-agent stopped at its turn budget ({} turns) without a final report; \
                     re-dispatch a narrower sub-task or do this step yourself",
                    cfg.max_turns
                )),
            }
        }
        Err(e) => {
            crate::out::banner(&format!("[task] sub-agent failed after {elapsed:.1}s"));
            crate::sesslog::emit(
                crate::sesslog::Level::Error,
                "subagent",
                serde_json::json!({
                    "depth": ctx.depth + 1u32,
                    "event": "failed",
                    "elapsed_s": elapsed,
                    "message": format!("{e:#}"),
                }),
            );
            err(format!("sub-agent failed: {e:#}"))
        }
    }
}

fn build_objective(task: &str, context: &str) -> String {
    if context.is_empty() {
        task.to_string()
    } else {
        format!("{task}\n\nContext from the parent agent:\n{context}")
    }
}

/// First line of `s`, trimmed and clipped to ~80 chars, for the start banner.
fn first_line(s: &str) -> String {
    let line = s.lines().next().unwrap_or(s).trim();
    let clipped: String = line.chars().take(80).collect();
    if clipped != line {
        format!("{clipped}…")
    } else {
        clipped
    }
}

#[cfg(test)]
mod tests {
    use super::{build_objective, dispatch};
    use crate::config::{Config, ReasoningEffortOverride, ReasoningPolicy, UpstreamType};
    use crate::mcp::McpPool;
    use crate::toolchain::gate::Gate;
    use crate::toolchain::ToolCtx;
    use crate::types::{Message, StreamEvent, StreamOutcome, ToolDef};
    use crate::upstream::Upstream;
    use anyhow::Result;
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn cfg(max_turns: usize) -> Config {
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
            max_turns,
            task_max_turns: None,
            deny_tools: vec![],
            gate_enabled: false,
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

    /// A canned upstream that records how many times `chat` was called.
    struct FakeUpstream {
        calls: AtomicUsize,
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
            _emitter: tokio::sync::mpsc::UnboundedSender<StreamEvent>,
        ) -> Result<StreamOutcome> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            (self.reply)()
        }
    }

    fn ctx<'a>(
        upstream: &'a dyn Upstream,
        cfg: &'a Config,
        mcp: &'a McpPool,
        depth: u32,
    ) -> ToolCtx<'a> {
        ToolCtx {
            cfg,
            mcp,
            upstream,
            gate: Gate::new(upstream, "objective", true),
            depth,
            plan_path: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    #[tokio::test]
    async fn refuses_nesting() {
        let fake = FakeUpstream::canned("should not be used");
        let c = cfg(5);
        let pool = McpPool::empty();
        let c = ctx(&fake, &c, &pool, 1);
        let out = dispatch(&json!({"task": "nested"}), &c).await;
        assert!(out.is_error);
        assert!(out.content.contains("inside a sub-agent"));
        assert_eq!(fake.calls(), 0);
    }

    #[tokio::test]
    async fn returns_subagent_final_text() {
        let fake = FakeUpstream::canned("the sub report");
        let c = cfg(5);
        let pool = McpPool::empty();
        let c = ctx(&fake, &c, &pool, 0);
        let out = dispatch(&json!({"task": "do x"}), &c).await;
        assert!(!out.is_error);
        assert_eq!(out.content, "the sub report");
    }

    #[tokio::test]
    async fn max_turns_reported_as_error() {
        // The sub-agent always requests a `glob` that matches nothing, so every
        // turn spends its budget and the nested loop ends in MaxTurns.
        let reply = Arc::new(|| {
            Ok(StreamOutcome {
                tool_calls: vec![crate::types::ToolCall {
                    id: "t1".into(),
                    name: "glob".into(),
                    arguments: json!({"pattern": "zz-no-match-*"}),
                }],
                ..StreamOutcome::default()
            })
        });
        let fake = FakeUpstream {
            calls: AtomicUsize::new(0),
            reply,
        };
        let mut c = cfg(5);
        c.task_max_turns = Some(2);
        let pool = McpPool::empty();
        let c = ctx(&fake, &c, &pool, 0);
        let out = dispatch(&json!({"task": "keep working"}), &c).await;
        assert!(out.is_error);
        assert!(out.content.contains("turn budget"), "got: {}", out.content);
    }

    #[tokio::test]
    async fn upstream_error_becomes_error_output() {
        let fake = FakeUpstream {
            calls: AtomicUsize::new(0),
            reply: Arc::new(|| Err(anyhow::anyhow!("boom"))),
        };
        let c = cfg(5);
        let pool = McpPool::empty();
        let c = ctx(&fake, &c, &pool, 0);
        let out = dispatch(&json!({"task": "do x"}), &c).await;
        assert!(out.is_error);
        assert!(out.content.starts_with("sub-agent failed"));
    }

    #[test]
    fn combines_task_and_context() {
        assert_eq!(build_objective("do x", ""), "do x");
        assert_eq!(
            build_objective("do x", "the files are here"),
            "do x\n\nContext from the parent agent:\nthe files are here"
        );
    }
}
