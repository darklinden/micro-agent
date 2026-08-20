# ma — Domain Context

`ma` 是一个微型 CLI agent（无 TUI），复用 `ai-bridge` 的 `UPSTREAM_*` 上游约定与 `MA_*` agent 行为约定。以下是领域术语表（不含实现细节）。

## Upstream

代理实际请求的 AI 模型 API，由 `UPSTREAM_TYPE` 显式声明。目前支持 `anthropic-messages`（Anthropic Messages 协议）与 `oai-chat`（OpenAI Chat Completions / 兼容端点）二者之一。_Avoid_: 上游, backend, provider, upstream type detection 的 URL 启发式。

统一命名：`UPSTREAM_TYPE` / `UPSTREAM_URL` / `UPSTREAM_API_KEY` / `UPSTREAM_MODEL`。上游格式是显式声明的，绝不通过 URL 猜（避免把 `anthropic.com/v1/messages` 误判为 chat）。

## Agent

`ma` 的主循环：把对话发给 Upstream，**流式**把模型文本打到 stdout，执行模型请求的工具调用并把结果回喂，直到模型给出纯文本答复或撞到回合预算 `MA_MAX_TURNS`。_Avoid_: chatbot, repl, interactive.

## 工具

模型可以调用的能力集合 = 内置工具 + MCP 工具（带 `mcp:` 前缀）。内置 8 个：`read_file` `write_file` `edit_file` `grep` `glob` `bash` `task` `web_fetch`。_Avoid_: function, plugin.

## 安全闸门（Gate）

只有 `bash` 类命令在真正执行前，会另起一次 LLM 请求，让模型结合「当前任务语境 + 命令」判定是否执行、危害多大；任何失败默认**拒绝**。`MA_GATE=0` 关闭。_Avoid_: approval, permission, trust dialog（此 agent 无人工确认/信任流程）。

## MCP

Model Context Protocol 客户端，支持 **stdio**（子进程）与 **SSE**（远程）两种传输。MCP server 暴露的工具合并进 Agent 的工具列表，名称带 `mcp:<server>:<tool>` 前缀。_Avoid_: mcp server（此 agent 是 client，不是 server）。

## stdout 与日志

**stdout**：用户唯一看到的通道——模型的流式文本 + 精简工具标记（`⧗ …`）。**日志**：`MA_LOG_FILE_DIR/<yyyyMMdd-HHmmss>.log` 每次运行一个文件，记录全部请求/响应/工具细节，供审计调试。二者分离。_Avoid_: logging 双写混淆。

## 系统提示词

构造顺序 `[MA_SYSTEM_PREFIX] + persona + [MA_SYSTEM_SUFFIX]`。prefix / suffix 可以是字面字符串**或文件路径**（指向 `CLAUDE.md` 即注入工程上下文）。`MA_PERSONA` 设置时**整体替换**内置 persona，否则用内置默认。

## 决策（Decision）索引

- 上游格式显式声明（`UPSTREAM_TYPE` 必填，不猜 URL）
- Agent 纯 auto、无人工确认 / 无信任 / 无权限弹窗
- 内置工具仅命令类过安全闸门
- MCP 工具统一 `mcp:` 前缀
- 系统提示词 prefix/suffix/persona 三段式
- 输出 stdout/日志分离

详见 `docs/adr/`。
