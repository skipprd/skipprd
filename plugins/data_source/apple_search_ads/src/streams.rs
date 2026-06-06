use std::collections::HashSet;

use serde::Deserialize;

/// Number of bronze namespaces in the full profile (single source of truth for tests/docs).
pub const FULL_STREAM_COUNT: usize = 4;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamProfile {
    /// Campaign daily only (discover / CI).
    Minimal,
    /// Campaign + ad group daily.
    Standard,
    /// All four daily grains (default).
    #[default]
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FanOut {
    None,
    PerCampaign,
    PerCampaignAdGroup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportGrain {
    Campaign,
    AdGroup,
    Keyword,
    SearchTerm,
}

#[derive(Clone, Copy, Debug)]
pub struct AsaStreamDef {
    pub namespace: &'static str,
    pub grain: ReportGrain,
    pub fan_out: FanOut,
    /// When set, overrides config `time_zone` for report requests (e.g. ORTZ for search terms).
    pub time_zone_override: Option<&'static str>,
}

pub const CURATED_STREAMS: &[AsaStreamDef] = &[
    AsaStreamDef {
        namespace: "apple_search_ads.campaign_daily",
        grain: ReportGrain::Campaign,
        fan_out: FanOut::None,
        time_zone_override: None,
    },
    AsaStreamDef {
        namespace: "apple_search_ads.ad_group_daily",
        grain: ReportGrain::AdGroup,
        fan_out: FanOut::PerCampaign,
        time_zone_override: None,
    },
    AsaStreamDef {
        namespace: "apple_search_ads.keyword_daily",
        grain: ReportGrain::Keyword,
        fan_out: FanOut::PerCampaignAdGroup,
        time_zone_override: None,
    },
    AsaStreamDef {
        namespace: "apple_search_ads.search_term_daily",
        grain: ReportGrain::SearchTerm,
        fan_out: FanOut::PerCampaign,
        time_zone_override: Some("ORTZ"),
    },
];

const MINIMAL_NAMESPACES: &[&str] = &["apple_search_ads.campaign_daily"];

const STANDARD_NAMESPACES: &[&str] = &[
    "apple_search_ads.campaign_daily",
    "apple_search_ads.ad_group_daily",
];

pub fn streams_for_profile(profile: StreamProfile) -> Vec<&'static AsaStreamDef> {
    CURATED_STREAMS
        .iter()
        .filter(|stream| stream_in_profile(stream, profile))
        .collect()
}

fn stream_in_profile(stream: &AsaStreamDef, profile: StreamProfile) -> bool {
    match profile {
        StreamProfile::Full => true,
        StreamProfile::Standard => STANDARD_NAMESPACES.contains(&stream.namespace),
        StreamProfile::Minimal => MINIMAL_NAMESPACES.contains(&stream.namespace),
    }
}

pub fn resolve_streams(
    profile: StreamProfile,
    explicit: Option<Vec<String>>,
) -> Vec<&'static AsaStreamDef> {
    let selected: HashSet<String> = explicit.unwrap_or_default().into_iter().collect();
    if selected.is_empty() {
        return streams_for_profile(profile);
    }
    CURATED_STREAMS
        .iter()
        .filter(|s| selected.contains(s.namespace))
        .collect()
}

pub fn stream_needs_campaign_enumeration(stream: &AsaStreamDef) -> bool {
    matches!(
        stream.fan_out,
        FanOut::PerCampaign | FanOut::PerCampaignAdGroup
    )
}

pub fn stream_needs_ad_group_enumeration(stream: &AsaStreamDef) -> bool {
    stream.fan_out == FanOut::PerCampaignAdGroup
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_profile_has_four_streams() {
        assert_eq!(
            streams_for_profile(StreamProfile::Full).len(),
            FULL_STREAM_COUNT
        );
        assert_eq!(CURATED_STREAMS.len(), FULL_STREAM_COUNT);
    }

    #[test]
    fn standard_profile_is_two_streams() {
        assert_eq!(streams_for_profile(StreamProfile::Standard).len(), 2);
    }

    #[test]
    fn minimal_profile_is_campaign_only() {
        let streams = streams_for_profile(StreamProfile::Minimal);
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].namespace, "apple_search_ads.campaign_daily");
    }

    #[test]
    fn search_term_stream_uses_ortz() {
        let st = CURATED_STREAMS
            .iter()
            .find(|s| s.namespace == "apple_search_ads.search_term_daily")
            .unwrap();
        assert_eq!(st.time_zone_override, Some("ORTZ"));
    }
}
