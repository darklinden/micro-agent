//! Environment-driven configuration.
//!
//! Mirrors the `ai-bridge` upstream convention under the `MA_` namespace: the
//! upstream format is declared explicitly via `MA_UPSTREAM_TYPE` (no URL
//! heuristics), with the unified `MA_UPSTREAM_URL` / `MA_UPSTREAM_API_KEY` /
//! `MA_UPSTREAM_MODEL` names. Agent behaviour is tuned through `MA_*` variables.

use crate::mcp::McpServerConfig;
use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamType {
    AnthropicMessages,
    OaiChat,
}

impl UpstreamType {
    fn parse(s: &str) -> Result<Self> {
        match s {
            "anthropic-messages" => Ok(UpstreamType::AnthropicMessages),
            "oai-chat" => Ok(UpstreamType::OaiChat),
            other => Err(anyhow!(
                "invalid MA_UPSTREAM_TYPE {other:?}: expected \"anthropic-messages\" or \"oai-chat\""
            )),
        }
    }

    fn default_model(self) -> &'static str {
        match self {
            UpstreamType::AnthropicMessages => "claude-sonnet-4-5",
            UpstreamType::OaiChat => "deepseek-v4-flash",
        }
    }
}

/// Reasoning-effort tuning for the `MA_THINKING_EFFORT` variable. Mirrors the
/// ai-bridge convention: a single value is adapted per provider — OpenAI-style
/// `reasoning_effort` for `oai-chat`, an Anthropic `thinking` block for
/// `anthropic-messages` — and `None` sends nothing (thinking left to the model).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingEffort {
    None,
    Low,
    High,
    Max,
}

impl ThinkingEffort {
    fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "" | "none" => Ok(ThinkingEffort::None),
            "low" => Ok(ThinkingEffort::Low),
            "high" => Ok(ThinkingEffort::High),
            "max" => Ok(ThinkingEffort::Max),
            other => Err(anyhow!(
                "invalid MA_THINKING_EFFORT {other:?}: expected \"none\", \"low\", \"high\", or \"max\""
            )),
        }
    }

    /// The OpenAI `reasoning_effort` value to emit for an `oai-chat` upstream,
    /// or `None` to omit the field entirely.
    pub fn as_audience_effort(self) -> Option<&'static str> {
        match self {
            ThinkingEffort::Low => Some("low"),
            ThinkingEffort::High => Some("high"),
            ThinkingEffort::Max => Some("max"),
            ThinkingEffort::None => None,
        }
    }

    /// Rough `budget_tokens` for an Anthropic `thinking` block, or `None` to
    /// omit thinking. Callers clamp the value below `MA_MAX_TOKENS`.
    pub fn anthropic_budget(self) -> Option<usize> {
        match self {
            ThinkingEffort::Low => Some(1024),
            ThinkingEffort::High => Some(4096),
            ThinkingEffort::Max => Some(16_384),
            ThinkingEffort::None => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub upstream_type: UpstreamType,
    pub url: String,
    pub api_key: String,
    pub model: String,
    pub max_tokens: usize,
    pub thinking_effort: ThinkingEffort,
    /// Extra headers applied to every upstream request, e.g. `Name: value`.
    pub extra_headers: Vec<(String, String)>,

    // Agent behaviour
    pub max_turns: usize,
    /// Comma-separated names of tools that must not be invoked (e.g. `bash`).
    pub deny_tools: Vec<String>,
    /// Whether the LLM safety gate guards `bash` execution. `MA_GATE=0` disables.
    pub gate_enabled: bool,
    pub max_tool_result_bytes: usize,

    // MCP
    pub mcp_servers: Vec<McpServerConfig>,
    pub mcp_list_tools_timeout_ms: u64,

    // System prompt
    pub system_prefix: Option<String>,
    pub system_suffix: Option<String>,
    pub persona: Option<String>,

    // Logging
    pub log_dir: Option<PathBuf>,
    pub log_level: String,
}

fn get(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("missing required environment variable {name}"))
}

fn get_opt(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.is_empty())
}

fn parse_extra_headers() -> Vec<(String, String)> {
    // `MA_HEADERS` is a JSON object of string pairs.
    let raw = match get_opt("MA_HEADERS") {
        Some(v) => v,
        None => return Vec::new(),
    };
    match serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&raw) {
        Ok(map) => map
            .into_iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
            .collect(),
        Err(_) => Vec::new(),
    }
}

pub fn from_env() -> Result<Config> {
    let upstream_type = UpstreamType::parse(&get("MA_UPSTREAM_TYPE")?)?;
    let url = get("MA_UPSTREAM_URL")?;
    let api_key = get("MA_UPSTREAM_API_KEY")?;
    let model = get_opt("MA_UPSTREAM_MODEL").unwrap_or_else(|| upstream_type.default_model().into());

    let max_tokens = get_opt("MA_MAX_TOKENS")
        .map(|v| v.parse().context("MA_MAX_TOKENS must be an integer"))
        .transpose()?
        .unwrap_or(4096);

    let max_turns = get_opt("MA_MAX_TURNS")
        .map(|v| v.parse().context("MA_MAX_TURNS must be an integer"))
        .transpose()?
        .unwrap_or(20);

    let deny_tools = get_opt("MA_DENY_TOOLS")
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let gate_enabled = match get_opt("MA_GATE") {
        Some(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
        None => true,
    };

    let max_tool_result_bytes = get_opt("MA_MAX_TOOL_RESULT_BYTES")
        .map(|v| v.parse().context("MA_MAX_TOOL_RESULT_BYTES must be an integer"))
        .transpose()?
        .unwrap_or(32 * 1024);

    let mcp_servers = match get_opt("MA_MCP_SERVERS") {
        Some(v) => serde_json::from_str::<Vec<McpServerConfig>>(&v)
            .with_context(|| "MA_MCP_SERVERS must be a JSON array")?,
        None => Vec::new(),
    };

    let mcp_list_tools_timeout_ms = get_opt("MA_MCP_LIST_TOOLS_TIMEOUT_MS")
        .map(|v| v.parse().context("MA_MCP_LIST_TOOLS_TIMEOUT_MS must be an integer"))
        .transpose()?
        .unwrap_or(10_000);

    let log_dir = get_opt("MA_LOG_FILE_DIR").map(PathBuf::from);
    let log_level = get_opt("MA_LOG_LEVEL").unwrap_or_else(|| "info".into());

    let thinking_effort = match get_opt("MA_THINKING_EFFORT") {
        Some(v) => ThinkingEffort::parse(&v)?,
        None => ThinkingEffort::High,
    };

    Ok(Config {
        upstream_type,
        url,
        api_key,
        model,
        max_tokens,
        thinking_effort,
        extra_headers: parse_extra_headers(),
        max_turns,
        deny_tools,
        gate_enabled,
        max_tool_result_bytes,
        mcp_servers,
        mcp_list_tools_timeout_ms,
        system_prefix: get_opt("MA_SYSTEM_PREFIX"),
        system_suffix: get_opt("MA_SYSTEM_SUFFIX"),
        persona: get_opt("MA_PERSONA"),
        log_dir,
        log_level,
    })
}
