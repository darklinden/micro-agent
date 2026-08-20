//! System prompt assembly: `[prefix] + persona + [suffix]`.
//!
//! `MA_SYSTEM_PREFIX` / `MA_SYSTEM_SUFFIX` may be either a literal string or a
//! path to a file whose contents are inlined (pointing a suffix at a
//! `CLAUDE.md` thus injects project context). `MA_PERSONA` — when set —
//! *replaces* the built-in persona entirely; otherwise the built-in default is
//! used.

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
            extra_headers: vec![],
            max_turns: 5,
            deny_tools: vec![],
            gate_enabled: true,
            max_tool_result_bytes: 1000,
            mcp_servers: vec![],
            mcp_list_tools_timeout_ms: 1000,
            system_prefix: Some("PREFIX".into()),
            system_suffix: Some("SUFFIX".into()),
            persona,
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
}
