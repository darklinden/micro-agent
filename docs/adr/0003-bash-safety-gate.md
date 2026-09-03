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
- Read-only commands short-circuit **before** the judge: a command (or a
  `;`/`&&` chain) is split into its individual segments, and when **every**
  segment is a whitelisted read-only command (git read-only subcommands incl.
  `git -C`, ls/grep/cat/head/tail/less/pwd/cd/find/du/file/echo/which/type)
  with no shell operator, it runs with no LLM round trip (audited in the
  session log). Anything else — a segment that starts with an unknown
  command, pipes, redirection, heredocs, command substitution, a background
  `&`, writes — goes to the judge. The pre-check only ever pre-allows, never
  pre-denies, and escape handling matches the shell (a backslash outside
  quotes escapes the next byte), so a `\`-escaped separator cannot hide a
  write behind a read-only-looking segment.
- Local git write operations (add/commit/restore/clean) that serve the
  current task are judged **allowed**; pushes to remotes or commits
  capturing task-unrelated files are not.
- A denial returns to the conversation as a three-part tool result: the
  refused command, the judge's reason verbatim, and a guard-rail sentence.
  Channel failures (unparseable judge answer, upstream error) say
  explicitly that the *channel* failed and the command may be retried
  as-is once — so the agent does not treat a fail-safe denial as a verdict
  on the command.
- Fail-safe denials are categorized in the session log (`kind`: Judge /
  Unparseable / UpstreamError) on both the `gate` and `bash_refused`
  events, so refusal classes are auditable without guessing.
