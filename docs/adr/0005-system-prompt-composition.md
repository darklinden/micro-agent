# 0005 — System prompt is `[prefix] + persona + [suffix]`, with a CLI/env override tier

- Status: Accepted
- Date: 2026-08-20

## Context

The system prompt must be configurable for different projects/use cases, and
it should be easy to inject project context (e.g. a `CLAUDE.md`) without
special-casing file discovery.

## Decision

Final system prompt = `MA_SYSTEM_PREFIX` + persona + `MA_SYSTEM_SUFFIX`.

- `MA_SYSTEM_PREFIX` / `MA_SYSTEM_SUFFIX` may be a literal string **or a path
  to a file**; if the value names an existing readable file its contents are
  inlined, otherwise it is treated as a literal string. Pointing a suffix at a
  project `CLAUDE.md` thus injects project context for free.
- `MA_PERSONA` — when set — **replaces** the built-in persona entirely;
  otherwise the built-in default is used.
- The default persona is distilled from Claude Code's identity sections (an
  autonomous agent that drives the tool loop), not a copy of its ~40 prompt
  sections.
- A higher-override tier replaces the whole composite: `-s/--system-prompt`
  (CLI) wins, then `MA_SYSTEM_PROMPT` (env), then the composite above. The
  plan/edit/run mode instructions are still appended after whichever base is
  chosen, so the workflow protocol survives a full swap.

## Consequences

- Project context is opt-in and explicit, no hidden auto-discovery.
- `MA_PERSONA` gives a clean override seam for custom agent personalities.
- `-s` / `MA_SYSTEM_PROMPT` gives a whole-prompt override seam for users who
  want to author the prompt outside the prefix/persona/suffix split.
