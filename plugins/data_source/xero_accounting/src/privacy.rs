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
    "firstname",
    "lastname",
    "legalname",
    "email",
    "emailaddress",
    "phone",
    "phones",
    "address",
    "addresses",
    "line1",
    "line2",
    "city",
    "region",
    "postalcode",
    "postcode",
    "attentionto",
    "payee",
    "payeeaccount",
    "reference",
    "description",
    "url",
    "taxnumber",
    "bankaccountnumber",
    "accountnumber",
    "contactpersons",
    "contactperson",
    "brandingtheme",
    "brandingthemeid",
    "memo",
    "note",
    "notes",
    "statementtext",
    "particulars",
    "details",
    "narrative",
    "tracking",
    "attachments",
    "hasattachments",
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
    fn upfoundry_safe_strips_contact_pii() {
        let cfg = upfoundry_safe();
        let mut row = json!({
            "ContactID": "c-1",
            "Name": "Acme Ltd",
            "EmailAddress": "billing@acme.example",
            "ContactStatus": "ACTIVE",
            "IsCustomer": true
        });
        cfg.redact_row(&mut row);
        assert!(row.get("Name").is_none());
        assert!(row.get("EmailAddress").is_none());
        assert_eq!(row["ContactID"], "c-1");
        assert_eq!(row["ContactStatus"], "ACTIVE");
    }

    #[test]
    fn upfoundry_safe_strips_invoice_reference_and_line_description() {
        let cfg = upfoundry_safe();
        let mut row = json!({
            "InvoiceID": "inv-1",
            "Reference": "PO-12345",
            "Total": 100.0,
            "LineItems": [
                {"LineItemID": "l-1", "Description": "Consulting services", "LineAmount": 100.0}
            ]
        });
        cfg.redact_row(&mut row);
        assert!(row.get("Reference").is_none());
        let line = &row["LineItems"][0];
        assert!(line.get("Description").is_none());
        assert_eq!(line["LineAmount"], 100.0);
    }

    #[test]
    fn upfoundry_safe_strips_bank_payee() {
        let cfg = upfoundry_safe();
        let mut row = json!({
            "BankTransactionID": "b-1",
            "Payee": "John Smith",
            "Total": 50.0,
            "Type": "SPEND"
        });
        cfg.redact_row(&mut row);
        assert!(row.get("Payee").is_none());
        assert_eq!(row["Total"], 50.0);
    }

    #[test]
    fn upfoundry_safe_strips_organisation_address_and_phone() {
        let cfg = upfoundry_safe();
        let mut row = json!({
            "OrganisationID": "org-1",
            "Name": "Demo Co",
            "OrganisationType": "COMPANY",
            "CountryCode": "GB",
            "BaseCurrency": "GBP",
            "Addresses": [{"City": "London"}],
            "Phones": [{"PhoneNumber": "+441234567890"}]
        });
        cfg.redact_row(&mut row);
        assert!(row.get("Name").is_none());
        assert!(row.get("Addresses").is_none());
        assert!(row.get("Phones").is_none());
        assert_eq!(row["BaseCurrency"], "GBP");
    }
}
