use std::collections::HashSet;

use serde::Deserialize;

pub const NAMESPACE_STORE_SNAPSHOT: &str = "shopify_store_snapshot";
pub const NAMESPACE_PRODUCT_SNAPSHOT: &str = "shopify_product_snapshot";
pub const NAMESPACE_ORDER_FACT: &str = "shopify_order_fact";
pub const NAMESPACE_ORDER_LINE_FACT: &str = "shopify_order_line_fact";
pub const NAMESPACE_SYNC_RUN_DAILY: &str = "shopify_sync_run_daily";

/// Logical sync streams from skippr.yml `streams:` list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShopifyStream {
    Store,
    Catalog,
    Orders,
    Content,
    Marketing,
    Collections,
    Discounts,
}

impl ShopifyStream {
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "store" => Some(Self::Store),
            "catalog" => Some(Self::Catalog),
            "orders" => Some(Self::Orders),
            "content" => Some(Self::Content),
            "marketing" => Some(Self::Marketing),
            "collections" => Some(Self::Collections),
            "discounts" => Some(Self::Discounts),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Store => "store",
            Self::Catalog => "catalog",
            Self::Orders => "orders",
            Self::Content => "content",
            Self::Marketing => "marketing",
            Self::Collections => "collections",
            Self::Discounts => "discounts",
        }
    }

    pub fn is_implemented(self) -> bool {
        true
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamProfile {
    /// store + catalog + orders (discover / CI).
    Minimal,
    /// Console default: store, catalog, orders, marketing, content.
    #[default]
    #[serde(alias = "console_default")]
    ConsoleDefault,
    /// All known logical streams (many are no-ops until implemented).
    Full,
}

const CONSOLE_DEFAULT_STREAMS: &[&str] = &["store", "catalog", "orders", "marketing", "content"];
const MINIMAL_STREAMS: &[&str] = &["store", "catalog", "orders"];
const FULL_STREAMS: &[&str] = &[
    "store",
    "catalog",
    "collections",
    "content",
    "orders",
    "marketing",
    "discounts",
];

pub fn streams_for_profile(profile: StreamProfile) -> Vec<ShopifyStream> {
    let names = match profile {
        StreamProfile::Minimal => MINIMAL_STREAMS,
        StreamProfile::ConsoleDefault => CONSOLE_DEFAULT_STREAMS,
        StreamProfile::Full => FULL_STREAMS,
    };
    names
        .iter()
        .filter_map(|name| ShopifyStream::from_name(name))
        .collect()
}

pub fn resolve_streams(
    profile: StreamProfile,
    explicit: Option<Vec<String>>,
) -> Vec<ShopifyStream> {
    let selected: HashSet<String> = explicit.unwrap_or_default().into_iter().collect();
    if selected.is_empty() {
        return streams_for_profile(profile);
    }
    selected
        .iter()
        .filter_map(|name| ShopifyStream::from_name(name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_default_includes_marketing_and_content() {
        let streams = streams_for_profile(StreamProfile::ConsoleDefault);
        let names: Vec<_> = streams.iter().map(|s| s.name()).collect();
        assert!(names.contains(&"marketing"));
        assert!(names.contains(&"content"));
        assert!(names.contains(&"orders"));
    }

    #[test]
    fn explicit_streams_override_profile() {
        let streams = resolve_streams(StreamProfile::Full, Some(vec!["store".into()]));
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0], ShopifyStream::Store);
    }

    #[test]
    fn full_profile_streams_are_implemented() {
        for stream in streams_for_profile(StreamProfile::Full) {
            assert!(stream.is_implemented());
        }
    }
}
