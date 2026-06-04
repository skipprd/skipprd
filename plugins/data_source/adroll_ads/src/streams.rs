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
pub enum AdRollStreamKind {
    Advertisers,
    Campaigns,
    AdGroups,
    Ads,
    Reporting,
}
#[derive(Clone, Copy, Debug)]
pub struct AdRollStreamDef {
    pub namespace: &'static str,
    pub kind: AdRollStreamKind,
    pub primary_key: &'static [&'static str],
    pub cursor: Option<&'static str>,
}
pub const CURATED_STREAMS: &[AdRollStreamDef] = &[
    AdRollStreamDef {
        namespace: "adroll_ads.advertisers",
        kind: AdRollStreamKind::Advertisers,
        primary_key: &["advertiser_id"],
        cursor: None,
    },
    AdRollStreamDef {
        namespace: "adroll_ads.campaigns",
        kind: AdRollStreamKind::Campaigns,
        primary_key: &["advertiser_id", "campaign_id"],
        cursor: None,
    },
    AdRollStreamDef {
        namespace: "adroll_ads.ad_groups",
        kind: AdRollStreamKind::AdGroups,
        primary_key: &["advertiser_id", "ad_group_id"],
        cursor: None,
    },
    AdRollStreamDef {
        namespace: "adroll_ads.ads",
        kind: AdRollStreamKind::Ads,
        primary_key: &["advertiser_id", "ad_id"],
        cursor: None,
    },
    AdRollStreamDef {
        namespace: "adroll_ads.reporting_daily",
        kind: AdRollStreamKind::Reporting,
        primary_key: &[
            "advertiser_id",
            "date",
            "campaign_id",
            "ad_group_id",
            "ad_id",
        ],
        cursor: Some("date"),
    },
];
const MINIMAL_NAMESPACES: &[&str] = &["adroll_ads.reporting_daily"];
const STANDARD_NAMESPACES: &[&str] = &[
    "adroll_ads.advertisers",
    "adroll_ads.campaigns",
    "adroll_ads.reporting_daily",
];
pub fn streams_for_profile(profile: StreamProfile) -> Vec<&'static AdRollStreamDef> {
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
) -> Vec<&'static AdRollStreamDef> {
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
    fn surface_is_adroll_specific() {
        assert_eq!(CURATED_STREAMS.len(), FULL_STREAM_COUNT);
        assert!(CURATED_STREAMS
            .iter()
            .any(|s| s.namespace == "adroll_ads.reporting_daily"));
        assert!(!CURATED_STREAMS
            .iter()
            .any(|s| s.namespace.contains("adset")));
    }
}
