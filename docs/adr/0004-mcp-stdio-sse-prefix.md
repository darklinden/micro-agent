# 0004 — MCP over stdio and SSE, tools namespaced with `mcp:`

- Status: Accepted
- Date: 2026-08-20

## Context

`ma` must surface third-party tools over MCP. Servers connect either as a
child process (stdio) or a remote endpoint (SSE). Tool names from different
servers can collide with each other and with built-ins (`read_file` etc.).

## Decision

- Support both transports now: **stdio** (spawn a child process, JSON-RPC over
  stdio) and **SSE** (remote endpoint). Servers are configured via
  `MA_MCP_SERVERS`, a JSON array; each entry has a `name` plus either
  `cmd`/`args`/`env` (stdio) or `url` (SSE).
- Use the `rmcp` crate as the MCP client.
- MCP tools are exposed to the model with an `mcp:` prefix, e.g.
  `mcp:fs.read_file`, so they never collide with built-ins and the routing
  prefix is obvious. Dispatch strips the prefix and routes to the owning server.

## Consequences

- Users can attach any stdio or SSE MCP server without code changes.
- Namespacing keeps the merged tool list unambiguous.
- SSE reliability (reconnect) is out of scope for v1.
- SSE endpoints work over both `http://` and `https://`; TLS uses the rustls
  provider `aws-lc-rs` (bundled, built via cmake — no system OpenSSL needed).
