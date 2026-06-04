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
    CrmRevenueOnly,
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
    pub drop_streams: Vec<String>,
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
    "phone",
    "mobilephone",
    "fax",
    "firstname",
    "last_name",
    "lastname",
    "firstname",
    "firstname",
    "address",
    "street",
    "city",
    "state",
    "zip",
    "country",
    "ipaddress",
    "message",
    "subject",
    "content",
    "body",
    "html",
    "text",
    "note",
    "notes",
    "hs_email_text",
    "hs_email_subject",
    "hs_ticket_subject",
    "hs_ticket_description",
];

const UPFOUNDRY_SAFE_CONTACT_PROPS: &[&str] = &[
    "lifecyclestage",
    "hs_lead_status",
    "createdate",
    "lastmodifieddate",
    "hs_analytics_source",
    "hs_analytics_source_data_1",
    "hs_analytics_source_data_2",
    "hs_analytics_first_url",
    "hs_latest_source",
    "hs_latest_source_data_1",
    "hs_latest_source_data_2",
    "utm_source",
    "utm_medium",
    "utm_campaign",
    "utm_content",
    "utm_term",
    "external_visitor_id",
    "upfoundry_visitor_id",
    "hs_object_id",
    "associatedcompanyid",
];

const UPFOUNDRY_SAFE_DEAL_PROPS: &[&str] = &[
    "dealname",
    "amount",
    "dealstage",
    "pipeline",
    "closedate",
    "createdate",
    "lastmodifieddate",
    "hs_is_closed_won",
    "hs_is_closed",
    "hubspot_owner_id",
    "hs_analytics_source",
    "hs_analytics_source_data_1",
    "hs_analytics_source_data_2",
    "utm_source",
    "utm_medium",
    "utm_campaign",
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
    if DENY_KEYS.iter().any(|d| lower.contains(d)) {
        return true;
    }
    false
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
            PrivacyProfile::UpfoundrySafe | PrivacyProfile::CrmRevenueOnly => {
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

    pub fn contact_property_allowlist(&self) -> Option<&'static [&'static str]> {
        match self.effective_mode() {
            PrivacyMode::Passthrough => None,
            PrivacyMode::Profile => match self.profile {
                PrivacyProfile::UpfoundrySafe | PrivacyProfile::CrmRevenueOnly => {
                    Some(UPFOUNDRY_SAFE_CONTACT_PROPS)
                }
                PrivacyProfile::Passthrough => None,
            },
            PrivacyMode::Allowlist => None,
        }
    }

    pub fn deal_property_allowlist(&self) -> Option<&'static [&'static str]> {
        match self.effective_mode() {
            PrivacyMode::Passthrough => None,
            PrivacyMode::Profile => match self.profile {
                PrivacyProfile::UpfoundrySafe | PrivacyProfile::CrmRevenueOnly => {
                    Some(UPFOUNDRY_SAFE_DEAL_PROPS)
                }
                PrivacyProfile::Passthrough => None,
            },
            PrivacyMode::Allowlist => None,
        }
    }

    pub fn redact_row(&self, row: &mut Value) -> PrivacyStats {
        let mut stats = PrivacyStats::default();
        redact_value(row, self, &mut stats);
        stats
    }
}

pub fn path_without_query(url: Option<&str>) -> Option<String> {
    url.map(|u| {
        let no_scheme = u.trim().split("//").nth(1).unwrap_or(u.trim());
        no_scheme.split('?').next().unwrap_or(no_scheme).to_string()
    })
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
        let mut row = json!({"email": "a@b.com", "lifecyclestage": "lead"});
        let stats = cfg.redact_row(&mut row);
        assert!(row.get("email").is_none());
        assert_eq!(row["lifecyclestage"], "lead");
        assert!(stats.properties_redacted > 0);
    }

    #[test]
    fn passthrough_keeps_custom_fields() {
        let cfg = PrivacyConfig::default();
        let mut row = json!({"custom_score": 42});
        cfg.redact_row(&mut row);
        assert_eq!(row["custom_score"], 42);
    }
}
