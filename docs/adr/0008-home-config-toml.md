# Configuration moves from environment variables to `~/.ma/config.toml`

All configuration now lives in a single file, `~/.ma/config.toml` (overridable per invocation with `--config <file>`), mirroring the `ai-bridge` profile convention (its ADR-0005) in one-file form. Required keys are `upstream_type` / `url` / `api_key`; everything else — model, `max_tokens`, `[headers]`, `[reasoning]`, agent behaviour (`max_turns`, `task_max_turns`, `deny_tools`, `gate`, `max_tool_result_bytes`), `[[mcp_servers]]` tables, logging (`log_file_dir`, `log_level`), and the system prompt (`system_prefix`, `system_suffix`, `persona`, `system_prompt`) — is optional with documented defaults. Unknown keys fail startup (`deny_unknown_fields`), so a typo cannot silently disable a setting.

Every previous environment variable (`MA_*`) was removed, along with `.env` loading (dotenvy). Only `$HOME` is still read — to resolve the default config path. When the requested file does not exist, startup reports an error and drops a fully-commented starter template at that path (never overwriting an existing file), so onboarding is: run once, fill three keys, run again.

Rationale:

- **No leakage** — exported variables and `.env` files leak into shells, process listings, and logs; a per-user config directory does not. An API key no longer travels through every child process environment.
- **Self-documenting and discoverable** — the starter template carries comments next to every key; `ma --config` + the startup banner (`config: <path> | upstream url: …`) answer "what am I running?" without reading shell rc files.
- **Typo safety** — `deny_unknown_fields` rejects misspelled keys at startup instead of ignoring them.
- **Low-cost first run** — the auto-written commented template turns "read the README to find variable names" into "uncomment three lines".
- **Native structure** — the former JSON-in-env values upgrade to native TOML: `MA_HEADERS` (a JSON object) becomes a `[headers]` table, and `MA_MCP_SERVERS` (a JSON array) becomes `[[mcp_servers]]` tables; malformed values are now hard errors instead of being silently ignored.

The reasoning configuration adopts ai-bridge's two-key `[reasoning]` table: `thinking` (master switch, default on — off strips all reasoning parameters from outbound requests) and `effort` (default `"max"`; `off|drop|none|disable|disabled` drop the field entirely; any other value is trimmed, lowercased, and passed through as-is for `oai-chat`). For `anthropic-messages` the known tiers map to `budget_tokens` (`low→1024`, `high→4096`, `max→16384`, clamped below `max_tokens`); an unknown custom value warns and falls back to the `high` tier.

Consequences: this is a breaking change for existing deployments — migrate by creating `~/.ma/config.toml` with the old `MA_*` values translated to their snake_case keys (`MA_LOG_FILE_DIR` → `log_file_dir`, comma-separated `MA_DENY_TOOLS=bash` → `deny_tools = ["bash"]`, etc.). Two deliberate behavior changes ride along: the default effort rose from `high` to `max`, and `-r/--run`'s classification of plan paths is unchanged but the session log's `run_start` event now records the resolved `config` path. Multiple simultaneous configurations are handled manually by keeping alternate files and passing `--config`.
