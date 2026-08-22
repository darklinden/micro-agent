# AGENTS.md

Guidance for AI coding agents working in this repository.

## What this is

`ma` is a lightweight, TUI-free autonomous CLI agent (a mini Claude Code) written in Rust (edition 2024). It runs a three-mode plan→edit→run workflow: `-p` writes a numbered plan (and prints it with its path), `-e <plan> -c <req>` revises one, and `-r` executes a plan — or, given a task description instead of a plan path, runs that directly — dispatching independent steps to sub-agents via the `task` tool. Each Agent drives a tool loop against an upstream LLM — calling tools, feeding results back, iterating until it returns plain text. Everything is configured through environment variables (`.env` is loaded at startup); there are no config files and no trust/approval prompts.

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

`manual-test/mock_upstream.py` is a local mock OpenAI-Chat SSE upstream; it answers the safety gate with allow, and on a first turn emits a tool_call selected by a marker in the prompt (`mock:plan`/`mock:edit` → `plan`, `mock:run` → `task`, otherwise `bash`), then a plain-text final answer once tool history exists:

```bash
python3 manual-test/mock_upstream.py 18080   # terminal 1
export MA_UPSTREAM_TYPE=oai-chat MA_UPSTREAM_URL=http://127.0.0.1:18080/v1 \
       MA_UPSTREAM_API_KEY=x
cargo run -- -p "mock:plan write a plan"     # terminal 2: [plan] .ma/plans/<ts>.md
cargo run -- -e .ma/plans/<ts>.md -c "mock:edit add a step"
cargo run -- -r .ma/plans/<ts>.md            # run; first turn defaults to `bash`
```

To exercise a real `task` dispatch under the mock in run mode, make the plan's first line start with `mock:run` (run mode's objective is the plan text, which the mock reads as the first user message).

Note on `--context` + mock: the mock treats a request with any prior tool history (which seeded logs always contain) as "follow-up turn" and answers plainly, so `-e/-p … --context <real log>` ends with "no plan was submitted" — that is the mock's limitation, not a bug. To exercise edit/plan modes with `--context`, synthesize a tool-free seed log (two `message` events: user text starting with `mock:edit`, assistant plain text).

- **Direct prompt, no plan file:** `cargo run -- -r "mock:run add a TODO parser"`. The
  prompt becomes the first user message, so the mock's `mock:run` substring match
  fires and it emits a `task` call.
- **Mistyped plan path:** `cargo run -- -r .ma/plans/does-not-exist.md; echo $?`
  prints `error: plan file does not exist: ...` + a hint and exits 2 before any
  upstream request, so the mock need not be running.

## Architecture

`main.rs` is the entry point: parse the three-mode CLI (`-p/--plan`, `-e/--edit-plan`+`-c/--change`, `-r/--run`, `--list-tools`), validate that exactly one mode is selected, load config, init the logger, connect MCP servers, build the system prompt + upstream client and run the turn loop. Each mode applies a deny overlay, a mode prompt section (`persona::MODE_*_INSTRUCTIONS`), and a mode-specific objective. Exit codes: `0` = clean answer (a plan was written for plan/edit), `2` = config/task error, bad CLI, `MA_MAX_TURNS` hit, or plan/edit finished without submitting a plan.

The design centers on a **neutral core that every upstream client adapts to a wire format**:

- `types.rs` — the crate-wide neutral types: `Message`, `ContentBlock` (`Text` / `ToolUse` / `ToolResult`), `ToolDef`, `ToolCall`, `StreamOutcome`. The turn loop and toolchain only ever talk in these types; no provider JSON leaks past `upstream/`.

- `config.rs` — all config from env. Everything lives under the `MA_` namespace (mirroring the `ai-bridge` upstream convention): required `MA_UPSTREAM_*` (type/url/api_key/model) plus `MA_*` (agent behaviour, MCP, system prompt, logging). `UpstreamType` is explicit (`anthropic-messages` | `oai-chat`) — never guessed from the URL (ADR-0001).

- `upstream/mod.rs` — the `Upstream` trait: `wire_tools()` + streaming `chat(system, messages, tools, emitter)`. New providers implement this trait; `build()` picks the client by `UpstreamType`. `sse.rs` is a minimal SSE parser both providers share (streams via `reqwest::Response::chunk()`, no external stream adapter).

- `persona.rs` — assembles the system prompt as `prefix + persona + suffix` (ADR-0005) and defines the mode prompt sections `MODE_PLAN/EDIT/RUN_INSTRUCTIONS`. `MA_SYSTEM_PREFIX`/`MA_SYSTEM_SUFFIX` may be a literal string **or a file path** (resolved to file contents if it exists — so pointing a suffix at a repo guidance file injects project context). `MA_PERSONA` replaces the built-in persona entirely.

- `loop_.rs` — the agent turn loop. `Agent` carries `cfg`, `upstream`, `system`, `objective`, `mcp`, `depth` (0 = top level; >0 ⇒ quiet), `max_turns`, an optional shared `plan_path` record, and `seed_messages` (`--context` replay; empty for fresh runs and sub-agents). `Agent::run` builds the tool list + `ToolCtx` + `Gate`, seeds the history with `seed_messages` + objective (logging every opening message as a `message` event), then per turn streams the chat (to stdout only when `depth == 0`), records the assistant turn, executes tool calls and pushes results back as a user message, repeating until plain text (`Done`) or the turn budget (`MaxTurns`). It returns `RunOutcome { result, final_text, turns }`; keep the `result`→exit-code mapping intact (see below).

- `toolchain/` — tool registry + dispatch. `builtin.rs` holds the 9 built-ins (`read_file`,`write_file`,`edit_file`,`grep`,`glob`,`bash`,`plan`,`task`,`web_fetch`) as thin `serde_json::Value`-in/string-out functions. `plan` writes the plan to `.ma/plans/<ts>.md` atomically and prints it; `task` routes to `subagent::dispatch`. `gate.rs` is the bash safety check (`MA_GATE`, default on). `mod.rs::run_tool` is dispatch order: **deny-list → bash gate → MCP (`mcp:`-prefixed) → built-in**; `ToolCtx` carries `cfg`, `mcp`, `upstream`, `gate`, `depth`, and `plan_path`.

- `toolchain/subagent.rs` — the `task` dispatcher: refuses nesting (`depth > 0`), builds a nested `Agent` at `depth+1` with `SUBAGENT_PERSONA` and its own objective, and returns the sub-agent's final text as the tool result (with `MA_TASK_MAX_TURNS` or inherited budget). The recursive call is `Box::pin`-ed to satisfy Rust's async-fn recursion rule.

- `mcp/mod.rs` — MCP client pool (`rmcp` crate). `MA_MCP_SERVERS` is a JSON array; each entry is stdio (`cmd`/`args`/`env`) or SSE (`url`). On connect it lists tools and exposes them to the model as `mcp:<server>:<tool>` (namespacing avoids collisions, ADR-0004). A server that fails to connect or times out on `list_tools` is logged and skipped — the rest keep working.

- `out.rs` vs `sesslog.rs` — strict stdout/log split. Stdout is the only user-facing channel (streamed model text + `[log]`/`[context]` banners via the mutex-guarded writer); the session log at `MA_LOG_FILE_DIR/<yyyyMMdd-HHmmss>.log` is a **strict JSONL** event stream written by `sesslog.rs`: session-header events (`run_start`/`system`/`tools`/`objective`) once at startup, then incremental events (`message`, `tool_call`, `tool_result_raw`, `gate`, `turn`, `subagent`, `plan_saved`, `request`, `run_end`) — never a full-request dump. `MA_LOG_LEVEL` is the write threshold; `sesslog::emit` is best-effort (logging failures never kill a run). `Message`/`ToolDef` are `Serialize`, so `message` events store the neutral types verbatim and `sesslog::load_messages` rebuilds them for replay (ADR-0007).

- `--context <session.log>` — replay data flow: `main::run` loads the log via `sesslog::load_messages` (errors → exit 2 before any upstream call), prints `[context] <path> (<n> messages)`, and passes the messages as `Agent.seed_messages`; `loop_::Agent::run` seeds its history with them before appending the objective. The whole opening history is re-emitted as `message` events, so each new log carries the full lineage. Sub-agents always get an empty seed (`subagent.rs`).

### Safety model (no human approval — see README + ADR-0003)

Because tools run with zero confirmation, safety is layered:
- `MA_DENY_TOOLS` — comma-separated tool names that are refused outright before any execution path.
- The **bash safety gate** — only `bash` passes through it. Before execution, a *separate* LLM call (same upstream config) judges whether the command serves the task and isn't destructive, expecting `{"allow": bool, "reason": string}`. **Any failure defaults to deny** (fail-safe); `MA_GATE=0` disables it. A denied command is fed back to the model as a tool result so it can change approach.

When changing the gate or dispatch order, keep that fail-safe property and the deny-before-gate ordering.

### Design decisions

ADR-0002: pure-autonomous multi-turn loop, no trust prompt, exit 2 on budget exhaustion. ADR-0006: three-mode plan/edit/run CLI with `task` sub-agent dispatch. Keep the `RunResult` (`Done`/`MaxTurns`) → exit-code mapping (now exposed as `RunOutcome.result`) intact when touching the loop, and keep deny-before-gate ordering in `run_tool`.
