use std::collections::HashSet;

use serde::Deserialize;

pub const NAMESPACE_ACCOUNT_SNAPSHOT: &str = "revolut_account_snapshot";
pub const NAMESPACE_TRANSACTION_FACT: &str = "revolut_transaction_fact";
pub const NAMESPACE_TRANSACTION_LEG_FACT: &str = "revolut_transaction_leg_fact";
pub const NAMESPACE_SYNC_RUN_DAILY: &str = "revolut_sync_run_daily";

pub const NAMESPACE_ACCOUNT_SOURCE_RAW: &str = "revolut_account_source_raw";
pub const NAMESPACE_TRANSACTION_SOURCE_RAW: &str = "revolut_transaction_source_raw";
pub const NAMESPACE_TRANSACTION_LEG_SOURCE_RAW: &str = "revolut_transaction_leg_source_raw";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevolutStream {
    Accounts,
    Transactions,
    TransactionLegs,
    Health,
}

impl RevolutStream {
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "accounts" => Some(Self::Accounts),
            "transactions" => Some(Self::Transactions),
            "transaction_legs" | "transaction-legs" => Some(Self::TransactionLegs),
            "health" => Some(Self::Health),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Accounts => "accounts",
            Self::Transactions => "transactions",
            Self::TransactionLegs => "transaction_legs",
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

const MINIMAL_STREAMS: &[&str] = &["accounts", "transactions", "health"];
const CONSOLE_DEFAULT_STREAMS: &[&str] =
    &["accounts", "transactions", "transaction_legs", "health"];
const FULL_STREAMS: &[&str] = &["accounts", "transactions", "transaction_legs", "health"];

pub fn streams_for_profile(profile: StreamProfile) -> Vec<RevolutStream> {
    let names = match profile {
        StreamProfile::Minimal => MINIMAL_STREAMS,
        StreamProfile::ConsoleDefault => CONSOLE_DEFAULT_STREAMS,
        StreamProfile::Full => FULL_STREAMS,
    };
    names
        .iter()
        .filter_map(|name| RevolutStream::from_name(name))
        .collect()
}

pub fn resolve_streams(
    profile: StreamProfile,
    explicit: Option<Vec<String>>,
) -> Vec<RevolutStream> {
    let selected: HashSet<String> = explicit.unwrap_or_default().into_iter().collect();
    if selected.is_empty() {
        return streams_for_profile(profile);
    }
    selected
        .iter()
        .filter_map(|name| RevolutStream::from_name(name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_profile_includes_transaction_legs() {
        let streams = streams_for_profile(StreamProfile::Full);
        let names: Vec<_> = streams.iter().map(|s| s.name()).collect();
        assert!(names.contains(&"accounts"));
        assert!(names.contains(&"transactions"));
        assert!(names.contains(&"transaction_legs"));
        assert!(names.contains(&"health"));
    }
}
