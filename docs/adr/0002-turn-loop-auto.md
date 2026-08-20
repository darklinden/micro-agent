# 0002 — Autonomous multi-turn loop without human confirmation

- Status: Accepted
- Date: 2026-08-20

## Context

`claude -p` runs an agent to completion: the model may call tools, get results
fed back, and continue until it produces a final answer. This requires a
turn loop. A simpler "single request, echo the reply" would make tools useless.

Claude Code also gates tool execution on human approval. `ma` is scriptable
and non-interactive (no TUI), so an interactive approval dialog is impossible.

## Decision

- Multi-turn loop: send conversation → execute any requested tools → feed
  results back → repeat until the model returns plain text (no tool calls).
- **No human confirmation for tool execution** (pure auto). Safety is provided
  by `MA_MAX_TURNS` (default 20) and `MA_DENY_TOOLS`.
- Hitting `MA_MAX_TURNS` without a plain-text reply exits with code 2 after
  printing whatever has been produced.
- There is no "trust this folder?" step and no permission prompts: the working
  directory is trusted by default (see also `0003` for the command gate).

## Consequences

- Script-friendly: exit code 0 on completion, 2 on error/budget exhaustion.
- Because nothing is interactive, the safety burden moves to the bash gate
  (0003) and the deny-list.
