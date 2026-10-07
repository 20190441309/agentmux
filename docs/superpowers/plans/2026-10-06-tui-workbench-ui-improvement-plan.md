# agentmux TUI 工作台 UI 优化计划

日期：2026-10-06
状态：U01-U08 已实施，U03-U08 已逐项通过最终验收；当前持续目标已完成
范围：按本轮 UI 评审优先级，提升多 Agent 工作台的信息可读性与操作效率

## 1. 目标与原则

当前版本已有多行编辑、历史滚动、独立草稿、响应式侧栏、Markdown 与代码高亮、权限审阅、工作区 diff 和原生 Agent 入口。本轮不重做视觉风格，也不重复实现旧优化清单中已完成的功能。

核心目标：让用户快速确认当前工作区、消息接收者、运行状态、需要处理的事项和实际代码改动，在窄屏与多会话场景下仍可高效操作。

- 保留对话优先布局、语义配色、独立草稿和不抢输入焦点的权限审阅。
- 保留 Session 与 Workspace 的关系，不引入群聊、默认广播或自动分工。
- 普通 `/...` 属于当前 Agent，工作台命令使用 `/mux ...`；按钮执行工作台操作。
- 原生模式与结构化模式明确区分，工作台快捷键不得破坏原生按键转发。
- 模型、用量、改动归属和恢复能力只显示真实已知信息，不补造数据。
- 保留当前未提交修改，不重启用户实际 daemon，不迁移或修改用户原生会话。
- 每项改动先核对最新实现，分项开发、分项验证，不进行无关重构。

相关既有计划：

- [多 Agent 新手体验优化](2026-09-30-tui-multi-agent-onboarding-plan.md)
- [Agent 能力完整性与原生托管](2026-09-30-agent-capability-preservation-plan.md)
- [既有 TUI 优化记录](../specs/2026-09-28-tui-improvement-backlog.md)

## 2. 评审与验证基线

本轮依据当前工作区源码、仓库设计截图，以及现有 `print_frame` 测试输出；渲染检查不是完整的真实终端验收。

| 项目 | 当前结果 | 后续处理 |
| --- | --- | --- |
| 140x38 渲染 | 对话、侧栏、代码块和输入区可呈现 | 保留宽屏体验，扩展到 120x30、160x40 回归 |
| 80x24 渲染 | 底栏固定预留 48 列，换行提示被截断 | U01 调整空间分配 |
| 40x16 渲染 | 顶部占 5 行、空输入区占 6 行，对话只剩几行 | U01 增加真正的紧凑模式 |
| Files | 列表来自当前 Agent 活动，打开后为 workspace diff | U03 明确范围并增加真实 Git 改动列表 |
| TUI 单元测试 | 157 通过、2 失败、3 跳过 | 实施前核查两个向导测试，不能将当前基线记为全绿 |

当前失败项：

- `app::tests::wizard_flow_new_workspace_with_name`：实际步骤为 `Agent`，测试预期为 `WorkspaceName`。
- `app::tests::wizard_name_step_edits_like_input`：实际名称为空，测试预期为 `a`。

两项测试均使用 `j` 从工作区选择进入名称输入。需核查新建默认选项及选择行为，确认是测试未同步还是流程回归；不得仅为通过测试而删掉断言，也不得未经核实认定实际向导不可用。

## 3. 优先级与实施顺序

| 顺序 | 编号 | 优先级 | 工作项 | 完成判断 |
| --- | --- | --- | --- | --- |
| 1 | U01 | P1 | 窄屏布局与信息优先级 | 正文空间改善，工作区、接收者与状态清楚可见 |
| 2 | U02 | P1 | 待处理事项与错误反馈 | 能定位待授权、失败和完成待查看的会话，错误不被运行状态遮住 |
| 3 | U03 | P1 | Files 范围与真实改动 | 明确区分工作区改动和 Agent 活动，可审阅真实 Git 改动 |
| 4 | U04 | P1 | 键盘焦点与取消一致性 | 焦点明确，常用操作可直达，原生按键不被误拦截 |
| 5 | U05 | P2 | 会话搜索、过滤与分组 | 多会话时可按名称、状态和范围快速定位 |
| 6 | U06 | P2 | 长对话搜索、复制与局部展开 | 能定位历史消息、提取内容、单独查看工具详情 |
| 7 | U07 | P2 | Agent 能力与模式展示 | 模型、模式、连接和命令归属可辨识，未知值明确 |
| 8 | U08 | P2 | 可操作的 Context 与引用 | 可查看共享内容，发送前能检查和移除引用 |

先完成 U01-U03，再完成 U04，最后按 U05-U08 顺序推进。与既有原生托管计划有关的底层能力保持其原有优先级，本计划不以界面优化替代原生功能完整性工作。

## 4. 实施前检查

- [x] 完成当前 UI 评审与代表性渲染检查。
- [x] 记录工作项、优先级、实现边界与验证基线。
- [x] 核查两个向导失败测试；必要时单独修正测试或行为并说明依据。
- [x] 核对原生主区、结构化聊天与临时原生入口的实际调用路径。
- [x] 确认隔离 mock daemon、mock Agent 与 PTY 测试依赖可用。

## 5. 第一阶段：窄屏、待处理事项和文件范围

### U01：窄屏布局与信息优先级

主要模块：`tui/src/shell.rs`、`ui.rs`、`interaction.rs`、`workbench.rs`。

- [x] 定义按宽度和高度共同适配的紧凑布局，不只依赖隐藏侧栏。
- [x] 窄屏顶部压缩为 1-2 行，优先展示工作区与当前会话；创建、帮助和面板操作可进入菜单。
- [x] 空输入区降低高度，多行输入按内容及可用高度展开，不因光标位置变化造成不必要的正文跳动。
- [x] 发送接收者始终可辨识；长标题使用按终端显示宽度计算的省略策略，不挤掉 Agent 身份。
- [x] 底栏按剩余空间收纳辅助操作，状态、错误和连接信息优先，不再固定抢占 48 列。
- [x] 重新计算点击区域与键盘焦点，缩放后不保留失效命中区域。

验收：空草稿、无弹层时，80x24 对话区至少可显示 14 行正文，40x16 至少可显示 6 行正文；不靠删掉接收者或连接状态达标。20x8 检查不崩溃、关键操作可达。覆盖中文、emoji、长名称、多行粘贴和窗口反复缩放；宽屏侧栏与草稿保持原有行为。

### U02：待处理事项与错误反馈

主要模块：`tui/src/app.rs`、`workbench.rs`、`reasoning.rs`、`shell.rs`、`ui.rs`。

- [x] 建立统一待处理入口，区分待授权、失败、完成待查看；宽屏与窄屏均可访问。
- [x] 基于真实会话状态和事件生成事项，按 Session 和请求身份去重，不按 token 或工具增量重复计数。
- [x] 支持跳转对应会话或权限请求；后台事件不切换当前会话、不抢编辑焦点。
- [x] 定义查看、解决和清除事项的规则，历史回放不重复制造待处理事项。
- [x] 将错误反馈与运行计时分离，避免 `run_status()` 覆盖操作失败提示；长错误可打开详情。
- [x] 核对断线、请求失败、权限响应失败和 Resume 失败的反馈及重试入口。
- [x] 评估可关闭的通知选项；本轮先完成站内待处理入口，通知不作为完成前提。

验收：多个 Agent 同时运行，其中一个待授权、一个失败、一个完成时，可直接定位各项；输入草稿和引用不变。运行中触发失败操作仍能看见错误。权限解决、重复事件及迟到历史不产生重复事项。未知或未加载历史不宣称完整审计。

### U03：Files 范围与真实改动

主要模块：`tui/src/app.rs`、`ui.rs`、`shell.rs`、`input.rs`、`main.rs`；如需工作区 Git 查询，扩展现有 core、RPC、server 和 client 边界。

- [x] 增加明确的“工作区改动 / 当前 Agent 活动”视图，真实工作区改动作为代码审阅入口。
- [x] 工作区改动来自实际 Git 状态，包含 staged、unstaged、untracked、删除和重命名；不依赖 Agent 事件记录完整性。
- [x] 复用现有 diff 查询能力；新增状态读取由 daemon 所属工作区执行，避免渲染线程执行 Git 或文件 I/O。
- [x] Git 状态使用机器可读、支持 NUL 分隔路径的格式解析，覆盖空格、中文及特殊字符路径。
- [x] 展示状态、可用的增删行统计与搜索入口；二进制、大文件及查询失败有明确结果。
- [x] diff 明确范围并支持 hunk 跳转；审阅期间刷新不随意重置阅读位置。
- [x] 保留多人文件活动提示，但不称作独占归属、文件锁或已证实冲突。
- [x] 异步查询结果绑定工作区、文件和请求身份，不覆盖用户已切换的会话或更新请求。

验收：手动修改及其他 Agent 修改的文件也能进入工作区列表；当前 Agent 活动与实际改动可分别查看。覆盖暂存后再修改、删除、重命名、未跟踪文件、二进制、特殊路径和 Git 失败。返回聊天恢复原阅读位置、草稿和引用。仅审阅，不自动暂存、提交或丢弃改动。

## 6. 第二阶段：键盘操作一致性

### U04：键盘焦点与取消行为

主要模块：`tui/src/input.rs`、`interaction.rs`、`shell.rs`、`app.rs`、`native.rs`。

- [x] 区分当前会话选中态、当前面板焦点和具体控件焦点，保持鼠标与键盘一致。
- [x] 增加输入区、导航区与检查区的直达操作，减少遍历全部按钮的成本。
- [x] 为结构化工作台明确统一的取消任务绑定；评估 `Ctrl+C` 一致性与现有兼容行为，先定义契约再改映射。
- [x] 原生模式的 `Ctrl+C` 及 Agent 原生按键继续按原生语义转发；工作台操作使用明确的前缀或已约定入口。
- [x] 核对 Tab、Shift+Tab、Esc、方向键、PageUp/PageDown 在弹层、编辑器、列表和 diff 中的行为。
- [x] 帮助与命令入口和实际绑定同步，快捷键是否配置化沿用项目现有配置结构，不单独引入配置系统。

验收：仅用键盘完成会话切换、编辑、发送、取消、授权审阅、打开 diff 和返回聊天。编辑字符不触发导航；弹层不穿透到后台控件。结构化取消不依赖隐藏模式。原生输入、粘贴、终端恢复及现有 Agent 控制不回归。

## 7. 第三阶段：导航、阅读和上下文能力

### U05：会话搜索、过滤与分组

主要模块：`tui/src/workbench.rs`、`shell.rs`、`ui.rs`、`input.rs`、`interaction.rs`。

- [x] 增加模糊匹配，保留标题、Agent 名称和工作区检索；沿用成熟匹配库或项目已有能力。
- [x] 增加运行中、待处理、未读与当前工作区过滤，明确组合规则和空结果状态。
- [x] 支持工作区折叠，选中会话可定位，过滤变化不影响实际发送目标。
- [x] 补充中文粘贴、同名会话、状态变化及已折叠分组的键盘与鼠标测试。

验收：20 条以上会话、多工作区及多个同类 Agent 时，可按名称和状态定位；切换仍保留独立草稿、引用与阅读位置，消息不误投。

### U06：长对话搜索、复制与局部展开

主要模块：`tui/src/ui.rs`、`markdown.rs`、`workbench.rs`、`interaction.rs`、`input.rs`、`main.rs`。

- [x] 增加当前会话搜索、高亮、上/下一结果，以及上/下一条用户消息跳转。
- [x] 定义搜索范围：默认已加载历史；扩大到历史记录时明确加载进度和边界，不将未搜索误报为无结果。
- [x] 增加消息和代码块复制，保留原始文本；剪贴板不可用时提供明确失败或可用替代入口。
- [x] 工具详情按 Session 和 tool call 单独展开，保留全局详情开关；更新事件仍聚合为同一次调用。
- [x] 搜索、折叠和流式更新保留阅读锚点，不破坏现有滚动缓存与延迟表现。
- [x] 支持外部编辑器修改长草稿，遵循 `VISUAL`/`EDITOR` 或既有配置；取消、失败及返回终端时保留草稿。

验收：长中文对话、跨增量代码块和大量工具调用可搜索、跳转和局部查看；复制无边框和样式字符。缺失剪贴板、编辑器失败和流式输出不丢草稿；原有滚动性能回归保持通过。

### U07：Agent 能力与当前模式

主要模块：`tui/src/commands.rs`、`shell.rs`、`workbench.rs`、`native.rs`；必要时核对已有适配器能力元数据。

- [x] 紧凑展示实际已知的模型、运行模式与连接状态；窄屏详情通过明确入口展开。
- [x] 命令建议区分工作台、Agent 和原生界面操作，未接入能力有明确入口或不可用原因。
- [x] 模式切换和恢复入口沿用现有所有权及会话身份契约，不启动第二个写入者。
- [x] 用量和成本仅在适配器真实提供且含义明确时显示；缺失显示未知，不估算成精确值。

验收：不同 Agent、未知模型、断线和仅原生可用能力可辨识。`/quit`、`/resume`、`/model` 等归属保持现有 Agent 语义，工作台操作保留 `/mux` 命名空间。

### U08：Context 与发送前引用检查

主要模块：`tui/src/shell.rs`、`ui.rs`、`workbench.rs`、`interaction.rs`、`main.rs`；文件读取与编辑接入已有工作区边界。

- [x] Context 展示共享上下文正文预览、活动摘要与实际加载状态，不只列文件名。
- [x] 提供共享上下文的外部编辑器入口，读取和编辑限于正确工作区；缺失文件与失败明确提示。
- [x] 发送前可查看引用内容、来源会话和事件，并逐条移除。
- [x] 保留准备引用草稿后由用户发送的行为，不自动转发或广播。
- [x] 预览、刷新和编辑结果不影响其他会话草稿，迟到响应不替换当前工作区内容。

验收：能直接确认共享了哪些内容，以及当前将向哪个 Agent 发送哪些引用；取消编辑、文件变化、切换会话和断线均不丢失原草稿。活动摘要不被描述为完整审计或锁机制。

## 8. 验证与完成规则

- 每个工作项先增加或更新针对性测试，再执行 TUI 测试；涉及 RPC、Git 查询或共享状态时扩大对应 core、client、server 回归范围。
- 渲染覆盖 40x16、80x24、120x30、160x40，另检查 20x8 降级；既有 140x38 预览作为宽屏比较参考。
- PTY 使用隔离 mock daemon、仓库和 Agent，覆盖鼠标、键盘、粘贴、缩放、重开和终端模式恢复，不访问用户现有 daemon。
- 保留发送路由、独立草稿、引用、历史锚点、权限非抢焦点、恢复语义和迟到响应相关回归。
- 涉及长历史时运行已有滚动延迟基准，记录环境和结果，不以固定机器的毫秒值作为跨环境保证。
- 执行 `cargo fmt --all -- --check`、`git diff --check`；按改动范围执行 Clippy 并区分已有告警与新增问题。
- 每项验收通过后才勾选完成，并在下方记录修改范围、测试结果和剩余边界；不因代码已写或单个快照通过而宣称完成。

参考命令：

```bash
cargo test -p agentmux-tui
cargo test -p agentmux-tui print_frame -- --ignored --nocapture
cargo test -p agentmux-tui benchmark_scroll_latency -- --ignored --nocapture
cargo build --workspace
python3 tui/tests/multi_agent_pty.py
python3 tui/tests/close_panel_pty.py
cargo fmt --all -- --check
git diff --check
```

Pi/ACP 命令或原生输入路径有改动时，再执行对应 `pi_commands_pty.py`、`acp_commands_pty.py` 及所需原生回归。真实 Agent 测试和安装依赖单独记录，不将 mock 结果宣称为真实 Agent 的完整兼容验收。

## 9. 本轮不包含

- 整体换肤、重写布局框架或机械替换所有术语。
- 群聊、默认广播、自动任务路由、自动分工和批量创建 Agent。
- 文件锁、改动独占归属判断、自动冲突解决及自动 Git 写操作。
- 为不支持的适配器伪造模型、上下文容量、token、成本或恢复能力。
- 自动重启生产 daemon、迁移原生会话或清理用户已有修改。

## 10. 实施记录

### 2026-10-06：计划建立

- 已将本轮八项 UI 建议按 P1/P2 顺序记录为实施计划。
- 已记录渲染与 TUI 测试基线，两个向导失败项保留待核查状态。
- 本次只新增本计划文档，尚未实施 U01-U08，也未重启 daemon 或修改用户会话。
- 下一步：核查基线与原生输入边界，随后从 U01 窄屏布局开始实施。

### 2026-10-06：U01 完成

实现范围：

- `tui/src/shell.rs`：小于 90 列或 24 行时采用两行顶部、三行空输入区；输入高度按整份草稿计算并限制占用，移动光标不改变正文视口高度。
- 紧凑顶部保留工作区与会话；小于 70 列时新建空间和侧栏开关移入 Menu，常规 40 列仍保留添加 Agent、切换器和帮助。
- 顶部控件始终为 Menu 保留空间，包括小窗口和待授权计数出现时；Menu 补齐侧栏开关和帮助入口。
- 长标题按终端显示宽度与 grapheme 省略，输入区优先保留 Agent 实例身份及队列/引用计数；长 Agent 名仍保留实例序号。
- 底栏按文字和按钮的真实显示宽度分配空间，长状态时收纳辅助按钮，80 列完整展示换行提示。
- `tui/src/main.rs`：Resize 清除旧点击区域与控件焦点；原生 `native.rs` 字节转发和脱离前缀未修改。
- `tui/src/app.rs`：两个向导测试直接确认已默认选中的新建工作区，新增选项导致的旧 `j` 操作不再误入项目目录；没有改变向导的生产行为。
- `tui/src/ui.rs`、README 与 PTY 操作路径同步菜单入口。

自动化与视觉检查：

- `cargo test -p agentmux-tui --quiet`：168 通过、0 失败、3 跳过；新增 9 项布局、省略、身份保留、状态空间和缩放回归。
- 40x16 空草稿正文视口为 9 行，80x24 为 17 行，达到本计划最低 6/14 行要求；包含工作区、接收者和连接状态。
- 当前 renderer 的 40x16、80x24、160x40 预览导出通过，40/80 列位图人工检查无控件遮挡；20x8、短宽屏、多行中文/emoji 与反复缩放通过单元测试。
- `cargo build --workspace`：通过，新二进制位于 `target/debug/`。
- `multi_agent_pty.py`：40x16、80x24、120x30、160x40 全部通过；新增实时缩放到 20x8、40x16、120x30、160x40 再返回 80x24，草稿保留且未误发。
- `close_panel_pty.py`：120/80/40 列通过，覆盖鼠标突发后输入、侧栏菜单重开、单次正确恢复发送、中文输入、原会话历史与终端模式恢复。
- `pi_commands_pty.py`：120/40 列命令与思考折叠通过；超过 1,000 个 Unicode delta 的补齐、1,800 行思考与代码历史滚动通过，本次最大滚动后输入延迟为 0.109 秒。
- Pi 命令回归每个尺寸使用独立对话并显式选中目标会话，保持纯文本回复不能生成思考的断言；不再将恢复后仍可见的旧思考误判为新生成内容。后续恢复、增量补齐和长历史检查仍使用原始测试会话。
- `cargo clippy -p agentmux-tui --all-targets`：命令通过，本轮新增代码无告警；既有 `registry.rs` 未使用函数与 `commands.rs` match 简化建议仍存在，未顺带修改。
- `rustfmt --edition 2021 --check tui/src/shell.rs tui/src/interaction.rs` 与 `git diff --check`：通过。
- `cargo fmt --all -- --check`：实施前已发现工作区多处现有格式差异，本轮未批量格式化其他模块，不能宣称全工作区格式检查通过。

部署与剩余边界：

- PTY 依赖 `pyte==0.8.2`、`wcwidth==0.2.13` 安装在 `/tmp/agentmux-u01-venv`，未增加项目运行时依赖。
- 本轮只构建和运行隔离 daemon/Agent 测试，没有重启用户实际 daemon、提交 Git 或修改原生会话。
- 本轮修复的是状态与底栏按钮之间的空间竞争；运行状态覆盖操作错误的问题仍归 U02，不标记为已解决。
- 更小于 20x8 的窗口只尽力降级，不保证全部文字可同时显示；原生 Agent 的真实兼容范围仍由原生托管计划验收。
- 下一步：U02，统一待处理入口并独立呈现错误反馈。

### 2026-10-06：U02 完成

实现范围与交互约定：

- 新增 `tui/src/attention.rs`，管理待处理事项、错误反馈、只读列表和错误详情；不修改 daemon API 或原生终端协议。
- 底栏的黄色 `Pending` 入口与 Menu 的 `Pending items` 覆盖窄屏和宽屏；工作台命令为 `/mux attention`、`/mux error`，普通 Agent 命令归属保持不变。
- 权限按 `(session_id, request_id)` 区分，直到解决或所属会话终止前保持待处理；查看不等于批准，选择会打开准确的请求，不依赖当前队列头或当前输入接收者。
- 失败将明确的操作错误与当前 Error 状态合并为每个 Session 一项，全局错误单独记录；后台失败不打开弹窗。尚未载入会话元数据的失败使用未知 Agent 与短 ID 标识，不假装属于当前 Agent。
- 完成提示来自已收到的活跃状态到 Ready/Done 的变化，当前正在阅读实时输出时不额外提醒；历史加载、初始化就绪和逐 token 更新不制造完成提示。选择完成事项跳到对应会话最新输出，查看、回到最新或下一轮开始清除旧事项。
- 错误在主区单独占一行，底栏运行计时继续显示；成功状态不会覆盖未查看的错误。详情完整保留、多行可滚动，提供当前来源会话可用的 Resume、权限审阅和失败消息恢复入口；最近查看的完整错误可再次打开，不重新增加计数。
- 迟到的 native 打开/恢复结果不会自动覆盖已打开的待处理面板；只读面板拦截文字和粘贴，草稿、引用和原生按键转发语义保持不变。
- 多行状态错误拆为真实渲染行，聊天内摘要保留既有行数上限，完整内容留在错误详情。弹窗清除相邻宽字符，避免底层中文破坏边框；只读面板不显示原编辑光标。
- 历史加载只更新状态事件去重基线，不重放旧完成与已查看失败；当前未决权限快照仍可补齐已加载事件，不能为了事件去重丢掉真实授权请求。

验证与回归：

- 新增针对性状态、渲染和异步回调测试，覆盖三类事项、重复事件、迟到历史、同 request ID 跨会话路由、草稿/引用保留、长错误、只读弹层、来源身份、未知会话元数据、原生恢复焦点和中文边框。
- 新增 `tui/tests/attention_pty.py`：40x16、80x24、120x30、160x40 验证忙碌 Agent 下真实项目注册失败仍可见、三类事项、正确授权对象、完成确认、原对话 Resume、草稿保留和只读列表；测试使用隔离 daemon、worktree 和 mock Agent。
- 多 Agent onboarding、侧栏/恢复发送以及 Pi 命令、增量修复和长历史滚动继续回归；具体最终运行结果见本轮收尾记录。
- 增加可导出的 `print_attention_frame` 手动预览，检查 40 列待处理列表、独立错误行与宽屏完整错误详情，不修改既有设计截图。

本轮最终运行结果：

- `cargo test -p agentmux-tui --quiet`：186 通过、0 失败、4 跳过；相对 U01 新增 18 项自动化测试及 1 项手动预览。
- `cargo build --workspace`：通过，当前二进制位于 `target/debug/`。
- `attention_pty.py`：40x16、80x24、120x30、160x40 全部通过，包括真实失败操作、授权精确路由、Resume 和草稿保留。
- `multi_agent_pty.py`：40/80/120/160 列全部通过，包括中文命名、消息路由、重开与实时缩放。
- `close_panel_pty.py`：120/80/40 列全部通过，包括正确恢复并发送一次、原历史保留、输入突发与终端模式恢复；最后一轮突发后输入延迟为 0.21/0.11/0.12 秒。
- `pi_commands_pty.py`：120/40 列命令和思考交互、超过 1,000 条 Unicode 增量修复、1,800 行思考与代码滚动通过，本轮最大滚动后输入延迟为 0.079 秒。
- `cargo clippy -p agentmux-tui --all-targets`：通过，本轮无新增告警；保留既有 `registry.rs` 未使用函数与 `commands.rs` match 简化建议。
- `rustfmt --edition 2021 --check tui/src/attention.rs tui/src/shell.rs tui/src/interaction.rs` 与 `git diff --check`：通过；全量格式检查的现有差异仍按 U01 记录保留。
- 已检查 40 列待处理列表的位图预览，120 列错误详情及中文边框保护的渲染与自动化断言通过。预览为本地测试 fixture，不宣称真实 Agent 完整兼容验收。

剩余边界与下一步：

- 本轮先使用站内提示，不启用声音、终端 bell 或系统通知；若后续增加，必须提供关闭配置并单独验证原生协议兼容性。
- 待处理确认仅在当前 TUI 中保留，不作为持久已读状态。重新打开时，仍处于 Error 的会话会再次显示；操作错误每个来源只保留最新未查看内容及最近一次查看快照，不是完整错误审计。
- `Finished` 表示观察到活跃回合结束，不保证任务成功；缺少结构化回合事件的原生 PTY 不推断完成，未加载历史不宣称已完整检查。
- 本轮未重启用户实际 daemon、修改原生会话、提交 Git 或格式化用户其他模块的现有改动。
- 下一步：U03，明确区分工作区真实改动与 Agent 文件活动。

### 2026-10-06：U03 完成，U04/U05 继续实施

- `core/src/workspace_files.rs` 新增只读 Git 查询与有界 diff：porcelain v1 / numstat 使用 NUL 分隔，重命名按 Git 原始路径处理；非 UTF-8 路径通过可选原始字节传输，不以替换字符错误选中文件。
- 新增 `workspace/changes` typed RPC，`workspace/diff` 保持旧 JSON 请求可用，增加 Head/Staged/Unstaged、原始路径、重命名前路径和显式 binary/truncated/scope 结果。同步 core、server、client，所有 Git I/O 在 daemon 阻塞任务中运行。
- `tui/src/files.rs` 默认展示真实 Git 工作区改动，独立保留当前 Agent 活动视图；支持搜索、状态及行数、二进制/大文件说明、刷新和 stale 标记，查询失败保留真实错误。
- diff 增加范围选择与按终端实际折行计算的 hunk 跳转。范围以服务端返回值为准，旧 daemon 忽略新参数时不假称已切换成功；同一文件刷新不重置阅读位置。
- 查询缓存及点击对象绑定 workspace ID、原始路径与请求序号；刷新保留选中文件，迟到/跨工作区响应不替换当前检查内容或草稿。
- Core 3 项 Git 测试通过：暂存后继续修改、重命名、删除、未跟踪、中文/空格/tab/换行、非 UTF-8 路径、二进制、大文件、无 HEAD、Git 失败及 symlink/父目录越界。
- Client/server 集成 `workbench_history_and_diff_roundtrip` 通过，验证 typed changes、Head/Staged/Unstaged，以及既有恢复历史和路径隔离。
- 新增 `files_pty.py`：40x16、80x24、120x30、160x40 全部通过，验证手动 Git 修改不依赖 Agent 事件、搜索、三个 diff 范围、hunk 导航、草稿不误投，检查前后 Git 状态完全相同。
- 当前 TUI 测试为 197 通过、0 失败、4 手动测试跳过；`git diff --check` 通过，workspace build 通过。此结果不是 U04-U08 的最终完整验收。
- U04 已加入焦点区域、F2/F3/F4 直达、F6 双向循环、结构化 Ctrl+C 一致取消、阅读区 Home/End 和 hunk 快捷键；原生字节转发包含 Ctrl+C/F-key/粘贴序列的原样转发测试。仍需完整键盘工作流与终端回归。
- U05 已加入 `nucleo-matcher 0.3.1` Unicode 模糊匹配、运行中/待处理/未读/当前空间过滤与工作区折叠；新增 25 会话的过滤、中文、草稿和接收者测试。仍需补充同名、动态状态、折叠命中及真实终端验收。
- 当前持续目标未缩减：继续完成 U04/U05 验收、实施 U06/U07/U08，再按整个计划逐项审计；未重启实际 daemon、未做 Git 提交。

### 2026-10-06：U06-U08 实现进展，最终验收仍未完成

- 新增 `transcript.rs`：已加载会话的逻辑消息搜索与高亮、相邻结果和用户消息跳转、完整原文与代码块复制、明确的未搜索旧历史边界；回复由既有流式投影拼接，代码由 CommonMark 解析。
- 工具调用按 Session/toolCallId 合并增量，提供单个展开开关并保留全局开关；冻结历史只使用锚点之前的工具数据，不读取后来输出。
- 新增 `external.rs`：使用 VISUAL/EDITOR 的 argv 解析调用编辑器，暂停 TUI 输入并恢复终端；取消/失败保留原稿。剪贴板命令失败不假报成功，可将原文保留在当前 TUI 生命周期内的私有临时文件。
- 新增 `agent_info.rs`：实际执行模式、连接状态及已报告模型/模式/版本/用量/成本；未报告显示 Unknown。Pi 使用只读 get_state/get_session_stats 查询，不通过发送提示词猜测能力。
- 新增 daemon `workspace/context`、`workspace/context/save` typed RPC 与 `workspace_context.rs`：限定工作区、限制读取大小、阻止越界 symlink、比较原内容并原子替换。UI 在冲突时保留编辑稿，不覆盖已发现的新内容。
- 新增 `context.rs` 和 `references.rs`：共享上下文/活动/未保存稿预览、外部编辑和显式 Save；引用可读取原始事件并逐项移除，保留原接收者与草稿，不自动发送。
- 当前 TUI 基础测试为 205 通过、0 失败、4 跳过；core 上下文 2 项测试通过，workspace build 通过。现有滚动基准 median 4.35 ms、p95 6.85 ms、max 8.97 ms，此数值仅代表本次环境。
- 全 workspace 测试发现并同步了旧四个 builtin profile 数量断言；当前真实默认配置包含四个 native 与四个 structured profile，client 12 项测试已通过。
- 完整回归还发现 Pi 恢复预检失败仍留在 Done 状态。已用只修改仍处终态记录的方式记录 Error，保留原始 ID/文件定位，避免干扰另一恢复者；`pi_missing_or_legacy_locator_does_not_silently_start_a_new_conversation` 修复后通过。
- 尚需完成：U04/U05 的真实键盘/动态过滤/折叠回归；U06-U08 的端到端编辑、复制、元信息、引用及并发上下文保存测试；所有 viewport 截图与像素/边框检查；完整 workspace 测试、格式、Clippy 与全部既有 PTY 回归；逐条完成审计后才勾选 U04-U08。
- 当前整体目标保持 active，未标记完成；未重启用户实际 daemon，未改用户原生对话，未提交 Git。

### 2026-10-06：U03-U08 最终逐项审计完成

原目标保持不变。本记录覆盖全部计划条目，而非只覆盖已实现的子集。U04-U08 上方任务清单在以下证据核对完成后勾选。

| 工作项 | 完成证据 | 验证范围 |
| --- | --- | --- |
| U03：真实改动、路径、状态、scope、刷新、hunk、失败与隔离 | `core/src/workspace_files.rs`、`tui/src/files.rs`、typed changes/diff RPC、core Git 测试、client/server roundtrip、`files_pty.py` | staged/unstaged/untracked/delete/rename、UTF-8 与原始路径、binary/large/escape/Git failure；四尺寸；审阅前后 Git 状态不变 |
| U04：选中态/焦点/直达/循环 | `focus.rs`、焦点 rails 与控件高亮、F2/F3/F4/F6/Shift+F6、`navigation_pty.py` | 三个区域、当前接收者和草稿不变、鼠标与键盘一致；四尺寸 |
| U04：取消及原生按键 | main-pane Ctrl+C、`native::Escape` 原始 Ctrl+C/F-key/paste 字节断言、native broker 所有权回归 | Fake Pi slow turn 的真实取消、草稿保留、原生字节原样转发；不增加第二写入者 |
| U04：弹层、翻页、返回与帮助 | `keyboard_only_send_permission_diff_and_return_workflow`、既有 Tab/BackTab/occlusion 测试、scrollable Help、menu Home/End/Page keys | 仅键盘发送、授权审阅、打开 diff、回到聊天；Page keys 作用于当前区域；配置评估采用现有绑定契约，不另建 keymap 系统 |
| U05：模糊匹配及过滤组合 | `nucleo-matcher 0.3.1`、25 会话单元测试、running/pending/unread/current-space 过滤 | Unicode、中文粘贴、状态变化、空结果；exact match 优先于当前空间弱模糊匹配 |
| U05：同名路由、折叠与定位 | exact-other-space 回归、collapsed-header 保持可见、member selection 自动展开、`navigation_pty.py` | 四尺寸各 25 个会话、两个同名跨空间会话、消息只投正确 session、未发送稿不丢失 |
| U06：搜索/高亮/结果与用户消息跳转 | `transcript.rs`、流式拼接/跨 span 高亮/前后用户消息单元测试、reader hit navigation | 当前 Session 逻辑消息；原始文本跨 chunk 拼接；跳转固定序列锚点 |
| U06：历史边界 | loaded-history scope、loading/older 状态、Older typed history action 与既有分页/增量补齐回归 | 不把未加载历史报为已无结果；不以历史回放覆盖新状态或冻结阅读 |
| U06：消息/代码复制及失败 | CommonMark code extraction、real clipboard helper、private fallback、`workbench_features_pty.py` | 四尺寸逐字节比对完整中文原文、缩进及换行；复制失败仍完整保存，临时文件权限 0600 |
| U06：工具单独展开与聚合 | per-Session/toolCallId 投影、局部 toggles/global switch、frozen-anchor tool test | rawInput/rawOutput/status 增量合并为一次调用；历史锚点排除未来输出 |
| U06：外部编辑与滚动性能 | argv parsing、VISUAL/EDITOR、PTY editor 验证 ICANON、success/failure keep draft；滚动 benchmark 与 Pi 长历史 PTY | 四尺寸编辑不自动发送；终端模式恢复；1800 行思考和代码、1000+ Unicode delta；缓存滚动不回退为每帧解析 |
| U07：真实信息与命令来源 | `agent_info.rs`、summary/Agent details、Pi get_state/get_session_stats、available commands | execution/daemon/agent/model/thinking/version/usage/cost，缺失 Unknown；不生成模型提示词来猜信息 |
| U07：刷新、原生/结构化与恢复 | `pi_commands_pty.py` 显式验证 Model: large / Thinking: high / Unknown usage+cost；ACP native-only draft protection、恢复所有权测试 | mode/身份契约不变，普通 `/...` 归 Agent，`/mux` 归工作台；定位失败记录 Error 且不新建对话 |
| U08：上下文、活动与实际状态 | context/activity/draft views、完整窄屏检查窗口、bounded daemon context RPC、missing/error/stale/truncated 表达 | 固定工作区路径；滚轮/键盘真实滚动；缺失与查询失败不伪造为空内容 |
| U08：外部编辑、保存、文件变化 | optimistic expected-content checks、atomic replacement、editor draft、持续 save_error、client/server context roundtrip | 四尺寸真实保存与外部并发改变拒绝覆盖，编辑稿和聊天稿都保留；越界 symlink/超限拒绝 |
| U08：引用内容、源、接收者与逐项移除 | canonical chunked event reads、references panel、marker tracking、original-recipient-after-selection-change 单元测试 | 来源 event 与 target Session 明确，移除只影响对应 draft；无自动发送/广播，迟到回复不改当前工作区 |

最终工程结果：

- `cargo test --workspace --quiet`：387 通过、0 失败、4 个手动测试跳过；其中 TUI 209 通过。四个跳过项是既有手动预览/性能测试及新增预览，代表性预览和性能基准已另行执行。
- `cargo build --workspace`：通过。
- `cargo clippy --workspace --all-targets -- -D warnings`：通过，零告警。
- `cargo fmt --all -- --check` 与 `git diff --check`：通过。为满足计划的全量格式 gate，采用 formatter 处理了原有排版差异；不回退任何既有语义改动。
- `navigation_pty.py`、`workbench_features_pty.py`、`files_pty.py`、`attention_pty.py`、`multi_agent_pty.py`：40x16、80x24、120x30、160x40 全部通过。
- `close_panel_pty.py`：120/80/40 列通过，native 恢复、单次发送、Unicode、终端模式恢复及重开历史通过。
- `pi_commands_pty.py`：120/40 列命令、真实元信息刷新、1000+ 增量修复、1800 行思考/代码、精确目标思考块折叠通过；本轮末次长历史 wheel-to-input 最大 0.107 秒。
- `acp_commands_pty.py`：120/40 列命令发现、补全、原生限定命令保留草稿、verbatim 路由通过。
- `benchmark_scroll_latency`：median 4.02 ms、p95 5.46 ms、max 6.19 ms；仅代表本次测试环境，不是跨机器保证。
- `print_frame`：通过；`audit::all_views_fit_and_render_nonblank_at_required_sizes`：通过，8 个视图 x 6 个 viewport，共 48 个渲染 artifact，覆盖 20x8、40x16、80x24、120x30、140x38、160x40。
- 全部 48 张 artifact 生成位图并检查非空像素方差，最小 RGB 方差和 1002.54；人工检查窄屏 reader/context/info、宽屏 picker，无文字/控件遮挡，命中区域均在边界内、cell 无 CR/LF。图片在 `/tmp/agentmux-ui-audit`，未覆盖既有设计文件。
- README 已同步新视图、命令、焦点/取消契约、历史边界、复制回退、上下文与引用行为。

部署与能力边界保持原计划：没有重启实际 daemon、调用用户实际模型/凭据、修改用户原生会话或提交 Git。PTY/渲染测试使用隔离 mock/fake Pi，不将其宣称为所有真实 Agent 版本的完整兼容性保证。当前依计划验收范围没有已知未修复错误，持续目标完成。

### 2026-10-06：顶部与侧栏视觉层级复核

根据用户提供的实际截图，右上角两排入口边界不齐，辅助操作与主操作混排，侧栏也有重复标题和计数。本次在保留功能契约的前提下调整：

- 宽屏顶部仅保留同一行的 New space / Add agent 与 Agents / Menu 两组，右边界与侧栏内部右边界一致，不使用按钮自动换行。
- 面板开关通过 Menu 和侧栏 Close panel 操作，Help 移到底栏；窄屏仍保留添加 Agent、切换器和 Menu，20 列保留 Menu 作为收纳入口。
- 侧栏标题使用实际空间名称和分支；相同分支名不重复展示；只在存在其他空间成员时显示 elsewhere 数量。取消 Agents tab 下重复的 Agents 标题，保留 relay 选择阶段标题。
- 新增顶部单行、右边界、间距与辅助入口位置测试；TUI 为 211 通过、0 失败、4 跳过。多 Agent PTY 40/80/120/160 列通过；面板关闭与重开、恢复发送 PTY 120/80/40 列通过；严格 TUI Clippy、格式和 diff 检查通过。
- 实际 renderer 140x38 与 80x24 预览检查通过，宽屏位图位于 `design/agentmux-header-refined-2026-10-06.png`，不覆盖既有设计图。README 已同步入口位置。
- 截图中的 workspace/context method-not-found 是旧服务兼容性线索；本次没有为了消除提示而重启用户实际 daemon 或中断运行中的 Agent。

### 2026-10-06：用户要求修复实际 Files/Context RPC 错误

- 实际 default socket 返回 `-32601`，旧 daemon PID 4074529 的 executable 已显示 deleted；磁盘上的新 server 已包含两个方法。确认 3 个会话均无 Prompting/WaitingPermission/Connecting 后执行升级。
- 在线 SQLite 备份、会话日志和配置保存在 `/root/.local/share/agentmux-rpc-backup-oi8c2v6s`，先保存原 session ID、workspace ID、agent ID 和原生 ID。
- 通过 server/shutdown 受控退出，保留原 socket/data/config 参数启动新 daemon PID 4050323。三个持久会话标识完全一致，没有删除对话。
- 对用户实际工作区 `111` 验证 `workspace/changes` 成功返回 2 个文件，`workspace/context` 成功返回 235 字符共享上下文和 282 字符活动；没有写入用户上下文或发送模型提示词。
- 尝试只恢复重启前 Ready 的连接：OpenCode 在原 session/native ID 上恢复成功；旧 Pi 记录没有保存可唯一定位的原生文件，恢复明确拒绝，未猜测或创建替代对话。原本 Done 的 Pi 保持 Done，所有历史保留。
- 现有 TUI 进程仍持有旧连接，需正常退出后重开；不强杀该窗口，避免丢失其进程内未发送稿。旧 Pi 如需继续，由用户从原生历史中明确选择原文件。
