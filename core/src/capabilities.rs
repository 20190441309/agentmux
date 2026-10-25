//! Normalised per-session adapter capabilities.
//!
//! ACP's `initialize` response (and `session/new`'s modes/models) is parsed
//! from raw JSON so callers never depend on the ACP crate's types. Anything
//! missing or unrecognised counts as *unsupported* — never guessed.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How the session is connected, which bounds what can be structured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CapabilityProtocol {
    Acp { version: Option<u64> },
    PiRpc,
    NativeTerminal,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PromptCaps {
    pub image: bool,
    pub audio: bool,
    pub embedded_context: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct McpCaps {
    pub http: bool,
    pub sse: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamedChoice {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentCapabilities {
    pub agent_name: Option<String>,
    pub agent_version: Option<String>,
    pub protocol: CapabilityProtocol,
    pub load_session: bool,
    pub prompt: PromptCaps,
    pub mcp: McpCaps,
    pub session_modes: Vec<NamedChoice>,
    pub current_mode: Option<String>,
    pub session_models: Vec<NamedChoice>,
    pub current_model: Option<String>,
    pub auth_methods: Vec<String>,
    /// Unrecognised `agentCapabilities` keys, kept for debugging display.
    #[serde(default)]
    pub extras: Value,
    /// True when the last re-handshake failed and this is the previous
    /// snapshot, so UIs can show it as possibly outdated.
    #[serde(default)]
    pub stale: bool,
    pub captured_at: DateTime<Utc>,
}

impl AgentCapabilities {
    fn blank(protocol: CapabilityProtocol) -> Self {
        Self {
            agent_name: None,
            agent_version: None,
            protocol,
            load_session: false,
            prompt: PromptCaps::default(),
            mcp: McpCaps::default(),
            session_modes: Vec::new(),
            current_mode: None,
            session_models: Vec::new(),
            current_model: None,
            auth_methods: Vec::new(),
            extras: Value::Null,
            stale: false,
            captured_at: Utc::now(),
        }
    }

    /// Native terminals expose no structured capabilities; the agent's own
    /// TUI provides whatever it supports.
    pub fn native(agent_name: Option<String>) -> Self {
        Self {
            agent_name,
            ..Self::blank(CapabilityProtocol::NativeTerminal)
        }
    }

    /// Parse an ACP `initialize` response.
    pub fn from_acp_initialize(init: &Value) -> Self {
        let mut caps = Self::blank(CapabilityProtocol::Acp {
            version: init.get("protocolVersion").and_then(Value::as_u64),
        });
        caps.agent_name = str_at(init, "/agentInfo/name");
        caps.agent_version = str_at(init, "/agentInfo/version");
        let ac = init.get("agentCapabilities");
        let flag = |p: &str| {
            ac.and_then(|v| v.pointer(p))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        };
        caps.load_session = flag("/loadSession");
        caps.prompt = PromptCaps {
            image: flag("/promptCapabilities/image"),
            audio: flag("/promptCapabilities/audio"),
            embedded_context: flag("/promptCapabilities/embeddedContext"),
        };
        caps.mcp = McpCaps {
            http: flag("/mcpCapabilities/http"),
            sse: flag("/mcpCapabilities/sse"),
        };
        caps.auth_methods = init
            .get("authMethods")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|m| str_at(m, "/id")).collect())
            .unwrap_or_default();
        const KNOWN: [&str; 4] = [
            "loadSession",
            "promptCapabilities",
            "mcpCapabilities",
            "sessionCapabilities",
        ];
        if let Some(obj) = ac.and_then(Value::as_object) {
            let extra: serde_json::Map<_, _> = obj
                .iter()
                .filter(|(k, _)| !KNOWN.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if !extra.is_empty() {
                caps.extras = Value::Object(extra);
            }
        }
        caps
    }

    /// Fill modes/models from a `session/new` or `session/load` response.
    pub fn merge_session_info(&mut self, resp: &Value) {
        if let Some(modes) = resp.get("modes") {
            self.session_modes = choices(modes, "availableModes", "id");
            self.current_mode = str_at(modes, "/currentModeId");
        }
        if let Some(models) = resp.get("models") {
            self.session_models = choices(models, "availableModels", "modelId");
            self.current_model = str_at(models, "/currentModelId");
        }
    }

    /// Map Pi's `get_state` result. Pi can switch to a saved session file,
    /// so conversation restore is supported.
    pub fn from_pi_state(state: &Value) -> Self {
        let mut caps = Self::blank(CapabilityProtocol::PiRpc);
        caps.agent_name = Some("pi".into());
        caps.load_session = true;
        if let Some(id) = str_at(state, "/model/id") {
            caps.session_models = vec![NamedChoice {
                name: str_at(state, "/model/name").unwrap_or_else(|| id.clone()),
                id: id.clone(),
                description: str_at(state, "/model/provider"),
            }];
            caps.current_model = Some(id);
        }
        caps
    }

    /// Keep the previous snapshot, marked outdated, after a failed handshake.
    pub fn into_stale(mut self) -> Self {
        self.stale = true;
        self
    }
}

fn str_at(v: &Value, ptr: &str) -> Option<String> {
    v.pointer(ptr)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn choices(v: &Value, list_key: &str, id_key: &str) -> Vec<NamedChoice> {
    v.get(list_key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|m| {
                    let id = m.get(id_key).and_then(Value::as_str)?.to_owned();
                    Some(NamedChoice {
                        name: str_at(m, "/name").unwrap_or_else(|| id.clone()),
                        description: str_at(m, "/description"),
                        id,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture(name: &str) -> Value {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn full_acp_response_is_normalised() {
        let caps = AgentCapabilities::from_acp_initialize(&fixture("acp_initialize_full.json"));
        assert_eq!(caps.protocol, CapabilityProtocol::Acp { version: Some(1) });
        assert_eq!(caps.agent_name.as_deref(), Some("example-agent"));
        assert_eq!(caps.agent_version.as_deref(), Some("1.2.3"));
        assert!(caps.load_session);
        assert!(caps.prompt.image && caps.prompt.embedded_context && !caps.prompt.audio);
        assert!(caps.mcp.http && !caps.mcp.sse);
        assert_eq!(caps.auth_methods, vec!["oauth"]);
        assert_eq!(caps.extras, json!({"vendorThing": {"x": 1}}));
    }

    #[test]
    fn minimal_acp_response_defaults_to_unsupported() {
        let caps = AgentCapabilities::from_acp_initialize(&fixture("acp_initialize_minimal.json"));
        assert!(!caps.load_session);
        assert_eq!(caps.prompt, PromptCaps::default());
        assert_eq!(caps.mcp, McpCaps::default());
        assert!(caps.agent_name.is_none() && caps.auth_methods.is_empty());
        assert_eq!(caps.extras, Value::Null);
    }

    #[test]
    fn garbage_input_never_panics_and_grants_nothing() {
        for v in [
            json!(null),
            json!([]),
            json!("x"),
            json!({"agentCapabilities": 5}),
        ] {
            let caps = AgentCapabilities::from_acp_initialize(&v);
            assert!(!caps.load_session && !caps.prompt.image);
        }
    }

    #[test]
    fn session_response_fills_modes_and_models() {
        let mut caps = AgentCapabilities::from_acp_initialize(&json!({}));
        caps.merge_session_info(&fixture("acp_session_new.json"));
        assert_eq!(caps.current_mode.as_deref(), Some("code"));
        assert_eq!(caps.session_modes.len(), 2);
        assert_eq!(caps.session_modes[1].id, "plan");
        assert_eq!(caps.current_model.as_deref(), Some("m-large"));
        assert_eq!(caps.session_models[0].name, "Small");
    }

    #[test]
    fn pi_state_maps_model_and_allows_restore() {
        let caps = AgentCapabilities::from_pi_state(
            &json!({"sessionId":"s","model":{"provider":"fake","id":"small"}}),
        );
        assert_eq!(caps.protocol, CapabilityProtocol::PiRpc);
        assert!(caps.load_session);
        assert_eq!(caps.current_model.as_deref(), Some("small"));
    }

    #[test]
    fn native_has_no_structured_capabilities() {
        let caps = AgentCapabilities::native(Some("claude-code".into()));
        assert_eq!(caps.protocol, CapabilityProtocol::NativeTerminal);
        assert!(!caps.load_session && caps.session_models.is_empty());
    }

    #[test]
    fn roundtrips_through_json_and_stale_flag_sticks() {
        let caps = AgentCapabilities::from_acp_initialize(&fixture("acp_initialize_full.json"))
            .into_stale();
        let back: AgentCapabilities =
            serde_json::from_str(&serde_json::to_string(&caps).unwrap()).unwrap();
        assert_eq!(back, caps);
        assert!(back.stale);
    }
}
