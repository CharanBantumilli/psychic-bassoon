//! Feature-flag abstraction backed by Flagsmith.
//!
//! The fleet uses flags for two things:
//!   1. Product-level rollout (e.g. `fleet.max_parallel`, `vibe_report.enabled`).
//!   2. Per-agent behavior tuning (e.g. `agent.mock.mode`, `agent.risk_threshold`).
//!
//! By default we ship an offline evaluator so the app runs with no network.
//! Enable the `live-flagsmith` feature and provide an environment key to talk
//! to a real Flagsmith instance.

use std::collections::HashMap;
use parking_lot::RwLock;

/// A resolved flag value.
#[derive(Debug, Clone, PartialEq)]
pub enum FlagValue {
    Bool(bool),
    Int(i64),
    Str(String),
}

impl FlagValue {
    pub fn as_bool(&self, default: bool) -> bool {
        match self {
            FlagValue::Bool(b) => *b,
            _ => default,
        }
    }
    pub fn as_i64(&self, default: i64) -> i64 {
        match self {
            FlagValue::Int(i) => *i,
            _ => default,
        }
    }
    pub fn as_str(&self, default: &str) -> String {
        match self {
            FlagValue::Str(s) => s.clone(),
            _ => default.to_string(),
        }
    }
}

/// Target a flag at a specific agent/user identity (Flagsmith traits).
#[derive(Debug, Clone, Default)]
pub struct Identity {
    pub key: String,
    pub traits: HashMap<String, FlagValue>,
}

/// Abstraction over a flag source. `get` never panics; on any failure it
/// returns the supplied default so the fleet keeps running.
pub trait FeatureFlags: Send + Sync {
    fn get(&self, key: &str, identity: &Identity) -> Option<FlagValue>;

    fn get_bool(&self, key: &str, identity: &Identity, default: bool) -> bool {
        self.get(key, identity).map(|v| v.as_bool(default)).unwrap_or(default)
    }
    fn get_i64(&self, key: &str, identity: &Identity, default: i64) -> i64 {
        self.get(key, identity).map(|v| v.as_i64(default)).unwrap_or(default)
    }
    fn get_str(&self, key: &str, identity: &Identity, default: &str) -> String {
        self.get(key, identity)
            .map(|v| v.as_str(default))
            .unwrap_or_else(|| default.to_string())
    }
}

// ---- Well-known flag keys -------------------------------------------------

pub mod keys {
    /// Cap on concurrently running agents.
    pub const MAX_PARALLEL: &str = "fleet.max_parallel";
    /// Whether to emit a vibe report after each task.
    pub const VIBE_REPORT_ENABLED: &str = "vibe_report.enabled";
    /// Mock agent behavior mode: "deterministic" | "chaotic".
    pub const AGENT_MOCK_MODE: &str = "agent.mock.mode";
    /// Per-agent risk escalation threshold (0-100).
    pub const AGENT_RISK_THRESHOLD: &str = "agent.risk_threshold";
    /// Auto-merge tasks whose risk is below this level.
    pub const AUTO_MERGE_BELOW: &str = "fleet.auto_merge_below";
}

// ---- Offline evaluator ----------------------------------------------------

/// A local, in-memory flag source. Seed it from config or remote defaults.
/// Supports simple percentage rollouts keyed on identity for realism.
#[derive(Debug, Default)]
pub struct LocalFlags {
    inner: RwLock<LocalFlagsInner>,
}

#[derive(Debug, Default)]
struct LocalFlagsInner {
    flags: HashMap<String, FlagValue>,
    /// key -> rollout percentage (0-100). When set, bool flags roll out by
    /// a stable hash of the identity key.
    rollouts: HashMap<String, u8>,
}

impl LocalFlags {
    pub fn new() -> Self {
        let f = Self::default();
        // Sensible defaults so the app is useful out of the box.
        f.set(keys::MAX_PARALLEL, FlagValue::Int(3));
        f.set(keys::VIBE_REPORT_ENABLED, FlagValue::Bool(true));
        f.set(keys::AGENT_MOCK_MODE, FlagValue::Str("deterministic".to_string()));
        f.set(keys::AGENT_RISK_THRESHOLD, FlagValue::Int(60));
        f.set(keys::AUTO_MERGE_BELOW, FlagValue::Int(30));
        f
    }

    pub fn set(&self, key: &str, value: FlagValue) {
        self.inner.write().flags.insert(key.to_string(), value);
    }

    pub fn set_rollout(&self, key: &str, pct: u8) {
        self.inner.write().rollouts.insert(key.to_string(), pct.min(100));
    }
}

fn stable_bucket(key: &str) -> u8 {
    // FNV-1a hash -> 0..99
    let mut h: u64 = 0xcbf29ce484222325;
    for b in key.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    (h % 100) as u8
}

impl FeatureFlags for LocalFlags {
    fn get(&self, key: &str, identity: &Identity) -> Option<FlagValue> {
        let inner = self.inner.read();
        if let Some(pct) = inner.rollouts.get(key) {
            // Rollout only gates boolean flags.
            if let Some(FlagValue::Bool(b)) = inner.flags.get(key) {
                return Some(FlagValue::Bool(*b && stable_bucket(&identity.key) < *pct));
            }
        }
        inner.flags.get(key).cloned()
    }
}

// ---- Live Flagsmith backend (feature-gated) -------------------------------

#[cfg(feature = "live-flagsmith")]
pub struct FlagsmithClient {
    client: flagsmith::Flagsmith,
}

#[cfg(feature = "live-flagsmith")]
impl FlagsmithClient {
    /// Build from a Flagsmith environment key. Network is used lazily.
    pub fn new(env_key: &str, base_url: &str) -> Self {
        let client = flagsmith::Flagsmith::new(
            base_url,
            env_key,
            Default::default(),
            None,
        );
        Self { client }
    }
}

#[cfg(feature = "live-flagsmith")]
impl FeatureFlags for FlagsmithClient {
    fn get(&self, key: &str, identity: &Identity) -> Option<FlagValue> {
        let user = flagsmith::Flags::new(identity.key.clone(), Default::default());
        let flags = self.client.get_flag_variants(&user).ok()?;
        let fv = flags.iter().find(|f| f.key == key)?;
        match fv.value.clone() {
            Some(s) => {
                if let Ok(b) = s.parse::<bool>() {
                    Some(FlagValue::Bool(b))
                } else if let Ok(i) = s.parse::<i64>() {
                    Some(FlagValue::Int(i))
                } else {
                    Some(FlagValue::Str(s))
                }
            }
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_present() {
        let f = LocalFlags::new();
        let id = Identity::default();
        assert_eq!(f.get_i64(keys::MAX_PARALLEL, &id, 0), 3);
        assert!(f.get_bool(keys::VIBE_REPORT_ENABLED, &id, false));
    }

    #[test]
    fn rollout_zero_disables() {
        let mut f = LocalFlags::new();
        f.set_rollout(keys::VIBE_REPORT_ENABLED, 0);
        let id = Identity {
            key: "agent-1".into(),
            ..Default::default()
        };
        assert!(!f.get_bool(keys::VIBE_REPORT_ENABLED, &id, true));
    }

    #[test]
    fn rollout_full_enables() {
        let mut f = LocalFlags::new();
        f.set_rollout(keys::VIBE_REPORT_ENABLED, 100);
        let id = Identity {
            key: "agent-1".into(),
            ..Default::default()
        };
        assert!(f.get_bool(keys::VIBE_REPORT_ENABLED, &id, false));
    }
}