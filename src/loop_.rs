//! The agent turn loop.
//!
//! Sends the conversation to the upstream, streams assistant text to stdout,
//! executes any requested tool calls (feeding results back as new messages),
//! and repeats until the model produces a plain-text reply or the turn budget
//! is exhausted.

use crate::config::Config;
use crate::mcp::McpPool;
use crate::out;
use crate::toolchain::gate::Gate;
use crate::toolchain::{build_tools, run_tool, ToolCtx};
use crate::types::{ContentBlock, Message, Role, StreamEvent, ToolDef};
use crate::upstream::Upstream;

/// How the run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunResult {
    Done,
    MaxTurns,
}

pub struct Agent<'a> {
    pub cfg: &'a Config,
    pub upstream: &'a dyn Upstream,
    pub system: String,
    pub objective: String,
    pub mcp: &'a McpPool,
}

impl<'a> Agent<'a> {
    /// Run the full agent loop. Returns the exit-code-relevant result.
    pub async fn run(&self) -> anyhow::Result<RunResult> {
        let tools: Vec<ToolDef> = build_tools(self.mcp);
        let mut messages = vec![Message::user_text(self.objective.clone())];
        let ctx = ToolCtx {
            cfg: self.cfg,
            mcp: self.mcp,
            gate: Gate::new(self.upstream, &self.objective),
        };

        out::user_block(0, &self.objective);

        for turn in 0..self.cfg.max_turns {
            tracing::info!(turn, "turn start");

            let (tx, mut rx): (tokio::sync::mpsc::UnboundedSender<StreamEvent>, _) =
                tokio::sync::mpsc::unbounded_channel();
            // Drain streamed, tag-aware output to stdout on a separate task
            // (chat is awaited inline below so the turn loop stays sequential).
            let turn = turn as u32;
            let drain = tokio::spawn(async move {
                let mut printer = out::TurnPrinter::new(turn);
                while let Some(ev) = rx.recv().await {
                    printer.event(ev);
                }
                printer.close();
            });
            let outcome = self.upstream.chat(&self.system, &messages, &tools, tx).await?;
            let _ = drain.await;

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

            // Plain-text reply -> done.
            if outcome.tool_calls.is_empty() {
                out::text("\n");
                tracing::info!(turn, "agent finished (no tool calls)");
                return Ok(RunResult::Done);
            }

            // Execute tools and feed results back.
            let mut result_blocks: Vec<ContentBlock> = Vec::new();
            for c in &outcome.tool_calls {
                out::run_marker(turn, &c.name, &c.arguments);
                let r = run_tool(&c.name, &c.arguments, &ctx).await;
                tracing::debug!(
                    tool = %c.name,
                    content = %crate::upstream::truncate(&r.content, 500),
                    is_error = r.is_error,
                    "tool result"
                );
                let content = crate::upstream::truncate(&r.content, self.cfg.max_tool_result_bytes);
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

            tracing::info!(turns_processed = turn + 1, "turn complete");
        }

        out::text("\n");
        out::banner("[reached MA_MAX_TURNS limit; stopping]");
        tracing::warn!(max_turns = self.cfg.max_turns, "hit max turns");
        Ok(RunResult::MaxTurns)
    }
}
