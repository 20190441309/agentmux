//! User configuration: an optional TOML file that customizes the agent table.
//!
//! The on-disk shape is deliberately flatter than [`AgentProfile`]: each
//! `[[agents]]` table carries `id`/`name`, a `kind` tag (`"acp"` by default
//! or `"pi-rpc"`), a `command` + `args`, and an `env` table. `available` is
//! not configurable — it is always `false` on load and only
//! [`AgentRegistry::probe`](crate::registry::AgentRegistry::probe) sets it.
//!
//! ```toml
//! init_timeout_secs = 10      # optional; bound on the agent handshake
//! prompt_timeout_secs = 600   # optional; bound on one prompt turn (0 = forever)
//!
//! [[agents]]
//! id = "claude-code"          # colliding with a built-in id overrides it
//! command = "/custom/claude"
//! args = ["--fast"]
//!
//! [[agents]]
//! name = "myagent"            # id omitted -> defaults to name (and vice versa)
//! kind = "pi-rpc"
//! command = "/usr/local/bin/pi"
//! args = ["--mode", "rpc"]
//!
//! [agents.env]
//! PI_DEBUG = "1"
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context};
use serde::Deserialize;

use crate::id::AgentId;
use crate::model::{AdapterKind, AgentProfile};
use crate::Result;

/// Default [`ConnTimeouts::init`] — bound on the agent `initialize`
/// handshake (`get_state` for pi). Ten seconds is plenty for a healthy
/// agent to answer its first RPC; a wedged spawn shouldn't stall session
/// setup longer than that.
pub const DEFAULT_INIT_TIMEOUT_SECS: u64 = 10;

/// Default [`ConnTimeouts::prompt`] — bound on one prompt turn, measured
/// from send to the agent's end-of-turn. Ten minutes is deliberately
/// generous — real turns legitimately run builds and test suites — but
/// *bounded*: before this existed a wedged agent turn hung the caller
/// forever and only `cancel`/`kill` could escape it (and pi's `abort`
/// itself can block). `0` disables the bound.
pub const DEFAULT_PROMPT_TIMEOUT_SECS: u64 = 600;

/// Timeouts the daemon applies to spawned-agent connections, resolved
/// from the optional top-level `*_timeout_secs` config keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnTimeouts {
    /// Bound on the `initialize` handshake (pi: `get_state`).
    pub init: Duration,
    /// Bound on one `prompt` turn; [`Duration::ZERO`] waits forever.
    pub prompt: Duration,
}

impl Default for ConnTimeouts {
    fn default() -> Self {
        ConnTimeouts {
            init: Duration::from_secs(DEFAULT_INIT_TIMEOUT_SECS),
            prompt: Duration::from_secs(DEFAULT_PROMPT_TIMEOUT_SECS),
        }
    }
}

/// Per-connection spawn options handed to `AcpConn::spawn` /
/// `PiConn::spawn`: the config-derived [`ConnTimeouts`] plus where the
/// agent's stderr should be captured.
#[derive(Debug, Clone, Default)]
pub struct SpawnOptions {
    /// Connection timeouts (see [`ConnTimeouts`]).
    pub timeouts: ConnTimeouts,
    /// File the child's stderr is mirrored to —
    /// `<data_dir>/sessions/<id>.stderr.log` in the orchestrator. `None`
    /// keeps capture in-memory only (the tail still surfaces on errors).
    pub stderr_log: Option<PathBuf>,
}

/// Parsed agentmux configuration.
///
/// [`Config::default`] is the built-in agent table — the same value
/// [`load`](Config::load) produces when the file does not exist.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Agent profiles: built-ins first, then user-defined extras in file
    /// order.
    pub agents: Vec<AgentProfile>,
    /// Connection timeouts from the top-level `init_timeout_secs` /
    /// `prompt_timeout_secs` keys (defaults apply when unset).
    pub timeouts: ConnTimeouts,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            agents: builtin_agents(),
            timeouts: ConnTimeouts::default(),
        }
    }
}

impl Config {
    /// Load the TOML config at `path`.
    ///
    /// * A missing file yields the built-in defaults; any other read error
    ///   (e.g. `path` is a directory) is an error.
    /// * Present-but-malformed TOML, or a `[[agents]]` entry missing required
    ///   fields (`command`, plus at least one of `id`/`name`), is an error.
    /// * `[[agents]]` entries merge *over* the built-ins: an entry whose `id`
    ///   matches a built-in replaces that built-in **in place** (fields it
    ///   leaves unset fall back to per-entry defaults like `name = id`, not
    ///   to the built-in's values); all other entries are appended in file
    ///   order. So `agents = []` still yields the built-ins — there is no
    ///   "disable all" syntax.
    pub fn load(path: &Path) -> Result<Config> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Config::default());
            }
            Err(e) => {
                return Err(e).with_context(|| format!("failed to read {}", path.display()));
            }
        };
        let raw: RawConfig =
            toml::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))?;

        let mut config = Config::default();
        if let Some(secs) = raw.init_timeout_secs {
            config.timeouts.init = Duration::from_secs(secs);
        }
        if let Some(secs) = raw.prompt_timeout_secs {
            config.timeouts.prompt = Duration::from_secs(secs);
        }
        for raw_agent in raw.agents {
            let profile = raw_agent.into_profile()?;
            match config.agents.iter_mut().find(|p| p.id == profile.id) {
                // A user entry with a built-in (or earlier user) id overrides it.
                Some(slot) => *slot = profile,
                None => config.agents.push(profile),
            }
        }
        Ok(config)
    }
}

/// The built-in agent table, in registry order. All `available: false` —
/// probing happens later, in the registry.
fn builtin_agents() -> Vec<AgentProfile> {
    let acp = |id: &str, name: &str, command: &str, args: &[&str]| AgentProfile {
        id: AgentId::new(id),
        name: name.into(),
        adapter: AdapterKind::Acp {
            command: PathBuf::from(command),
            args: args.iter().map(|s| s.to_string()).collect(),
        },
        env: BTreeMap::new(),
        available: false,
    };
    vec![
        acp("claude-code", "Claude Code", "claude-code-acp", &[]),
        acp("codex", "Codex", "codex-acp", &[]),
        acp("opencode", "OpenCode", "opencode", &["acp"]),
        AgentProfile {
            id: AgentId::new("pi"),
            name: "Pi".into(),
            adapter: AdapterKind::PiRpc {
                command: PathBuf::from("pi"),
                args: vec!["--mode".into(), "rpc".into()],
            },
            env: BTreeMap::new(),
            available: false,
        },
    ]
}

/// On-disk shape of the config file. `[[agents]]` and the top-level
/// `*_timeout_secs` keys are read; every other key is ignored so newer
/// fields can appear without breaking old binaries.
#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default)]
    agents: Vec<RawAgent>,
    /// Optional bound on the agent `initialize` handshake, in seconds
    /// (default [`DEFAULT_INIT_TIMEOUT_SECS`]).
    init_timeout_secs: Option<u64>,
    /// Optional bound on one `prompt` turn, in seconds (default
    /// [`DEFAULT_PROMPT_TIMEOUT_SECS`]; `0` disables the bound).
    prompt_timeout_secs: Option<u64>,
}

/// One `[[agents]]` table entry — a flattened, friendlier projection of
/// [`AgentProfile`] where `kind` picks the [`AdapterKind`] variant and
/// `command`/`args`/`env` sit inline.
#[derive(Debug, Deserialize)]
struct RawAgent {
    /// Stable identifier; defaults to `name` when omitted.
    id: Option<String>,
    /// Display name; defaults to `id` when omitted.
    name: Option<String>,
    /// `"acp"` (default) or `"pi-rpc"` — separators/case are ignored, so
    /// `"pi_rpc"` and `"PiRpc"` work too.
    kind: Option<String>,
    /// Binary to spawn: an absolute or `dir/file` path checked directly, or
    /// a bare name resolved on `PATH` at probe time.
    command: PathBuf,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

impl RawAgent {
    /// Validate a parsed `[[agents]]` entry into a profile.
    fn into_profile(self) -> Result<AgentProfile> {
        // `id` and `name` default to each other — at least one must be given.
        let (id, name) = match (self.id, self.name) {
            (Some(id), Some(name)) => (id, name),
            (Some(id), None) => (id.clone(), id),
            (None, Some(name)) => (name.clone(), name),
            (None, None) => bail!("[[agents]] entry needs at least one of `id` / `name`"),
        };

        let kind_tag = self.kind.unwrap_or_else(|| "acp".into());
        // Normalized so "pi-rpc", "pi_rpc" and "PiRpc" all mean PiRpc.
        let normalized: String = kind_tag
            .chars()
            .filter(|c| *c != '-' && *c != '_')
            .flat_map(char::to_lowercase)
            .collect();
        let command = self.command;
        let adapter = match normalized.as_str() {
            "acp" => AdapterKind::Acp {
                command,
                args: self.args,
            },
            "pirpc" => AdapterKind::PiRpc {
                command,
                args: self.args,
            },
            _ => bail!("unknown [[agents]] kind {kind_tag:?} (expected \"acp\" or \"pi-rpc\")"),
        };

        Ok(AgentProfile {
            id: AgentId::new(id),
            name,
            adapter,
            env: self.env,
            // Probe-managed: a file cannot pre-set availability.
            available: false,
        })
    }
}
