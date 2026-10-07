# agentmux

Orchestrate multiple AI coding agents — Claude Code, Codex, OpenCode, pi —
from one terminal UI.

agentmux is a local, open-source tool that unifies existing agent CLIs behind
the [Agent Client Protocol](https://agentclientprotocol.com) (ACP) and adds the
orchestration layer on top: parallel sessions, per-task git worktrees, *shared*
workspaces where several agents collaborate, and a persistent event log you can
replay.

**Status:** v1 is a daemon + terminal UI. A desktop app (Tauri) is planned for
Phase 2 — the daemon API is already client-agnostic so it plugs in without
rework.

## Features

- **Multi-agent sessions over ACP** — built-in profiles for Claude Code
  (`claude-code-acp`), Codex (`codex-acp`), OpenCode (`opencode acp`) and pi
  (`pi --mode rpc`, translated internally). Any other ACP-compatible agent
  (kimi, auggie, …) plugs in via `[[agents]]` config — no code changes.
- **Client–server architecture** — `agentmux-server` owns all sessions and
  state and exposes a newline-delimited JSON-RPC 2.0 API over a Unix socket.
  `agentmux-tui` is a pure client: quit or crash it and your agents keep
  running.
- **Git worktree isolation** — each workspace is a real worktree at
  `<repo>/.agentmux/worktrees/<name>` on branch `agentmux/<name>`.
- **Shared workspaces** — attach several sessions to one worktree. Agents
  coordinate through a `.agentmux/` blackboard (`context.md` you and agents
  can edit, `activity.md` the daemon maintains automatically), and a shared
  context preamble is injected into prompts while ≥2 sessions share a
  workspace.
- **Manual relay** — pick an event or file edit from session A, hit `@`,
  choose session B: it lands in B's next prompt as quoted context.
- **Event persistence** — every normalized event is appended to
  `<data-dir>/sessions/<id>.jsonl` and metadata lives in a SQLite store, so
  session history survives restarts (`session/resume` is in the API).

### Honest v1 limitations

- **Permissions are reviewed in the TUI.** Requests wait in an inbox;
  `/permissions` opens the next request without interrupting your typing.
  Allow once / always (when offered), reject, or leave it pending with `Esc`.
- **No conflict arbitration in shared worktrees.** Shared workspaces suit
  sequential handoff (A scaffolds, B fills in) and partitioned parallel work;
  two agents editing the same file concurrently is a real git conflict.
- **Linux / macOS only** (Unix sockets), local machine only — no remote
  daemons.
- **No auto-delegation** — you choose which agent gets which prompt; a
  coordinator/router is on the backlog.
- Agents' own TUIs are not embedded (no PTY passthrough) — sessions are
  driven purely through the structured ACP event stream.
- Projects can be registered with `/project /absolute/repo/path` in the TUI.
  Custom agent profiles still use the config or JSON-RPC API.
- Unsent prompt queues belong to the current TUI process. `/unqueue` restores
  the last queued prompt for editing; quitting warns before discarding queues.

## Requirements

- Linux or macOS
- Rust stable toolchain, git
- One or more agent CLIs, installed **and authenticated** with their
  provider — see the built-in table under [Configuration](#configuration).
  agentmux only checks that each configured binary exists; spawning agents
  that can't authenticate will surface as session errors at prompt time.
- No API keys needed for development/testing — the workspace ships a
  deterministic `agentmux-mock-agent` fixture (see
  [Smoke test without real agents](#smoke-test-without-real-agents)).

## Install

```bash
git clone <this repo> && cd agentmux
cargo install --locked --path server   # installs agentmux-server
cargo install --locked --path tui      # installs agentmux-tui
```

Both binaries must be reachable together: the TUI auto-starts the daemon by
locating `agentmux-server` as a sibling of its own executable, on `$PATH`, or
via `$AGENTMUX_SERVER_BIN`. `cargo install` puts both in `~/.cargo/bin`, which
just works. A plain `cargo build --release` gives you the same binaries under
`target/release/`.

## Quickstart

```bash
# 1. Start the daemon (optional — agentmux-tui auto-starts it if absent)
agentmux-server --daemon

# 2. Register a project (a git repository). v1 has no CLI for this yet;
#    any JSON-RPC client works — e.g. with socat:
echo '{"jsonrpc":"2.0","id":1,"method":"project/register","params":{"root_path":"/abs/path/to/your-repo"}}' \
  | socat - UNIX-CONNECT:$HOME/.local/share/agentmux/agentmux.sock

#    or python3 (no extra tools needed):
python3 - <<'EOF'
import json, os, socket
sock = os.environ.get("AGENTMUX_SOCK",
                      os.path.expanduser("~/.local/share/agentmux/agentmux.sock"))
req = {"jsonrpc": "2.0", "id": 1, "method": "project/register",
       "params": {"root_path": "/abs/path/to/your-repo"}}
s = socket.socket(socket.AF_UNIX); s.connect(sock)
s.sendall(json.dumps(req).encode() + b"\n")
print(s.makefile().readline())
EOF

# 3. Open the TUI
agentmux-tui

# After updating binaries: restart the daemon and open the TUI in one command
agentmux-tui --restart-daemon
```

Normal startup connects to the existing daemon, or starts it if absent.
`--restart-daemon` stops the background service before opening the UI and
preserves its data/config paths. Running agent connections end; use **Resume**
or **Resume & send** to continue a saved task on a fresh adapter conversation.
Older daemons that do not report their paths use `AGENTMUX_DATA_DIR` and
`AGENTMUX_CONFIG` (or defaults); set these to the original paths if customized.

Inside the TUI, click **+ New space** (or **Menu → New space** in a narrow
terminal). Choose a project (or **Add project** and paste your Git repository
path), create a space and choose its first agent.
To add another agent to the same codebase, click **+ Add agent**, choose an
agent and send it a task. The target space and branch are shown before adding.
No Vim keys or slash commands are required.

### Multiple agents in one space

A **space** is a Git worktree and branch. Its agents share the files, but each
has an independent conversation, draft and prompt queue. Adding an agent does
not copy the current chat or automatically start a task. Existing shared
blackboard context still applies when several sessions share a worktree.

**New space** starts separate work and defaults to creating a new worktree;
the wizard also allows choosing an existing space. **Add agent** goes directly
to agent selection in the current space, without selecting the project again.
The same agent profile can be added more than once. **Agents** groups the
current space's conversations before other spaces, and **To:** in the composer
identifies the recipient. Messages are sent only to that conversation.

Use **Menu → Name agent conversation** to give an agent a task or role name,
such as "Regression tests". Names are persisted in the event log and survive
reopening the TUI; they reset when Resume starts a fresh conversation. This
workbench name is separate from Pi's native `/name` control. Naming requires
the updated daemon as well as the TUI; an older daemon reports a clear RPC
error and keeps the name field for retry.

Creation is guarded against duplicate submission. Closing the creation panel
after submitting lets it finish in the background without changing the active
conversation. Adapter setup failures appear as failed agent conversations;
**Resume** retries that same session rather than adding another one.

## Terminal workbench

The conversation and multiline editor occupy the main area. At 110 columns
and wider, a right sidebar shows **Agents / Files / Context**; narrower terminals
use a temporary sidebar. Below 90 columns or 24 rows, the compact workbench
keeps the space and conversation in a two-line header and reduces the empty
composer to three rows. Draft height follows the whole text rather than the
cursor position. The header uses a single row of creation and navigation
actions; **Menu → Show / hide sidebar** controls the panel and **Help** stays
in the footer. Below 70 columns, **Menu → New space** starts another space.
Task names come from the
first prompt and survive reopening through the persisted event log.

Conversation presentation follows [OpenCode's user panels and Markdown styles](design/OPENCODE-NOTICE.md):
user messages fill a subtly raised panel with a left accent rail; assistant prose
uses the main canvas. Fenced and indented code has a language label, syntax
highlighting and a bordered surface. Long code wraps by terminal columns while
preserving indentation and Chinese text. Reply chunks are assembled before
Markdown rendering so streaming, history repair and scrolling retain fence context.
Scrolling reuses cached physical rows and paints only the visible viewport;
older rows are prefetched in batches, and daemon history pages are requested
only near the oldest loaded row. Thought bodies retain their layout while
the timer updates independently. Width changes, folding and new visible content
refresh the layout. Input bursts are coalesced into frames at up to 60 FPS.

To measure scrolling render latency with the long Chinese reasoning fixture:
`cargo test -p agentmux-tui benchmark_scroll_latency -- --ignored --nocapture`.
The Pi PTY regression also exercises wheel bursts over long reasoning/code,
immediate typing, return to latest, and mouse folding after scrolling.

The OpenCode-inspired layout uses a dark canvas, neutral sidebar, warm input
accent and a single highlighted Send action. Routine lifecycle events stay in
**Details**; tool updates collapse into one row per call. Conversation prose
renders headings, bold text, inline code, lists and fenced code blocks.

- Click an agent conversation in the sidebar, or **Agents** to search. Each keeps its own
  draft, references and reading position while the TUI is open.
- The sidebar counts members and active agents in the current space separately
  from the total across all spaces. **+ Add agent** is also available there and
  in the conversation picker, including on narrow terminals.
- Click **Send** (or press `Enter`); while the agent is busy it becomes **Queue**.
  **New line** adds a line, and **Stop** cancels the current turn.
- Pi reasoning streams into a separate, expanded **Thinking** block by default.
  It becomes **Thought · Ns** when finished; click its header to fold or expand
  that block. **Fold thoughts / Show thoughts** in the footer controls all blocks
  (also available in Menu on narrow terminals). The bottom status bar shows Waiting,
  Thinking, Running tool or Responding with elapsed time; the composer keeps just
  the agent name and queue/reference counts. Only reasoning actually
  returned by the provider is shown; Pi's `/thinking` still sets the model's
  reasoning level, while `/mux thinking` folds or expands the display.
- Streaming redraws are capped at 30 frames per second so token bursts do not
  block event consumption. Missed notifications are repaired automatically from
  paginated session logs, including reasoning and reply text; transport lag
  notices stay out of the conversation. Reopening also fills an initial page
  that starts partway through the current conversation.
- If a task stopped or shows `daemon restarted`, the primary action becomes
  **Resume & send**. It reconnects to the original native conversation and sends
  the submitted message once the agent is ready. **Resume** alone reconnects
  without sending the draft. Pi uses its saved session file; ACP adapters must
  advertise `loadSession` support. Chat, title, and native context remain intact
  across reconnections and daemon restarts. Missing files, unsupported adapters,
  or cancelled restoration fail explicitly; no replacement conversation is
  silently created. Unsent messages remain recoverable as drafts.
  Older Pi records without a saved session file cannot yet be restored through
  this path. Their history remains readable; do not delete their native files.
  A failed first setup with no native identity may retry initial creation.
- Click **Files**, then a filename to inspect the **whole workspace** diff
  against HEAD, including staged changes and untracked files. **Back to chat**
  or `Esc` returns to the conversation.
- Files defaults to **Git / Workspace** changes, independent of agent events.
  **Activity** shows only the selected agent's loaded file activity. **Find**
  filters paths; **Refresh** keeps the selected path. Diff scopes are **HEAD**,
  **Staged** and **Unstaged**; **Prev hunk / Next hunk** navigate wrapped rows.
  Binary, large, truncated, stale and unavailable results are explicit.
- The **Agents** picker uses Unicode fuzzy matching and combines **Running**,
  **Pending** or **Unread** with **This space**. Exact names precede weaker
  fuzzy matches. Workspace headers fold/unfold; picking a hidden member expands
  its group without copying or broadcasting conversations.
- **Menu → Search conversation** searches logical messages in loaded history,
  including text assembled across streamed chunks. **Older** loads additional
  history explicitly. The reader highlights exact text matches, provides result
  navigation, message/code-block copying and **Jump** back to chat. Individual
  tool headers expand independently of the global details switch.
- **Menu → Edit draft externally** uses `VISUAL`, then `EDITOR`, then `vi`.
  Successful edits remain unsent; failed/cancelled editors keep the old draft.
  Clipboard commands use the local clipboard utility or
  `AGENTMUX_CLIPBOARD_COMMAND`; failures retain readable original text in a
  private temporary file until this TUI closes, rather than reporting success.
- **Agent details** shows actual execution/connection state and reported
  model, thinking, version, usage and cost. Missing values are **Unknown**.
  **Refresh** queries Pi metadata without sending a prompt; adapter/native-only
  controls stay in their own command namespace.
- **Context** previews shared context and activity. **Open** provides a full
  inspection window, also used automatically on narrow terminals. **Edit**
  prepares an unsaved external-editor draft; **Save** checks the original file
  before committing it. Concurrent changes reject the save and preserve both
  the shared file and local edit. **Draft references** previews each quoted
  event with its source and recipient; **Remove** detaches it without sending.
- File activity marks paths seen in more than one agent's loaded event history.
  This is an activity hint, not a file lock, complete audit or proof of a conflict.
- Click **Permissions** in the sidebar when requests are waiting, or
  **Menu → Review permissions**. **Allow once**, **Reject**,
  **Later**, and **Always allow** (when offered) are explicit buttons.
  Background requests never grab the editor's focus.
- **Pending** in the footer, or **Menu → Pending items**, groups permission
  requests, failures and unviewed turn endings. Each item identifies its agent
  and space; selecting a permission opens that exact request, selecting a
  finished turn returns to its latest output. Drafts and references stay with
  their original conversations.
- Operation errors remain in a separate error line while the run timer
  continues. **Error details** opens the full, scrollable message and offers
  **Resume**, **Permissions** or **Recover message** when applicable. Reviewing
  a failure acknowledges its pending item; **Menu → Error details** can reopen
  the last reviewed error. Other successful operations do not erase unreviewed
  errors, and background failures do not open dialogs automatically.
- The mouse wheel scrolls the pane under the pointer. **Latest** returns to
  live output; **Details** expands tool output and lifecycle events.
  These actions are also available in Menu on narrow terminals.
- **Menu** contains sidebar controls, context, quoting events to another task,
  thinking visibility, message recovery, resume, and exit. Restoring a message
  keeps the previous draft and its references available through message recovery. Quoting uses two
  clicks: the source event and the target task, then Send when ready.
- Click within the editor to position the text cursor, including CJK and emoji.

Pending acknowledgements and operation errors belong to the current TUI.
An agent still in an Error state is shown again after reopening. Ordinary
history pages do not generate completion notices. **Finished** means an
observed active turn ended, not proof that the task succeeded; native PTYs
without structured turn events do not fabricate turn-completion notices.
The inbox is not a complete historical audit, and desktop/sound notifications
are not enabled.

Set `AGENTMUX_THEME=dark` (default), `light`, or `terminal` before starting the TUI.
The terminal variant preserves the terminal's own background. Mouse interaction
requires a terminal that forwards mouse events; keyboard controls remain available.

## Keyboard controls

`F2` focuses input, `F3` chat/diff and `F4` navigation. `F6` / `Shift+F6`
cycles panes; focus rails differ from the selected-agent highlight. `Ctrl+C`
cancels the selected structured turn from any main pane and keeps its draft.
Attached native terminals retain native Ctrl+C, function keys and paste;
`Ctrl+]` then `m` returns to the workbench. `PageUp/PageDown` act on the focused
pane; diff reading also supports `Home/End` and `[` / `]` hunk navigation.
Tab / Shift+Tab focuses visible controls. Help is scrollable on small terminals.
These bindings follow the existing workbench contract, without a separate
keymap configuration system.

Type `/` to open command suggestions. Use arrows and Enter, click a command,
or press Tab to complete it. For Pi tasks, `/model`, `/thinking`, `/settings`,
`/compact`, `/session`, and `/name <name>` use the running Pi adapter's RPC
controls. `/new` creates a new Pi conversation in the current workspace.
Pi's discovered extension commands, prompt templates and `/skill:...` entries
are sent with the leading slash intact, including in multi-agent workspaces.

ACP agents (including OpenCode) publish their commands through
`available_commands_update`; the workbench now uses those per-session catalogs
for completion, including descriptions and argument hints. Catalogs are restored
when reconnecting an agent or reopening the TUI, even when their notifications
fall outside the latest transcript page. Agent-defined commands override
adapter defaults, but `/mux` remains reserved for workbench actions.

For OpenCode ACP tasks, `/init`, `/review`, configured commands and skills are
discovered from the running agent and sent unchanged. `/compact` uses OpenCode's
ACP compaction command; `/new` and `/clear` create a conversation in the same
workspace, while `/exit`, `/quit` and `/q` stop only that agent. Native UI commands
such as `/models`, `/sessions`, `/undo` and `/export` are marked as native-only:
they keep the draft and are never silently turned into model prompts. Add an
**OpenCode (native)** agent in the current workspace to use its original command
menus inside the managed terminal. This opens a separate native conversation;
it does not transfer the existing ACP conversation.

With any agent selected, use `/mux <command>` for workbench actions, e.g.
`/mux new` opens the space wizard, `/mux add-agent` adds to the current space,
`/mux rename` names the workbench conversation, and `/mux resume` restores a stopped agent's native conversation.
Unqualified slash commands belong to the selected agent, even when they share
names with workbench actions. Pi `/quit` stops that agent; `/mux quit` exits
the workbench without stopping agents. With no agent selected, legacy local
slash commands remain available for onboarding.
The clickable workbench buttons keep their original meanings. Pi native UI
features such as `/login`, `/tree` and its native session browser `/resume` are not yet
embedded here; they show an explicit limitation instead of being sent to the
model. Extension commands requiring Pi's extension UI dialogs still require
a compatible native client; this workbench does not answer those dialogs.

The Pi controls require the updated **daemon and TUI**, because `session/pi`
is a new RPC endpoint. Restarting only the TUI against an older daemon will
report `method not found: session/pi`. Use `agentmux-tui --restart-daemon`
to load the new daemon binary and open the UI together.

| Key | Action |
|---|---|
| `Tab` / `Shift+Tab` | Select visible controls; `Enter` activates the selection |
| `Enter` in the editor | Send, or queue until this agent's turn finishes |
| `Alt+Enter` / `Shift+Enter` | Insert newline (terminal support varies; the New line button always works) |
| Arrows / `Home` / `End` / `Delete` | Edit text; arrows select rows in lists |
| `Esc` | Go back or close; never cancels a running turn |
| `PageUp` / `PageDown` | Scroll output or permission details |
| `Ctrl+P` / `Ctrl+B` | Find a task / toggle sidebar |
| `F1` | Open getting-started help |
| `Alt+↑` / `Alt+↓` | Input history for the selected task |

Workbench commands remain available under `/mux`: `new`, `add-agent`, `rename`, `project <path>`,
`tasks`, `focus`, `files`, `context`, `permissions`, `relay`, `cancel`,
`resume`, `kill`, `latest`, `tools`, `thinking`, `unqueue`, `recover`, `quit`.
Legacy browse shortcuts remain optional; normal typing starts in the editor and
stays there after `Esc`. Prompt queues live in this TUI until sent. If prompts
are queued, exiting requires choosing Exit again to discard them; agents continue
running in the daemon.

## Architecture

The product direction is **native hosting for completeness, structured adapters
for enhanced workflows**. Native PTY hosting and native history browsing are
planned, not implemented yet. Current structured-mode limitations are tracked
in [the capability preservation plan](docs/superpowers/plans/2026-09-30-agent-capability-preservation-plan.md).

```
┌──────────┐     ┌──────────┐
│ agentmux │     │ desktop  │  (Phase 2, Tauri — planned)
│  -tui    │     │          │
│(ratatui) │     └────┬─────┘
└────┬─────┘          │
     │  JSON-RPC 2.0 over Unix socket (newline-delimited)
     └────────┬───────┘
              ▼
     ┌─────────────────┐
     │ agentmux-server │   daemon: session ownership, state machines,
     └────────┬────────┘   event broadcast, persistence
              │
     ┌────────┴────────┐
     │  agentmux-core  │   domain model, AgentAdapter, ACP client,
     └────────┬────────┘   WorktreeManager, EventStore
              │  ACP (JSON-RPC over stdio)
   ┌──────────┼───────────┬──────────┐
   ▼          ▼           ▼          ▼
claude-   codex-acp   opencode    pi --mode rpc
code-acp               acp      (internal translator)
```

Cargo workspace:

| crate | role |
|---|---|
| `agentmux-core` | domain model, agent adapters, event store, worktree/collab logic — no UI deps |
| `agentmux-server` | the daemon binary: unix-socket JSON-RPC API, event broadcast, auto-start support |
| `agentmux-client` | typed client SDK used by the TUI (and any future frontend) |
| `agentmux-tui` | ratatui terminal client |
| `agentmux-mock-agent` | deterministic ACP agent used by tests and smoke runs |

Sessions are owned by the daemon and progress through
`created → connecting → ready → prompting → waiting_permission → done | error`;
a finished or errored session can be resumed (`session/resume`).

## Daemon API

Newline-delimited JSON-RPC 2.0 on the unix socket — one request per line,
one response per request, `session/event` notifications pushed to subscribed
connections. Every method name and params shape lives in
`core/src/rpc.rs`:

| method | purpose |
|---|---|
| `project/register` · `project/list` · `project/remove` | manage registered git repos |
| `workspace/create` · `workspace/list` · `workspace/remove` | worktree-backed workspaces |
| `session/create` · `session/prompt` · `session/cancel` · `session/kill` · `session/list` · `session/resume` | session lifecycle; setup failures return a persisted session in Error for retry |
| `session/title` | persisted workbench conversation title, independent of adapter names |
| `session/subscribe` | stream normalized events (same stream to every subscriber) |
| `session/history` | read persisted events before a sequence cursor, bounded by 200 events and serialized bytes; includes title and unresolved permissions |
| `session/event/read` | read UTF-8 chunks of an oversized persisted event; the client SDK reassembles these transparently |
| `workspace/diff` | inspect a literal file path inside a worktree against HEAD |
| `agent/list` · `agent/register` | configured agents + availability probe |
| `server/status` · `server/shutdown` | daemon introspection |

## Configuration

The optional TOML config registers agents. Resolution order:
`--config` > `$AGENTMUX_CONFIG` > `$XDG_CONFIG_HOME/agentmux/config.toml`
(default `~/.config/agentmux/config.toml`). See
[`config.example.toml`](config.example.toml) for an annotated copy.

```toml
[[agents]]
id = "kimi"            # `id`/`name` default to each other; one is enough
name = "Kimi"
kind = "acp"           # "acp" (default) or "pi-rpc"
command = "kimi-acp"   # required — PATH name or path with a dir component
args = ["--stdio"]

[agents.env]           # env vars for the spawned process
# SOME_VAR = "value"
```

An entry whose `id` matches a built-in replaces it in place; other entries
are appended. Built-ins:

| id | command | kind |
|---|---|---|
| `claude-code` | `claude-code-acp` | acp |
| `codex` | `codex-acp` | acp |
| `opencode` | `opencode acp` | acp |
| `pi` | `pi --mode rpc` | pi-rpc |

## Files and paths

| what | where |
|---|---|
| socket | `$AGENTMUX_SOCK` > `<data-dir>/agentmux.sock` |
| data dir | `--data-dir` > `$AGENTMUX_DATA_DIR` > `~/.local/share/agentmux` |
| metadata db | `<data-dir>/db.sqlite` |
| event logs | `<data-dir>/sessions/<session-id>.jsonl` |
| config | `--config` > `$AGENTMUX_CONFIG` > `~/.config/agentmux/config.toml` |
| worktrees | `<repo>/.agentmux/worktrees/<name>` (branch `agentmux/<name>`) |
| shared blackboard | `<worktree>/.agentmux/{context.md, activity.md}` |

`agentmux-server` modes: `--serve` runs in the foreground; `--daemon` spawns a
detached `--serve` child that survives the terminal, then exits once the
socket accepts.

## Smoke test without real agents

The workspace ships `agentmux-mock-agent`, a deterministic ACP agent (echo
reply + a fake `src/lib.rs` edit; prompt tokens `crash` / `exit42` / `hang`
trigger the matching failure modes). To exercise the full stack with zero API
keys:

```bash
cargo build -p agentmux-mock-agent
```

then register it in `~/.config/agentmux/config.toml`:

```toml
[[agents]]
id = "mock"
name = "Mock Agent"
kind = "acp"
command = "/abs/path/to/agentmux/target/debug/agentmux-mock-agent"
```

(re)start the daemon, register any scratch `git init`'d repo via
`project/register`, open `agentmux-tui`, `/new` → your repo → new workspace →
**Mock Agent**, type "hello" → `Enter`. You should see the echo chunk, a
tool-call event, a `files touched` entry under `Tab`, and a new line in the
worktree's `.agentmux/activity.md`.

## Development

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace
```

Tests are self-contained: `cargo test` builds the mock-agent binary itself,
worktree tests create real repos (git required; repo-local identity is set),
and the pi-rpc tests drive `core/tests/fake_pi.py` (python3 required). CI runs
the same three commands — see `.github/workflows/ci.yml`. Real-agent smoke
runs are manual and never run in CI.

## Roadmap

- Desktop app (Tauri v2) on the same daemon API — Phase 2
- Agent auto-delegation / coordinator mode
- PTY passthrough for agents' own TUIs
- File-lock / conflict arbitration for shared worktrees
- Remote daemon (SSH)
- Headless adapters for `claude -p` / `codex exec`-style non-ACP CLIs
- Project/agent registration CLI (RPC-only today)
- Web frontend

## Acknowledgements

agentmux takes inspiration from [vibe-kanban](https://vibekanban.com)
(orchestration layer), [claude-squad](https://github.com/smtg-ai/claude-squad)
(multi-agent terminal UX), and [opencode](https://opencode.ai) (client–server
split). Agents speak the [Agent Client Protocol](https://agentclientprotocol.com)
via the official `agent-client-protocol` crate (Zed Industries).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in this project by you shall be dual-licensed as
above, without any additional terms or conditions.
