use std::collections::HashSet;

use serde::Deserialize;

/// Bronze namespaces in the full profile (excludes config-gated `url_inspection_daily`).
pub const FULL_STREAM_COUNT: usize = 9;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamProfile {
    /// `site_daily` only (discover / dev).
    Minimal,
    /// Core Search Analytics breakdowns.
    Standard,
    /// Full catalog including high-cardinality and sitemap snapshots.
    #[default]
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GscStreamKind {
    SearchAnalytics,
    SitemapSnapshot,
    SiteRunAggregate,
}

#[derive(Clone, Copy, Debug)]
pub struct GscStreamDef {
    pub namespace: &'static str,
    pub dimensions: &'static [&'static str],
    pub kind: GscStreamKind,
    /// When true, API errors for unsupported dimensions skip this stream only.
    pub optional: bool,
}

pub const CURATED_STREAMS: &[GscStreamDef] = &[
    GscStreamDef {
        namespace: "google_ads.site_daily",
        dimensions: &["date"],
        kind: GscStreamKind::SearchAnalytics,
        optional: false,
    },
    GscStreamDef {
        namespace: "google_ads.query_daily",
        dimensions: &["date", "query"],
        kind: GscStreamKind::SearchAnalytics,
        optional: false,
    },
    GscStreamDef {
        namespace: "google_ads.page_daily",
        dimensions: &["date", "page"],
        kind: GscStreamKind::SearchAnalytics,
        optional: false,
    },
    GscStreamDef {
        namespace: "google_ads.device_daily",
        dimensions: &["date", "device"],
        kind: GscStreamKind::SearchAnalytics,
        optional: false,
    },
    GscStreamDef {
        namespace: "google_ads.country_daily",
        dimensions: &["date", "country"],
        kind: GscStreamKind::SearchAnalytics,
        optional: false,
    },
    GscStreamDef {
        namespace: "google_ads.page_query_daily",
        dimensions: &["date", "page", "query"],
        kind: GscStreamKind::SearchAnalytics,
        optional: false,
    },
    GscStreamDef {
        namespace: "google_ads.search_appearance_daily",
        dimensions: &["date", "searchAppearance"],
        kind: GscStreamKind::SearchAnalytics,
        optional: true,
    },
    GscStreamDef {
        namespace: "google_ads.sitemap_daily",
        dimensions: &["date", "path"],
        kind: GscStreamKind::SitemapSnapshot,
        optional: false,
    },
    GscStreamDef {
        namespace: "google_ads.site_run_daily",
        dimensions: &["date"],
        kind: GscStreamKind::SiteRunAggregate,
        optional: false,
    },
];

pub const URL_INSPECTION_NAMESPACE: &str = "google_ads.url_inspection_daily";

const MINIMAL_NAMESPACES: &[&str] = &["google_ads.site_daily"];

const STANDARD_EXCLUDED: &[&str] = &[
    "google_ads.page_query_daily",
    "google_ads.search_appearance_daily",
    "google_ads.sitemap_daily",
    "google_ads.site_run_daily",
];

pub fn streams_for_profile(profile: StreamProfile) -> Vec<&'static GscStreamDef> {
    CURATED_STREAMS
        .iter()
        .filter(|stream| stream_in_profile(stream, profile))
        .collect()
}

fn stream_in_profile(stream: &GscStreamDef, profile: StreamProfile) -> bool {
    match profile {
        StreamProfile::Full => true,
        StreamProfile::Standard => !STANDARD_EXCLUDED.contains(&stream.namespace),
        StreamProfile::Minimal => MINIMAL_NAMESPACES.contains(&stream.namespace),
    }
}

pub fn resolve_streams(
    profile: StreamProfile,
    explicit: Option<Vec<String>>,
) -> Vec<&'static GscStreamDef> {
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
        assert_eq!(streams_for_profile(StreamProfile::Full).len(), FULL_STREAM_COUNT);
        assert_eq!(CURATED_STREAMS.len(), FULL_STREAM_COUNT);
    }

    #[test]
    fn standard_profile_is_five_streams() {
        let streams = streams_for_profile(StreamProfile::Standard);
        assert_eq!(streams.len(), 5);
        for name in STANDARD_EXCLUDED {
            assert!(!streams.iter().any(|s| s.namespace == *name));
        }
    }

    #[test]
    fn minimal_profile_is_site_daily_only() {
        let streams = streams_for_profile(StreamProfile::Minimal);
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].namespace, "google_ads.site_daily");
    }

    #[test]
    fn explicit_streams_filter_overrides_profile() {
        let streams = resolve_streams(
            StreamProfile::Full,
            Some(vec!["google_ads.query_daily".into()]),
        );
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].namespace, "google_ads.query_daily");
    }
}
