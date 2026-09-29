//! Integration tests for [`Config`] TOML loading and [`AgentRegistry`]
//! availability probing.
//!
//! `Config::load` reads an optional TOML file whose `[[agents]]` tables merge
//! over the built-in agent table; `AgentRegistry::probe` then resolves each
//! adapter's `command` to fill `available`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use agentmux_core::{AdapterKind, AgentId, AgentProfile, AgentRegistry, Config, ConnTimeouts};
use tempfile::TempDir;

/// Write `contents` to `<dir>/config.toml` and return its path.
fn write_config(dir: &TempDir, contents: &str) -> PathBuf {
    let path = dir.path().join("config.toml");
    std::fs::write(&path, contents).unwrap();
    path
}

/// `registry.get(id)` unwrapped, taking a plain `&str` for ergonomics.
fn get<'a>(registry: &'a AgentRegistry, id: &str) -> &'a AgentProfile {
    registry
        .get(&AgentId::new(id))
        .unwrap_or_else(|| panic!("profile {id:?} should be registered"))
}

#[test]
fn missing_config_file_falls_back_to_builtin_agents() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::load(&dir.path().join("does-not-exist.toml"))
        .expect("a missing config file must yield the built-in defaults");

    assert_eq!(cfg, Config::default());

    let registry = AgentRegistry::from_config(&cfg);
    let ids: Vec<&str> = registry
        .profiles()
        .iter()
        .map(|p| p.id.0.as_str())
        .collect();
    assert_eq!(ids, ["claude-code", "codex", "opencode", "pi"]);
    assert!(
        registry.profiles().iter().all(|p| !p.available),
        "profiles must start unavailable until probe() runs"
    );

    // The built-in adapter table, verbatim.
    assert_eq!(
        get(&registry, "claude-code").adapter,
        AdapterKind::Acp {
            command: PathBuf::from("claude-code-acp"),
            args: vec![]
        }
    );
    assert_eq!(
        get(&registry, "codex").adapter,
        AdapterKind::Acp {
            command: PathBuf::from("codex-acp"),
            args: vec![]
        }
    );
    assert_eq!(
        get(&registry, "opencode").adapter,
        AdapterKind::Acp {
            command: PathBuf::from("opencode"),
            args: vec!["acp".into()]
        }
    );
    assert_eq!(
        get(&registry, "pi").adapter,
        AdapterKind::PiRpc {
            command: PathBuf::from("pi"),
            args: vec!["--mode".into(), "rpc".into()]
        }
    );

    assert!(registry.get(&AgentId::new("ghost")).is_none());
}

#[test]
fn config_file_adds_custom_agents_and_overrides_builtins_by_id() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(
        &dir,
        r#"
# Unrelated top-level keys are ignored for forward compatibility.
theme = "dark"

[[agents]]
name = "myagent"           # id omitted -> defaults to name
command = "/bin/true"

[[agents]]
id = "claude-code"         # collides with a built-in id -> overrides it
command = "/custom/claude"
args = ["--fast"]

[[agents]]
id = "pi-alt"
name = "Pi alt"
kind = "pi-rpc"
command = "/usr/local/bin/pi"
args = ["--mode", "rpc"]

[agents.env]
PI_DEBUG = "1"
"#,
    );

    let registry = AgentRegistry::from_config(&Config::load(&path).unwrap());

    // 4 built-ins + `myagent` + `pi-alt`; the `claude-code` entry was an
    // in-place override, not an addition.
    let ids: Vec<&str> = registry
        .profiles()
        .iter()
        .map(|p| p.id.0.as_str())
        .collect();
    assert_eq!(
        ids,
        [
            "claude-code",
            "codex",
            "opencode",
            "pi",
            "myagent",
            "pi-alt"
        ]
    );

    let my = get(&registry, "myagent");
    assert_eq!(
        my.name, "myagent",
        "name doubles as the id when id is omitted"
    );
    assert_eq!(
        my.adapter,
        AdapterKind::Acp {
            command: PathBuf::from("/bin/true"),
            args: vec![]
        },
        "kind defaults to acp"
    );
    assert!(my.env.is_empty());
    assert!(!my.available);

    // The override replaced the whole built-in entry at its position:
    // fields the user did not set fall back to per-entry defaults
    // (name = id), not to the built-in's values.
    let claude = get(&registry, "claude-code");
    assert_eq!(claude.name, "claude-code");
    assert_eq!(
        claude.adapter,
        AdapterKind::Acp {
            command: PathBuf::from("/custom/claude"),
            args: vec!["--fast".into()]
        }
    );

    let pi_alt = get(&registry, "pi-alt");
    assert_eq!(pi_alt.name, "Pi alt");
    assert_eq!(
        pi_alt.adapter,
        AdapterKind::PiRpc {
            command: PathBuf::from("/usr/local/bin/pi"),
            args: vec!["--mode".into(), "rpc".into()]
        }
    );
    assert_eq!(
        pi_alt.env,
        BTreeMap::from([("PI_DEBUG".to_string(), "1".to_string())])
    );
}

#[test]
fn empty_or_agentless_config_still_uses_builtin_defaults() {
    let dir = tempfile::tempdir().unwrap();
    // An empty file, a file with only unrelated keys, and an explicit empty
    // `agents` list all resolve to the same built-in table.
    for (i, contents) in ["", "theme = \"dark\"\n", "agents = []\n"]
        .into_iter()
        .enumerate()
    {
        let path = write_config(&dir, contents);
        let cfg = Config::load(&path).unwrap_or_else(|e| panic!("config {i} should load, got {e}"));
        assert_eq!(cfg, Config::default(), "config {i} must yield defaults");
    }
}

#[test]
fn malformed_config_and_incomplete_agents_error() {
    let dir = tempfile::tempdir().unwrap();

    // Not TOML at all.
    let path = write_config(&dir, "this is = [not valid toml");
    assert!(Config::load(&path).is_err(), "malformed TOML must error");

    // [[agents]] without `command` — a required field.
    let path = write_config(&dir, "[[agents]]\nname = \"no-cmd\"\n");
    assert!(Config::load(&path).is_err());

    // [[agents]] with neither `id` nor `name`.
    let path = write_config(&dir, "[[agents]]\ncommand = \"/bin/true\"\n");
    assert!(Config::load(&path).is_err());

    // Unknown kind tag.
    let path = write_config(
        &dir,
        "[[agents]]\nid = \"x\"\nkind = \"telepathy\"\ncommand = \"/bin/true\"\n",
    );
    assert!(Config::load(&path).is_err());

    // A directory is a read error, not a missing file.
    assert!(Config::load(dir.path()).is_err());
}

#[test]
fn available_flag_in_file_is_ignored_until_probe() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(
        &dir,
        "[[agents]]\nid = \"pretend\"\ncommand = \"/bin/true\"\navailable = true\n",
    );
    let registry = AgentRegistry::from_config(&Config::load(&path).unwrap());
    assert!(
        !get(&registry, "pretend").available,
        "available is probe-managed; the file cannot pre-set it"
    );
}

#[test]
fn probe_marks_availability_per_command() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(
        &dir,
        r#"
[[agents]]
id = "ok-agent"
command = "/bin/true"          # absolute path, executable

[[agents]]
id = "path-agent"
command = "sh"                 # bare name resolved via PATH search

[[agents]]
id = "missing-agent"
command = "/definitely/not/exist"

[[agents]]
id = "missing-relative"
command = "./no/such/file"     # has a directory part -> checked directly
"#,
    );

    let registry = AgentRegistry::from_config(&Config::load(&path).unwrap());
    let probed = registry.probe();

    // probe() returns a full copy; the registry itself is untouched.
    assert_eq!(probed.len(), registry.profiles().len());
    assert!(!get(&registry, "ok-agent").available);

    let available_of = |id: &str| {
        probed
            .iter()
            .find(|p| p.id.0 == id)
            .unwrap_or_else(|| panic!("{id} should be probed"))
            .available
    };

    assert!(available_of("ok-agent"), "/bin/true must probe available");
    assert!(available_of("path-agent"), "sh must resolve on PATH");
    assert!(!available_of("missing-agent"));
    assert!(!available_of("missing-relative"));

    // Probing does not touch other fields.
    let probed_ok = probed.iter().find(|p| p.id.0 == "ok-agent").unwrap();
    assert_eq!(probed_ok.id, get(&registry, "ok-agent").id);
    assert_eq!(probed_ok.adapter, get(&registry, "ok-agent").adapter);
}

#[test]
fn from_config_collapses_duplicate_ids_last_wins() {
    // Hand-constructed Config (not via Config::load): the same override rule
    // applies so `get` stays unambiguous.
    let mk = |id: &str, command: &str| AgentProfile {
        id: AgentId::new(id),
        name: id.into(),
        adapter: AdapterKind::Acp {
            command: PathBuf::from(command),
            args: vec![],
        },
        env: BTreeMap::new(),
        available: false,
    };
    let cfg = Config {
        agents: vec![
            mk("dup", "/first"),
            mk("solo", "/solo"),
            mk("dup", "/second"),
        ],
        ..Config::default()
    };

    let registry = AgentRegistry::from_config(&cfg);
    let ids: Vec<&str> = registry
        .profiles()
        .iter()
        .map(|p| p.id.0.as_str())
        .collect();
    assert_eq!(ids, ["dup", "solo"], "later duplicate replaces in place");
    assert_eq!(
        get(&registry, "dup").adapter,
        AdapterKind::Acp {
            command: PathBuf::from("/second"),
            args: vec![]
        }
    );
}

/// The `*_timeout_secs` top-level keys feed `Config::timeouts`: unset
/// keys fall back to the built-in defaults (the point is "not forever"),
/// set keys parse as whole seconds, and `prompt_timeout_secs = 0` is the
/// documented "wait forever" escape hatch.
#[test]
fn timeout_keys_parse_and_default() {
    let dir = tempfile::tempdir().unwrap();

    // Defaults: missing file and a file without the keys agree.
    let defaults = Config::load(&dir.path().join("missing.toml")).unwrap();
    assert_eq!(defaults.timeouts, ConnTimeouts::default());
    let path = write_config(&dir, "theme = \"dark\"\n");
    assert_eq!(
        Config::load(&path).unwrap().timeouts,
        ConnTimeouts::default()
    );
    assert_eq!(defaults.timeouts.init, Duration::from_secs(10));
    assert_eq!(defaults.timeouts.prompt, Duration::from_secs(600));

    // Both keys parse and survive the registry handoff.
    let path = write_config(&dir, "init_timeout_secs = 3\nprompt_timeout_secs = 42\n");
    let cfg = Config::load(&path).unwrap();
    assert_eq!(cfg.timeouts.init, Duration::from_secs(3));
    assert_eq!(cfg.timeouts.prompt, Duration::from_secs(42));
    let registry = AgentRegistry::from_config(&cfg);
    assert_eq!(registry.timeouts(), cfg.timeouts);

    // `0` disables the prompt timeout (documented escape hatch).
    let path = write_config(&dir, "prompt_timeout_secs = 0\n");
    let cfg = Config::load(&path).unwrap();
    assert_eq!(cfg.timeouts.prompt, Duration::ZERO);
    assert_eq!(
        cfg.timeouts.init,
        ConnTimeouts::default().init,
        "an unset key keeps its default"
    );

    // A registry built from a hand-constructed Config still defaults.
    let hand = Config {
        agents: vec![],
        ..Config::default()
    };
    assert_eq!(hand.timeouts, ConnTimeouts::default());
}
