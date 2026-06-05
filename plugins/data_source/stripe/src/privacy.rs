use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyMode {
    #[default]
    Passthrough,
    Profile,
    Allowlist,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyProfile {
    #[default]
    Passthrough,
    #[serde(alias = "upfoundry_safe")]
    UpfoundrySafe,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnViolation {
    #[default]
    Drop,
    Deadletter,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PrivacyConfig {
    #[serde(default)]
    pub mode: PrivacyMode,
    #[serde(default)]
    pub profile: PrivacyProfile,
    #[serde(default)]
    pub drop_properties: Vec<String>,
    #[serde(default)]
    pub keep_properties: Vec<String>,
    #[serde(default)]
    pub hash_properties: Vec<String>,
    #[serde(default)]
    pub on_violation: OnViolation,
}

#[derive(Debug, Default)]
pub struct PrivacyStats {
    pub properties_redacted: u64,
    pub rows_skipped: u64,
}

const DENY_KEYS: &[&str] = &[
    "email",
    "name",
    "phone",
    "address",
    "line1",
    "line2",
    "city",
    "state",
    "postal_code",
    "country",
    "description",
    "receipt_email",
    "billing_details",
    "shipping",
    "customer_name",
    "customer_email",
    "customer_phone",
    "card",
    "payment_method_details",
    "source",
    "metadata",
];

fn key_denied(key: &str, config: &PrivacyConfig) -> bool {
    let lower = key.to_ascii_lowercase();
    if config
        .drop_properties
        .iter()
        .any(|k| k.eq_ignore_ascii_case(key))
    {
        return true;
    }
    DENY_KEYS.iter().any(|d| lower.contains(d))
}

fn apply_deny_map(map: &mut Map<String, Value>, config: &PrivacyConfig, stats: &mut PrivacyStats) {
    let keys: Vec<String> = map.keys().cloned().collect();
    for key in keys {
        if key_denied(&key, config) {
            map.remove(&key);
            stats.properties_redacted += 1;
        }
    }
    for v in map.values_mut() {
        redact_value(v, config, stats);
    }
}

pub fn redact_value(value: &mut Value, config: &PrivacyConfig, stats: &mut PrivacyStats) {
    match config.effective_mode() {
        PrivacyMode::Passthrough => {
            if let Value::Object(map) = value {
                for key in DENY_KEYS {
                    if map.remove(*key).is_some() {
                        stats.properties_redacted += 1;
                    }
                }
            }
        }
        PrivacyMode::Profile => match config.profile {
            PrivacyProfile::Passthrough => redact_value(
                value,
                &PrivacyConfig {
                    mode: PrivacyMode::Passthrough,
                    ..Default::default()
                },
                stats,
            ),
            PrivacyProfile::UpfoundrySafe => {
                if let Value::Object(map) = value {
                    apply_deny_map(map, config, stats);
                }
            }
        },
        PrivacyMode::Allowlist => {
            if let Value::Object(map) = value {
                let keep: std::collections::HashSet<String> = config
                    .keep_properties
                    .iter()
                    .map(|k| k.to_ascii_lowercase())
                    .collect();
                let keys: Vec<String> = map.keys().cloned().collect();
                for key in keys {
                    if !keep.contains(&key.to_ascii_lowercase()) {
                        map.remove(&key);
                        stats.properties_redacted += 1;
                    }
                }
            }
        }
    }
}

impl PrivacyConfig {
    pub fn effective_mode(&self) -> PrivacyMode {
        match self.mode {
            PrivacyMode::Passthrough if self.profile != PrivacyProfile::Passthrough => {
                PrivacyMode::Profile
            }
            other => other,
        }
    }

    pub fn redact_row(&self, row: &mut Value) -> PrivacyStats {
        let mut stats = PrivacyStats::default();
        redact_value(row, self, &mut stats);
        stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn upfoundry_safe_strips_email() {
        let cfg = PrivacyConfig {
            mode: PrivacyMode::Profile,
            profile: PrivacyProfile::UpfoundrySafe,
            ..Default::default()
        };
        let mut row = json!({"email": "a@b.com", "customer_id": "cus_1"});
        let stats = cfg.redact_row(&mut row);
        assert!(row.get("email").is_none());
        assert_eq!(row["customer_id"], "cus_1");
        assert!(stats.properties_redacted > 0);
    }
}
