use std::collections::HashSet;

use serde::Deserialize;

pub const NAMESPACE_ORGANISATION_SNAPSHOT: &str = "xero_organisation_snapshot";
pub const NAMESPACE_ACCOUNT_SNAPSHOT: &str = "xero_account_snapshot";
pub const NAMESPACE_CONTACT_SNAPSHOT: &str = "xero_contact_snapshot";
pub const NAMESPACE_INVOICE_FACT: &str = "xero_invoice_fact";
pub const NAMESPACE_INVOICE_LINE_FACT: &str = "xero_invoice_line_fact";
pub const NAMESPACE_PAYMENT_FACT: &str = "xero_payment_fact";
pub const NAMESPACE_BANK_TRANSACTION_FACT: &str = "xero_bank_transaction_fact";
pub const NAMESPACE_SYNC_RUN_DAILY: &str = "xero_sync_run_daily";

pub const NAMESPACE_ORGANISATION_SOURCE_RAW: &str = "xero_organisation_source_raw";
pub const NAMESPACE_ACCOUNT_SOURCE_RAW: &str = "xero_account_source_raw";
pub const NAMESPACE_CONTACT_SOURCE_RAW: &str = "xero_contact_source_raw";
pub const NAMESPACE_INVOICE_SOURCE_RAW: &str = "xero_invoice_source_raw";
pub const NAMESPACE_INVOICE_LINE_SOURCE_RAW: &str = "xero_invoice_line_source_raw";
pub const NAMESPACE_PAYMENT_SOURCE_RAW: &str = "xero_payment_source_raw";
pub const NAMESPACE_BANK_TRANSACTION_SOURCE_RAW: &str = "xero_bank_transaction_source_raw";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XeroStream {
    Organisation,
    Accounts,
    Contacts,
    Invoices,
    Payments,
    Bank,
    Health,
}

impl XeroStream {
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "organisation" | "organization" => Some(Self::Organisation),
            "accounts" => Some(Self::Accounts),
            "contacts" => Some(Self::Contacts),
            "invoices" => Some(Self::Invoices),
            "payments" => Some(Self::Payments),
            "bank" | "bank_transactions" => Some(Self::Bank),
            "health" => Some(Self::Health),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Organisation => "organisation",
            Self::Accounts => "accounts",
            Self::Contacts => "contacts",
            Self::Invoices => "invoices",
            Self::Payments => "payments",
            Self::Bank => "bank",
            Self::Health => "health",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamProfile {
    Minimal,
    #[default]
    #[serde(alias = "console_default")]
    ConsoleDefault,
    Full,
}

const MINIMAL_STREAMS: &[&str] = &["organisation", "invoices", "payments", "health"];
const CONSOLE_DEFAULT_STREAMS: &[&str] = &[
    "organisation",
    "accounts",
    "invoices",
    "payments",
    "bank",
    "health",
];
const FULL_STREAMS: &[&str] = &[
    "organisation",
    "accounts",
    "contacts",
    "invoices",
    "payments",
    "bank",
    "health",
];

pub fn streams_for_profile(profile: StreamProfile) -> Vec<XeroStream> {
    let names = match profile {
        StreamProfile::Minimal => MINIMAL_STREAMS,
        StreamProfile::ConsoleDefault => CONSOLE_DEFAULT_STREAMS,
        StreamProfile::Full => FULL_STREAMS,
    };
    names
        .iter()
        .filter_map(|name| XeroStream::from_name(name))
        .collect()
}

pub fn resolve_streams(profile: StreamProfile, explicit: Option<Vec<String>>) -> Vec<XeroStream> {
    let selected: HashSet<String> = explicit.unwrap_or_default().into_iter().collect();
    if selected.is_empty() {
        return streams_for_profile(profile);
    }
    selected
        .iter()
        .filter_map(|name| XeroStream::from_name(name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_profile_includes_contacts() {
        let streams = streams_for_profile(StreamProfile::Full);
        let names: Vec<_> = streams.iter().map(|s| s.name()).collect();
        assert!(names.contains(&"contacts"));
        assert!(names.contains(&"invoices"));
    }

    #[test]
    fn console_default_excludes_contacts() {
        let streams = streams_for_profile(StreamProfile::ConsoleDefault);
        let names: Vec<_> = streams.iter().map(|s| s.name()).collect();
        assert!(!names.contains(&"contacts"));
        assert!(names.contains(&"bank"));
    }
}
