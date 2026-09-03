# ma — Domain Context

`ma` 是一个微型 CLI agent（无 TUI），配置收敛在单一 TOML 文件 `~/.ma/config.toml`（可用 `--config` 换文件；ADR-0008），复用 `ai-bridge` 的上游与 `[reasoning]` 约定。以下是领域术语表（不含实现细节）。

## Upstream

代理实际请求的 AI 模型 API，由 `upstream_type` 显式声明。目前支持 `anthropic-messages`（Anthropic Messages 协议）与 `oai-chat`（OpenAI Chat Completions / 兼容端点）二者之一。_Avoid_: 上游, backend, provider, upstream type detection 的 URL 启发式。

统一命名：`upstream_type` / `url` / `api_key` / `model`（config.toml 键）。上游格式是显式声明的，绝不通过 URL 猜（避免把 `anthropic.com/v1/messages` 误判为 chat）。

## Agent

`ma` 的主循环：把对话发给 Upstream，**流式**把模型文本打到 stdout，执行模型请求的工具调用并把结果回喂，直到模型给出纯文本答复或撞到回合预算 `max_turns`。_Avoid_: chatbot, repl, interactive.

## 工作流（Workflow）

三种互斥模式的选择由 CLI 结构强制（没有计划就没有 `-r` 的输入，无需运行时门禁）：
- `-p/--plan` 规划：只用读工具探索，用 `plan` 工具提交编号计划；打印计划全文与 `[plan] <路径>`。
- `-e/--edit-plan` + `-c/--change` 修改：修订既有计划，写**新时间戳文件**（旧版保留成演进链），输出新路径。
- `-r/--run` 执行：按计划逐步执行，独立步骤用 `task` 派发给子代理；`plan` 被冻结（禁用）。
三种模式均可叠加 `--context <log>`（见「上下文重放」）。模式 = deny 叠加 + 模式提示词（`MODE_PLAN/EDIT/RUN_INSTRUCTIONS`）+ objective 构造；deny 只叠加不删除用户的 `deny_tools`。

## 工具

模型可以调用的能力集合 = 内置工具 + MCP 工具（带 `mcp:` 前缀）。内置 9 个：`read_file` `write_file` `edit_file` `grep` `glob` `bash` `plan` `task` `web_fetch`。`plan` 提交/更新本运行的编号计划（打印全文、原子写入 `.ma/plans/<yyyymmdd-hhmmss>.md`）；`task` 派发一个子代理执行聚焦子任务（独立回合循环、最终报告作为工具结果、不可嵌套）。_Avoid_: function, plugin.

## 子代理（Sub-agent）

由 `task` 派发的嵌套 `Agent` 运行，深度上限 1；看不到父对话，最终消息即报告；stdout 静默（仅 `[task] started/finished` 横幅），细节进日志（`depth` 字段）；回合预算 `task_max_turns`（缺省继承 `max_turns`）。_Avoid_: process, thread, spawn（非操作系统进程）、plan mode、approval.

## 计划文件（Plan file）

`.ma/plans/<yyyymmdd-hhmmss>.md` 一次运行一份；`edit` 产生新时间戳文件、旧版保留成演进链。写入走同目录 `.tmp` + `rename` 原子替换——进程中段被杀只会留下旧版或完整新版，绝不截断。_Avoid_: plan mode.

## 安全闸门（Gate）

只有 `bash` 类命令在真正执行前会过闸。可证明只读的命令（白名单命令，或拆段后**每一段**都是白名单只读的 `;`/`&&` 链）在 judge 之前短路、直接放行，无 LLM 往返；其余命令另起一次 LLM 请求，让模型结合「当前任务语境 + 命令」判定是否执行、危害多大；judge 任何失败默认**拒绝**（fail-safe），拒绝按 `kind`（Judge/Unparseable/UpstreamError）记入会话日志。`gate = false` 关闭。_Avoid_: approval, permission, trust dialog（此 agent 无人工确认/信任流程）。

## MCP

Model Context Protocol 客户端，支持 **stdio**（子进程）与 **SSE**（远程）两种传输。MCP server 暴露的工具合并进 Agent 的工具列表，名称带 `mcp:<server>:<tool>` 前缀。_Avoid_: mcp server（此 agent 是 client，不是 server）。

## 会话日志（SessionLog）

`log_file_dir/<yyyyMMdd-HHmmss>.log`，每次运行一个**严格 JSONL** 文件：每行一个 JSON 对象，公共字段 `v`/`ts`/`level`/`ev`。session 级事实（`run_start`/`system`/`tools`/`objective`）只在启动写一次，此后只追加增量事件（`message`/`tool_call`/`tool_result_raw`/`gate`/`turn`/`subagent`/`plan_saved`/`request`/`run_end`），绝无整包请求转储。stdout 打 `[log] <路径>` 横幅指路。_Avoid_: tracing 全量 body dump, 多行 JSON, 双写混淆。

## 上下文重放（Context replay）

`--context <log>`（仅长形式）把上次运行的会话日志中 depth==0 的 `message` 事件反解为消息历史，作为本次的种子，再追加新的任务指令——类似续聊。三种模式通用；种子连同本轮对话会重新写入新日志，故每个日志都自含完整血统链。子代理（depth>0）的消息不参与重放。_Avoid_: resume（非会话恢复语义）、summary 注入（是完整重放而非摘要）、`-c`（已被 `--change` 占用）。

## stdout 与日志

**stdout**：用户唯一看到的通道——模型的流式文本 + 精简工具标记（`⧗ …`）。**会话日志**：见上，机器可解析的 JSONL 事件流，供审计调试与 `--context` 重放。二者分离。_Avoid_: logging 双写混淆。

## 系统提示词

构造顺序 `[system_prefix] + persona + [system_suffix]`。prefix / suffix 可以是字面字符串**或文件路径**（指向 `CLAUDE.md` 即注入工程上下文）。`persona` 设置时**整体替换**内置 persona，否则用内置默认；`system_prompt` 或 CLI `-s` 则替换整个组合。

## 配置文件（Config）

单一 TOML 文件 `~/.ma/config.toml`（ADR-0008）：必填 `upstream_type`/`url`/`api_key`，其余可选带默认；未知键启动即报错（typo 安全）；文件缺失时自动落全注释 starter 模板再报 missing field。`--config <file>` 手动切换多套配置。推理策略用 ai-bridge 的 `[reasoning]` 双键：`thinking` 总开关 + `effort`（off/drop/none/disable/disabled = 不发字段，其余值小写透传）。_Avoid_: env vars、`.env` 加载（已移除，仅 `$HOME` 用于定位配置）、profile 自动记录。

## 决策（Decision）索引

- 上游格式显式声明（`upstream_type` 必填，不猜 URL）
- 配置收敛于 `~/.ma/config.toml` 单文件：deny_unknown_fields + starter 模板 + `--config` 覆盖；env 与 `.env` 全部移除（0008）
- 推理参数走 `[reasoning]` 双键（thinking 总开关 / effort 透传，对齐 ai-bridge；anthropic 已知档位映射 budget_tokens）
- Agent 纯 auto、无人工确认 / 无信任 / 无权限弹窗
- 内置工具仅命令类过安全闸门
- MCP 工具统一 `mcp:` 前缀
- 系统提示词 prefix/suffix/persona 三段式
- 输出 stdout/日志分离
- 三段式工作流：`-p` plan / `-e -c` edit / `-r` run，顺序由 CLI 结构强制（0006）
- 计划 `.ma/plans/<ts>.md`，edit 写新时间戳文件；`task` 派发子代理（深度 1）（0006）
- 会话日志为严格 JSONL 事件流（session 头一次 + 增量追加），tracing 依赖移除（0007）
- `--context <log>` 完整重放顶层对话为种子，日志自含血统链（0007）

详见 `docs/adr/`。
