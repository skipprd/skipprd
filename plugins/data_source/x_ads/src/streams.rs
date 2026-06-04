use serde::Deserialize;
use std::collections::HashSet;

pub const FULL_STREAM_COUNT: usize = 5;
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamProfile {
    Minimal,
    Standard,
    #[default]
    Full,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XAdsStreamKind {
    Accounts,
    Campaigns,
    LineItems,
    PromotedPosts,
    Analytics,
}
#[derive(Clone, Copy, Debug)]
pub struct XAdsStreamDef {
    pub namespace: &'static str,
    pub kind: XAdsStreamKind,
    pub primary_key: &'static [&'static str],
    pub cursor: Option<&'static str>,
}
pub const CURATED_STREAMS: &[XAdsStreamDef] = &[
    XAdsStreamDef {
        namespace: "x_ads.accounts",
        kind: XAdsStreamKind::Accounts,
        primary_key: &["account_id"],
        cursor: None,
    },
    XAdsStreamDef {
        namespace: "x_ads.campaigns",
        kind: XAdsStreamKind::Campaigns,
        primary_key: &["account_id", "campaign_id"],
        cursor: None,
    },
    XAdsStreamDef {
        namespace: "x_ads.line_items",
        kind: XAdsStreamKind::LineItems,
        primary_key: &["account_id", "line_item_id"],
        cursor: None,
    },
    XAdsStreamDef {
        namespace: "x_ads.promoted_posts",
        kind: XAdsStreamKind::PromotedPosts,
        primary_key: &["account_id", "promoted_post_id"],
        cursor: None,
    },
    XAdsStreamDef {
        namespace: "x_ads.analytics_daily",
        kind: XAdsStreamKind::Analytics,
        primary_key: &["account_id", "date", "entity_type", "entity_id"],
        cursor: Some("date"),
    },
];
const MINIMAL_NAMESPACES: &[&str] = &["x_ads.analytics_daily"];
const STANDARD_NAMESPACES: &[&str] = &[
    "x_ads.accounts",
    "x_ads.campaigns",
    "x_ads.line_items",
    "x_ads.analytics_daily",
];
pub fn streams_for_profile(profile: StreamProfile) -> Vec<&'static XAdsStreamDef> {
    CURATED_STREAMS
        .iter()
        .filter(|s| match profile {
            StreamProfile::Full => true,
            StreamProfile::Standard => STANDARD_NAMESPACES.contains(&s.namespace),
            StreamProfile::Minimal => MINIMAL_NAMESPACES.contains(&s.namespace),
        })
        .collect()
}
pub fn resolve_streams(
    profile: StreamProfile,
    explicit: Option<Vec<String>>,
) -> Vec<&'static XAdsStreamDef> {
    let selected: HashSet<String> = explicit.unwrap_or_default().into_iter().collect();
    if selected.is_empty() {
        return streams_for_profile(profile);
    }
    CURATED_STREAMS
        .iter()
        .filter(|s| selected.contains(s.namespace))
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn x_ads_surface_is_native() {
        assert_eq!(CURATED_STREAMS.len(), FULL_STREAM_COUNT);
        assert!(CURATED_STREAMS
            .iter()
            .any(|s| s.namespace == "x_ads.line_items"));
        assert!(CURATED_STREAMS
            .iter()
            .any(|s| s.namespace == "x_ads.promoted_posts"));
    }
}
