# ma — a lightweight autonomous CLI agent

`ma` is a minimal, TUI-free agent in the spirit of `claude -p`. It runs a
**plan → edit → run** workflow: plan a task, revise the plan, then execute it —
dispatching independent steps to sub-agents. Each step drives a tool loop
(built-in tools + MCP) against an upstream LLM.

Everything is configured through environment variables. No config files, no
trust prompts — the working directory is trusted and tools run autonomously
(with a fail-safe LLM safety gate on shell commands, see below).

## Quick start

```bash
# oai-chat (DeepSeek / Ollama / vLLM / any OpenAI-compatible endpoint)
export MA_UPSTREAM_TYPE=oai-chat
export MA_UPSTREAM_URL=https://api.example.com/v1
export MA_UPSTREAM_API_KEY=sk-...
export MA_UPSTREAM_MODEL=deepseek-v4-flash

# or anthropic-messages
# export MA_UPSTREAM_TYPE=anthropic-messages
# export MA_UPSTREAM_URL=https://api.anthropic.com/v1
# export MA_UPSTREAM_API_KEY=sk-ant-...

ma -p "read the README and propose a CI layout"   # plan -> prints plan + [plan] path
ma -r .ma/plans/<latest>.md                        # execute it (dispatches sub-agents)
ma -r "add a changelog section for the new flags"  # or run a task prompt directly
```

A `.env` file in the working directory is loaded at startup (existing env vars
take precedence).

## Install / build

```bash
cargo build --release
# binary at ./target/release/ma
```

## Usage

Three modes — exactly one per invocation:

```bash
# 1) Plan: explore (read-only) and write a numbered plan. Prints the plan text
#    to stdout, then the saved path as `[plan] .ma/plans/<ts>.md`.
ma -p "read the README and propose a CI layout"

# 2) Edit: revise an existing plan; writes a NEW timestamped plan file (the old
#    one is kept), then prints the revised plan and its path.
ma -e .ma/plans/20260821-093000.md -c "add a lint step"

# 3) Run: execute a plan, or run a task prompt directly — dispatching
#    independent steps to sub-agents via `task`.
ma -r .ma/plans/20260821-093000.md
ma -r "add a changelog section for the new flags"

# Replace the entire system prompt (string or file path). Works in any mode;
# mode instructions are still appended after it.
ma -s "you are a code archaeologist" -r "map the dependency graph"

ma --list-tools        # list all available tools (incl. MCP) and exit
ma --help
```

`-e/--edit-plan` requires `-c/--change`; `-e` accepts an existing plan path only.
`-r` takes an existing plan file to execute, or a task description to run
directly: a value that is neither an existing file nor path-shaped is run as a
prompt, while a non-existent value that still looks like a file path (e.g. a
mistyped `missing.md`) errors with code 2. There is no stdin-prompt fallback —
misuse exits with code 2.

### Exit codes

| code | meaning                                              |
|------|------------------------------------------------------|
| 0    | plan/edit wrote a plan; run finished with a plain-text answer |
| 2    | configuration error, task/CLI error, a `-r` value that looks like a missing plan path, plan/edit produced no plan, or `MA_MAX_TURNS` hit |

## How it works

1. `ma` builds the system prompt as
   `[MA_SYSTEM_PREFIX] + persona + [MA_SYSTEM_SUFFIX]` (see below), connects any
   configured MCP servers, and merges their tools — with an `mcp:` prefix —
   into the tool set exposed to the model.
2. It streams a request to the upstream. The model's prose is printed to
   **stdout** as it arrives.
3. If the model requests tool calls, `ma` executes them (inline `⧗ tool …` marks
   appear on stdout) and feeds the results back as new messages.
4. It repeats until the model returns plain text (success, exit 0) or the turn
   budget `MA_MAX_TURNS` is exhausted (exit 2).

Everything runs autonomously: no human approval prompts and no "trust this
folder?" step. Safety comes from `MA_DENY_TOOLS` and the bash safety gate.

## Plans & sub-agents

- **Planning** is the `plan` tool: it prints and saves a numbered plan to
  `.ma/plans/<yyyymmdd-hhmmss>.md`. In `-p`/`-e` mode `write_file`/`edit_file`/
  `task` are disabled, so the agent can only explore and submit a plan. `-e`
  writes a **new** timestamped file — old versions are kept as a revision chain.
  Plan writes are atomic (`.tmp` + `rename`): a kill mid-write never leaves a
  truncated plan.
- **Execution** (in `-r` mode) uses the `task` tool: each call spawns a
  sub-agent with its own tool loop, its own `SUBAGENT_PERSONA`, and its own
  safety gate, then returns the sub-agent's final report as the tool result.
  Sub-agents are **quiet** on stdout (only `[task] started/finished` banners;
  detail goes to the log); nesting is capped at depth 1 (sub-agents cannot call
  `task`). Budget via `MA_TASK_MAX_TURNS` (default: `MA_MAX_TURNS`).



## Env variables

### Upstream (required) — unified `MA_UPSTREAM_*` names

| variable             | required | default* | meaning                                   |
|----------------------|----------|----------|-------------------------------------------|
| `MA_UPSTREAM_TYPE`   | yes      | —        | `anthropic-messages` \| `oai-chat`        |
| `MA_UPSTREAM_URL`    | yes      | —        | base URL of the API                       |
| `MA_UPSTREAM_API_KEY`| yes      | —        | API key                                   |
| `MA_UPSTREAM_MODEL`  | no       | type*    | model id                                  |
| `MA_MAX_TOKENS`   | no       | `4096`   | max output tokens per assistant turn      |
| `MA_HEADERS`      | no       | —        | JSON object of extra request headers      |

\* defaults: `oai-chat` → `deepseek-v4-flash`; `anthropic-messages` →
`claude-sonnet-4-5`.

The format is chosen by `MA_UPSTREAM_TYPE` (never guessed from the URL), matching
the `ai-bridge` convention.

### Agent behaviour

| variable                 | default   | meaning                                                          |
|--------------------------|-----------|------------------------------------------------------------------|
| `MA_MAX_TURNS`           | `20`      | max tool-loop iterations before giving up (exit 2)               |
| `MA_TASK_MAX_TURNS`      | `MA_MAX_TURNS` | turn budget for each `task` sub-agent                       |
| `MA_DENY_TOOLS`          | —         | comma-separated tool names that must never run (e.g. `bash`)     |
| `MA_GATE`                | `1`       | `0` disables the bash safety gate (pure auto)                    |
| `MA_MAX_TOOL_RESULT_BYTES` | `32768` | cap on tool output fed back into context                         |
| `MA_THINKING_EFFORT`       | `high`  | reasoning intensity: `none` \| `low` \| `high` \| `max` (see below) |

### Reasoning (thinking / effort)

`MA_THINKING_EFFORT` tunes reasoning intensity with a single value, mapped per
provider (mirrors the `ai-bridge` convention):

| value   | `oai-chat` upstream          | `anthropic-messages` upstream                    |
|---------|-------------------------------|--------------------------------------------------|
| `none`  | no field sent (default)       | no `thinking` block                              |
| `low`   | `reasoning_effort: "low"`     | `thinking{type:enabled, budget≈1024}`            |
| `high`  | `reasoning_effort: "high"`    | `thinking{type:enabled, budget≈4096}`            |
| `max`   | `reasoning_effort: "max"`     | `thinking{type:enabled, budget≈16384}`           |

For `oai-chat` the value is emitted as the top-level `reasoning_effort` (DeepSeek
and OpenAI o-series understand it). For `anthropic-messages` it becomes an
Anthropic `thinking` block whose `budget_tokens` is auto-clamped below
`MA_MAX_TOKENS` (Anthropic requires `1024 ≤ budget < max_tokens`). Default is
`high`; set `none` to send nothing.

When the upstream emits reasoning (DeepSeek `reasoning_content`, Anthropic
`thinking_delta`), it is streamed to stdout labelled as thinking and replayed in
the assistant turn — including the Anthropic `signature` handoff token — so
multi-turn tool loops stay lossless (matching `ai-bridge`'s reasoning bridge).

### Safety gate

Only `bash` passes through the gate: before a command runs, `ma` makes a
separate LLM query asking whether the command serves the task and is not
destructive. **Any failure (network error, unparseable answer, refusal)
defaults to denying the command** (fail-safe). A denied command is returned to
the model as a tool result so it can change approach. `MA_GATE=0` disables the
gate; `MA_DENY_TOOLS` still applies.

### System prompt

The base system prompt is resolved with this priority:

1. `-s <value>` / `--system-prompt` (CLI) — replaces the prompt entirely
2. `MA_SYSTEM_PROMPT` — replaces the prompt entirely when `-s` is absent
3. default: `MA_SYSTEM_PREFIX` + persona + `MA_SYSTEM_SUFFIX`

Every value — the `-s` flag, `MA_SYSTEM_PROMPT`, `MA_SYSTEM_PREFIX`, and
`MA_SYSTEM_SUFFIX` — may be a literal string **or a file path** whose contents
are inlined. The plan/edit/run mode instructions are always appended after the
base prompt.

| variable            | meaning                                                                          |
|---------------------|----------------------------------------------------------------------------------|
| `MA_SYSTEM_PROMPT`  | string **or file path** that **replaces** the whole prompt (below `-s`)         |
| `MA_SYSTEM_PREFIX`  | string **or file path** prepended to the system prompt                          |
| `MA_SYSTEM_SUFFIX`  | string **or file path** appended (point at a `CLAUDE.md` to inject repo context) |
| `MA_PERSONA`        | when set, **replaces** the built-in persona                                      |

### MCP servers

`MA_MCP_SERVERS` is a JSON array. Each entry has `name` plus either stdio
(`cmd` + `args` + `env`) or SSE (`url`):

```bash
export MA_MCP_SERVERS='[
  {"name":"fs","cmd":"npx","args":["-y","@modelcontextprotocol/server-fs"]},
  {"name":"remote","url":"https://mcp.example.com/sse"}
]'
```

| variable                      | default  | meaning                                        |
|-------------------------------|----------|------------------------------------------------|
| `MA_MCP_LIST_TOOLS_TIMEOUT_MS`| `10000`  | per-server connect + `list_tools` timeout (ms) |

- MCP tools are exposed to the model as `mcp:<server>:<tool>` (e.g.
  `mcp:fs.read_file`).
- **stdio** works with any local command (e.g. `npx`, `uvx`, a compiled binary).
- **SSE** servers are supported over **both `http://` and `https://`** URLs
  (TLS via `aws-lc-rs`/rustls).
- A server that fails to connect or times out is logged and skipped — the rest
  still work.

### Logging

| variable          | default | meaning                                                        |
|-------------------|---------|----------------------------------------------------------------|
| `MA_LOG_FILE_DIR` | —       | directory for per-launch log files (`<yyyymmdd-HHmmss>.log`); when unset, only stdout is used |
| `MA_LOG_LEVEL`    | `info`  | tracing level for internal/file logging                        |

**stdout** shows only the model's streamed text + compact tool marks (`⧗ …`).
All request/response/tool detail goes to the log file.

## Built-in tools

| tool          | arguments                                             | notes                      |
|---------------|-------------------------------------------------------|----------------------------|
| `read_file`   | `path`                                                |                            |
| `write_file`  | `path`, `content`                                     | creates parent dirs        |
| `edit_file`   | `path`, `old_string`, `new_string`                    | replaces first occurrence  |
| `grep`        | `pattern`, `path?`, `file_glob?`                      | substring match            |
| `glob`        | `pattern`                                             |                            |
| `bash`        | `command`                                             | **gated by safety gate**   |
| `plan`        | `plan`                                                | save+print this run's numbered plan to `.ma/plans/` |
| `task`        | `task`, `context?`                                    | dispatch a sub-agent; returns its report (no nesting) |
| `web_fetch`   | `url`, `max_bytes?`                                   | GET a URL                  |

Tool arguments are JSON objects (e.g. `{"path": "src/main.rs"}`).

## Documentation

- `CONTEXT.md` — domain glossary
- `docs/adr/` — architectural decisions (upstream type, turn loop, gate, MCP, system prompt, plan/edit/run workflow)
- `manual-test/mock_upstream.py` — a local mock upstream for end-to-end testing
  of the agent loop without a real API key
