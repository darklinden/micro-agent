# ma — a lightweight autonomous CLI agent

`ma` is a minimal, TUI-free agent in the spirit of `claude -p`: you give it a
task on the command line, and it drives a tool loop (built-in tools + MCP)
against an upstream LLM until it has an answer.

Everything is configured through environment variables. No config files, no
trust prompts — the working directory is trusted and tools run autonomously
(with a fail-safe LLM safety gate on shell commands, see below).

```
$ export UPSTREAM_TYPE=oai-chat
$ export UPSTREAM_URL=https://api.example.com/v1
$ export UPSTREAM_API_KEY=sk-...
$ export UPSTREAM_MODEL=deepseek-v4-flash
$ ma -p "read the README and summarize it in three bullets"
```

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

## Env variables

### Upstream (required) — unified `UPSTREAM_*` names (ai-bridge convention)

| variable          | required | default               | meaning                                  |
|-------------------|----------|-----------------------|------------------------------------------|
| `UPSTREAM_TYPE`   | yes      | —                     | `anthropic-messages` \| `oai-chat`       |
| `UPSTREAM_URL`    | yes      | —                     | base URL of the API                       |
| `UPSTREAM_API_KEY`| yes      | —                     | API key                                   |
| `UPSTREAM_MODEL`  | no       | type default*         | model id                                  |
| `MA_MAX_TOKENS`   | no       | 4096                  | max output tokens per assistant turn      |
| `MA_HEADERS`      | no       | —                     | JSON object of extra request headers      |

\* `oai-chat` default `deepseek-v4-flash`; `anthropic-messages` default
`claude-sonnet-4-5`.

### Agent behaviour

| variable                | default       | meaning                                                        |
|-------------------------|---------------|----------------------------------------------------------------|
| `MA_MAX_TURNS`          | `20`          | max tool-loop iterations before giving up (exit 2)             |
| `MA_DENY_TOOLS`         | —             | comma-separated tool names that must never run (e.g. `bash`)   |
| `MA_GATE`               | `1`           | `0` disables the bash safety gate (pure auto)                  |
| `MA_MAX_TOOL_RESULT_BYTES` | `32768`    | cap on tool output fed back into context                       |

### Safety gate

Only `bash` passes through the gate: before a command runs, `ma` makes a
separate LLM query asking whether the command serves the task and is not
destructive. Any failure denies the command (fail-safe). Denied commands are
returned to the model so it can change approach.

### System prompt

Final system prompt = `MA_SYSTEM_PREFIX` + persona + `MA_SYSTEM_SUFFIX`.

| variable            | meaning                                                                        |
|---------------------|--------------------------------------------------------------------------------|
| `MA_SYSTEM_PREFIX`  | string **or file path** prepended to the system prompt                        |
| `MA_SYSTEM_SUFFIX`  | string **or file path** appended (point at a `CLAUDE.md` to inject repo context) |
| `MA_PERSONA`        | when set, **replaces** the built-in persona                                    |

### MCP servers

`MA_MCP_SERVERS` is a JSON array. Each entry has `name` plus either stdio
(`cmd` + `args` + `env`) or SSE (`url`):

```bash
export MA_MCP_SERVERS='[
  {"name":"fs","cmd":"npx","args":["-y","@modelcontextprotocol/server-fs"]},
  {"name":"remote","url":"https://mcp.example.com/sse"}
]'
```

MCP tools are exposed to the model as `mcp:<server>:<tool>` (e.g.
`mcp:fs.read_file`).

> **stdio** works with any local command (e.g. `npx`, `uvx`, a compiled binary).
> SSE servers are supported over **both `http://` and `https://`** URLs (TLS via
> `aws-lc-rs`/rustls).

### Logging

| variable            | default | meaning                                        |
|---------------------|---------|------------------------------------------------|
| `MA_LOG_FILE_DIR`   | —       | directory for per-launch log files (`<yymmdd-hhmmss>.log`); when unset, only stdout is used |
| `MA_LOG_LEVEL`      | `info`  | tracing level for internal/file logging        |

**stdout** shows only the model's streamed text + compact tool marks (`⧗ …`).
All request/response/tool detail goes to the log file.

`.env` files are loaded at startup (existing env vars take precedence).

## Built-in tools

`read_file`, `write_file`, `edit_file`, `grep`, `glob`, `bash` (gated),
`task`, `web_fetch`.

## Documentation

- `CONTEXT.md` — domain glossary
- `docs/adr/` — architectural decisions (upstream type, turn loop, gate, MCP, system prompt)
