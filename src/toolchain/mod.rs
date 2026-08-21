//! Tool registry, dispatch, and the bash safety gate.
//!
//! The model sees one merged list: built-in tools plus MCP tools (MCP tools
//! carry an `mcp:` prefix). Dispatch routes a call to a built-in or to the
//! matching MCP server, applying the deny-list and the bash gate when needed.

pub mod builtin;
pub mod compress;
pub mod gate;
pub mod subagent;

use crate::config::Config;
use crate::mcp::McpPool;
use crate::toolchain::builtin::BASH;
use crate::toolchain::gate::Gate;
use crate::types::ToolDef;
use crate::upstream::Upstream;
use serde_json::Value;

/// The result of one tool invocation, fed back to the model as a tool result.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

/// Everything a tool execution needs access to.
pub struct ToolCtx<'a> {
    pub cfg: &'a Config,
    pub mcp: &'a McpPool,
    /// Shared upstream client (needed by the `task` sub-agent dispatcher).
    pub upstream: &'a dyn Upstream,
    pub gate: Gate<'a>,
    /// Nesting depth of the running agent (0 = top level). `task` refuses
    /// when depth > 0.
    pub depth: u32,
    /// This run's plan file path: `plan` records it on first write and
    /// overwrites it on updates, so one run produces one plan.
    pub plan_path: std::sync::Arc<std::sync::Mutex<Option<std::path::PathBuf>>>,
}

/// Merge built-in + MCP tool definitions into the list exposed to the model.
pub fn build_tools(mcp: &McpPool) -> Vec<ToolDef> {
    let mut tools = builtin::builtin_defs();
    tools.extend(mcp.tools_defs());
    tools
}

/// Execute a tool by its full name (built-in or `mcp:<server>:<tool>`).
pub async fn run_tool(name: &str, args: &Value, ctx: &ToolCtx<'_>) -> ToolOutput {
    // 1. Deny-list (immediate, no execution path).
    if ctx.cfg.deny_tools.iter().any(|d| d == name) {
        return ToolOutput {
            content: format!(
                "tool `{name}` is denied by configuration (MA_DENY_TOOLS); use another approach"
            ),
            is_error: true,
        };
    }

    // 2. Bash safety gate.
    if name == BASH && ctx.cfg.gate_enabled {
        let cmd = args
            .get("command")
            .and_then(|c| c.as_str())
            .unwrap_or_default();
        let allowed = ctx.gate.check(cmd).await.unwrap_or(false);
        if !allowed {
            tracing::info!(command = %cmd, "bash command refused by gate");
            return ToolOutput {
                content: "The bash command was refused by the safety gate. Adjust the command or \
                          find a safer tool-based alternative (e.g. read_file/write_file/edit_file)."
                    .into(),
                is_error: true,
            };
        }
    }

    // 3. MCP tools.
    if let Some(rest) = name.strip_prefix("mcp:") {
        return ctx.mcp.call(rest, args).await;
    }

    // 4. Built-in tools.
    if let Some(out) = builtin::run(name, args, ctx).await {
        return out;
    }

    ToolOutput {
        content: format!("unknown tool `{name}`"),
        is_error: true,
    }
}
