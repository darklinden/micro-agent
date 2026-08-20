# AGENTS.md

Guidance for AI coding agents working in this repository.

## What this is

`ma` is a lightweight, TUI-free autonomous CLI agent (a mini Claude Code) written in Rust (edition 2024). You give it a task with `-p`, and it drives a tool loop against an upstream LLM — calling tools, feeding results back, iterating until it returns plain text. Everything is configured through environment variables (`.env` is loaded at startup); there are no config files and no trust/approval prompts.

Key docs:
- `README.md` — full user-facing docs: env vars, usage, exit codes.
- `CONTEXT.md` — domain glossary (upstream, gate, MCP, tool, etc.) plus the terms to avoid.
- `docs/adr/` — numbered architectural decisions (upstream type, turn loop, gate, MCP, system prompt).

## Build / test

```bash
cargo build                 # dev binary at target/debug/ma
cargo build --release       # release binary at target/release/ma
cargo test                  # unit tests (no network, no API key needed)
cargo test gate::tests::parses_fenced_json   # run a single test by its full path
cargo clippy --all-targets -- -D warnings    # lint
```

`.env` is gitignored; `/target` is the only gitignore entry.

### Manual end-to-end test (no real API key)

`manual-test/mock_upstream.py` is a local mock OpenAI-Chat SSE upstream that answers a first turn with a `bash` tool call, allows the safety gate, then emits a final answer:

```bash
python3 manual-test/mock_upstream.py 18080   # terminal 1
export UPSTREAM_TYPE=oai-chat UPSTREAM_URL=http://127.0.0.1:18080/v1 \
       UPSTREAM_API_KEY=x
cargo run -- -p "do a thing"                 # terminal 2
```

## Architecture

`main.rs` is the entry point: parse CLI (`-p`, `--list-tools`), load config, init the logger, connect MCP servers, then build the system prompt + upstream client and run the turn loop. Exit codes: `0` = clean answer, `2` = config/task error or `MA_MAX_TURNS` hit.

The design centers on a **neutral core that every upstream client adapts to a wire format**:

- `types.rs` — the crate-wide neutral types: `Message`, `ContentBlock` (`Text` / `ToolUse` / `ToolResult`), `ToolDef`, `ToolCall`, `StreamOutcome`. The turn loop and toolchain only ever talk in these types; no provider JSON leaks past `upstream/`.

- `config.rs` — all config from env. Two namespaces mirror the `ai-bridge` convention: required `UPSTREAM_*` (type/url/api_key/model) and `MA_*` (agent behaviour, MCP, system prompt, logging). `UpstreamType` is explicit (`anthropic-messages` | `oai-chat`) — never guessed from the URL (ADR-0001).

- `upstream/mod.rs` — the `Upstream` trait: `wire_tools()` + streaming `chat(system, messages, tools, emitter)`. New providers implement this trait; `build()` picks the client by `UpstreamType`. `sse.rs` is a minimal SSE parser both providers share (streams via `reqwest::Response::chunk()`, no external stream adapter).

- `persona.rs` — assembles the system prompt as `prefix + persona + suffix` (ADR-0005). `MA_SYSTEM_PREFIX`/`MA_SYSTEM_SUFFIX` may be a literal string **or a file path** (resolved to file contents if it exists — so pointing a suffix at a repo guidance file injects project context). `MA_PERSONA` replaces the built-in persona entirely. `persona::build` returns a `Result` because prefix/suffix file reads can fail.

- `loop_.rs` — the agent turn loop (`Agent::run`): build the tool list + `ToolCtx` + `Gate`, then for each turn stream the chat to stdout, record the assistant turn, and if there are tool calls execute them and push results back as a user message; repeat until plain text (`Done`) or `MA_MAX_TURNS` (`MaxTurns`). Streaming text is drained from a channel to stdout by a spawned task while the chat is awaited inline, keeping the loop sequential.

- `toolchain/` — tool registry + dispatch. `builtin.rs` holds the 8 built-ins (`read_file`,`write_file`,`edit_file`,`grep`,`glob`,`bash`,`task`,`web_fetch`) as thin `serde_json::Value`-in/string-out functions; `gate.rs` is the bash safety check (`MA_GATE`, default on); `mod.rs::run_tool` is dispatch order: **deny-list → bash gate → MCP (`mcp:`-prefixed) → built-in**.

- `mcp/mod.rs` — MCP client pool (`rmcp` crate). `MA_MCP_SERVERS` is a JSON array; each entry is stdio (`cmd`/`args`/`env`) or SSE (`url`). On connect it lists tools and exposes them to the model as `mcp:<server>:<tool>` (namespacing avoids collisions, ADR-0004). A server that fails to connect or times out on `list_tools` is logged and skipped — the rest keep working.

- `out.rs` vs `logger.rs` — strict stdout/log split. Stdout is the only user-facing channel (streamed model text + `⧗` tool marks via the mutex-guarded writer); full request/response/tool detail goes only to the per-launch file at `MA_LOG_FILE_DIR/<yyyyMMdd-HHmmss>.log` (via `tracing`).

### Safety model (no human approval — see README + ADR-0003)

Because tools run with zero confirmation, safety is layered:
- `MA_DENY_TOOLS` — comma-separated tool names that are refused outright before any execution path.
- The **bash safety gate** — only `bash` passes through it. Before execution, a *separate* LLM call (same upstream config) judges whether the command serves the task and isn't destructive, expecting `{"allow": bool, "reason": string}`. **Any failure defaults to deny** (fail-safe); `MA_GATE=0` disables it. A denied command is fed back to the model as a tool result so it can change approach.

When changing the gate or dispatch order, keep that fail-safe property and the deny-before-gate ordering.

### Design decisions

ADR-0002: pure-autonomous multi-turn loop, no trust prompt, exit 2 on budget exhaustion. Keep the `RunResult` (`Done`/`MaxTurns`) → exit-code mapping intact when touching the loop.
