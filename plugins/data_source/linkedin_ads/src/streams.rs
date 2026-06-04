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
pub enum LinkedInStreamKind {
    AdAccounts,
    CampaignGroups,
    Campaigns,
    Creatives,
    AdAnalytics,
}

#[derive(Clone, Copy, Debug)]
pub struct LinkedInStreamDef {
    pub namespace: &'static str,
    pub kind: LinkedInStreamKind,
    pub primary_key: &'static [&'static str],
    pub cursor: Option<&'static str>,
}

pub const CURATED_STREAMS: &[LinkedInStreamDef] = &[
    LinkedInStreamDef {
        namespace: "linkedin_ads.ad_accounts",
        kind: LinkedInStreamKind::AdAccounts,
        primary_key: &["account_urn"],
        cursor: None,
    },
    LinkedInStreamDef {
        namespace: "linkedin_ads.campaign_groups",
        kind: LinkedInStreamKind::CampaignGroups,
        primary_key: &["campaign_group_urn"],
        cursor: None,
    },
    LinkedInStreamDef {
        namespace: "linkedin_ads.campaigns",
        kind: LinkedInStreamKind::Campaigns,
        primary_key: &["campaign_urn"],
        cursor: None,
    },
    LinkedInStreamDef {
        namespace: "linkedin_ads.creatives",
        kind: LinkedInStreamKind::Creatives,
        primary_key: &["creative_urn"],
        cursor: None,
    },
    LinkedInStreamDef {
        namespace: "linkedin_ads.ad_analytics_daily",
        kind: LinkedInStreamKind::AdAnalytics,
        primary_key: &["account_urn", "date", "pivot_value"],
        cursor: Some("date"),
    },
];
const MINIMAL_NAMESPACES: &[&str] = &["linkedin_ads.ad_analytics_daily"];
const STANDARD_NAMESPACES: &[&str] = &[
    "linkedin_ads.ad_accounts",
    "linkedin_ads.campaign_groups",
    "linkedin_ads.campaigns",
    "linkedin_ads.ad_analytics_daily",
];

pub fn streams_for_profile(profile: StreamProfile) -> Vec<&'static LinkedInStreamDef> {
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
) -> Vec<&'static LinkedInStreamDef> {
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
    fn contracts_cover_linkedin_marketing_surface() {
        assert_eq!(CURATED_STREAMS.len(), FULL_STREAM_COUNT);
        assert!(CURATED_STREAMS
            .iter()
            .any(|s| s.namespace == "linkedin_ads.campaign_groups"));
        assert!(CURATED_STREAMS
            .iter()
            .any(|s| s.namespace == "linkedin_ads.ad_analytics_daily"));
    }
}
