//! MCP (Model Context Protocol) client pool.
//!
//! Connects to configured MCP servers — stdio (child process) or SSE — lists
//! their tools with an `mcp-` prefix, and routes tool calls back to them.

use crate::config::Config;
use crate::toolchain::ToolOutput;
use crate::types::ToolDef;
use anyhow::{Context, Result};
use rmcp::model::{CallToolRequestParams, Tool};
use rmcp::service::RunningService;
use rmcp::transport::child_process::TokioChildProcess;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// One configured MCP server (from `MA_MCP_SERVERS`, a JSON array).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    /// stdio: command to spawn.
    #[serde(default)]
    pub cmd: Option<String>,
    /// stdio: arguments for the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// stdio: extra environment variables for the child process.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// SSE: remote endpoint URL (mutually exclusive with `cmd`).
    #[serde(default)]
    pub url: Option<String>,
}

/// A connected MCP server and its native tool list.
struct McpConnection {
    client: RunningService<RoleClient, ()>,
    tools: Vec<Tool>,
}

/// Pool of all connected MCP servers.
pub struct McpPool {
    /// `mcp-<server>-<tool>` defs exposed to the model.
    defs: Vec<ToolDef>,
    connections: Vec<(String, McpConnection)>,
}

impl McpPool {
    /// An empty pool (no servers, no tools) for tests.
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        McpPool {
            defs: Vec::new(),
            connections: Vec::new(),
        }
    }

    pub async fn connect(cfg: &Config) -> Result<McpPool> {
        let timeout = std::time::Duration::from_millis(cfg.mcp_list_tools_timeout_ms);
        let mut defs: Vec<ToolDef> = Vec::new();
        let mut connections: Vec<(String, McpConnection)> = Vec::new();

        for sc in &cfg.mcp_servers {
            let conn = match tokio::time::timeout(timeout, connect_one(sc)).await {
                Ok(Ok(c)) => c,
                Ok(Err(e)) => {
                    tracing::warn!(server = %sc.name, "MCP connect failed: {e:#}; skipping");
                    continue;
                }
                Err(_) => {
                    tracing::warn!(server = %sc.name, "MCP connect/list_tools timed out; skipping");
                    continue;
                }
            };

            for t in &conn.tools {
                let schema = Value::Object((*t.input_schema).clone());
                let desc = t
                    .description
                    .as_deref()
                    .unwrap_or("(no description)")
                    .to_string();
                defs.push(ToolDef {
                    name: format!("mcp-{}--{}", sc.name, t.name),
                    description: format!("MCP tool (server `{}`): {desc}", sc.name),
                    input_schema: schema,
                });
            }
            tracing::info!(server = %sc.name, tools = conn.tools.len(), "MCP server connected");
            connections.push((sc.name.clone(), conn));
        }

        Ok(McpPool { defs, connections })
    }

    /// Tool definitions exposed to the model (MCP tools carry `mcp-` prefix).
    pub fn tools_defs(&self) -> Vec<ToolDef> {
        self.defs.clone()
    }

    /// Call an MCP tool by its `mcp-`-stripped `server--tool` name.
    pub async fn call(&self, full: &str, args: &Value) -> ToolOutput {
        let Some((server, tool)) = full.split_once("--") else {
            return ToolOutput {
                content: format!("invalid MCP tool name `{full}` (expected `server--tool`)"),
                is_error: true,
            };
        };
        let Some((_, conn)) = self.connections.iter().find(|(n, _)| n == server) else {
            return ToolOutput {
                content: format!("unknown MCP server `{server}`"),
                is_error: true,
            };
        };
        if !conn.tools.iter().any(|t| t.name.as_ref() == tool) {
            return ToolOutput {
                content: format!("server `{server}` has no tool `{tool}`"),
                is_error: true,
            };
        }

        let out_params = params_for(tool, args);
        let result = match conn.client.call_tool(out_params).await {
            Ok(r) => r,
            Err(e) => {
                return ToolOutput {
                    content: format!("MCP tool call failed: {e}"),
                    is_error: true,
                }
            }
        };

        let mut text = String::new();
        for block in &result.content {
            if let Some(t) = block.as_text() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&t.text);
            }
        }
        if text.is_empty() {
            text = "(no text content returned)".into();
        }
        ToolOutput {
            content: text,
            is_error: result.is_error == Some(true),
        }
    }
}

fn params_for(tool: &str, args: &Value) -> CallToolRequestParams {
    let obj = args.as_object().cloned().unwrap_or_default();
    CallToolRequestParams::new(tool.to_string()).with_arguments(obj)
}

async fn connect_one(sc: &McpServerConfig) -> Result<McpConnection> {
    let client = if let Some(url) = &sc.url {
        // SSE over streamable HTTP.
        let transport = StreamableHttpClientTransport::from_uri(url.clone());
        ().serve(transport)
            .await
            .map_err(|e| anyhow::anyhow!("SSE connect failed: {e}"))?
    } else {
        // stdio: spawn a child process and speak JSON-RPC over its stdio.
        let cmd = sc.cmd.clone().context("MCP server needs `cmd` or `url`")?;
        let mut command = tokio::process::Command::new(&cmd);
        command.args(&sc.args);
        if !sc.env.is_empty() {
            command.envs(&sc.env);
        }
        let transport = TokioChildProcess::new(command)
            .map_err(|e| anyhow::anyhow!("failed to spawn `{cmd}`: {e}"))?;
        ().serve(transport)
            .await
            .map_err(|e| anyhow::anyhow!("stdio connect failed: {e}"))?
    };

    let tools = client
        .list_tools(None)
        .await
        .map_err(|e| anyhow::anyhow!("list_tools failed: {e}"))?
        .tools;

    Ok(McpConnection { client, tools })
}
