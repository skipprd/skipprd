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
    "name",
    "beneficiary",
    "beneficiary_address",
    "beneficiaryaddress",
    "reference",
    "description",
    "card",
    "merchant",
    "email",
    "phone",
    "address",
    "iban",
    "account_number",
    "accountnumber",
    "pan",
    "cvv",
    "card_number",
    "cardnumber",
    "first_name",
    "firstname",
    "last_name",
    "lastname",
    "counterparty",
    "counterparty_name",
    "memo",
    "note",
    "notes",
    "bank_details",
    "bankdetails",
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
            PrivacyProfile::UpfoundrySafe => match value {
                Value::Object(map) => apply_deny_map(map, config, stats),
                Value::Array(items) => {
                    for item in items.iter_mut() {
                        redact_value(item, config, stats);
                    }
                }
                _ => {}
            },
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

    fn upfoundry_safe() -> PrivacyConfig {
        PrivacyConfig {
            mode: PrivacyMode::Profile,
            profile: PrivacyProfile::UpfoundrySafe,
            ..Default::default()
        }
    }

    #[test]
    fn upfoundry_safe_strips_account_name() {
        let cfg = upfoundry_safe();
        let mut row = json!({
            "id": "acc-1",
            "name": "Operating Account",
            "currency": "GBP",
            "balance": 1000.0,
            "state": "active"
        });
        cfg.redact_row(&mut row);
        assert!(row.get("name").is_none());
        assert_eq!(row["currency"], "GBP");
    }

    #[test]
    fn upfoundry_safe_strips_transaction_reference_and_merchant() {
        let cfg = upfoundry_safe();
        let mut row = json!({
            "id": "tx-1",
            "type": "card_payment",
            "reference": "Coffee shop",
            "description": "Payment memo",
            "merchant": {"name": "Acme Corp", "city": "London"},
            "card": {"pan": "4111111111111111"}
        });
        cfg.redact_row(&mut row);
        assert!(row.get("reference").is_none());
        assert!(row.get("description").is_none());
        assert!(row.get("merchant").is_none());
        assert!(row.get("card").is_none());
        assert_eq!(row["type"], "card_payment");
    }

    #[test]
    fn upfoundry_safe_strips_beneficiary() {
        let cfg = upfoundry_safe();
        let mut row = json!({
            "id": "tx-2",
            "type": "transfer",
            "beneficiary": {"name": "Supplier Ltd", "iban": "GB00TEST"},
            "beneficiary_address": {"street": "1 High St"}
        });
        cfg.redact_row(&mut row);
        assert!(row.get("beneficiary").is_none());
        assert!(row.get("beneficiary_address").is_none());
    }
}
