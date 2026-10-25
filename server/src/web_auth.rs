//! Device pairing and tokens for the web gateway.
//!
//! The gateway may sit on the public internet, so the URL is treated as
//! public and every protection lives here:
//!
//! - A **pairing code** is minted only over the local unix socket
//!   (`web/pair_create`), is single-use and expires after [`CODE_TTL`].
//! - Redeeming a code yields a **device token** (256 random bits). Only its
//!   SHA-256 is stored (`<data_dir>/web_devices.json`, mode 0600).
//! - Failed redemptions are throttled per client address and globally; too
//!   many global failures close the pairing door for a while and burn all
//!   outstanding codes.
//! - Devices can be listed and revoked; revocation takes effect for new
//!   connections immediately and for open ones via [`WebAuth::is_active`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use rand::{distributions::Alphanumeric, Rng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const CODE_TTL: Duration = Duration::from_secs(300);
const CODE_LEN: usize = 10;
const MAX_LIVE_CODES: usize = 5;
const IP_FAIL_LIMIT: usize = 5;
const GLOBAL_FAIL_LIMIT: usize = 20;
const FAIL_WINDOW: Duration = Duration::from_secs(300);
const LOCKOUT: Duration = Duration::from_secs(120);
/// Tokens unused for this long stop working (sliding expiry).
const TOKEN_IDLE_TTL_DAYS: i64 = 30;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    token_hash: String,
    pub created_at: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PairError {
    /// Wrong, expired or already-used code (deliberately indistinguishable).
    Invalid,
    /// Too many failures; retry later.
    Throttled,
}

#[derive(Default)]
struct State {
    codes: HashMap<String, Instant>,
    devices: Vec<Device>,
    fails: Vec<(String, Instant)>,
    locked_until: Option<Instant>,
}

pub struct WebAuth {
    file: Option<PathBuf>,
    state: Mutex<State>,
}

fn hash(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl WebAuth {
    /// Non-persistent instance (tests, daemons built without a data dir).
    pub fn in_memory() -> WebAuth {
        WebAuth {
            file: None,
            state: Mutex::new(State::default()),
        }
    }

    /// Load (or start) the device list stored in `data_dir`.
    pub fn open(data_dir: &std::path::Path) -> WebAuth {
        let file = data_dir.join("web_devices.json");
        let devices = std::fs::read_to_string(&file)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        WebAuth {
            file: Some(file),
            state: Mutex::new(State {
                devices,
                ..State::default()
            }),
        }
    }

    fn save(&self, st: &State) {
        let Some(file) = &self.file else { return };
        let Ok(text) = serde_json::to_string_pretty(&st.devices) else {
            return;
        };
        let _ = write_private(file, &text);
    }

    /// Mint a single-use pairing code (local callers only).
    pub fn create_code(&self) -> String {
        let mut st = self.state.lock().unwrap();
        let now = Instant::now();
        st.codes.retain(|_, exp| *exp > now);
        if st.codes.len() >= MAX_LIVE_CODES {
            // Drop the oldest so a flood of requests cannot widen the guess space.
            if let Some(oldest) = st
                .codes
                .iter()
                .min_by_key(|(_, e)| **e)
                .map(|(c, _)| c.clone())
            {
                st.codes.remove(&oldest);
            }
        }
        let code: String = rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .filter(|c| !matches!(*c, b'0' | b'O' | b'o' | b'1' | b'l' | b'I'))
            .take(CODE_LEN)
            .map(char::from)
            .collect::<String>()
            .to_uppercase();
        st.codes.insert(code.clone(), now + CODE_TTL);
        code
    }

    /// Exchange a pairing code for a device token.
    pub fn redeem(
        &self,
        code: &str,
        name: &str,
        client: &str,
    ) -> Result<(Device, String), PairError> {
        let mut st = self.state.lock().unwrap();
        let now = Instant::now();
        if st.locked_until.is_some_and(|t| t > now) {
            return Err(PairError::Throttled);
        }
        st.fails
            .retain(|(_, t)| now.duration_since(*t) < FAIL_WINDOW);
        if st.fails.iter().filter(|(c, _)| c == client).count() >= IP_FAIL_LIMIT {
            return Err(PairError::Throttled);
        }
        let code = code.trim().to_uppercase();
        let valid = st.codes.remove(&code).is_some_and(|exp| exp > now);
        if !valid {
            st.fails.push((client.to_string(), now));
            if st.fails.len() >= GLOBAL_FAIL_LIMIT {
                st.codes.clear();
                st.fails.clear();
                st.locked_until = Some(now + LOCKOUT);
            }
            return Err(PairError::Invalid);
        }
        let token = random_token();
        let at = Utc::now();
        let name: String = name.chars().take(60).collect();
        let device = Device {
            id: uuid::Uuid::new_v4().to_string(),
            name: if name.trim().is_empty() {
                "device".into()
            } else {
                name
            },
            token_hash: hash(&token),
            created_at: at,
            last_seen: at,
        };
        st.devices.push(device.clone());
        self.save(&st);
        Ok((device, token))
    }

    /// Verify a bearer token; returns the device id and refreshes `last_seen`.
    pub fn verify(&self, token: &str) -> Option<Device> {
        let h = hash(token);
        let mut st = self.state.lock().unwrap();
        let now = Utc::now();
        let dev = st.devices.iter_mut().find(|d| d.token_hash == h)?;
        if (now - dev.last_seen).num_days() >= TOKEN_IDLE_TTL_DAYS {
            return None;
        }
        // Persist at most hourly; verification runs on every connection.
        let stale = (now - dev.last_seen).num_minutes() >= 60;
        dev.last_seen = now;
        let out = dev.clone();
        if stale {
            self.save(&st);
        }
        Some(out)
    }

    pub fn is_active(&self, device_id: &str) -> bool {
        self.state
            .lock()
            .unwrap()
            .devices
            .iter()
            .any(|d| d.id == device_id)
    }

    pub fn list(&self) -> Vec<Device> {
        self.state.lock().unwrap().devices.clone()
    }

    pub fn revoke(&self, device_id: &str) -> bool {
        let mut st = self.state.lock().unwrap();
        let before = st.devices.len();
        st.devices.retain(|d| d.id != device_id);
        let removed = st.devices.len() != before;
        if removed {
            self.save(&st);
        }
        removed
    }
}

fn write_private(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("json.tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_is_single_use_and_yields_a_working_token() {
        let a = WebAuth::in_memory();
        let code = a.create_code();
        let (dev, token) = a.redeem(&code.to_lowercase(), "phone", "1.1.1.1").unwrap();
        assert_eq!(a.verify(&token).unwrap().id, dev.id);
        assert_eq!(
            a.redeem(&code, "x", "1.1.1.1").unwrap_err(),
            PairError::Invalid
        );
        assert!(a.verify("nope").is_none());
    }

    #[test]
    fn revoked_devices_stop_working() {
        let a = WebAuth::in_memory();
        let (dev, token) = a.redeem(&a.create_code(), "p", "ip").unwrap();
        assert!(a.revoke(&dev.id));
        assert!(a.verify(&token).is_none());
        assert!(!a.is_active(&dev.id));
    }

    #[test]
    fn per_ip_failures_throttle_even_a_correct_code() {
        let a = WebAuth::in_memory();
        for _ in 0..IP_FAIL_LIMIT {
            assert_eq!(
                a.redeem("WRONG", "p", "9.9.9.9").unwrap_err(),
                PairError::Invalid
            );
        }
        let code = a.create_code();
        assert_eq!(
            a.redeem(&code, "p", "9.9.9.9").unwrap_err(),
            PairError::Throttled
        );
        assert!(a.redeem(&code, "p", "8.8.8.8").is_ok());
    }

    #[test]
    fn global_failures_lock_pairing_and_burn_codes() {
        let a = WebAuth::in_memory();
        let code = a.create_code();
        for i in 0..GLOBAL_FAIL_LIMIT {
            let _ = a.redeem("WRONG", "p", &format!("ip{i}"));
        }
        assert_eq!(
            a.redeem(&code, "p", "fresh").unwrap_err(),
            PairError::Throttled
        );
    }

    #[test]
    fn devices_persist_as_hashes_only() {
        let dir = tempfile::tempdir().unwrap();
        let a = WebAuth::open(dir.path());
        let (_, token) = a.redeem(&a.create_code(), "laptop", "ip").unwrap();
        let text = std::fs::read_to_string(dir.path().join("web_devices.json")).unwrap();
        assert!(!text.contains(&token));
        let b = WebAuth::open(dir.path());
        assert!(b.verify(&token).is_some());
    }
}
