# 0001 — Upstream type is explicit, not guessed

- Status: Accepted (with `micro-agent`)
- Date: 2026-08-20

## Context

`ma` can forward to two provider families: Anthropic Messages and OpenAI Chat
Completions (plus compatible endpoints such as DeepSeek/Ollama/vLLM). Their
request/response shapes differ enough that the format must be known up front.

Detecting the format from the URL is fragile: `anthropic.com/v1/messages`
could be misjudged as a chat endpoint, and self-hosted proxies blur the line.

## Decision

Adopt the `ai-bridge` convention: `UPSTREAM_TYPE` is **required** and declares
the format explicitly — `anthropic-messages` or `oai-chat`. Configuration uses
the unified names `UPSTREAM_URL` / `UPSTREAM_API_KEY` / `UPSTREAM_MODEL`.
No URL heuristics.

## Consequences

- Missing `UPSTREAM_TYPE` is a configuration error (exit 2).
- Adding a third provider family only means a new `UPSTREAM_TYPE` value,
  not new detection logic.
- Providers whose clients speak one of the two formats are usable unchanged.
