use std::collections::HashSet;

use serde::Deserialize;

pub const NAMESPACE_ACCOUNT_SNAPSHOT: &str = "stripe_account_snapshot";
pub const NAMESPACE_PRODUCT_SNAPSHOT: &str = "stripe_product_snapshot";
pub const NAMESPACE_PRICE_SNAPSHOT: &str = "stripe_price_snapshot";
pub const NAMESPACE_CUSTOMER_SNAPSHOT: &str = "stripe_customer_snapshot";
pub const NAMESPACE_SUBSCRIPTION_SNAPSHOT: &str = "stripe_subscription_snapshot";
pub const NAMESPACE_INVOICE_FACT: &str = "stripe_invoice_fact";
pub const NAMESPACE_INVOICE_LINE_FACT: &str = "stripe_invoice_line_fact";
pub const NAMESPACE_CHARGE_FACT: &str = "stripe_charge_fact";
pub const NAMESPACE_PAYMENT_INTENT_FACT: &str = "stripe_payment_intent_fact";
pub const NAMESPACE_REFUND_FACT: &str = "stripe_refund_fact";
pub const NAMESPACE_DISPUTE_FACT: &str = "stripe_dispute_fact";
pub const NAMESPACE_BALANCE_TRANSACTION_FACT: &str = "stripe_balance_transaction_fact";
pub const NAMESPACE_PAYOUT_FACT: &str = "stripe_payout_fact";
pub const NAMESPACE_COUPON_SNAPSHOT: &str = "stripe_coupon_snapshot";
pub const NAMESPACE_PROMOTION_CODE_SNAPSHOT: &str = "stripe_promotion_code_snapshot";
pub const NAMESPACE_SYNC_RUN_DAILY: &str = "stripe_sync_run_daily";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StripeStream {
    Account,
    Catalog,
    Customers,
    Subscriptions,
    Invoices,
    Payments,
    Disputes,
    Cash,
    Promotions,
}

impl StripeStream {
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "account" => Some(Self::Account),
            "catalog" => Some(Self::Catalog),
            "customers" => Some(Self::Customers),
            "subscriptions" => Some(Self::Subscriptions),
            "invoices" => Some(Self::Invoices),
            "payments" => Some(Self::Payments),
            "disputes" => Some(Self::Disputes),
            "cash" => Some(Self::Cash),
            "promotions" => Some(Self::Promotions),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Account => "account",
            Self::Catalog => "catalog",
            Self::Customers => "customers",
            Self::Subscriptions => "subscriptions",
            Self::Invoices => "invoices",
            Self::Payments => "payments",
            Self::Disputes => "disputes",
            Self::Cash => "cash",
            Self::Promotions => "promotions",
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

const MINIMAL_STREAMS: &[&str] = &["account", "subscriptions", "invoices", "payments"];
const CONSOLE_DEFAULT_STREAMS: &[&str] = &[
    "account",
    "subscriptions",
    "invoices",
    "payments",
    "customers",
    "catalog",
    "cash",
];
const FULL_STREAMS: &[&str] = &[
    "account",
    "catalog",
    "customers",
    "subscriptions",
    "invoices",
    "payments",
    "disputes",
    "cash",
    "promotions",
];

pub fn streams_for_profile(profile: StreamProfile) -> Vec<StripeStream> {
    let names = match profile {
        StreamProfile::Minimal => MINIMAL_STREAMS,
        StreamProfile::ConsoleDefault => CONSOLE_DEFAULT_STREAMS,
        StreamProfile::Full => FULL_STREAMS,
    };
    names
        .iter()
        .filter_map(|name| StripeStream::from_name(name))
        .collect()
}

pub fn resolve_streams(
    profile: StreamProfile,
    explicit: Option<Vec<String>>,
) -> Vec<StripeStream> {
    let selected: HashSet<String> = explicit.unwrap_or_default().into_iter().collect();
    if selected.is_empty() {
        return streams_for_profile(profile);
    }
    selected
        .iter()
        .filter_map(|name| StripeStream::from_name(name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_profile_includes_disputes_and_promotions() {
        let streams = streams_for_profile(StreamProfile::Full);
        let names: Vec<_> = streams.iter().map(|s| s.name()).collect();
        assert!(names.contains(&"disputes"));
        assert!(names.contains(&"promotions"));
    }
}
