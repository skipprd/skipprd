use std::collections::HashSet;

use serde::Deserialize;

pub const FULL_STREAM_COUNT: usize = 6;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamProfile {
    Minimal,
    Standard,
    #[default]
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GoogleAdsStreamKind {
    Customer,
    Campaign,
    AdGroup,
    Keyword,
    SearchTerm,
    LandingPage,
}

#[derive(Clone, Copy, Debug)]
pub struct GoogleAdsStreamDef {
    pub namespace: &'static str,
    pub kind: GoogleAdsStreamKind,
    pub gaql: &'static str,
    pub primary_key: &'static [&'static str],
}

pub const CURATED_STREAMS: &[GoogleAdsStreamDef] = &[
    GoogleAdsStreamDef {
        namespace: "google_ads.account_daily",
        kind: GoogleAdsStreamKind::Customer,
        gaql: "SELECT segments.date, customer.id, customer.descriptive_name, metrics.impressions, metrics.clicks, metrics.cost_micros, metrics.conversions FROM customer WHERE segments.date BETWEEN '{start_date}' AND '{end_date}'",
        primary_key: &["customer_id", "date"],
    },
    GoogleAdsStreamDef {
        namespace: "google_ads.campaign_daily",
        kind: GoogleAdsStreamKind::Campaign,
        gaql: "SELECT segments.date, customer.id, campaign.id, campaign.name, campaign.status, metrics.impressions, metrics.clicks, metrics.cost_micros, metrics.conversions FROM campaign WHERE segments.date BETWEEN '{start_date}' AND '{end_date}'",
        primary_key: &["customer_id", "campaign_id", "date"],
    },
    GoogleAdsStreamDef {
        namespace: "google_ads.ad_group_daily",
        kind: GoogleAdsStreamKind::AdGroup,
        gaql: "SELECT segments.date, customer.id, campaign.id, ad_group.id, ad_group.name, ad_group.status, metrics.impressions, metrics.clicks, metrics.cost_micros, metrics.conversions FROM ad_group WHERE segments.date BETWEEN '{start_date}' AND '{end_date}'",
        primary_key: &["customer_id", "campaign_id", "ad_group_id", "date"],
    },
    GoogleAdsStreamDef {
        namespace: "google_ads.keyword_daily",
        kind: GoogleAdsStreamKind::Keyword,
        gaql: "SELECT segments.date, customer.id, campaign.id, ad_group.id, ad_group_criterion.criterion_id, ad_group_criterion.keyword.text, ad_group_criterion.keyword.match_type, metrics.impressions, metrics.clicks, metrics.cost_micros, metrics.conversions FROM keyword_view WHERE segments.date BETWEEN '{start_date}' AND '{end_date}'",
        primary_key: &["customer_id", "campaign_id", "ad_group_id", "criterion_id", "date"],
    },
    GoogleAdsStreamDef {
        namespace: "google_ads.search_term_daily",
        kind: GoogleAdsStreamKind::SearchTerm,
        gaql: "SELECT segments.date, customer.id, campaign.id, ad_group.id, search_term_view.search_term, metrics.impressions, metrics.clicks, metrics.cost_micros, metrics.conversions FROM search_term_view WHERE segments.date BETWEEN '{start_date}' AND '{end_date}'",
        primary_key: &["customer_id", "campaign_id", "ad_group_id", "search_term", "date"],
    },
    GoogleAdsStreamDef {
        namespace: "google_ads.landing_page_daily",
        kind: GoogleAdsStreamKind::LandingPage,
        gaql: "SELECT segments.date, customer.id, landing_page_view.unexpanded_final_url, metrics.impressions, metrics.clicks, metrics.cost_micros, metrics.conversions FROM landing_page_view WHERE segments.date BETWEEN '{start_date}' AND '{end_date}'",
        primary_key: &["customer_id", "landing_page_url", "date"],
    },
];

const MINIMAL_NAMESPACES: &[&str] = &["google_ads.account_daily"];
const STANDARD_NAMESPACES: &[&str] = &[
    "google_ads.account_daily",
    "google_ads.campaign_daily",
    "google_ads.ad_group_daily",
];

pub fn streams_for_profile(profile: StreamProfile) -> Vec<&'static GoogleAdsStreamDef> {
    CURATED_STREAMS
        .iter()
        .filter(|stream| match profile {
            StreamProfile::Full => true,
            StreamProfile::Standard => STANDARD_NAMESPACES.contains(&stream.namespace),
            StreamProfile::Minimal => MINIMAL_NAMESPACES.contains(&stream.namespace),
        })
        .collect()
}

pub fn resolve_streams(
    profile: StreamProfile,
    explicit: Option<Vec<String>>,
) -> Vec<&'static GoogleAdsStreamDef> {
    let selected: HashSet<String> = explicit.unwrap_or_default().into_iter().collect();
    if selected.is_empty() {
        return streams_for_profile(profile);
    }
    CURATED_STREAMS
        .iter()
        .filter(|s| selected.contains(s.namespace))
        .collect()
}

pub fn render_gaql(template: &str, start_date: &str, end_date: &str) -> String {
    template
        .replace("{start_date}", start_date)
        .replace("{end_date}", end_date)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn profile_counts_are_provider_specific() {
        assert_eq!(CURATED_STREAMS.len(), FULL_STREAM_COUNT);
        assert_eq!(
            streams_for_profile(StreamProfile::Minimal)[0].namespace,
            "google_ads.account_daily"
        );
        assert_eq!(streams_for_profile(StreamProfile::Standard).len(), 3);
    }
    #[test]
    fn gaql_templates_are_google_ads_not_search_console() {
        let q = render_gaql(CURATED_STREAMS[1].gaql, "2024-01-01", "2024-01-02");
        assert!(q.contains("FROM campaign"));
        assert!(q.contains("segments.date BETWEEN '2024-01-01' AND '2024-01-02'"));
    }
}
