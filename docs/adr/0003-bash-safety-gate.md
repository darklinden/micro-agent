# 0003 — LLM safety gate guards command execution

- Status: Accepted
- Date: 2026-08-20

## Context

Because `ma` executes tools with no human approval (0002), the highest-risk
tool is `bash` (arbitrary command execution). We need a check that happens at
execution time and adapts to the actual task, not a static allow/deny list.

## Decision

- Only command-execution tools (currently `bash`) pass through the gate.
- Read-only / local tools (`read_file`, `grep`, `glob`, ...) and MCP reads do
  **not** go through the gate; writer tools and MCP writes are deferred.
- Before running the command, a **separate LLM request** (same `MA_UPSTREAM_*`
  config) is issued: given the task objective and the full command, the model
  judges whether the command serves the task and is not destructive, and
  returns `{"allow": bool, "reason": string}`.
- **Fail-safe**: any failure (network error, unparseable answer, refusal)
  defaults to **deny**.
- A denied command is returned to the main conversation as a tool result
  ("refused by the safety gate") so the model can adapt its approach.
- `MA_GATE=0` disables the gate (pure auto); `MA_DENY_TOOLS` still applies.

## Consequences

- Destructive commands require explicit model justification in context.
- Adds one extra LLM round-trip per bash call (acceptable for safety).
- The gate is only as good as the judge model; it is a guardrail, not a
  guarantee.
