# agentmux Phase 1 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 构建 agentmux Phase 1：Rust daemon + ACP 编排内核 + ratatui TUI，统一并行编排 Claude Code / Codex / OpenCode / pi，含共享 workspace 协作空间。

**Architecture:** Cargo workspace。`core` 库持有领域模型 / ACP client / WorktreeManager / EventStore / 协作层；`server` 是薄 daemon，在 Unix socket 上暴露 JSON-RPC 2.0；`client` 是 daemon 的 Rust client SDK（含自动拉起）；`tui` 是纯 client；`mock-agent` 是测试用 ACP agent。

**Tech Stack:** Rust (edition 2021), tokio, `agent-client-protocol` crate, rusqlite (bundled), serde/serde_json, ratatui + crossterm, git CLI（worktree 操作用子进程调用）。

**Spec:** `/home/docs/superpowers/specs/2026-09-28-agentmux-design.md`

## Global Constraints

- 项目根目录：`/home/agentmux`，cargo workspace，成员：`core`、`server`、`client`、`tui`、`mock-agent`
- crate 名约定：`agentmux-core`、`agentmux-server`、`agentmux-client`、`agentmux-tui`、`agentmux-mock-agent`
- ACP 依赖：`agent-client-protocol` crate（daemon 实现其 `Client` trait）
- SQLite 用 `rusqlite` 的 `bundled` feature，不引入 sqlx/diesel
- JSON-RPC 2.0 over Unix socket，socket 路径 `~/.local/share/agentmux/agentmux.sock`
- 数据目录 `~/.local/share/agentmux/`（`db.sqlite` + `sessions/<id>.jsonl`）；配置 `~/.config/agentmux/config.toml`
- worktree 默认位于 `<project>/.agentmux/worktrees/<name>`
- 每个 Session 对应一个 ACP stdio 子进程
- CI 测试不得依赖真实 agent binary 或 API key——一律走 mock-agent
- 许可证：MIT / Apache-2.0 双协议

## Review Focus

以下输入/失败模式 spec 未展开但会咬人，各自已在所属 task 里有测试钉住：

1. **agent binary 不在 PATH 上** → `agent.list` 返回 `available=false`，`session.create` 返回清晰错误（Task 4、9 测试）
2. **agent 子进程在 prompt 中途退出** → session 转 `Error`，产生 `AgentExited` 事件，JSONL 已写内容不丢（Task 9 测试）
3. **对 `Prompting` 状态的 session 再次 prompt** → 返回 `session busy` 错误，不排队（v1 决策）（Task 9 测试）
4. **worktree 创建失败（分支已存在/脏仓库）** → 不留半成品目录，错误透传（Task 2 测试）
5. **client 发来畸形 JSON-RPC** → 返回标准 `-32700` parse error，连接保持不断（Task 11 测试）

---

### Task 1: Workspace 骨架 + 领域模型

**Files:**
- Create: `/home/agentmux/Cargo.toml`（workspace 定义）
- Create: `/home/agentmux/core/Cargo.toml`
- Create: `/home/agentmux/core/src/lib.rs`
- Create: `/home/agentmux/core/src/model.rs`
- Create: `/home/agentmux/core/src/id.rs`
- Test: `/home/agentmux/core/src/model.rs`（inline `#[cfg(test)]`）

**Interfaces:**
- Produces（后续全部任务依赖）:
  - `pub struct ProjectId(pub Uuid)` / `WorkspaceId(pub Uuid)` / `SessionId(pub Uuid)` / `AgentId(pub String)`，均 `Serialize + Deserialize`
  - `pub struct Project { id: ProjectId, root_path: PathBuf, name: String }`
  - `pub struct Workspace { id: WorkspaceId, project_id: ProjectId, name: String, worktree_path: PathBuf, branch: String, created_at: DateTime<Utc> }`
  - `pub enum AdapterKind { Acp { command: PathBuf, args: Vec<String> }, PiRpc { command: PathBuf, args: Vec<String> } }`
  - `pub struct AgentProfile { id: AgentId, name: String, adapter: AdapterKind, env: BTreeMap<String, String>, available: bool }`
  - `pub enum SessionState { Created, Connecting, Ready, Prompting, WaitingPermission, Done, Error(String) }`
  - `pub struct SessionRef { pub session_id: SessionId, pub event_seq: u64 }`
  - `pub struct Session { id: SessionId, workspace_id: WorkspaceId, agent_id: AgentId, state: SessionState, acp_session_id: Option<String>, references: Vec<SessionRef>, created_at: DateTime<Utc> }`
  - `pub enum EventKind { SessionUpdate(serde_json::Value), StateChanged { from: SessionState, to: SessionState }, FileEdited { path: PathBuf }, AgentExited { code: Option<i32> }, Orchestrator(String) }`
    （`SessionUpdate` 先包 `serde_json::Value`，Task 5 接 ACP crate 后可在内部转换为强类型）
  - `pub struct Event { session_id: SessionId, seq: u64, ts: DateTime<Utc>, kind: EventKind }`

- [ ] **Step 1: 初始化仓库**

```bash
mkdir -p /home/agentmux && cd /home/agentmux && git init
# workspace Cargo.toml: members = ["core", "server", "client", "tui", "mock-agent"], resolver = "2"
cargo new core --lib --name agentmux-core
```

- [ ] **Step 2: 写失败测试**

```rust
#[test]
fn session_state_roundtrips_and_error_carries_message() {
    let s = SessionState::Error("agent exited".into());
    let v = serde_json::to_value(&s).unwrap();
    let back: SessionState = serde_json::from_value(v).unwrap();
    assert!(matches!(back, SessionState::Error(m) if m == "agent exited"));
}
```

- [ ] **Step 3: 确认失败** — `cargo test -p agentmux-core` → 编译错误（类型不存在）

- [ ] **Step 4: 实现 `id.rs` + `model.rs`**（按 Interfaces 定义，加 `#[derive]`、uuid/serde/chrono 依赖）

- [ ] **Step 5: 测试通过** — `cargo test -p agentmux-core` PASS

- [ ] **Step 6: Commit** — `git add -A && git commit -m "feat(core): domain model types"`

---

### Task 2: WorktreeManager

**Files:**
- Create: `/home/agentmux/core/src/worktree.rs`
- Test: `/home/agentmux/core/tests/worktree_test.rs`

**Interfaces:**
- Consumes: `core/src/model.rs` 的 `Workspace`
- Produces:
  - `pub struct WorktreeManager;`
  - `impl WorktreeManager { pub fn create(repo_root: &Path, name: &str, base: &str) -> Result<(PathBuf, String)>; pub fn remove(repo_root: &Path, worktree_path: &Path) -> Result<()>; }`
    - `create` 执行 `git worktree add -b agentmux/<name> <repo_root>/.agentmux/worktrees/<name> <base>`，返回 `(worktree_path, branch)`
    - `remove` 执行 `git worktree remove --force <path>` 并删除对应 `agentmux/` 分支

- [ ] **Step 1: 写失败测试** — tempfile + `git init` 一个仓库，调用 `create("demo","main")`，断言返回路径存在且 `git worktree list` 含新条目；对重复 `name` 二次调用断言返回 Err 且 `worktrees/` 下无残留半成品目录。

- [ ] **Step 2: 跑测试确认失败** — `cargo test -p agentmux-core --test worktree_test` → FAIL（`WorktreeManager` 不存在）

- [ ] **Step 3: 实现** — `std::process::Command` 调 git；非零退出码时捕获 stderr 进 Err；失败时 `git worktree remove --force` 清理残留。

- [ ] **Step 4: 测试通过** — 同上命令 PASS

- [ ] **Step 5: Commit** — `git commit -m "feat(core): git worktree manager"`

---

### Task 3: EventStore（SQLite + JSONL）

**Files:**
- Create: `/home/agentmux/core/src/store.rs`
- Test: `/home/agentmux/core/tests/store_test.rs`

**Interfaces:**
- Consumes: Task 1 全部 model 类型
- Produces:
  - `pub struct Store { /* rusqlite::Connection + PathBuf */ }`
  - `Store::open(data_dir: &Path) -> Result<Store>`（建表：projects/workspaces/agents/sessions）
  - `insert_project/get_project/list_projects`、`insert_workspace/get_workspace/list_workspaces(project_id)`、`upsert_agent/list_agents`、`insert_session/update_session_state/list_sessions(workspace_id)`
  - `append_event(&self, ev: &Event) -> Result<()>` — 追加 `<data_dir>/sessions/<session_id>.jsonl`
  - `read_events(&self, session_id: SessionId) -> Result<Vec<Event>>` — 按 seq 排序

- [ ] **Step 1: 写失败测试** — 打开临时 Store，insert+get roundtrip 各实体；`append_event` 3 条后 `read_events` 返回同序同内容；`update_session_state` 后 list 反映新状态。

- [ ] **Step 2: 确认失败** — `cargo test -p agentmux-core --test store_test` FAIL

- [ ] **Step 3: 实现** — rusqlite `bundled`，启动时 `CREATE TABLE IF NOT EXISTS`；枚举列存 serde_json 字符串。

- [ ] **Step 4: PASS + Commit** — `git commit -m "feat(core): sqlite + jsonl event store"`

---

### Task 4: Config 加载 + AgentRegistry + 可用性探测

**Files:**
- Create: `/home/agentmux/core/src/config.rs`
- Create: `/home/agentmux/core/src/registry.rs`
- Test: `/home/agentmux/core/tests/registry_test.rs`

**Interfaces:**
- Consumes: Task 1 `AgentProfile`/`AdapterKind`/`AgentId`
- Produces:
  - `pub struct Config { pub agents: Vec<AgentProfile> }`
  - `Config::load(path: &Path) -> Result<Config>` — TOML，`[[agents]]` 缺省时落内置默认表（见下）
  - `pub struct AgentRegistry { /* profiles */ }`
  - `AgentRegistry::from_config(cfg: &Config) -> AgentRegistry`
  - `AgentRegistry::probe(&self) -> Vec<AgentProfile>` — 对每个 profile 用 `which`-式探测填 `available`
  - `AgentRegistry::get(&self, id: &AgentId) -> Option<&AgentProfile>`
- 内置默认表：`claude-code`→`claude-code-acp`(Acp)、`codex`→`codex-acp`(Acp)、`opencode`→`["acp"]`(Acp)、`pi`→`["--mode","rpc"]`(PiRpc)

- [ ] **Step 1: 写失败测试** — ① 无 config 文件时 `from_config` 产出 4 个内置 profile；② 写一份含自定义 `[[agents]] name="myagent" command="/bin/true"` 的 toml，load 后 registry 含该项；③ 对一个 `command="/definitely/not/exist"` 的 profile `probe()` 返回 `available=false`。

- [ ] **Step 2: 确认失败 → Step 3: 实现 → Step 4: PASS**

- [ ] **Step 5: Commit** — `git commit -m "feat(core): agent registry + config loading"`

---

### Task 5: ACP Client 封装

**Files:**
- Create: `/home/agentmux/core/src/acp_conn.rs`
- Modify: `/home/agentmux/core/Cargo.toml`（加 `agent-client-protocol`）
- Test: `/home/agentmux/core/tests/acp_conn_test.rs`（用 Task 6 的 mock-agent；本任务先落接口，联调在 Task 6）

**Interfaces:**
- Consumes: Task 1 `Event`/`EventKind`，`agent-client-protocol` crate
- Produces:
  - `pub struct AcpConn;`
  - `AcpConn::spawn(command: &Path, args: &[String], env: &BTreeMap<String,String>, cwd: &Path) -> Result<AcpConn>` — 起子进程、接管 stdin/stdout、建 `ClientSideConnection`、后台 task 驱动 io
  - `async fn initialize(&mut self) -> Result<serde_json::Value>` — ACP `initialize`，声明 fs/terminal client capabilities
  - `async fn new_session(&mut self, cwd: &Path) -> Result<String>` — 返回 acp session id
  - `async fn prompt(&mut self, session_id: &str, text: String) -> Result<()>` — `session/prompt`，turn 结束返回
  - `async fn cancel(&mut self, session_id: &str) -> Result<()>` — `session/cancel` notification
  - `fn events(&self) -> broadcast::Receiver<Event>` — 所有 `session/update` 归一化为 `EventKind::SessionUpdate` 广播；子进程退出产生 `AgentExited`
  - `async fn shutdown(&mut self) -> Result<()>`
  - Client trait 实现：`session/request_permission` 转发为 `EventKind::Orchestrator("permission-request")` + oneshot 等待 daemon 决策（v1 默认拒绝的安全实现也可接受，但接口留好）；`fs/read_text_file`、`fs/write_text_file`、`terminal/*` 按 cwd 内实现或返回不支持

- [ ] **Step 1: 写失败测试** — 单测：① 对 `AcpConn` 的 spawn 参数构造（env 传递、cwd 设置）与 fake 命令（`/bin/cat`）验证进程起来、events receiver 存活；② `initialize` 用 `tokio::time::timeout` 包裹（默认 10s，可由 `Config` 覆盖），对 `/bin/cat`（永不应答）断言在缩短后的超时窗口内返回 Err。

- [ ] **Step 2: 确认失败 → Step 3: 实现 → Step 4: PASS**

- [ ] **Step 5: Commit** — `git commit -m "feat(core): acp client connection wrapper"`

---

### Task 6: mock-agent（测试用 ACP agent）

**Files:**
- Create: `/home/agentmux/mock-agent/Cargo.toml`
- Create: `/home/agentmux/mock-agent/src/main.rs`
- Test: `/home/agentmux/core/tests/acp_e2e_test.rs`

**Interfaces:**
- Consumes: `agent-client-protocol` crate（实现 `Agent` trait）
- Produces: binary `agentmux-mock-agent`，行为契约：
  - `initialize` → 返回正常 InitializeResponse
  - `session/new` → 返回固定 session id
  - `session/prompt` → 依次推 `session/update`：一条 `agent_message_chunk`("mock reply")、一个 `tool_call`(kind=edit, path=`src/lib.rs`)，然后返回 `PromptResponse(stop_reason="end_turn")`
  - 若 prompt 文本含 `"crash"` → 直接 `std::process::exit(1)`
- 供 Task 5/9/11/13 的测试使用

- [ ] **Step 1: 写失败 e2e 测试** — `AcpConn::spawn` 拉起 `cargo run -p agentmux-mock-agent`，initialize→new_session→prompt，断言收到 `SessionUpdate` 事件至少 2 条、prompt 正常返回。

- [ ] **Step 2: 确认失败**（binary 不存在）

- [ ] **Step 3: 实现 mock-agent** — 按 crate 的 `examples/agent.rs` 模式实现 `Agent` trait。

- [ ] **Step 4: e2e PASS + Commit** — `git commit -m "test: mock acp agent + e2e prompt flow"`

---

### Task 7: pi RPC 翻译器

**Files:**
- Create: `/home/agentmux/core/src/pi_rpc.rs`
- Test: `/home/agentmux/core/tests/pi_rpc_test.rs`

**Interfaces:**
- Consumes: Task 5 的 `AcpConn` 同款对外形状（见下）、Task 1 `Event`/`EventKind`
- Produces:
  - `pub struct PiConn;` — 方法与 `AcpConn` 完全同名同签名（spawn/initialize/new_session/prompt/cancel/events/shutdown），内部 spawn `pi --mode rpc`
  - 行分割规则：**只按 `\n` 切**，不用通用 line reader（pi 文档明确 U+2028/U+2029 会误切）
  - 翻译规则：pi `event` JSON → `EventKind::SessionUpdate(原始 json)`；`response` → 对应 pending 命令的应答；`new_session` 命令 → pi session 名作为 session id

- [ ] **Step 1: 写失败测试** — 不依赖真 pi：写一个本地 stub 脚本/mini binary 模拟 pi rpc 输入输出（或直接单测 `translate_line(json_line) -> Option<Event>` 纯函数，覆盖 event/response/含 U+2028 的 payload 三种行）。

- [ ] **Step 2: 确认失败 → Step 3: 实现 → Step 4: PASS**

- [ ] **Step 5: Commit** — `git commit -m "feat(core): pi rpc→event translator"`

---

### Task 8: 协作层（`.agentmux/` + 上下文注入 + activity 摘要）

**Files:**
- Create: `/home/agentmux/core/src/collab.rs`
- Test: `/home/agentmux/core/tests/collab_test.rs`

**Interfaces:**
- Consumes: Task 1 `Session`/`EventKind`，Task 2 `Workspace`
- Produces:
  - `pub fn init_shared_dir(worktree_path: &Path) -> Result<()>` — 建 `.agentmux/`、`context.md`（空模板）、`activity.md`（空）
  - `pub fn append_activity(worktree_path: &Path, agent_name: &str, summary: &str) -> Result<()>` — 追加一行带时间戳
  - `pub fn summarize_event(kind: &EventKind) -> Option<String>` — `FileEdited`→`"edited <path>"`，其余→None
  - `pub fn shared_context_preamble(worktree_path: &Path, session_count: usize) -> Result<Option<String>>` — `session_count >= 2` 且（context.md 非空或 activity.md 非空）时返回拼好的 preamble 文本；否则 None

- [ ] **Step 1: 写失败测试** — ① init 后两文件存在；② append_activity 追加成行；③ `session_count=1` 时 preamble 为 None；④ `session_count=2` 且有内容时 preamble 含 context.md 文本与 activity 行。

- [ ] **Step 2-4: 失败 → 实现 → PASS**

- [ ] **Step 5: Commit** — `git commit -m "feat(core): shared workspace collab layer"`

---

### Task 9: Orchestrator（SessionManager）

**Files:**
- Create: `/home/agentmux/core/src/orchestrator.rs`
- Modify: `/home/agentmux/core/src/lib.rs`（导出）
- Test: `/home/agentmux/core/tests/orchestrator_test.rs`

**Interfaces:**
- Consumes: Store(3)、AgentRegistry(4)、AcpConn/PiConn(5/7)、collab(8)、WorktreeManager(2)
- Produces:
  - `pub enum SpawnedConn { Acp(AcpConn), Pi(PiConn) }` — 统一方法名委托
  - `pub struct Orchestrator;`
  - `Orchestrator::new(store: Store, registry: AgentRegistry, data_dir: PathBuf) -> Orchestrator`
  - `async fn create_workspace(&mut self, project_id: ProjectId, name: &str, base: &str) -> Result<WorkspaceId>` — 建 worktree + `collab::init_shared_dir` + 落库
  - `async fn create_session(&mut self, workspace_id: WorkspaceId, agent_id: &AgentId, prompt: Option<String>) -> Result<SessionId>` — spawn conn、initialize、new_session(cwd=worktree)、state→Ready；agent `available=false` 时直接 Err
  - `async fn prompt(&self, session_id: SessionId, text: String, refs: Vec<SessionRef>) -> Result<()>` — state→Prompting；同 workspace 其他 session ≥1 时前置 `shared_context_preamble`；refs 文本追加进 prompt；调用 conn.prompt；结束回 Ready/Done。**state==Prompting 时返回 `session busy` Err**
  - `async fn cancel/kill(&self, session_id)` — conn.cancel / drop 子进程 + state 更新
  - `async fn resume(&mut self, session_id: SessionId) -> Result<()>` — 对 `Done`/`Error` session 重新 spawn conn 并 `new_session`；若 agent 侧支持 `session/load` 则优先复用旧 acp_session_id，否则新建会话并把历史事件回放入新上下文（降级行为写注释说明）
  - `fn subscribe(&self) -> broadcast::Receiver<Event>` — 全局事件总线订阅（client 侧自行按 session_id 过滤）
  - 内部：adapter 的 events receiver → 归一化转发到总线；`FileEdited` 事件同时 `append_activity`；conn 事件流结束/进程死 → `AgentExited` + `Error`

- [ ] **Step 1: 写失败测试**（用 mock-agent 内置 profile + tempfile git repo）：
  ① `create_workspace`→`create_session(mock)`→`prompt("hi")` → subscribe 收到 `SessionUpdate` 流 + state 最终回 Ready；
  ② prompt 进行中再次 `prompt` → `session busy` Err；
  ③ prompt 文本 `"crash"` → 收到 `AgentExited`、session state Error、JSONL 文件存在且非空；
  ④ 同 workspace 两个 session，`append_activity` 后 prompt B → mock 收到的 prompt 文本含 preamble（mock 侧 echo 验证即可——mock 已把 prompt 回显，断言事件流里出现 context 内容）；
  ⑤ `agent_id` 不存在 → Err；
  ⑥ `Done` 状态的 session 调 `resume` → 重新 spawn、state 回 `Ready`。

- [ ] **Step 2: 确认失败 → Step 3: 实现 → Step 4: PASS**

- [ ] **Step 5: Commit** — `git commit -m "feat(core): session orchestrator with shared-context injection"`

---

### Task 10: RPC wire 类型（core::rpc）

**Files:**
- Create: `/home/agentmux/core/src/rpc.rs`
- Test: inline

**Interfaces:**
- Produces（server 与 client 共用，避免漂移）：
  - 方法名常量：`pub const M_PROJECT_REGISTER: &str = "project/register";` 等，覆盖 spec §7 全部方法：`project/register`、`project/list`、`project/remove`、`workspace/create`、`workspace/list`、`workspace/remove`、`session/create`、`session/prompt`、`session/cancel`、`session/kill`、`session/list`、`session/resume`、`session/subscribe`、`agent/list`、`agent/register`、`server/status`、`server/shutdown`
  - 各方法的 `XxxParams`/`XxxResult` serde 类型（字段与 spec §7 对应）
  - `pub struct RpcError { pub code: i64, pub message: String }`

- [ ] **Step 1: 写失败测试** — params/result serde roundtrip 抽查 2-3 个。

- [ ] **Step 2-4: 失败 → 实现 → PASS → Commit** — `git commit -m "feat(core): json-rpc wire types"`

---

### Task 11: server daemon

**Files:**
- Create: `/home/agentmux/server/Cargo.toml` + `server/src/main.rs`
- Create: `/home/agentmux/server/src/rpc_server.rs`（监听、dispatch、订阅扇出）
- Test: `/home/agentmux/server/tests/server_test.rs`

**Interfaces:**
- Consumes: `Orchestrator`(9)、`core::rpc`(10)
- Produces:
  - binary `agentmux-server`：`--serve` 前台常驻 / `--daemon` 后台（默认 TUI 自动拉起用 detached spawn）
  - `async fn dispatch(orch: &Orchestrator, method: &str, params: Value) -> Result<Value, RpcError>`
  - `session/subscribe` 语义：连接升级为通知流，把 `Orchestrator::subscribe()` 的事件以 `session/event` notification 推给该连接
  - 畸形 JSON → `-32700` parse error 响应且连接保持
  - 启动时 socket 文件若 stale（无进程监听）则先 unlink

- [ ] **Step 1: 写失败测试** — 起 server 于临时 socket 路径：① `server/status` 返回 ok；② 发非 JSON 字节 → `-32700`；③ `session/subscribe` 后用 mock agent create_session+prompt → 连接上收到 `session/event` 通知流；④ 第二个连接同时 subscribe 收到相同事件。

- [ ] **Step 2: 确认失败 → Step 3: 实现 → Step 4: PASS**

- [ ] **Step 5: Commit** — `git commit -m "feat(server): unix-socket json-rpc daemon"`

---

### Task 12: client SDK + daemon 自动拉起

**Files:**
- Create: `/home/agentmux/client/Cargo.toml` + `client/src/lib.rs`
- Test: `/home/agentmux/client/tests/client_test.rs`

**Interfaces:**
- Consumes: `core::rpc`(10)、server(11)
- Produces:
  - `pub struct DaemonClient;`
  - `DaemonClient::connect() -> Result<DaemonClient>` — 连默认 socket；`connect` 内部先 `ensure_daemon()`
  - `async fn call(&mut self, method: &str, params: impl Serialize) -> Result<Value>`
  - `async fn subscribe_events(&mut self) -> Result<impl Stream<Item = Event>>` — `session/subscribe` 的通知流
  - `fn ensure_daemon() -> Result<()>` — socket 不通时 spawn `agentmux-server --daemon`，轮询等待可连接（超时 5s）

- [ ] **Step 1: 写失败测试** — socket 不存在时 `DaemonClient::connect()` 自动拉起 server 后 `server/status` 成功；`call("agent/list")` 返回内置 4 项。

- [ ] **Step 2: 确认失败 → Step 3: 实现 → Step 4: PASS**

- [ ] **Step 5: Commit** — `git commit -m "feat(client): daemon client sdk + auto-spawn"`

---

### Task 13: TUI 骨架（连接 + 布局 + 会话列表）

**Files:**
- Create: `/home/agentmux/tui/Cargo.toml` + `tui/src/main.rs`
- Create: `/home/agentmux/tui/src/app.rs`（App 状态）、`tui/src/ui.rs`（布局绘制）、`tui/src/input.rs`（按键处理）
- Test: `/home/agentmux/tui/src/app.rs`（inline，状态机单测）

**Interfaces:**
- Consumes: `DaemonClient`(12)、`core::rpc` 类型、`Event`
- Produces:
  - `pub struct App { sessions: Vec<SessionView>, selected: usize, events: Vec<Event>, mode: InputMode }`
  - `pub enum InputMode { Normal, Editing, RelayPick }`
  - `struct SessionView { session: Session, agent_name: String, workspace_name: String }`
  - `App::handle_event(&mut self, ev: Event)`、`App::handle_key(&mut self, key: KeyEvent) -> AppAction`
  - `pub enum AppAction { None, Quit, Submit(String), NewSession, CancelPrompt, Relay }`
  - 布局：左栏会话列表（workspace 分组 + 状态徽标）、右栏事件流、底部输入框

- [ ] **Step 1: 写失败测试** — App 状态机单测：`handle_event(StateChanged)` 更新徽标；`j/k` 移动 selected；Normal 模式 `q`→Quit、`i`→Editing；Editing 模式 Enter→Submit(内容)。

- [ ] **Step 2: 确认失败 → Step 3: 实现**（ratatui+crossterm 主循环：client.subscribe_events() 与 crossterm event 流 select! 合并）

- [ ] **Step 4: PASS + 手动跑通** — `cargo run -p agentmux-tui` 连上 daemon 显示内置 agent/空会话列表

- [ ] **Step 5: Commit** — `git commit -m "feat(tui): app skeleton with session list"`

---

### Task 14: TUI 会话交互（prompt / 权限弹窗 / 手动接力 / 新建会话）

**Files:**
- Modify: `tui/src/app.rs`、`tui/src/ui.rs`、`tui/src/input.rs`
- Create: `tui/src/newsession.rs`（建会话向导：选 project→workspace→agent）
- Test: inline app 测试

**Interfaces:**
- Consumes: Task 13 App/AppAction，`session/create`、`session/prompt`、`session/cancel` RPC
- Produces:
  - Editing 模式输入文本 → Submit → `session/prompt`（带 `references`）
  - `EventKind::Orchestrator("permission-request")` → 弹确认框（y/n 回传；v1 若 AcpConn 用默认拒绝，则 UI 只展示）— **对齐点：以 Task 5 实现的 permission 语义为准**
  - Normal 模式 `@` → RelayPick：选当前会话某事件 → 选目标 session → 其内容格式化为引用文本放入输入框
  - `n` → 新建会话向导（三步选择）；`ctrl-c` → `session/cancel`；`Tab` → 预览/diff 切换（diff 面板渲染 FileEdited 路径列表即可，真 diff 高亮可简化）

- [ ] **Step 1: 写失败测试** — ① Submit 路径产生正确 RPC params（含 references）；② RelayPick 流程：选中事件后 App 内部生成引用文本进输入缓冲；③ 权限事件到达 → mode 切到确认态。

- [ ] **Step 2: 确认失败 → Step 3: 实现 → Step 4: PASS**

- [ ] **Step 5: 手动冒烟** — 用 mock-agent profile 全链路：`n` 建会话 → 发 prompt → 看事件流 → `@` 接力到第二个会话 → `ctrl-c` 中断。

- [ ] **Step 6: Commit** — `git commit -m "feat(tui): prompt flow, permission dialog, manual relay"`

---

### Task 15: 打磨（README / LICENSE / CI / 冒烟脚本）

**Files:**
- Create: `README.md`、`LICENSE-MIT`、`LICENSE-APACHE`、`.github/workflows/ci.yml`、`config.example.toml`
- Modify: workspace `Cargo.toml`（metadata）

**Interfaces:**
- Consumes: 全部

- [ ] **Step 1: README** — 项目简介、架构图（spec §3）、安装前置（需已认证的各 agent CLI）、`cargo install`、快速开始、对标项目致谢

- [ ] **Step 2: CI** — fmt + clippy(`-D warnings`) + `cargo test --workspace`

- [ ] **Step 3: 全量验证** — `cargo test --workspace` 绿；`cargo build --release` 绿；clippy 无警告

- [ ] **Step 4: Commit** — `git commit -m "chore: docs, license, ci"`

---

## 依赖顺序

```
T1 model ─┬─ T2 worktree ─┐
          ├─ T3 store ────┤
          ├─ T4 registry ─┤
          ├─ T5 acp_conn ──┼─ T6 mock-agent ─┐
          ├─ T7 pi_rpc ────┤                 │
          └─ T8 collab ────┴─► T9 orchestrator ─► T10 rpc ─► T11 server ─► T12 client ─► T13 tui ─► T14 tui交互 ─► T15 打磨
```

（T5 的联调测试依赖 T6；两者可并行起步，T6 先行落地更稳。）
