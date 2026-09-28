//! The agent registry: the configured [`AgentProfile`]s plus availability
//! probing.
//!
//! [`AgentRegistry`] is built from a loaded [`Config`]; [`probe`](AgentRegistry::probe)
//! resolves each adapter's `command` (a `which`-style `PATH` search, or a
//! direct executable check when the command contains a directory component)
//! and returns copies with `available` filled in — the registry itself is
//! never mutated, so callers decide whether to persist or diff the result.

use std::env;
use std::path::Path;

use crate::config::Config;
use crate::id::AgentId;
use crate::model::{AdapterKind, AgentProfile};

/// The agents the orchestrator knows about, in config order.
pub struct AgentRegistry {
    profiles: Vec<AgentProfile>,
}

impl AgentRegistry {
    /// Build a registry from loaded config.
    ///
    /// Duplicate ids collapse last-wins at their first-seen position — the
    /// same rule [`Config::load`] applies between `[[agents]]` entries and
    /// built-ins — so [`get`](Self::get) stays unambiguous even for
    /// hand-constructed `Config`s that bypassed `load`.
    pub fn from_config(cfg: &Config) -> AgentRegistry {
        let mut profiles: Vec<AgentProfile> = Vec::with_capacity(cfg.agents.len());
        for agent in &cfg.agents {
            match profiles.iter_mut().find(|p| p.id == agent.id) {
                Some(slot) => *slot = agent.clone(),
                None => profiles.push(agent.clone()),
            }
        }
        AgentRegistry { profiles }
    }

    /// Build a registry and probe every profile's adapter `command` —
    /// `available` reflects the filesystem at construction time.
    ///
    /// Config-loaded profiles always carry `available: false`; a daemon
    /// that creates sessions must probe (here, or lazily via
    /// `list_agents`/`register_agent`) or every spawn is refused.
    pub fn probed(cfg: &Config) -> AgentRegistry {
        let registry = Self::from_config(cfg);
        AgentRegistry {
            profiles: registry.probe(),
        }
    }

    /// All profiles, in config order.
    pub fn profiles(&self) -> &[AgentProfile] {
        &self.profiles
    }

    /// Look up a profile by id.
    pub fn get(&self, id: &AgentId) -> Option<&AgentProfile> {
        self.profiles.iter().find(|p| &p.id == id)
    }

    /// Insert or update a profile at runtime (`agent/register`).
    ///
    /// Duplicate ids collapse last-wins at their first-seen position — the
    /// same rule [`from_config`](Self::from_config) applies.
    pub fn register(&mut self, profile: AgentProfile) {
        match self.profiles.iter_mut().find(|p| p.id == profile.id) {
            Some(slot) => *slot = profile,
            None => self.profiles.push(profile),
        }
    }

    /// A copy of `profile` with `available` set by probing its adapter's
    /// `command` — the single-profile version of [`probe`](Self::probe).
    pub fn probe_profile(profile: &AgentProfile) -> AgentProfile {
        AgentProfile {
            available: command_exists(adapter_command(&profile.adapter)),
            ..profile.clone()
        }
    }

    /// A copy of the registry's profiles with `available` set by probing each
    /// adapter's `command`.
    ///
    /// Probing is a pure filesystem lookup — no agent process is spawned, so
    /// it cannot hang or produce side effects.
    pub fn probe(&self) -> Vec<AgentProfile> {
        self.profiles.iter().map(Self::probe_profile).collect()
    }
}

/// The executable an adapter would spawn.
fn adapter_command(adapter: &AdapterKind) -> &Path {
    match adapter {
        AdapterKind::Acp { command, .. } | AdapterKind::PiRpc { command, .. } => command,
    }
}

/// Whether `cmd` resolves to an executable file.
///
/// A `cmd` with a directory component (`/abs/path` or `dir/file`) is checked
/// directly; a bare file name is searched along `PATH`, `which`-style.
fn command_exists(cmd: &Path) -> bool {
    // `Path::new("sh").parent()` is `Some("")` — a bare name has an empty
    // parent, anything else counts as having a directory component.
    let has_dir = cmd.parent().is_some_and(|p| !p.as_os_str().is_empty());
    if cmd.is_absolute() || has_dir {
        return is_executable(cmd);
    }
    env::var_os("PATH")
        .is_some_and(|path| env::split_paths(&path).any(|dir| is_executable(&dir.join(cmd))))
}

/// Whether `path` is a regular file with at least one executable bit.
///
/// On non-unix platforms executability is extension-based, so this falls
/// back to `is_file`.
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.is_file()
            && path
                .metadata()
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_executable_checks_the_exec_bit() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("agent-bin");
        std::fs::write(&file, b"#!/bin/sh\nexit 0\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Fresh files are 0644 — not executable.
            assert!(!is_executable(&file));
            let mut perms = file.metadata().unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&file, perms).unwrap();
            assert!(is_executable(&file));
        }
        #[cfg(not(unix))]
        assert!(is_executable(&file));

        // A missing file and a directory are not commands.
        assert!(!is_executable(&dir.path().join("missing")));
        assert!(!is_executable(dir.path()));
    }

    #[test]
    fn command_exists_resolves_absolute_and_rejects_missing() {
        assert!(command_exists(Path::new("/bin/true")));
        assert!(!command_exists(Path::new("/definitely/not/exist")));
        // Bare names go through PATH; `sh` is a POSIX guarantee.
        assert!(command_exists(Path::new("sh")));
        assert!(!command_exists(Path::new(
            "definitely-not-an-agentmux-agent"
        )));
    }
}
