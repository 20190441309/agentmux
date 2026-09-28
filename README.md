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

- **Permission prompts are observe-only.** ACP `session/request_permission`
  is auto-denied by the daemon; the TUI shows a notice but cannot approve —
  run agents in a permission mode that doesn't require interactive approval.
- **No conflict arbitration in shared worktrees.** Shared workspaces suit
  sequential handoff (A scaffolds, B fills in) and partitioned parallel work;
  two agents editing the same file concurrently is a real git conflict.
- **Linux / macOS only** (Unix sockets), local machine only — no remote
  daemons.
- **No auto-delegation** — you choose which agent gets which prompt; a
  coordinator/router is on the backlog.
- Agents' own TUIs are not embedded (no PTY passthrough) — sessions are
  driven purely through the structured ACP event stream.
- Project/agent registration is JSON-RPC only for now — there is no
  `agentmux add <path>` CLI yet (see quickstart).

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
```

Inside the TUI: press `n` → pick the project → pick (or create) a workspace →
pick an agent. Then `i` to type a prompt and `Enter` to send it.

## Keybindings

| Mode | Key | Action |
|---|---|---|
| Normal | `q` | quit |
| Normal | `j` / `k`, `↓` / `↑` | move session selection |
| Normal | `i` / `a` | edit prompt |
| Normal | `n` | new-session wizard (project → workspace → agent) |
| Normal | `@` | relay: pick one of this session's events → target session |
| Normal | `Tab` | toggle the "files touched" panel (path list — no diff highlighting yet) |
| Normal | `x` | kill the selected session (recoverable via `r`) |
| Normal | `r` | resume a `done`/`error` session on a fresh adapter connection |
| Normal | `Ctrl-C` | cancel the selected session's in-flight turn |
| Editing | `Enter` | send prompt (staged `@` relays ride along) |
| Editing | `Esc` / `Ctrl-C` | back to Normal |
| Relay pick | `j` / `k` | move cursor within the stage |
| Relay pick | `Enter` | confirm stage (event → session) |
| Relay pick | `Esc` / `q` | abort |
| Wizard | `j` / `k` | move · `Enter` confirm · `Esc` step back · `q` abort |
| Permission notice | any key | dismiss (request was already auto-denied by the daemon) |

## Architecture

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
| `session/create` · `session/prompt` · `session/cancel` · `session/kill` · `session/list` · `session/resume` | session lifecycle |
| `session/subscribe` | stream normalized events (same stream to every subscriber) |
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
`project/register`, open `agentmux-tui`, `n` → your repo → new workspace →
**Mock Agent**, `i` → "hello" → `Enter`. You should see the echo chunk, a
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
