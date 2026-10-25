# Web 端 · 设计

日期：2026-10-25
状态：草案，待评审
承接：README Roadmap "Desktop app (Tauri v2) on the same daemon API"

## 1. 目标

让用户在**任意网络下的其他设备**（手机、平板、另一台电脑）通过**一个公网 URL** 在浏览器里使用同一个 daemon：查看会话、收发消息、批准权限请求、审阅并批准委派计划、浏览文件与 diff。

核心原则：
- Web 端只是 daemon 的又一个客户端，与 TUI 并列；核心逻辑不为它改动。
- daemon 能执行命令、改文件。**把它暴露到网络等于暴露远程代码执行**，所以认证与默认安全优先于功能。
- 仍然是"一个二进制 + 静态资源"：前端构建产物嵌入或随 daemon 托管，不引入第二个常驻服务。

## 2. 非目标（本期）

- 多账号体系 / 第三方登录（OAuth）；本期是"单所有者"模型，外部 SSO 可由前置的 Cloudflare Access / oauth2-proxy 提供。
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
- 默认**不监听**。需要显式 `--web`（或配置 `[web]`）才启用。

## 3.1 公网可达（核心需求）

用户期望"启动 web 后直接用 URL 从外网访问"。机器通常在 NAT 后、没有公网 IP，所以提供三种**暴露模式**，由 `--web-expose` 选择：

| 模式 | 适用 | 做法 |
|---|---|---|
| `tunnel`（默认的推荐路径） | 家用 / 公司机器，在 NAT 后 | daemon 拉起 `cloudflared`（出站连接，无需开端口），解析并打印公网 URL 与二维码。临时隧道（trycloudflare）无需账号；配置了命名隧道 token 与域名则得到固定 URL |
| `direct` | 有公网 IP / VPS | daemon 自己监听 `0.0.0.0:443`，用 rustls + ACME（Let's Encrypt）自动签发证书，需要 `--domain` |
| `local` | 仅本机 / 已有反向代理 / Tailscale | 只绑 `127.0.0.1`，TLS 与暴露交给外部 |

要点：
- `tunnel` 与 `direct` 对外**强制 HTTPS**；daemon 不提供公网明文 HTTP。
- 隧道只是传输：daemon 把它当作"不可信网络"，认证全部由 daemon 自己做（见 §4），不依赖隧道提供的保护。
- 先做 `tunnel` + `local`（最快满足需求），`direct`（ACME）放第二批。
- 调研提醒：Cloudflare 临时隧道文档注明仅用于测试（并发请求有限、不支持 SSE；URL 持有即可访问且曾被搜索引擎收录）。我们的事件流用 WebSocket 而非 SSE，所以可行，但文档与启动输出必须说明：**临时 URL 不是秘密，不能当作认证**；长期使用推荐命名隧道 + 固定域名。
- 隧道进程由 daemon 监管：崩溃自动重启，URL 变化时更新并提示；daemon 退出时一并退出。

## 4. 认证与安全（本设计的重点）

### 4.1 威胁

1. 同网段其他主机扫到端口 → 未授权执行。
2. 恶意网页通过浏览器向本地端口发请求（CSRF / WebSocket 劫持）。
3. token 泄露（日志、URL、Referer、浏览器历史）。
4. 明文传输被嗅探（局域网 / 公共 Wi-Fi）。

### 4.2 措施

- **URL 不含任何秘密，且被视为公开**：假设任何人都能发现并访问该 URL，所有保护来自认证本身。未认证时只能访问登录 / 配对页的静态资源。
- **默认只绑 `127.0.0.1`**；只有 `--web-expose tunnel|direct` 才对外，且二者强制 HTTPS。直接绑非回环地址的明文 HTTP 只在 `--insecure-http` 显式确认下允许。
- **配对流程**，避免把长期 token 放进 URL：
  1. 在本机运行 `agentmux pair`（或 TUI 菜单项），daemon 生成一次性配对码（8 位以上、5 分钟有效、仅可用一次），显示为文本 + 二维码（编码 `https://host/pair#code=...`，用 fragment 避免进入服务端日志与 Referer）。
  2. 新设备打开页面提交配对码到 `POST /api/pair`，daemon 校验后签发**设备 token**（256 位随机，只存哈希）并返回；前端存入 `localStorage`，并以 `Authorization`/子协议传递。
  3. 设备 token 可列出、命名、单独吊销（`agentmux devices` / TUI / Web 设置页）；支持过期时间，默认 30 天滑动续期。
- **WebSocket 握手校验**：必须带有效设备 token（通过 `Sec-WebSocket-Protocol` 传递，不放 query string）；校验 `Origin` 必须与 `Host` 同源或在 `[web] allowed_origins` 白名单内，否则 403。HTTP API 同样要求 `Authorization: Bearer`，不使用 cookie，从根上避免 CSRF。
- **限速与锁定**：配对接口与失败的 token 校验按来源 IP 限速；配对码连续错误 5 次即作废。
- **响应头**：严格 CSP（仅同源脚本，无内联）、`X-Content-Type-Options`、`Referrer-Policy: no-referrer`、`Cache-Control: no-store`（API）。
- **审计**：设备配对、吊销、登录失败写入 daemon 日志；高风险 RPC（执行命令类的权限批准）在事件流里本来就有记录。
- **TLS**：`tunnel` 模式由 Cloudflare 终结；`direct` 模式用 rustls + ACME 自动证书。使用反向代理时必须透传 `Host`（code-server 的经验：缺失会导致 Origin 校验把正常 WebSocket 全部拒绝），文档给出 Caddy / nginx 示例。
- **公网暴露专项加固**（调研得出）：code-server 对公网暴露的官方立场是"绝不能无认证、无加密"，并对登录做每分钟 2 次 + 每小时 12 次的节流；OpenCode 的 web 服务不设密码就是裸奔，文档反复要求绑定非本地地址时必须设密码。因此我们：① 没有认证配置则 `--web-expose` 拒绝启动；② 首次启用必须先在本机完成配对（设备 token 才可用），远程设备永远不能自行"首次设置密码"；③ 登录 / 配对失败按 IP 与全局双重节流，全局失败过多时暂时关闭配对入口；④ 可选第二因素：把配对绑定为 passkey（WebAuthn），设备 token 泄露不足以接管；⑤ 可选前置 Cloudflare Access 做 SSO（我们只校验其注入的 JWT，作为额外一层，不作为唯一一层）。
- **高风险操作二次确认**：通过 Web 通道批准“执行命令 / 写文件范围外”类权限时，默认需要在页面上显式确认（不提供“一律允许”的一键按钮），Web 来源在审计事件里标注设备名。
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
  - M1：配对 / 登录、工作区与会话列表（含状态点）、收件箱（待批准）、对话流（文本、工具调用折叠、编辑文件摘要）、发送消息与取消、权限请求批准 / 拒绝。
  - M2：委派计划审阅（任务编辑、改派、批准、进度）、文件树与 diff 查看、会话标题 / 重命名、overlap 提示。
  - M3：搜索、共享上下文编辑、设备管理、PWA（可添加到主屏、通知）。
- 渲染：Markdown 用 `react-markdown` + 严格消毒（禁止原始 HTML）；代码高亮用 `shiki` 按需加载；所有来自 agent 的内容一律视为不可信文本。
- 与 TUI 的一致性：状态文案与按钮语义对齐（如计划的 Approve / Resume / Retry / Skip）。

## 6.1 界面设计（借鉴调研）

**信息架构**
- 侧栏按**工作区分组折叠**，其下列会话；每个会话带实时状态点（空闲 / 运行中 / 等待批准 / 出错）和最近活动时间（ocui 的做法）。会话可像浏览器标签一样同时打开多个。
- 顶层增加**收件箱视图**：汇总所有会话里"等待批准"的权限请求和"需要决定"的计划（暂停的任务），按时间排序，一处处理完（调研中的统一权限收件箱 / 监控网格思路）。这比逐个会话切换更适合手机，也正好承接我们已有的多 Agent 并行。
- 手机：底部标签栏（会话 / 收件箱 / 计划 / 文件），触控友好，面板可侧滑（OpenCode web 的触控与滑动手势）；桌面：三栏（侧栏 / 对话 / 变更与计划）。

**对话流**
- 工具调用渲染为**可折叠的内联卡片**：默认一行摘要（图标 + 动作 + 目标 + 状态），展开才看输入输出；不直接倾倒原始日志。
- 长对话的"滚动纪律"：用户向上滚动时不强制拉回底部，显示"有 N 条新消息"的跳转按钮；重要动作（权限请求、编辑文件、失败）即使折叠也保持可见的徽标，避免被淹没。
- 编辑文件的卡片直接给出 +/- 行数，点击进入 diff。

**权限批准（避免“橡皮图章”）**
- 请求文案必须具体到动作与对象（"在 core/src 下运行 `cargo test`"，而不是"工具执行需要批准"），并展示将发生什么的预览。
- 作用域选择：仅此一次 / 本会话内同类 / 不再询问仅限低风险；Web 端高风险类别不提供“始终允许”。拒绝后在对话里留下一条确认，并可附带说明让 Agent 换方案。
- 手机上按钮足够大、与输入区分离，防止误触；批准后在时间线标注批准设备。

**Diff 与文件**
- 默认并排 / 行内可切换（手机默认行内）；长文件按文件折叠、按需虚拟渲染；先显示变更文件列表和统计，再逐个展开（Cursor 的内联 diff 为参照）。
- 变更来源标注是哪个 Agent / 哪个任务产生的，与已有 overlap 提示联动。

**委派计划**
- 以"计划 + 步骤账本"呈现：任务列表带依赖、负责 Agent、状态；批准前只读预览加可编辑，批准后显示进度；暂停任务直接在卡片上给出 Resume / Retry / Skip / 改派。与 TUI 按钮语义保持一致。

**通用**
- 明暗主题、可访问性（键盘可达、对比度、`prefers-reduced-motion`）；停止（Stop）与撤销是两个独立控件，停止不暗示已回滚。
- 参考项目：OpenCode Portal（移动优先客户端）、Garcon（自托管多 Agent 浏览器 / 手机工作区，含手机端批准）。

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
2b. **公网暴露**：`--web-expose tunnel`（监管 cloudflared、解析 URL、二维码），随后 `direct`（rustls + ACME）。
3. **最小前端 M1**：配对、会话列表、对话、权限批准；Playwright 冒烟。
4. **M2**：计划审阅、文件 / diff。
5. **M3**：搜索、上下文、设备管理、PWA；部署文档（Cloudflare 命名隧道、Tailscale、Caddy 示例）。

每一步独立可合并；步骤 2 完成前不对外宣称"可远程访问"。

## 9.1 业界做法调研摘要

- code-server：默认密码登录 + 登录节流；官方明确不要无认证无加密暴露；4.10.1 起对 WebSocket 做 Origin 校验防跨站劫持，反向代理必须透传 `Host`。
- OpenCode `web`：默认仅 localhost；`OPENCODE_SERVER_PASSWORD` 启用 HTTP Basic，绑 `0.0.0.0` 时要求必设密码；`attach` 可让 TUI 连远程服务。
- VS Code 远程 agent 会话：走 dev tunnel，默认要求认证，并警告匿名隧道会让任何拿到 URL 的人启动 agent 会话。
- Cloudflare 临时隧道：免账号即得公网 URL，但仅用于测试、URL 持有即访问；可用 `--allowed-mail` 加邮箱一次性口令，或用命名隧道 + Cloudflare Access。
- 社区 agent 远程工具：多数是"回环绑定 + SSH 转发"（认证靠 SSH key），或"本地网关 + Cloudflare Tunnel + 前置身份层"；商业产品有端到端加密中继（配对时 X25519 交换密钥）。

我们的取舍：采用"出站隧道获得 URL + daemon 自己做配对认证和设备吊销"，吸收 code-server 的节流与 Origin 校验、OpenCode 的"无认证拒绝对外"，端到端加密中继暂不做。

## 10. 风险

- **安全面显著扩大**：直接面向公网，这是最大的风险，因此默认关闭、默认回环、对外强制 HTTPS 与配对认证、同源校验、节流。上线前需要一次专门的安全评审（`/security-review`）。
- **前后端类型漂移**：靠类型生成 + CI 约束。
- **移动端体验**：长对话与大 diff 的渲染性能，需虚拟列表与分页。
- **嵌入资源增大二进制**：可接受；开发期用 `--web-dir` 热更新。

## 11. 待定问题

- `tunnel` 模式依赖外部 `cloudflared` 二进制：是要求用户自行安装，还是首次使用时提示下载？是否同时支持 Tailscale Funnel / frp 等可插拔实现？
- passkey 是否本期做，还是作为 M3 增强。
- 设备 token 过期默认值（30 天滑动）是否合适？
- 是否允许 Web 端执行管理类操作（新增 Agent、修改配置）？建议 M3 之后再议，默认只读配置。
- Web 端创建原生终端会话是否永远不支持？

## 12. MVP 实现状态（2026-10-25）

已实现：
- 传输泛化：`handle_conn_io` 对任意字节流工作，`ConnScope::{Local,Remote}` 限制远程可调用的方法（`server/shutdown`、`web/*` 仅本地）。
- 网关：axum，`/ws`（设备 token 经 WebSocket 子协议传递、同源 Origin 校验、撤销检测）、`/api/pair`、`/api/whoami`、静态资源（嵌入或 `--web-dir`）、安全响应头。
- 认证：一次性配对码（5 分钟）、设备 token 仅存哈希、按 IP 与全局节流、撤销；CLI `--pair/--devices/--revoke`。
- 暴露：`--web-expose local|tunnel`（监管 `cloudflared`、解析 trycloudflare URL、自动重启、打印配对链接）；非回环明文 HTTP 需 `--insecure-http`。
- 前端（React + Vite + TS）：配对、工作区/会话列表（状态点）、对话流（Markdown、可折叠工具卡片、滚动纪律）、权限批准（Web 不提供“始终允许”）、收件箱、委派计划（提议/改派/批准/恢复/重试/跳过/取消）、手机底部标签栏。
- 测试：Rust 单元与集成测试、Vitest 单元测试、Playwright 端到端（手机 / 桌面视口）。

与设计的差异 / 未做：
- 未做：`direct`（rustls + ACME）、passkey、文件树与 diff、搜索、上下文编辑、设备管理页（仅 CLI）、PWA、`ts-rs` 类型生成（现为手写 `types.ts`）、二维码输出、计划任务的标题/提示词编辑。
- 对话流目前每个会话只拉取最新一页历史，较早内容通过“加载更早消息”分页。
- 吊销对已打开的连接在 ≤5 秒内生效。
- `cloudflared` 隧道未在真实网络上验证（环境中无该二进制）；URL 解析有单元测试。

## 13. 公网暴露方式：搁置的决定（2026-10-25）

状态：**搁置，不再继续投入**；代码保持现状，待用户环境明确后再议。

背景：`--web-expose tunnel` 依赖外部 `cloudflared`，原因是目标机器通常在 NAT 后、没有公网 IP；cloudflared 主动向外连接，无需开端口、域名或证书，HTTPS 由 Cloudflare 终结。

已知代价：流量经过 Cloudflare（其处终结 TLS，内容可见）；临时 `trycloudflare.com` URL 每次启动都变且官方仅建议测试用；需额外安装二进制；隧道未在真实网络上验证过。

可选替代（认证与配对模型不变，只换“外网如何到达”）：
- Tailscale / WireGuard：不暴露公网、不经第三方转发内容，最安全；每台设备需安装。配合 `--web`（仅本机）即可，无需任何代码改动。
- 有公网 IP / VPS：Caddy 或 nginx 反向代理，或实现 `direct` 模式（rustls + ACME）。
- SSH 反向隧道、frp / rathole 自建中转：需要一台公网机器。

重启此议题时先确认两点：机器在 NAT 后还是有公网 IP；更看重“对端零安装”还是“流量不经第三方”。据此决定默认推荐，并是否实现 `direct` / 其他隧道后端。
