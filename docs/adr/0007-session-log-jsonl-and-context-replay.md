# 0007 — JSONL session log and `--context` replay

- Status: Accepted
- Date: 2026-08-22

## Context

The per-launch log file was unreadable and unusable for anything but scrolling:
each turn dumped the **entire wire request body** on one line — system prompt,
full message history, and the whole tool table (MCP schemas alone are tens of
KB). A 10-turn run repeated the tool definitions ten times and grew the history
quadratically; the single-line JSON could be neither read nor parsed back.

Independently, continuing work across runs was impossible: each invocation
started from a blank conversation, with no way to say "keep going from where
the last run stopped".

## Decision

**1. The log file becomes a strict JSONL event stream**, written by a new
`sesslog.rs` that replaces the `tracing`/`tracing-subscriber`/
`tracing-appender` stack (those dependencies are removed):

- One JSON object per line, every line carrying common fields
  `v`(=1) / `ts` / `level` / `ev`. `MA_LOG_LEVEL` stays, reinterpreted as the
  write threshold (`debug < info < warn < error`).
- Session-level facts are written **once** at startup — `run_start`, `system`,
  `tools`, `objective` — instead of being re-dumped every turn.
- Everything afterwards is incremental: `message` (every appended
  conversation message, stored as the neutral `Message` type verbatim),
  `tool_call`, `tool_result_raw` (pre-compression content, truncated),
  `gate`, `turn`, `request` (sizes only: n_msgs/bytes), `subagent`,
  `plan_saved`, `run_end`.
- Sub-agent records carry `depth`; their `message` events are logged for
  debugging but excluded from replay.
- Writes go through a mutex-guarded buffered file with flush-per-event,
  best-effort: a logging failure never kills a run.
- stdout gains a `[log] <path>` banner so the file is discoverable.

**2. `--context <session.log>` replays the previous run's top-level
conversation.** Long-form only (`-c` remains `--change`). `sesslog::load_messages`
collects the depth-0 `message` events in file order into neutral `Message`s;
`Agent.seed_messages` prepends them before this run's objective. Valid in all
three modes; missing/unparseable/no-conversation logs exit 2 before any
upstream call. stdout prints `[context] <path> (<n> messages)`.

The seeded history is re-emitted as `message` events in the new run's own log,
so every session log is self-contained: replaying log B (itself seeded from A)
yields A+B's full chain.

## Consequences

- Log size scales with *new* content only; the tool table appears exactly once.
  Same-mock-task comparisons dropped ~27 KB → ~15 KB files, and the gap widens
  with real MCP tool sets.
- The old human-format logs are gone; reading now means `jq`. Old `.log`
  files are incompatible with `--context` (breaking format change).
- Cross-upstream replay degrades gracefully rather than erroring: Anthropic
  thinking blocks (with signatures) replay losslessly against Anthropic;
  against oai-chat they are re-sent as `reasoning_content` (existing behavior).
- Replay assumes provider-neutral ordering (history starts with user text;
  tool results follow their tool_use) which normal runs guarantee.
- `tracing` leaves the dependency tree; structured logging is ~150 lines of
  project code with no filter DSL to learn.

## Rejected alternatives

- **Keep tracing + separate transcript sidecar** — two writers, two formats,
  and the "messy log" complaint would survive in the diagnostic file.
- **Human-formatted sectioned log** — prettier raw, but replay parsing needs
  fragile marker conventions; JSONL is both parseable and jq-readable.
- **Summary injection instead of replay** (paste a digest of the previous log
  into the objective) — simpler but lossy, burns tokens restating context, and
  cannot continue a tool loop mid-flight.
- **Short flag `-c` for `--context`** — occupied by `--change`; keeping edit
  mode's muscle memory won.
