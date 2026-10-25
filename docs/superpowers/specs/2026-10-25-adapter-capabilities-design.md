# 适配器能力清单 · 设计

日期：2026-10-25
状态：待实现
承接：[Agent 能力完整性计划](../plans/2026-09-30-agent-capability-preservation-plan.md) 第三阶段第 1 项

## 1. 问题

- ACP `initialize` 的响应（`agentCapabilities`、`agentInfo`、`authMethods`）目前只在 `orchestrator.rs:1383` 被读一次，用来判断 `loadSession`，随后丢弃。
- Pi 没有握手；原生托管会话没有结构化连接。TUI 无法得知某个会话"实际能做什么"，只能对所有 Agent 展示同一套功能（共同最小集）。
- 后续的模型/模式切换、附件、fork、导出都需要先知道能力，否则只能硬编码按 Agent 名称判断。

## 2. 目标 / 非目标

目标：
- 每个会话在连接成功后有一份**规范化能力清单**，可经 RPC 查询、随事件推送，并持久化到会话记录（daemon 重启后在重新握手前仍可展示上次已知能力）。
- 不同适配器（ACP / Pi RPC / 原生托管）统一到同一结构，调用方不依赖 ACP crate 类型。
- 未知或缺失一律视为"不支持"，不猜测。

非目标：
- 本期不实现任何新的受控功能（模型切换、fork 等），只提供判断依据。
- 不改变现有 `loadSession` 的行为，只把它的判断改为读取清单。

## 3. 数据模型

放在 `core/src/model.rs`：

```rust
pub struct AgentCapabilities {
    pub agent_name: Option<String>,        // agentInfo.name
    pub agent_version: Option<String>,     // agentInfo.version
    pub protocol: CapabilityProtocol,      // Acp { version } | PiRpc | NativeTerminal
    pub load_session: bool,
    pub prompt: PromptCaps,                // image / audio / embedded_context
    pub mcp: McpCaps,                      // http / sse
    pub session_modes: Vec<ModeInfo>,      // 来自 session/new 响应，可能为空
    pub session_models: Vec<ModelInfo>,
    pub auth_methods: Vec<String>,
    pub extras: serde_json::Value,         // 未识别字段原样保留，仅供调试展示
    pub captured_at: DateTime<Utc>,
}
```

要点：
- `session_modes` / `session_models` 不在 `initialize` 里，而在 `session/new`（或 `session/load`）响应里；因此清单分两阶段填充：握手后写入静态部分，会话建立后补全。
- `NativeTerminal` 的清单只含 `protocol` 和 `agent_name`，其余全部为默认值（不支持）——原生会话的真实能力由 Agent 自己的 TUI 提供。
- Pi：由 `get_state` 的结果映射 `agent_version`、当前模型；`load_session` 取决于是否有原生会话文件。

## 4. 数据流

1. `AcpConn::initialize` 仍返回原始 JSON（保持接口不变）；新增 `capabilities::from_acp_initialize(&Value) -> AgentCapabilities`，纯函数，便于表驱动测试。
2. `new_session` / `load_session` 的响应经 `capabilities::merge_session_info` 补全模式与模型。
3. Orchestrator 把清单存入会话（SQLite 新增 `capabilities_json` 列，沿用现有迁移机制，缺失时为 NULL），并发出新事件 `EventKind::CapabilitiesUpdated`。
4. RPC：`session/capabilities`（按会话查询）；`session/list` 的结果不内嵌清单，避免体积膨胀。
5. 重新握手时覆盖旧清单；握手失败保留旧清单并标记 `stale`（TUI 以灰色显示）。

## 5. TUI 使用方式

- Agent 信息面板（`tui/src/agent_info.rs`）显示版本与能力摘要。
- 命令/按键按能力显示或置灰，并给出原因（"该 Agent 不支持恢复历史会话，请使用原生托管"），而不是静默隐藏。
- 附件入口只在 `prompt.image` 等为真时可用。

## 6. 测试

- 表驱动：为 Claude Code、Codex、OpenCode、Pi 各保存一份真实 `initialize` 响应样本（脱敏）到 `core/tests/fixtures/`，断言规范化结果。
- 未知字段进入 `extras`、缺字段为 false 的反例。
- 数据库升级：旧库（无该列）打开后正常，读取为 `None`。
- 集成：mock-agent 增加可配置能力，端到端验证 `session/capabilities` 与事件。

## 7. 分步交付

1. 模型 + `from_acp_initialize` + 样本测试（无行为变化）。
2. 存储列 + 迁移 + orchestrator 写入 + RPC。
3. `loadSession` 判断改读清单；Pi 映射。
4. TUI 展示与置灰。

## 8. 待定问题

- `extras` 是否会无意泄漏敏感字段（如 authMethods 里的环境变量名）？实现时先白名单化。
- 能力随 Agent 升级而变化，是否需要在 daemon 启动时主动刷新？倾向于仅在下次连接时刷新。
