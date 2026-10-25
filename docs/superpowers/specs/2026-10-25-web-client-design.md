# Web 端 · 设计

日期：2026-10-25
状态：草案，待评审
承接：README Roadmap "Desktop app (Tauri v2) on the same daemon API"

## 1. 目标

让用户在**其他设备**（手机、平板、另一台电脑）的浏览器里使用同一个 daemon：查看会话、收发消息、批准权限请求、审阅并批准委派计划、浏览文件与 diff。

核心原则：
- Web 端只是 daemon 的又一个客户端，与 TUI 并列；核心逻辑不为它改动。
- daemon 能执行命令、改文件。**把它暴露到网络等于暴露远程代码执行**，所以认证与默认安全优先于功能。
- 仍然是"一个二进制 + 静态资源"：前端构建产物嵌入或随 daemon 托管，不引入第二个常驻服务。

## 2. 非目标（本期）

- 公网直接暴露（不自带 TLS 终结与账号体系）；远程访问走反向代理 / Tailscale / SSH 隧道。
- 多用户、多租户、细粒度权限（本期一个 token = 完全控制）。
- 原生终端（native terminal）的网页内嵌；先只支持结构化 ACP 会话。
- Tauri 桌面壳（后续复用同一前端）。

## 3. 总体架构

```
浏览器 ──HTTPS(反代)──▶ agentmux-server ── HTTP/WS 网关 ─┐
                                                          ├─▶ Daemon::dispatch / 事件总线
TUI / CLI ────── Unix socket ─────────────────────────────┘
```

- 新增 `server/src/web.rs`：基于 `axum`（与现有 tokio 一致）。
- **不新写一套 API**：WebSocket 上跑与 Unix socket 完全相同的 JSON-RPC 2.0（每帧一条消息）。复用 `handle_conn` 的逻辑——先把它从 `UnixStream` 泛化为任意 `AsyncRead + AsyncWrite` 的行协议，或抽出"收一条 / 发一条"的传输 trait，让 Unix 与 WebSocket 共享同一份分发、订阅、背压（有界出站队列、`Lagged` 合成事件）实现。
- 静态资源：`GET /` 及 `/assets/*` 由 daemon 托管（`rust-embed` 嵌入 `web/dist`，开发期可用 `--web-dir` 指向目录）。
- 默认**不监听**。需要显式 `--web <addr>`（或配置 `[web] listen = "..."`）才启用。

## 4. 认证与安全（本设计的重点）

### 4.1 威胁

1. 同网段其他主机扫到端口 → 未授权执行。
2. 恶意网页通过浏览器向本地端口发请求（CSRF / WebSocket 劫持）。
3. token 泄露（日志、URL、Referer、浏览器历史）。
4. 明文传输被嗅探（局域网 / 公共 Wi-Fi）。

### 4.2 措施

- **默认绑定 `127.0.0.1`**。绑定非回环地址时必须同时满足：已配置 token，且启动时打印醒目警告；未启用 TLS 且地址非回环时，除非 `--insecure-http` 显式确认，否则拒绝启动，并提示用反向代理或 Tailscale。
- **配对流程**，避免把长期 token 放进 URL：
  1. 在本机运行 `agentmux pair`（或 TUI 菜单项），daemon 生成一次性配对码（8 位以上、5 分钟有效、仅可用一次），显示为文本 + 二维码（编码 `https://host/pair#code=...`，用 fragment 避免进入服务端日志与 Referer）。
  2. 新设备打开页面提交配对码到 `POST /api/pair`，daemon 校验后签发**设备 token**（256 位随机，只存哈希）并返回；前端存入 `localStorage`，并以 `Authorization`/子协议传递。
  3. 设备 token 可列出、命名、单独吊销（`agentmux devices` / TUI / Web 设置页）；支持过期时间，默认 30 天滑动续期。
- **WebSocket 握手校验**：必须带有效设备 token（通过 `Sec-WebSocket-Protocol` 传递，不放 query string）；校验 `Origin` 必须与 `Host` 同源或在 `[web] allowed_origins` 白名单内，否则 403。HTTP API 同样要求 `Authorization: Bearer`，不使用 cookie，从根上避免 CSRF。
- **限速与锁定**：配对接口与失败的 token 校验按来源 IP 限速；配对码连续错误 5 次即作废。
- **响应头**：严格 CSP（仅同源脚本，无内联）、`X-Content-Type-Options`、`Referrer-Policy: no-referrer`、`Cache-Control: no-store`（API）。
- **审计**：设备配对、吊销、登录失败写入 daemon 日志；高风险 RPC（执行命令类的权限批准）在事件流里本来就有记录。
- **TLS**：本期不内置。文档给出两种推荐部署：① Tailscale / WireGuard（零配置、不开公网）；② Caddy/nginx 反向代理终结 TLS。可选后续：`--tls-cert/--tls-key` 用 rustls 直接提供 HTTPS。
- 控制面 RPC 在 Web 通道上做**方法白名单**：禁用 `server/shutdown` 等仅应由本机操作的方法（可由 `[web] allow_admin = true` 打开）。

## 5. 协议与事件

- 请求 / 响应 / 通知格式与 Unix socket 一致，前端直接复用现有 RPC 方法：`session/*`、`workspace/*`、`plan/*`（`plan_*`）、文件 / 上下文相关方法。
- `session/subscribe` 之后收到 `session/event` 通知；断线重连后，前端以最后收到的 `seq` 调用现有的按序号拉取事件接口补齐（每会话事件日志是权威来源，总线有损已有约定）。
- 单帧大小沿用 8 MiB 上限；文件与 diff 大内容走分页接口，不一次推送。
- 心跳：服务端每 20 秒发 WebSocket ping，60 秒无响应断开；前端指数退避重连。
- 在 `core` 的 RPC 类型上用 `ts-rs`（或 `specta`）生成 TypeScript 类型到 `web/src/api/types.ts`，CI 校验生成物无差异，防止前后端漂移。

## 6. 前端

- 技术栈：**TypeScript + React + Vite**，状态用 Zustand（或 TanStack Query 管理请求缓存 + 一个事件 store），样式用 Tailwind；无 SSR，纯静态 SPA。
- 目录：`web/`（独立 `package.json`），`npm run build` 产出 `web/dist`，由 `build.rs` / CI 校验后嵌入二进制。
- 响应式优先考虑手机：单栏导航（工作区 → 会话 → 对话），宽屏时三栏。
- 功能范围按阶段（见 §9）：
  - M1：配对 / 登录、工作区与会话列表、对话流（文本、工具调用折叠、编辑文件摘要）、发送消息与取消、权限请求批准 / 拒绝。
  - M2：委派计划审阅（任务编辑、改派、批准、进度）、文件树与 diff 查看、会话标题 / 重命名、overlap 提示。
  - M3：搜索、共享上下文编辑、设备管理、PWA（可添加到主屏、通知）。
- 渲染：Markdown 用 `react-markdown` + 严格消毒（禁止原始 HTML）；代码高亮用 `shiki` 按需加载；所有来自 agent 的内容一律视为不可信文本。
- 与 TUI 的一致性：状态文案与按钮语义对齐（如计划的 Approve / Resume / Retry / Skip）。

## 7. daemon 端改动清单

| 位置 | 改动 |
|---|---|
| `server/src/rpc_server.rs` | 连接处理泛化为传输无关；可注入"方法白名单"与连接身份 |
| `server/src/web.rs`（新） | axum 路由：静态资源、`/api/pair`、`/ws`；Origin / token 校验；限速 |
| `core/src/` 新增 `devices.rs` | 设备 token（哈希存储）、配对码、吊销、过期；落到现有 SQLite |
| `core/src/rpc.rs` | 新增 `device/list`、`device/revoke`、`pair/create`（仅本地通道可用） |
| `server/src/main.rs` | `--web <addr>`、`--insecure-http`、`--web-dir`；配置 `[web]` 段 |
| `client` / `tui` | `agentmux pair` / `agentmux devices` 命令与 TUI 菜单项 |
| `web/`（新） | 前端工程与类型生成 |

## 8. 测试

- 核心 / 服务端（Rust）：
  - 配对码：一次性、过期、错误次数锁定、并发使用只有一个成功。
  - 设备 token：吊销后现存 WebSocket 立即断开；过期拒绝；只存哈希。
  - WebSocket：无 token / 错 Origin 拒绝；合法连接可走完 `subscribe → prompt → event`；`Lagged` 与超长帧行为与 Unix 通道一致（共用一套传输无关的测试）。
  - 方法白名单：被禁方法返回方法不存在 / 无权限。
  - 默认安全：非回环地址且无 token / 无 TLS 确认时拒绝启动。
- 前端：Vitest 单元测试（事件 reducer、重连补齐逻辑）；Playwright 端到端对着 mock agent 跑完"配对 → 发消息 → 批准权限 → 审阅并批准计划"，覆盖手机与桌面两种视口。
- 类型生成一致性检查纳入 CI。

## 9. 分步交付

1. **传输泛化**：把连接处理抽成传输无关，Unix 通道行为不变，全部既有测试保持绿。
2. **网关 + 认证**：`web.rs`、配对、设备 token、Origin 校验、限速、启动保护；用 Rust 集成测试（WebSocket 客户端）验证。
3. **最小前端 M1**：配对、会话列表、对话、权限批准；Playwright 冒烟。
4. **M2**：计划审阅、文件 / diff。
5. **M3**：搜索、上下文、设备管理、PWA；部署文档（Tailscale / Caddy 示例）。

每一步独立可合并；步骤 2 完成前不对外宣称"可远程访问"。

## 10. 风险

- **安全面显著扩大**：这是最大的风险，因此默认关闭、默认回环、配对而非长期 URL token、同源校验。上线前需要一次专门的安全评审（`/security-review`）。
- **前后端类型漂移**：靠类型生成 + CI 约束。
- **移动端体验**：长对话与大 diff 的渲染性能，需虚拟列表与分页。
- **嵌入资源增大二进制**：可接受；开发期用 `--web-dir` 热更新。

## 11. 待定问题

- 是否需要内置 TLS（rustls）？取决于用户是否接受依赖 Tailscale / 反向代理。
- 设备 token 过期默认值（30 天滑动）是否合适？
- 是否允许 Web 端执行管理类操作（新增 Agent、修改配置）？建议 M3 之后再议，默认只读配置。
- Web 端创建原生终端会话是否永远不支持？
