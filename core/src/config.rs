//! User configuration: an optional TOML file that customizes the agent table.
//!
//! The on-disk shape is deliberately flatter than [`AgentProfile`]: each
//! `[[agents]]` table carries `id`/`name`, a `kind` tag (`"acp"` by default
//! or `"pi-rpc"`), a `command` + `args`, and an `env` table. `available` is
//! not configurable — it is always `false` on load and only
//! [`AgentRegistry::probe`](crate::registry::AgentRegistry::probe) sets it.
//!
//! ```toml
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

use anyhow::{bail, Context};
use serde::Deserialize;

use crate::id::AgentId;
use crate::model::{AdapterKind, AgentProfile};
use crate::Result;

/// Parsed agentmux configuration.
///
/// [`Config::default`] is the built-in agent table — the same value
/// [`load`](Config::load) produces when the file does not exist.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Agent profiles: built-ins first, then user-defined extras in file
    /// order.
    pub agents: Vec<AgentProfile>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            agents: builtin_agents(),
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

/// On-disk shape of the config file. Only `[[agents]]` is read; every other
/// key is ignored so newer fields can appear without breaking old binaries.
#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default)]
    agents: Vec<RawAgent>,
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
