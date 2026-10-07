# agentmux — 多 Agent CLI 统一编排工具 · 设计文档

日期：2026-09-28
状态：待评审
代号：agentmux（可更换）

## 1. 项目概述

一个开源的本地工具，将现有的多个 AI coding agent CLI 统一接入并编排。v1 内置支持 Claude Code、Codex、OpenCode、pi 四个 agent；Gemini CLI、kimi、auggie 等其余 ACP 兼容 agent 通过 `[[agents]]` 自定义注册接入，无需改代码。核心形态是 **client-server 架构**：一个 daemon 持有全部会话与编排状态，TUI 和桌面端（二期）作为纯 client 接入。

定位：开源项目，对标 vibe-kanban（编排层）与 claude-squad（TUI），架构同构于 opencode 的 client-server 分层。

**2026-09-30 定位修订（用户已确认）：** agentmux 是管理 Agent 的壳，不应裁剪 Agent 原生能力。采用“原生托管保完整、结构化接入做增强”；下文 v1 排除 PTY 的旧边界被修订，原生托管现在是必做项。恢复必须加载原生对话，不能静默新建替代。实施与验收见 [Agent 能力完整性计划](../plans/2026-09-30-agent-capability-preservation-plan.md)。

技术栈：**Rust**。ACP（Agent Client Protocol）作为统一事件模型与 agent 接入协议。

## 2. 目标与非目标

### v1 目标

- 多 agent 并行会话管理：会话创建 / 切换 / 状态监控 / 中断
- 每个任务独立的 git worktree 隔离
- **共享工作区**：多个 agent session 可挂载到同一 worktree 协同开发
- **共享黑板**：workspace 内的 `.agentmux/context.md` + 自动维护的 `activity.md`
- **手动接力**：将某会话的输出/diff 一键引用进另一会话的 prompt
- TUI 前端（ratatui）
- daemon + JSON-RPC API（为桌面端二期留好接口）

### 非目标（v1 明确不做）

- agent 间自动委派路由（coordinator 模式）— backlog
- ~~PTY 透传模式（不内嵌 agent 自带 TUI）— 只做结构化接入~~ — 2026-09-30 已改为必做项，分阶段实现
- 共享 worktree 内的文件锁 / 并发冲突仲裁 — 依赖 git 本身
- 远程执行（SSH/daemon 在远端）— 本地单机
- 桌面端 GUI — Phase 2
- 移动端 / Web 前端

## 3. 总体架构

```
┌─────────┐   ┌─────────┐
│   TUI   │   │ Desktop │  (Phase 2, Tauri v2)
│(ratatui)│   │         │
└────┬────┘   └────┬────┘
     │  JSON-RPC 2.0 over Unix socket
     └──────┬──────┘
            ▼
     ┌─────────────┐
     │   server    │  daemon：会话所有权、状态机、事件广播
     └──────┬──────┘
            │
     ┌──────┴──────┐
     │    core     │  领域模型 + AgentAdapter + EventStore + WorktreeManager
     └──────┬──────┘
            │ ACP (JSON-RPC over stdio)
   ┌────────┼────────┬─────────┐
   ▼        ▼        ▼         ▼
claude-  codex-   opencode   pi(rpc→ACP
code-acp  acp      acp       翻译器)
```

Cargo workspace 结构：

| crate | 职责 |
|---|---|
| `core` | 领域模型、AgentAdapter trait、ACP client、WorktreeManager、EventStore。不依赖任何 UI |
| `server` | daemon 二进制：Unix socket 服务、JSON-RPC API、事件广播、自动拉起支持 |
| `tui` | ratatui 前端，纯 client，不直接接触 core |
| `desktop` | （二期）Tauri v2，复用 server API |

关键原则：**TUI 第一天就当 client 写**，桌面端二期接入零架构重构。会话归 daemon 所有，client 崩溃/退出不影响 agent 运行。

## 4. 领域模型

| 实体 | 字段要点 | 说明 |
|---|---|---|
| `Project` | id, root_path, name | 注册的 git 仓库根目录 |
| `Workspace` | id, project_id, name, worktree_path, branch | 一级实体，独立生命周期；对应一个 git worktree |
| `AgentProfile` | id, name, command, args, adapter_kind, env, capabilities, available | 可编排 agent 的描述与可用性探测结果 |
| `Session` | id, workspace_id, agent_id, state, acp_session_id, references[] | 一次编排会话；N 个 session 可共享同一 workspace |
| `Event` | session_id, seq, kind, payload, ts | 归一化事件 = ACP SessionUpdate 超集 + 编排事件（状态/worktree/进程健康） |

`Session` 状态机：`created → connecting → ready → prompting → waiting_permission → done | error`（可 resume）。

## 5. Agent 适配层

### 接口

每个 `Session` 对应一个 spawn 出的 ACP stdio 子进程（崩溃隔离、生命周期简单）。daemon 侧实现 ACP `Client` trait（使用官方 `agent-client-protocol` crate）。

### 内置适配

| Agent | 接入方式 |
|---|---|
| Claude Code | `claude-code-acp` 适配器子进程 |
| Codex | `codex-acp` 适配器子进程 |
| OpenCode | `opencode acp` 原生 ACP 命令 |
| pi | 内部翻译器：spawn `pi --mode rpc`，将其 JSONL 命令/事件协议翻译为 ACP client 接口 |

### 自定义 agent

`config.toml` 中 `[[agents]]` 注册任意 ACP 命令（kimi、auggie 等），零改动接入。daemon 启动时探测各 binary 可用性，`agent.list` 返回结果。

## 6. 共享协作空间（多 agent 协同）

### 共享 worktree

`session.create` 接受已有 `workspace_id` → 多个 agent session 在同一 worktree 工作，彼此可见文件改动。

适用范围（诚实声明）：适合**先后接力**（A 搭架子、B 填肉）与**分区并行**（各改各的目录）；不支持两个 agent 同时改同一文件的并发协作——那是 git 层的真实冲突，v1 不加锁。

### 共享黑板

每个 workspace 建立 `.agentmux/` 目录：

- `context.md` — 共享上下文，人和 agent 均可写
- `activity.md` — daemon 自动维护的活动摘要：从 ACP `session/update` 中提取文件编辑/tool 事件，追加「哪个 agent 改了哪些文件、什么时间」

### 上下文注入

当 workspace 挂载 ≥2 个 session 时，adapter 在每次 `session/prompt` 前自动拼接共享上下文摘要（context.md 内容 + 最近 activity 摘要）。所有 ACP agent 零协议成本获得协作感知。

### 手动接力

TUI 中选中会话 A 的事件/diff → `@` → 选目标 session → 内容作为引用文本拼进 B 的 prompt。纯 client 侧拼装；`Session.references` 落库仅用于溯源标注。

## 7. Daemon API（JSON-RPC 2.0 over Unix socket）

方法概要（schema 刻意做成 ACP 镜像形状）：

- `project.register` / `project.list` / `project.remove`
- `workspace.create` / `workspace.list` / `workspace.remove`
- `session.create(workspace_id, agent_id, prompt?)` / `session.prompt` / `session.cancel` / `session.kill` / `session.list` / `session.resume`
- `session.subscribe` → 服务端推送归一化 Event 流（多 client 订阅同一会话收到相同流）
- `agent.list`（含可用性探测）/ `agent.register`
- `server.status` / `server.shutdown`

## 8. 前端

### TUI v1

- 布局：左侧会话列表（按 workspace 分组、状态徽标），右侧事件流视图（消息 / tool call / diff / 权限弹窗），底部输入框
- 快捷键：`n` 新会话、`j/k` 切换、`Tab` 预览↔diff、`@` 手动接力、`ctrl-c` 中断 prompt、`q` 退出
- 渲染：ratatui + markdown 渲染

### Desktop（Phase 2）

Tauri v2，webview 前端连接同一 daemon API。设计预留：所有 UI 状态必须从 Event 流可重建。

## 9. 数据流（发一条 prompt 的完整路径）

1. TUI → server: `session/prompt`
2. `SessionManager` 路由到该 Session 的 adapter
3. adapter 拼接共享上下文（若共享 workspace）
4. ACP `session/prompt` over stdio → agent 子进程
5. agent 推 `session/update` notifications
6. adapter 归一化为 `Event` → EventStore 落盘 + 广播给所有订阅 client
7. 若事件为文件编辑 → 追加 workspace `activity.md`

## 10. 持久化与文件布局

- 配置：`~/.config/agentmux/config.toml`（agents 注册、超时、默认行为）
- 数据：`~/.local/share/agentmux/db.sqlite`（元数据）+ `sessions/<id>.jsonl`（事件流，可回放重建 UI 状态）
- socket：`~/.local/share/agentmux/agentmux.sock`（Unix socket，daemon 监听地址）
- worktree：默认 `<project>/.agentmux/worktrees/<name>`，可配置

## 11. 错误处理

| 场景 | 策略 |
|---|---|
| agent 子进程崩溃 | session → error，事件日志保留，可 resume/restart |
| TUI 启动时 socket 不存在 | 自动拉起 daemon（detached）；`--serve` 常驻模式可选 |
| worktree 创建失败 | 回滚，报错给 client |
| ACP `session/request_permission` | TUI 弹确认框，选择经 ACP 回传 |
| initialize/prompt 超时 | 可配置超时，超时标 error 并保留现场 |
| daemon 崩溃 | client 显示断连，事件流可从 JSONL 恢复 |

## 12. 测试策略

- core 单测：worktree 操作、事件归一化、SQLite store、pi 翻译器
- 端到端：mock ACP agent（echo / 假 tool call）跑通 spawn→prompt→事件流，**CI 不需要真实 API key**
- 真 agent 冒烟测试：本地手动 gated，不进 CI
- TUI：ratatui test backend snapshot 测试

## 13. 开源周边

- 双协议 MIT / Apache-2.0
- GitHub Actions CI：fmt + clippy + 测试
- README 对标项目致谢（vibe-kanban / claude-squad / opencode / Zed ACP）

## 14. Backlog（v1 之后）

- Tauri 桌面端
- agent 自动委派路由 / coordinator 模式
- PTY 透传模式
- 共享 worktree 文件锁 / 冲突仲裁
- 远程 daemon（SSH）
- `claude -p` / `codex exec` 原生 headless 格式适配（不走 ACP 适配器的路径）
- Web 前端
