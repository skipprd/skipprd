use std::collections::HashSet;

use serde::Deserialize;

/// Number of bronze namespaces in the full profile (single source of truth for tests/docs).
pub const FULL_STREAM_COUNT: usize = 5;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamProfile {
    /// Account daily only (discover / CI).
    Minimal,
    /// Account + campaign + ad set daily.
    Standard,
    /// All five daily grains (default).
    #[default]
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsightsLevel {
    Account,
    Campaign,
    Adset,
    Ad,
}

#[derive(Clone, Copy, Debug)]
pub struct MetaStreamDef {
    pub namespace: &'static str,
    pub level: InsightsLevel,
    /// When true, request `breakdowns=platform_position` at campaign level.
    pub placement_breakdown: bool,
}

pub const CURATED_STREAMS: &[MetaStreamDef] = &[
    MetaStreamDef {
        namespace: "meta_ads.account_daily",
        level: InsightsLevel::Account,
        placement_breakdown: false,
    },
    MetaStreamDef {
        namespace: "meta_ads.campaign_daily",
        level: InsightsLevel::Campaign,
        placement_breakdown: false,
    },
    MetaStreamDef {
        namespace: "meta_ads.adset_daily",
        level: InsightsLevel::Adset,
        placement_breakdown: false,
    },
    MetaStreamDef {
        namespace: "meta_ads.ad_daily",
        level: InsightsLevel::Ad,
        placement_breakdown: false,
    },
    MetaStreamDef {
        namespace: "meta_ads.campaign_placement_daily",
        level: InsightsLevel::Campaign,
        placement_breakdown: true,
    },
];

const MINIMAL_NAMESPACES: &[&str] = &["meta_ads.account_daily"];

const STANDARD_NAMESPACES: &[&str] = &[
    "meta_ads.account_daily",
    "meta_ads.campaign_daily",
    "meta_ads.adset_daily",
];

pub fn streams_for_profile(profile: StreamProfile) -> Vec<&'static MetaStreamDef> {
    CURATED_STREAMS
        .iter()
        .filter(|stream| stream_in_profile(stream, profile))
        .collect()
}

fn stream_in_profile(stream: &MetaStreamDef, profile: StreamProfile) -> bool {
    match profile {
        StreamProfile::Full => true,
        StreamProfile::Standard => STANDARD_NAMESPACES.contains(&stream.namespace),
        StreamProfile::Minimal => MINIMAL_NAMESPACES.contains(&stream.namespace),
    }
}

pub fn resolve_streams(
    profile: StreamProfile,
    explicit: Option<Vec<String>>,
) -> Vec<&'static MetaStreamDef> {
    let selected: HashSet<String> = explicit.unwrap_or_default().into_iter().collect();
    if selected.is_empty() {
        return streams_for_profile(profile);
    }
    CURATED_STREAMS
        .iter()
        .filter(|s| selected.contains(s.namespace))
        .collect()
}

pub fn insights_level_param(level: InsightsLevel) -> &'static str {
    match level {
        InsightsLevel::Account => "account",
        InsightsLevel::Campaign => "campaign",
        InsightsLevel::Adset => "adset",
        InsightsLevel::Ad => "ad",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_profile_has_five_streams() {
        assert_eq!(
            streams_for_profile(StreamProfile::Full).len(),
            FULL_STREAM_COUNT
        );
        assert_eq!(CURATED_STREAMS.len(), FULL_STREAM_COUNT);
    }

    #[test]
    fn standard_profile_is_three_streams() {
        assert_eq!(streams_for_profile(StreamProfile::Standard).len(), 3);
    }

    #[test]
    fn minimal_profile_is_account_only() {
        let streams = streams_for_profile(StreamProfile::Minimal);
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].namespace, "meta_ads.account_daily");
    }

    #[test]
    fn resolve_streams_explicit_override_filters_catalog() {
        let selected = resolve_streams(StreamProfile::Full, Some(vec!["meta_ads.ad_daily".into()]));
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].namespace, "meta_ads.ad_daily");
    }

    #[test]
    fn placement_stream_uses_campaign_level_with_breakdown() {
        let placement = CURATED_STREAMS
            .iter()
            .find(|s| s.namespace == "meta_ads.campaign_placement_daily")
            .unwrap();
        assert_eq!(placement.level, InsightsLevel::Campaign);
        assert!(placement.placement_breakdown);
    }
}
