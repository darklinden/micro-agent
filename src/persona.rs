//! System prompt assembly: `[prefix] + persona + [suffix]`.
//!
//! `MA_SYSTEM_PREFIX` / `MA_SYSTEM_SUFFIX` may be either a literal string or a
//! path to a file whose contents are inlined (pointing a suffix at a
//! `CLAUDE.md` thus injects project context). `MA_PERSONA` — when set —
//! *replaces* the built-in persona entirely; otherwise the built-in default is
//! used.
//!
//! Override priority ([`build_effective`]): a CLI `-s/--system-prompt` value
//! wins, then the `MA_SYSTEM_PROMPT` environment variable; both replace the
//! whole composite above. With neither set, the prefix+persona+suffix composite
//! is used unchanged.

use anyhow::Result;
use std::path::Path;

/// The built-in default persona, distilled from Claude Code's identity
/// sections: an autonomous agent that drives the tool loop to completion.
pub const DEFAULT_PERSONA: &str = r#"You are ma, a lightweight autonomous CLI agent that helps users with software engineering tasks.

You work end-to-end using the tools available to you. Respond to the user's request by taking whatever tool actions are needed, then give a concise final answer.

Guidelines:
- Use tools liberally rather than guessing: read files before editing, list directories before searching blind, and prefer existing content over assumptions.
- When a tool request fails or is rejected, adapt: change your approach and try again rather than giving up.
- Keep commentary during tool use brief. Your final message is the deliverable — make it clear, accurate, and complete.
- Never invent tool output that you did not actually observe."#;

/// Instructions appended to the system prompt in planning mode: explore with
/// read-only tools and submit a complete numbered plan via the `plan` tool.
pub const MODE_PLAN_INSTRUCTIONS: &str = r#"You are in the PLANNING phase. Your deliverable is a plan, not an implementation.

Guidelines:
- Explore with the read-only tools (read_file, grep, glob, web_fetch, and gated bash) to ground the plan in the actual codebase before writing it.
- Produce a complete, numbered, actionable plan: each step has a clear goal (ideally independently dispatchable and verifiable), names the files/paths it touches, and notes any expected risks.
- Submit the FULL plan with the `plan` tool — that plan is this run's product.
- Do NOT try to implement anything yourself: write_file, edit_file, and task are disabled here."#;

/// Instructions appended to the system prompt in edit mode: revise an existing
/// plan and submit the complete revised version via the `plan` tool.
pub const MODE_EDIT_INSTRUCTIONS: &str = r#"You are revising an existing plan. You will be shown the current plan and a revision request.

Guidelines:
- Keep the parts the request does not touch unchanged.
- Output the COMPLETE revised plan (not a diff) via the `plan` tool; one run produces one revised plan file.
- write_file, edit_file, and task are disabled here — you have only the read-only tools and `plan`."#;

/// Instructions appended to the system prompt in run mode: execute the task,
/// dispatching independent steps to sub-agents via `task`. Serves both a plan
/// file (the fenced plan text is the objective) and an ad-hoc task prompt.
pub const MODE_RUN_INSTRUCTIONS: &str = r#"You are executing a task. The objective below is your fixed input — do not change it (`plan` is disabled).

Guidelines:
- Work through the task in order (for a plan run, that means the plan's steps).
- Dispatch independent, well-scoped sub-tasks to sub-agents with the `task` tool, passing any findings the sub-agent needs as `context`. Do tightly coupled work yourself.
- If reality diverges from the objective, report the deviation in your final answer rather than editing it."#;

/// Resolve a prefix/suffix value that may be either a literal string or a
/// path to a file. If `value` names an existing readable file on disk, its
/// contents are returned; otherwise `value` is treated as a literal string.
fn resolve(value: &str) -> Result<String> {
    let p = Path::new(value);
    if p.is_file() {
        let content = std::fs::read_to_string(p)?;
        Ok(content.trim_end().to_string())
    } else {
        Ok(value.to_string())
    }
}

/// Build the final system prompt for this run.
pub fn build(cfg: &crate::config::Config) -> Result<String> {
    let persona = match &cfg.persona {
        Some(p) => resolve(p)?,
        None => DEFAULT_PERSONA.to_string(),
    };

    let mut parts: Vec<String> = Vec::new();
    if let Some(prefix) = &cfg.system_prefix {
        parts.push(resolve(prefix)?);
    }
    parts.push(persona);
    if let Some(suffix) = &cfg.system_suffix {
        parts.push(resolve(suffix)?);
    }

    Ok(parts.join("\n\n"))
}

/// Build the effective base system prompt for a run.
///
/// Priority: a CLI `-s/--system-prompt` value wins, then the `MA_SYSTEM_PROMPT`
/// environment variable — both *replace* the entire prompt (an empty string is
/// treated as unset). With neither, fall back to the
/// `[prefix] + persona + [suffix]` composite via [`build`].
pub fn build_effective(cfg: &crate::config::Config, cli_override: Option<&str>) -> Result<String> {
    if let Some(p) = cli_override
        .filter(|s| !s.is_empty())
        .or(cfg.system_prompt.as_deref())
    {
        return resolve(p); // full replacement (string or file path)
    }
    build(cfg)
}

#[cfg(test)]
mod tests {
    use super::resolve;

    #[test]
    fn literal_string_used_when_not_a_file() {
        assert_eq!(resolve("just some text").unwrap(), "just some text");
    }

    #[test]
    fn file_contents_used_when_path_exists() {
        let dir = std::env::temp_dir().join(format!("ma-persona-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("proj.md");
        std::fs::write(&p, "Hello from CLAUDE-like file\n").unwrap();
        assert_eq!(resolve(p.to_str().unwrap()).unwrap(), "Hello from CLAUDE-like file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn test_cfg(persona: Option<String>) -> crate::config::Config {
        crate::config::Config {
            upstream_type: crate::config::UpstreamType::OaiChat,
            url: "http://x".into(),
            api_key: "k".into(),
            model: "m".into(),
            max_tokens: 100,
            thinking_effort: crate::config::ThinkingEffort::None,
            extra_headers: vec![],
            max_turns: 5,
            task_max_turns: None,
            deny_tools: vec![],
            gate_enabled: true,
            max_tool_result_bytes: 1000,
            mcp_servers: vec![],
            mcp_list_tools_timeout_ms: 1000,
            system_prefix: Some("PREFIX".into()),
            system_suffix: Some("SUFFIX".into()),
            persona,
            system_prompt: None,
            log_dir: None,
            log_level: "info".into(),
        }
    }

    #[test]
    fn composes_prefix_persona_suffix() {
        let built = crate::persona::build(&test_cfg(None)).unwrap();
        assert!(built.starts_with("PREFIX"));
        assert!(built.ends_with("SUFFIX"));
        // Built-in persona present when not overridden.
        assert!(built.contains("lightweight autonomous CLI agent"));
    }

    #[test]
    fn persona_override_replaces_default() {
        let built = crate::persona::build(&test_cfg(Some("you are the BOX".into()))).unwrap();
        assert!(!built.contains("lightweight autonomous CLI agent"));
        assert!(built.contains("you are the BOX"));
    }

    #[test]
    fn cli_override_replaces_the_composite() {
        // `-s` value fully replaces prefix+persona+suffix and ignores MA_PERSONA.
        let cfg = test_cfg(Some("you are the BOX".into()));
        let built = crate::persona::build_effective(&cfg, Some("you are the CLI")).unwrap();
        assert_eq!(built, "you are the CLI");
    }

    #[test]
    fn env_override_replaces_the_composite() {
        // MA_SYSTEM_PROMPT (cfg.system_prompt) replaces the composite when `-s` is absent.
        let mut cfg = test_cfg(None);
        cfg.system_prompt = Some("you are the ENV".into());
        let built = crate::persona::build_effective(&cfg, None).unwrap();
        assert_eq!(built, "you are the ENV");
    }

    #[test]
    fn cli_wins_over_env_and_empty_cli_is_unset() {
        let mut cfg = test_cfg(None);
        cfg.system_prompt = Some("you are the ENV".into());
        // Empty `-s` treated as unset → env tier applies.
        assert_eq!(crate::persona::build_effective(&cfg, Some("")).unwrap(), "you are the ENV");
        // Non-empty `-s` beats MA_SYSTEM_PROMPT.
        assert_eq!(
            crate::persona::build_effective(&cfg, Some("you are the CLI")).unwrap(),
            "you are the CLI"
        );
    }

    #[test]
    fn no_override_uses_composite() {
        let built = crate::persona::build_effective(&test_cfg(None), None).unwrap();
        assert!(built.starts_with("PREFIX"));
        assert!(built.ends_with("SUFFIX"));
    }

    #[test]
    fn mode_instructions_cover_the_workflow() {
        // Plan mode must point at the `plan` tool; run mode at executing via `task`.
        assert!(crate::persona::MODE_PLAN_INSTRUCTIONS.contains("plan` tool"));
        assert!(crate::persona::MODE_EDIT_INSTRUCTIONS.contains("COMPLETE revised plan"));
        assert!(crate::persona::MODE_RUN_INSTRUCTIONS.contains("`task`"));
    }
}
