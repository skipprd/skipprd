use std::collections::HashSet;

use serde::Deserialize;

/// Number of bronze namespaces in the full profile (single source of truth for tests/docs).
pub const FULL_STREAM_COUNT: usize = 23;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamProfile {
    /// Original four acquisition/event namespaces (dev/CI).
    Minimal,
    /// Full catalog minus ecommerce, publisher ads, and demographic breakdowns.
    Standard,
    /// All 23 daily fact grains (default).
    #[default]
    Full,
}

#[derive(Clone, Copy, Debug)]
pub struct Ga4StreamDef {
    pub namespace: &'static str,
    pub dimensions: &'static [&'static str],
    pub metrics: &'static [&'static str],
    /// When true, API errors for invalid dimension/metric combos skip this stream only.
    pub optional: bool,
}

const CONTENT_PAGE_METRICS: &[&str] = &[
    "screenPageViews",
    "sessions",
    "totalUsers",
    "engagementRate",
];
const DEMOGRAPHIC_METRICS: &[&str] = &["sessions", "totalUsers", "newUsers"];
const SESSION_METRICS: &[&str] = &["sessions", "totalUsers", "conversions"];

pub const CURATED_STREAMS: &[Ga4StreamDef] = &[
    // Acquisition and engagement (6)
    Ga4StreamDef {
        namespace: "google_analytics.traffic_acquisition_daily",
        dimensions: &[
            "date",
            "sessionDefaultChannelGroup",
            "sessionSource",
            "sessionMedium",
        ],
        metrics: SESSION_METRICS,
        optional: false,
    },
    Ga4StreamDef {
        namespace: "google_analytics.traffic_campaign_daily",
        dimensions: &[
            "date",
            "sessionCampaignName",
            "sessionSource",
            "sessionMedium",
        ],
        metrics: SESSION_METRICS,
        optional: false,
    },
    Ga4StreamDef {
        namespace: "google_analytics.user_acquisition_daily",
        dimensions: &[
            "date",
            "firstUserDefaultChannelGroup",
            "firstUserSource",
            "firstUserMedium",
        ],
        metrics: &["newUsers", "totalUsers"],
        optional: false,
    },
    Ga4StreamDef {
        namespace: "google_analytics.user_acquisition_campaign_daily",
        dimensions: &[
            "date",
            "firstUserCampaignName",
            "firstUserSource",
            "firstUserMedium",
        ],
        metrics: &["newUsers", "totalUsers"],
        optional: false,
    },
    Ga4StreamDef {
        namespace: "google_analytics.events_daily",
        dimensions: &["date", "eventName"],
        metrics: &["eventCount", "totalUsers"],
        optional: false,
    },
    Ga4StreamDef {
        namespace: "google_analytics.conversions_daily",
        dimensions: &["date", "eventName"],
        metrics: &["conversions", "totalRevenue"],
        optional: false,
    },
    // Audience (2)
    Ga4StreamDef {
        namespace: "google_analytics.audience_daily",
        dimensions: &["date"],
        metrics: &[
            "activeUsers",
            "newUsers",
            "sessions",
            "engagedSessions",
            "averageSessionDuration",
        ],
        optional: false,
    },
    Ga4StreamDef {
        namespace: "google_analytics.audience_retention_daily",
        dimensions: &["date"],
        metrics: &["active1DayUsers", "active7DayUsers", "active28DayUsers"],
        optional: false,
    },
    // Content (4)
    Ga4StreamDef {
        namespace: "google_analytics.content_pages_daily",
        dimensions: &["date", "pagePath"],
        metrics: CONTENT_PAGE_METRICS,
        optional: false,
    },
    Ga4StreamDef {
        namespace: "google_analytics.content_titles_daily",
        dimensions: &["date", "pageTitle"],
        metrics: CONTENT_PAGE_METRICS,
        optional: false,
    },
    Ga4StreamDef {
        namespace: "google_analytics.content_screens_daily",
        dimensions: &["date", "unifiedScreenClass"],
        metrics: CONTENT_PAGE_METRICS,
        optional: false,
    },
    Ga4StreamDef {
        namespace: "google_analytics.content_group_daily",
        dimensions: &["date", "contentGroup"],
        metrics: CONTENT_PAGE_METRICS,
        optional: false,
    },
    // Geo (1)
    Ga4StreamDef {
        namespace: "google_analytics.geo_daily",
        dimensions: &["date", "country", "region", "city"],
        metrics: DEMOGRAPHIC_METRICS,
        optional: false,
    },
    // Demographics (4)
    Ga4StreamDef {
        namespace: "google_analytics.demographics_age_daily",
        dimensions: &["date", "userAgeBracket"],
        metrics: DEMOGRAPHIC_METRICS,
        optional: false,
    },
    Ga4StreamDef {
        namespace: "google_analytics.demographics_gender_daily",
        dimensions: &["date", "userGender"],
        metrics: DEMOGRAPHIC_METRICS,
        optional: false,
    },
    Ga4StreamDef {
        namespace: "google_analytics.demographics_interest_daily",
        dimensions: &["date", "brandingInterest"],
        metrics: DEMOGRAPHIC_METRICS,
        optional: false,
    },
    Ga4StreamDef {
        namespace: "google_analytics.demographics_language_daily",
        dimensions: &["date", "language"],
        metrics: DEMOGRAPHIC_METRICS,
        optional: false,
    },
    // Tech (3)
    Ga4StreamDef {
        namespace: "google_analytics.tech_daily",
        dimensions: &["date", "deviceCategory", "operatingSystem", "browser"],
        metrics: &["sessions", "totalUsers"],
        optional: false,
    },
    Ga4StreamDef {
        namespace: "google_analytics.devices_daily",
        dimensions: &["date", "deviceCategory", "mobileDeviceModel"],
        metrics: &["sessions", "totalUsers"],
        optional: false,
    },
    Ga4StreamDef {
        namespace: "google_analytics.tech_platform_daily",
        dimensions: &["date", "platform", "deviceCategory"],
        metrics: &["sessions", "totalUsers"],
        optional: false,
    },
    // Ecommerce (2, optional — property may not have ecommerce)
    Ga4StreamDef {
        namespace: "google_analytics.ecommerce_items_daily",
        dimensions: &["date", "itemName"],
        metrics: &["itemsPurchased", "itemRevenue", "itemsAddedToCart"],
        optional: true,
    },
    Ga4StreamDef {
        namespace: "google_analytics.ecommerce_categories_daily",
        dimensions: &["date", "itemCategory"],
        metrics: &["itemsPurchased", "itemRevenue"],
        optional: true,
    },
    // Publisher ads (1, optional)
    Ga4StreamDef {
        namespace: "google_analytics.publisher_ads_daily",
        dimensions: &["date", "adSourceName", "adFormat", "adUnitName"],
        metrics: &[
            "publisherAdClicks",
            "publisherAdImpressions",
            "adUnitExposure",
        ],
        optional: true,
    },
];

const MINIMAL_NAMESPACES: &[&str] = &[
    "google_analytics.traffic_acquisition_daily",
    "google_analytics.user_acquisition_daily",
    "google_analytics.events_daily",
    "google_analytics.conversions_daily",
];

const STANDARD_EXCLUDED: &[&str] = &[
    "google_analytics.demographics_age_daily",
    "google_analytics.demographics_gender_daily",
    "google_analytics.demographics_interest_daily",
    "google_analytics.demographics_language_daily",
    "google_analytics.ecommerce_items_daily",
    "google_analytics.ecommerce_categories_daily",
    "google_analytics.publisher_ads_daily",
];

pub fn streams_for_profile(profile: StreamProfile) -> Vec<&'static Ga4StreamDef> {
    CURATED_STREAMS
        .iter()
        .filter(|stream| stream_in_profile(stream, profile))
        .collect()
}

fn stream_in_profile(stream: &Ga4StreamDef, profile: StreamProfile) -> bool {
    match profile {
        StreamProfile::Full => true,
        StreamProfile::Standard => !STANDARD_EXCLUDED.contains(&stream.namespace),
        StreamProfile::Minimal => MINIMAL_NAMESPACES.contains(&stream.namespace),
    }
}

pub fn resolve_streams(
    profile: StreamProfile,
    explicit: Option<Vec<String>>,
) -> Vec<&'static Ga4StreamDef> {
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
    fn full_profile_has_expected_count() {
        assert_eq!(
            streams_for_profile(StreamProfile::Full).len(),
            FULL_STREAM_COUNT
        );
        assert_eq!(CURATED_STREAMS.len(), FULL_STREAM_COUNT);
    }

    #[test]
    fn standard_profile_excludes_optional_subject_areas() {
        let streams = streams_for_profile(StreamProfile::Standard);
        assert_eq!(streams.len(), 16);
        for name in STANDARD_EXCLUDED {
            assert!(!streams.iter().any(|s| s.namespace == *name));
        }
    }

    #[test]
    fn minimal_profile_is_four_streams() {
        let streams = streams_for_profile(StreamProfile::Minimal);
        assert_eq!(streams.len(), 4);
    }

    #[test]
    fn explicit_streams_filter_overrides_profile() {
        let streams = resolve_streams(
            StreamProfile::Full,
            Some(vec!["google_analytics.events_daily".into()]),
        );
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].namespace, "google_analytics.events_daily");
    }
}
