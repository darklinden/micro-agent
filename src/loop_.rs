//! The agent turn loop.
//!
//! Sends the conversation to the upstream, streams assistant text to stdout,
//! executes any requested tool calls (feeding results back as new messages),
//! and repeats until the model produces a plain-text reply or the turn budget
//! is exhausted.

use crate::config::Config;
use crate::mcp::McpPool;
use crate::out;
use crate::sesslog::{self, Level};
use crate::toolchain::compress;
use crate::toolchain::gate::{DENIAL_MARKER, Gate};
use crate::toolchain::{build_tools, run_tool, ToolCtx};
use crate::types::{ContentBlock, Message, Role, StreamEvent, ToolDef};
use crate::upstream::Upstream;
use serde_json::json;

/// How the run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunResult {
    Done,
    MaxTurns,
}

/// What a completed run produced, beyond the exit-code-relevant result.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub result: RunResult,
    /// Final assistant plain-text reply (empty unless `result == Done`).
    pub final_text: String,
    /// Turns the loop actually ran.
    pub turns: usize,
}

pub struct Agent<'a> {
    pub cfg: &'a Config,
    pub upstream: &'a dyn Upstream,
    pub system: String,
    pub objective: String,
    pub mcp: &'a McpPool,
    /// Sub-agent nesting depth: 0 = top-level run, >=1 = inside a `task`
    /// dispatch. Depth > 0 implies quiet (no stdout output).
    pub depth: u32,
    /// Turn budget for THIS loop (top level: cfg.max_turns; sub-agents:
    /// task_max_turns or inherited).
    pub max_turns: usize,
    /// Shared record of this run's plan file path, so the caller (main) can
    /// print it after the run. `None` -> the run creates a fresh one (sub-agents).
    pub plan_path: Option<std::sync::Arc<std::sync::Mutex<Option<std::path::PathBuf>>>>,
    /// Conversation replayed from a previous run's session log (`--context`);
    /// pushed before this run's objective. Empty for fresh runs and sub-agents.
    pub seed_messages: Vec<Message>,
}

impl<'a> Agent<'a> {
    fn quiet(&self) -> bool {
        self.depth > 0
    }

    /// Run the full agent loop. Returns the exit-code-relevant result plus the
    /// final text and turn count.
    pub async fn run(&self) -> anyhow::Result<RunOutcome> {
        let tools: Vec<ToolDef> = build_tools(self.mcp);
        // `--context` replay: the seeded conversation comes first, then this
        // run's objective as a fresh user turn.
        let mut messages = self.seed_messages.clone();
        messages.push(Message::user_text(self.objective.clone()));
        // Log the whole opening history (seeded replay + objective) like any
        // other message, so every session log carries its complete lineage and
        // a later `--context` on THIS log sees the full chain, not just this
        // run's turns.
        for m in &messages {
            sesslog::emit(Level::Info, "message", json!({"depth": self.depth, "msg": m}));
        }
        let plan_path = self
            .plan_path
            .clone()
            .unwrap_or_else(|| std::sync::Arc::new(std::sync::Mutex::new(None)));
        let ctx = ToolCtx {
            cfg: self.cfg,
            mcp: self.mcp,
            upstream: self.upstream,
            gate: Gate::new(self.upstream, &self.objective, self.depth),
            depth: self.depth,
            plan_path,
        };

        if !self.quiet() {
            out::user_block(0, &self.objective);
        }

        for turn in 0..self.max_turns {
            sesslog::emit(
                Level::Info,
                "turn",
                json!({"depth": self.depth, "turn": turn, "status": "start"}),
            );

            let (tx, mut rx): (tokio::sync::mpsc::UnboundedSender<StreamEvent>, _) =
                tokio::sync::mpsc::unbounded_channel();
            // Drain streamed, tag-aware output to stdout on a separate task
            // (chat is awaited inline below so the turn loop stays sequential).
            // A quiet (sub-agent) run still drains the channel — upstreams
            // ignore send errors — but prints nothing.
            let quiet = self.quiet();
            let turn = turn as u32;
            let drain = tokio::spawn(async move {
                let mut printer = out::TurnPrinter::new(turn);
                while let Some(ev) = rx.recv().await {
                    if !quiet {
                        printer.event(ev);
                    }
                }
                printer.close();
            });
            let outcome = self.upstream.chat(&self.system, &messages, &tools, tx).await?;
            let _ = drain.await;

            // Compact per-turn request summary (debug): sizes only — the full
            // payload is already in the log as incremental message events.
            if sesslog::enabled(Level::Debug) {
                let bytes = serde_json::to_string(&messages).map(|s| s.len()).unwrap_or(0);
                sesslog::emit(
                    Level::Debug,
                    "request",
                    json!({
                        "depth": self.depth,
                        "turn": turn,
                        "n_msgs": messages.len(),
                        "bytes": bytes,
                    }),
                );
            }

            // Record the assistant turn. Order matters for Anthropic replay:
            // the thinking block (with its handoff signature) precedes text and
            // tool_use so multi-turn tool loops stay lossless.
            let mut blocks: Vec<ContentBlock> = Vec::new();
            if let Some(thinking) = &outcome.assistant_thinking
                && !thinking.thinking.is_empty()
            {
                blocks.push(ContentBlock::Thinking {
                    thinking: thinking.thinking.clone(),
                    signature: thinking.signature.clone(),
                });
            }
            if !outcome.assistant_text.is_empty() {
                blocks.push(ContentBlock::Text(outcome.assistant_text.clone()));
            }
            for c in &outcome.tool_calls {
                blocks.push(ContentBlock::ToolUse {
                    id: c.id.clone(),
                    name: c.name.clone(),
                    input: c.arguments.clone(),
                });
            }
            messages.push(Message {
                role: Role::Assistant,
                blocks,
            });
            sesslog::emit(
                Level::Info,
                "message",
                json!({"depth": self.depth, "msg": messages.last().expect("just pushed")}),
            );

            // Plain-text reply -> done.
            if outcome.tool_calls.is_empty() {
                if !self.quiet() {
                    out::text("\n");
                }
                sesslog::emit(
                    Level::Info,
                    "run_end",
                    json!({"depth": self.depth, "result": "done", "turns": turn as usize + 1}),
                );
                return Ok(RunOutcome {
                    result: RunResult::Done,
                    final_text: outcome.assistant_text.clone(),
                    turns: turn as usize + 1,
                });
            }

            // Execute tools and feed results back.
            let mut result_blocks: Vec<ContentBlock> = Vec::new();
            for c in &outcome.tool_calls {
                if !self.quiet() {
                    out::run_marker(turn, &c.name, &c.arguments);
                }
                sesslog::emit(
                    Level::Info,
                    "tool_call",
                    json!({
                        "depth": self.depth,
                        "turn": turn,
                        "id": c.id,
                        "name": c.name,
                        "input": c.arguments,
                    }),
                );
                let r = run_tool(&c.name, &c.arguments, &ctx).await;
                sesslog::emit(
                    Level::Debug,
                    "tool_result_raw",
                    json!({
                        "depth": self.depth,
                        "id": c.id,
                        "content": crate::upstream::truncate(&r.content, 500),
                        "is_error": r.is_error,
                    }),
                );
                // A gate refusal already carries its verdict text in-band; skip
                // the compression LLM round trip so the denial reaches the agent
                // verbatim (channel failures must not mutate into another one).
                let content = if r.is_error && r.content.starts_with(DENIAL_MARKER) {
                    r.content
                } else {
                    compress::prepare_tool_result(
                        self.upstream,
                        &self.objective,
                        self.cfg.max_tool_result_bytes,
                        &r.content,
                    )
                    .await
                };
                result_blocks.push(ContentBlock::ToolResult {
                    tool_use_id: c.id.clone(),
                    content,
                    is_error: r.is_error,
                });
            }
            messages.push(Message {
                role: Role::User,
                blocks: result_blocks,
            });
            sesslog::emit(
                Level::Info,
                "message",
                json!({"depth": self.depth, "msg": messages.last().expect("just pushed")}),
            );

            sesslog::emit(
                Level::Info,
                "turn",
                json!({"depth": self.depth, "turn": turn, "status": "complete"}),
            );
        }

        if !self.quiet() {
            out::text("\n");
            out::banner("[reached turn limit; stopping]");
        }
        sesslog::emit(
            Level::Warn,
            "run_end",
            json!({"depth": self.depth, "result": "max_turns", "turns": self.max_turns}),
        );
        Ok(RunOutcome {
            result: RunResult::MaxTurns,
            final_text: String::new(),
            turns: self.max_turns,
        })
    }
}
