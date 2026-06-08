use std::collections::HashSet;

use serde::Deserialize;

pub const NAMESPACE_MERCHANT_SNAPSHOT: &str = "sumup_merchant_snapshot";
pub const NAMESPACE_TRANSACTION_FACT: &str = "sumup_transaction_fact";
pub const NAMESPACE_PAYOUT_FACT: &str = "sumup_payout_fact";
pub const NAMESPACE_CHECKOUT_SNAPSHOT: &str = "sumup_checkout_snapshot";
pub const NAMESPACE_SYNC_RUN_DAILY: &str = "sumup_sync_run_daily";

pub const NAMESPACE_MERCHANT_SOURCE_RAW: &str = "sumup_merchant_source_raw";
pub const NAMESPACE_TRANSACTION_SOURCE_RAW: &str = "sumup_transaction_source_raw";
pub const NAMESPACE_PAYOUT_SOURCE_RAW: &str = "sumup_payout_source_raw";
pub const NAMESPACE_CHECKOUT_SOURCE_RAW: &str = "sumup_checkout_source_raw";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SumUpStream {
    Merchant,
    Transactions,
    Payouts,
    Checkouts,
    Health,
}

impl SumUpStream {
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "merchant" => Some(Self::Merchant),
            "transactions" => Some(Self::Transactions),
            "payouts" => Some(Self::Payouts),
            "checkouts" => Some(Self::Checkouts),
            "health" => Some(Self::Health),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Merchant => "merchant",
            Self::Transactions => "transactions",
            Self::Payouts => "payouts",
            Self::Checkouts => "checkouts",
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

const MINIMAL_STREAMS: &[&str] = &["merchant", "transactions", "health"];
const CONSOLE_DEFAULT_STREAMS: &[&str] = &["merchant", "transactions", "payouts", "health"];
const FULL_STREAMS: &[&str] = &["merchant", "transactions", "payouts", "checkouts", "health"];

pub fn streams_for_profile(profile: StreamProfile) -> Vec<SumUpStream> {
    let names = match profile {
        StreamProfile::Minimal => MINIMAL_STREAMS,
        StreamProfile::ConsoleDefault => CONSOLE_DEFAULT_STREAMS,
        StreamProfile::Full => FULL_STREAMS,
    };
    names
        .iter()
        .filter_map(|name| SumUpStream::from_name(name))
        .collect()
}

pub fn resolve_streams(profile: StreamProfile, explicit: Option<Vec<String>>) -> Vec<SumUpStream> {
    let selected: HashSet<String> = explicit.unwrap_or_default().into_iter().collect();
    if selected.is_empty() {
        return streams_for_profile(profile);
    }
    selected
        .iter()
        .filter_map(|name| SumUpStream::from_name(name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_default_includes_core_data_streams() {
        let streams = streams_for_profile(StreamProfile::ConsoleDefault);
        let names: Vec<_> = streams.iter().map(|s| s.name()).collect();
        assert!(names.contains(&"merchant"));
        assert!(names.contains(&"transactions"));
        assert!(names.contains(&"payouts"));
        assert!(names.contains(&"health"));
        assert!(!names.contains(&"checkouts"));
        let full_names: Vec<_> = streams_for_profile(StreamProfile::Full)
            .iter()
            .map(|s| s.name())
            .collect();
        assert!(full_names.contains(&"checkouts"));
    }
}
