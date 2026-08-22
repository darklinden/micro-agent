# 0006 — Three-mode workflow: plan → edit-plan → run

- Status: Accepted
- Date: 2026-08-21

## Amendment (2026-08-22)

`-r/--run` no longer strictly requires a plan file on disk. A `-r` value that
is not an existing file is treated as an ad-hoc requirement prompt and executed
directly in run mode — same deny overlay (`plan` disabled, `task` allowed) and
same objective → exit-code mapping. A non-existent value that still *looks like*
a path (no whitespace, and a `/`, a leading `.` or `/`, or a `.md` suffix)
hard-errors with code 2, catching mistyped plan paths. The plan→edit→run
*practice* is unchanged (`-p` then `-r <plan>` still works), and this does not
resurrect the removed `-p/--prompt` single-prompt mode; the value runs with
run-mode semantics, not planning semantics.

## Context

Claude Code implements "plan mode" as a permission mode: everything becomes
read-only except a single writable carve-out (the session plan file), two
sentinel tools (`EnterPlanMode`/`ExitPlanMode`) transition in and out, and
exiting asks the user to approve the plan before any edits. Sub-agents are
nested autonomous loops with restricted tool pools.

`ma` cannot copy that shape directly: ADR-0002 rules out human confirmation
entirely (no TUI, pure auto), and there are no permission modes to toggle. We
wanted the same *practice* — inspect first, write a plan, then execute by
delegating independent steps — without an interactive approval gate.

## Decision

Expose planning as an explicit three-mode CLI instead of an in-loop mode:

```
ma -p/--plan "<task>"                           # write a plan, print it + its path
ma -e/--edit-plan <plan> -c/--change "<req>"    # revise a plan, write a new file
ma -r/--run <plan>                              # execute a plan, dispatching steps
```

- **Ordering is structural**: `-r` needs a plan file on disk, so planning
  provably precedes execution — no runtime "you must plan first" gate needed.
- A **mode = deny overlay + mode prompt + objective construction**. `plan` and
  `edit` deny `write_file`/`edit_file`/`task` (read-only exploration plus the
  `plan` tool); `run` denies `plan` (the plan is a frozen input). Overlays only
  add to — never remove — the user's `MA_DENY_TOOLS` (deny-before-gate ordering
  preserved). The mode instructions append to the persona as a system-prompt
  section (`MODE_PLAN/EDIT/RUN_INSTRUCTIONS`).
- Plans live under `.ma/plans/<yyyymmdd-hhmmss>.md` (timestamped like the log
  files). `edit` writes a **new** timestamped file and keeps the old one, so a
  plan's revisions form a natural chain on disk. The `plan` tool prints the
  full plan to stdout and the run's final path is printed as `[plan] <path>`.
- Writes are **atomic**: content is written to a sibling `<name>.md.tmp` then
  `rename`d, so a kill mid-write leaves either the old or the complete new
  file, never a truncated fragment.
- **`task` becomes a real sub-agent dispatcher** (it was a no-op acknowledgement).
  It spawns a nested `loop_::Agent` at depth+1 with its own safety gate and
  `SUBAGENT_PERSONA`; the sub-agent's final text is returned as the tool result
  (and flows through normal tool-result compression). Nesting is hard-limited
  to depth 1. Sub-agent output is quiet on stdout (only `[task] started/finished`
  banners), full detail goes to the log with a `depth` field.
- Sub-agent turn budget: optional `MA_TASK_MAX_TURNS`, defaulting to inherit
  `MA_MAX_TURNS`.
- `Agent::run` now returns `RunOutcome { result, final_text, turns }` wrapping
  `RunResult`; the exit-code mapping (Done→0, MaxTurns→2) is unchanged.

## Consequences

- Removing the old single `-p/--prompt` "just do it" mode is a **breaking
  change**; users now always pick one of the three modes.
- Each `task` dispatch costs an additional full agent run (money/latency);
  bounded by `MA_TASK_MAX_TURNS` and depth 1.
- Exact enforcement of "plan before run" is at the CLI boundary, not inside the
  loop — internal flexibility is traded for a simpler, stateless core.

## Rejected alternatives

- **In-place overwrite on edit** — not atomic with `fs::write` and it discards
  the previous version, so "preserving history" was self-contradictory.
- **Runtime plan gate** (refuse `task` until a plan file exists) — that would
  re-introduce shared state the CLI already gives us for free.
- **Interactive approval flow** — conflicts with ADR-0002 and `ma` has no UI
  for it.
