# ma — a lightweight autonomous CLI agent

`ma` is a minimal, TUI-free agent in the spirit of `claude -p`: you give it a
task on the command line, and it drives a tool loop (built-in tools + MCP)
against an upstream LLM until it has an answer.

Everything is configured through environment variables. No config files, no
trust prompts — the working directory is trusted and tools run autonomously
(with a fail-safe LLM safety gate on shell commands, see below).

## Quick start

```bash
# oai-chat (DeepSeek / Ollama / vLLM / any OpenAI-compatible endpoint)
export UPSTREAM_TYPE=oai-chat
export UPSTREAM_URL=https://api.example.com/v1
export UPSTREAM_API_KEY=sk-...
export UPSTREAM_MODEL=deepseek-v4-flash

# or anthropic-messages
# export UPSTREAM_TYPE=anthropic-messages
# export UPSTREAM_URL=https://api.anthropic.com/v1
# export UPSTREAM_API_KEY=sk-ant-...

ma -p "read the README and summarize it in three bullets"
```

A `.env` file in the working directory is loaded at startup (existing env vars
take precedence).

## Install / build

```bash
cargo build --release
# binary at ./target/release/ma
```

## Usage

```bash
ma -p "task text"      # run a task, print result to stdout, exit
ma --list-tools        # list all available tools (incl. MCP) and exit
ma --help
```

`-p/--prompt` is required — there is no stdin-prompt fallback; running without
it exits with code 2.

### Exit codes

| code | meaning                                              |
|------|------------------------------------------------------|
| 0    | agent finished with a plain-text answer             |
| 2    | configuration error, task error, or `MA_MAX_TURNS` hit |

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

## Env variables

### Upstream (required) — unified `UPSTREAM_*` names

| variable          | required | default* | meaning                                   |
|-------------------|----------|----------|-------------------------------------------|
| `UPSTREAM_TYPE`   | yes      | —        | `anthropic-messages` \| `oai-chat`        |
| `UPSTREAM_URL`    | yes      | —        | base URL of the API                       |
| `UPSTREAM_API_KEY`| yes      | —        | API key                                   |
| `UPSTREAM_MODEL`  | no       | type*    | model id                                  |
| `MA_MAX_TOKENS`   | no       | `4096`   | max output tokens per assistant turn      |
| `MA_HEADERS`      | no       | —        | JSON object of extra request headers      |

\* defaults: `oai-chat` → `deepseek-v4-flash`; `anthropic-messages` →
`claude-sonnet-4-5`.

The format is chosen by `UPSTREAM_TYPE` (never guessed from the URL), matching
the `ai-bridge` convention.

### Agent behaviour

| variable                 | default   | meaning                                                          |
|--------------------------|-----------|------------------------------------------------------------------|
| `MA_MAX_TURNS`           | `20`      | max tool-loop iterations before giving up (exit 2)               |
| `MA_DENY_TOOLS`          | —         | comma-separated tool names that must never run (e.g. `bash`)     |
| `MA_GATE`                | `1`       | `0` disables the bash safety gate (pure auto)                    |
| `MA_MAX_TOOL_RESULT_BYTES` | `32768` | cap on tool output fed back into context                         |

### Safety gate

Only `bash` passes through the gate: before a command runs, `ma` makes a
separate LLM query asking whether the command serves the task and is not
destructive. **Any failure (network error, unparseable answer, refusal)
defaults to denying the command** (fail-safe). A denied command is returned to
the model as a tool result so it can change approach. `MA_GATE=0` disables the
gate; `MA_DENY_TOOLS` still applies.

### System prompt

Final system prompt = `MA_SYSTEM_PREFIX` + persona + `MA_SYSTEM_SUFFIX`.

| variable            | meaning                                                                          |
|---------------------|----------------------------------------------------------------------------------|
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
| `task`        | `task`                                                | record a sub-task / step   |
| `web_fetch`   | `url`, `max_bytes?`                                   | GET a URL                  |

Tool arguments are JSON objects (e.g. `{"path": "src/main.rs"}`).

## Documentation

- `CONTEXT.md` — domain glossary
- `docs/adr/` — architectural decisions (upstream type, turn loop, gate, MCP, system prompt)
- `manual-test/mock_upstream.py` — a local mock upstream for end-to-end testing
  of the agent loop without a real API key
