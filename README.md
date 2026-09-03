# ma — a lightweight autonomous CLI agent

`ma` is a minimal, TUI-free agent in the spirit of `claude -p`. It runs a
**plan → edit → run** workflow: plan a task, revise the plan, then execute it —
dispatching independent steps to sub-agents. Each step drives a tool loop
(built-in tools + MCP) against an upstream LLM.

Everything is configured through a single TOML file — `~/.ma/config.toml` by
default, overridable with `--config`. No trust prompts — the working directory
is trusted and tools run autonomously (with a fail-safe LLM safety gate on
shell commands, see below).

## Quick start

Create `~/.ma/config.toml` (a missing file auto-writes a commented starter
template, then exits telling you which keys to fill):

```toml
# oai-chat (DeepSeek / Ollama / vLLM / any OpenAI-compatible endpoint)
upstream_type = "oai-chat"
url = "https://api.example.com/v1"
api_key = "sk-..."

# or anthropic-messages:
# upstream_type = "anthropic-messages"
# url = "https://api.anthropic.com/v1"
# api_key = "sk-ant-..."
```

```bash
ma -p "read the README and propose a CI layout"   # plan -> prints plan + [plan] path
ma -r .ma/plans/<latest>.md                        # execute it (dispatches sub-agents)
ma -r "add a changelog section for the new flags"  # or run a task prompt directly
```

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

# Continue from a previous run: replay its session log as conversation
# context before this run's task. Works with all three modes.
ma -r .ma/plans/20260821-093000.md --context ma-logs/20260821-093005.log

# Replace the entire system prompt (string or file path). Works in any mode;
# mode instructions are still appended after it.
ma -s "you are a code archaeologist" -r "map the dependency graph"

ma --config ~/work.toml -r "tidy the changelog"  # use an alternate config file
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
| 2    | configuration error, task/CLI error, a `-r` value that looks like a missing plan path, plan/edit produced no plan, or the `max_turns` budget hit |

## How it works

1. `ma` builds the system prompt as
   `system_prefix + persona + system_suffix` (see below), connects any
   configured MCP servers, and merges their tools — with an `mcp:` prefix —
   into the tool set exposed to the model.
2. It streams a request to the upstream. The model's prose is printed to
   **stdout** as it arrives.
3. If the model requests tool calls, `ma` executes them (inline `⧗ tool …` marks
   appear on stdout) and feeds the results back as new messages.
4. It repeats until the model returns plain text (success, exit 0) or the turn
   budget `max_turns` is exhausted (exit 2).

Everything runs autonomously: no human approval prompts and no "trust this
folder?" step. Safety comes from `deny_tools` and the bash safety gate.

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
  `task`). Budget via `task_max_turns` (default: inherits `max_turns`).



## Configuration

One file, `~/.ma/config.toml` (ADR-0008) — or whatever `--config <file>` points
at. Required keys: `upstream_type`, `url`, `api_key`; everything else is
optional with a default. Unknown keys fail startup, so a typo can never
silently disable a setting. When the file does not exist, `ma` writes a
fully-commented starter template to that path and exits naming the missing
fields — fill in the keys and rerun. Multiple setups = multiple files plus an
explicit `--config`.

```toml
# ---- required ----
upstream_type = "oai-chat"              # "oai-chat" | "anthropic-messages" (never guessed from the URL)
url = "https://api.example.com/v1"
api_key = "sk-..."

# ---- optional top-level keys (TOML: these must precede any [table]) ----
model = "deepseek-v4-flash"             # default follows upstream_type:
                                        #   oai-chat -> deepseek-v4-flash
                                        #   anthropic-messages -> claude-sonnet-4-5
max_tokens = 128000                     # max output tokens per assistant turn
max_turns = 20                          # tool-loop budget before giving up (exit 2)
task_max_turns = 10                     # per-sub-agent budget; default inherits max_turns
deny_tools = ["bash"]                   # tools refused outright before any execution path
gate = true                             # bash safety gate; false disables it
max_tool_result_bytes = 32768           # cap on tool output fed back into context
mcp_list_tools_timeout_ms = 10000       # per-server connect + list_tools timeout

log_file_dir = "ma-logs"                # JSONL session logs (<ts>.log); unset = no logging
log_level = "info"                      # debug | info | warn | error

system_prefix = "..."                   # each of these may be a literal string OR a file path
system_suffix = "~/proj/CLAUDE.md"      # (file contents are inlined)
persona = "..."                         # replaces the built-in persona entirely
system_prompt = "..."                   # replaces the whole prefix+persona+suffix composite

# ---- tables come last (TOML rule) ----
[headers]                               # extra headers on every upstream request
X-Tenant = "default"

[reasoning]
thinking = true                         # master switch; false strips ALL reasoning params outbound
effort = "max"                          # see "Reasoning" below

[[mcp_servers]]                         # stdio server (cmd [+ args/env]) …
name = "fs"
cmd = "npx"
args = ["-y", "@modelcontextprotocol/server-fs"]
env = { KEY = "value" }

[[mcp_servers]]                         # … or SSE server (url; http and https both work)
name = "remote"
url = "https://mcp.example.com/sse"
```

Migrating from the removed `MA_*` environment variables: same value, snake_case
key (`MA_LOG_FILE_DIR` → `log_file_dir`); comma lists become arrays
(`MA_DENY_TOOLS=bash` → `deny_tools = ["bash"]`); the two JSON values become
native tables (`[headers]`, `[[mcp_servers]]`).

### Reasoning (`[reasoning]`)

Two keys, mirroring the `ai-bridge` convention — the configured value is the
single source of truth:

- **`thinking`** (default `true`) — master switch. `false` strips every
  reasoning parameter from outbound requests and `effort` is ignored.
- **`effort`** (default `"max"`) — `off` / `drop` / `none` / `disable` /
  `disabled` (case-insensitive) drop the effort field entirely; **any other
  value** is trimmed, lowercased, and passed through as-is.

Per upstream:

| upstream            | what gets sent                                                              |
|---------------------|------------------------------------------------------------------------------|
| `oai-chat`          | top-level `reasoning_effort: "<effort>"` (any custom value passes through)  |
| `anthropic-messages`| known tiers map to a `thinking` block's `budget_tokens`: `low→1024`, `high→4096`, `max→16384`; unknown values warn once and fall back to the `high` tier |

For `anthropic-messages` the budget is auto-clamped below `max_tokens`
(Anthropic requires `1024 ≤ budget < max_tokens`) and thinking is disabled
entirely when `max_tokens ≤ 1024`.

When the upstream emits reasoning (DeepSeek `reasoning_content`, Anthropic
`thinking_delta`), it is streamed to stdout labelled as thinking and replayed in
the assistant turn — including the Anthropic `signature` handoff token — so
multi-turn tool loops stay lossless (matching `ai-bridge`'s reasoning bridge).

### Safety gate

Only `bash` passes through the gate. Commands that are provably read-only — a
whitelisted command (git read-only subcommands, ls/grep/cat/head/tail/less/
pwd/cd/find/du/file/echo/which/type) or a `;`/`&&` chain where **every**
segment is one — run immediately with no LLM round trip. Anything else is
judged by a separate LLM query: whether the command serves the task and is
not destructive. **Any failure of the judge (network error, unparseable
answer, refusal) denies the command** (fail-safe). A denial is returned to
the model as a tool result — the refused command, the judge's reason, and a
guard-rail note (a channel failure may be retried once) — so it can change
approach. `gate = false` disables the gate; `deny_tools` still applies.

### System prompt

The base system prompt is resolved with this priority:

1. `-s <value>` / `--system-prompt` (CLI) — replaces the prompt entirely
2. `system_prompt` — replaces the prompt entirely when `-s` is absent
3. default: `system_prefix` + persona + `system_suffix`

Every value — the `-s` flag, `system_prompt`, `system_prefix`, `persona`, and
`system_suffix` — may be a literal string **or a file path** whose contents
are inlined. The plan/edit/run mode instructions are always appended after the
base prompt.

### MCP servers

Each `[[mcp_servers]]` table has a `name` plus either stdio (`cmd` + `args` +
`env`) or SSE (`url`) — see the example above. Behaviour:

- MCP tools are exposed to the model as `mcp:<server>:<tool>` (e.g.
  `mcp:fs.read_file`).
- **stdio** works with any local command (e.g. `npx`, `uvx`, a compiled binary).
- **SSE** servers are supported over **both `http://` and `https://`** URLs
  (TLS via `aws-lc-rs`/rustls).
- A server that fails to connect or times out (`mcp_list_tools_timeout_ms`,
  default `10000`) is logged and skipped — the rest still work.

### Logging — JSONL session logs

Set `log_file_dir` to write per-launch session logs (`<yyyymmdd-HHmmss>.log`);
unset, nothing is written. `log_level` (default `info`) is the write threshold:
`debug` | `info` | `warn` | `error`.

Each run writes one **strict JSONL** file — one JSON object per line, every
line carrying `v` / `ts` / `level` / `ev`. Session-level facts are written once
at startup (`run_start`, `system`, `tools`, `objective`); everything after is
incremental — `message` (each conversation turn), `tool_call`,
`tool_result_raw`, `gate`, `turn`, `subagent`, `plan_saved`, `run_end`. There
are no full-request dumps, so the file stays small and jq-friendly:

```bash
jq -r 'select(.ev=="message") | .msg.role' ma-logs/<ts>.log   # the conversation
jq -c 'select(.ev=="gate")'                ma-logs/<ts>.log   # safety verdicts
```

`gate` events carry `depth` (which agent level the command came from) and,
when a command is refused, `kind` — `Judge`, `Unparseable`, or
`UpstreamError` — so refusal classes are auditable without guessing.

**stdout** shows only the model's streamed text + compact tool marks (`⧗ …`),
plus a `[log] <path>` banner at startup so the session log is easy to find.

### Continuing from a previous run (`--context`)

`--context <session.log>` replays a previous run's top-level conversation (its
depth-0 `message` events) as this run's starting history, then appends the new
task as a fresh user turn — like resuming a chat. It works with all three
modes:

- `-p "<task>" --context prev.log` — plan with the previous conversation in view;
- `-e plan.md -c "<req>" --context prev.log` — revise a plan knowing what happened since;
- `-r … --context prev.log` — continue a run that hit its turn budget.

The replayed history is re-recorded into the *new* run's session log, so every
log carries its complete lineage. A missing/unparseable/empty log exits with
code 2 before any upstream request.

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

## One-shot usage examples

[`one-shot-usage/`](one-shot-usage/) holds ready-to-run wrappers that drive
`ma` with a single detailed `-r` prompt — no plan file needed:

| script     | what it does                                                                                                                                          |
|------------|-------------------------------------------------------------------------------------------------------------------------------------------------------|
| `commit`   | Runs `ma -r "<prompt>"`: commit all changes in the current repo **and every nested submodule**, deepest first (submodule pointers land in the root commit). The prompt encodes Conventional Commits rules (`type(scope): subject`, header ≤ 72 chars, breaking-change `!`, …) and forbids touching anything above `git rev-parse --show-toplevel`; the agent diffs each dirty repo, writes a fitting message, then commits bottom-up. |
| `push-all` | Pure zsh loop over the submodules in `.gitmodules` plus the repo root itself: clean → `git push`, dirty → shell out to `commit` first, then push.        |

Both scripts expect a `ma` binary next to them (`commit` invokes
`$SCRIPT_DIR/ma`) and rely on `commit` being resolvable on `PATH` for the
dirty-repo path in `push-all` — e.g. copy/symlink the scripts into the same
directory as `target/release/ma` and put that directory on your `PATH`.

```bash
./one-shot-usage/commit      # conventional-commits everything, submodules first
./one-shot-usage/push-all    # commit-if-dirty + push, repo and its submodules
```

They also double as templates for your own one-shot tasks: one carefully
written `-r` prompt is all it takes.

## Documentation

- `CONTEXT.md` — domain glossary
- `docs/adr/` — architectural decisions (upstream type, turn loop, gate, MCP, system prompt, plan/edit/run workflow, config file)
- `one-shot-usage/` — one-shot `-r` wrapper scripts (`commit`, `push-all`)
- `manual-test/mock_upstream.py` — a local mock upstream for end-to-end testing
  of the agent loop without a real API key
