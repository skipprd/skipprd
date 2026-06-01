use std::collections::HashSet;

use serde::Deserialize;

/// Bronze namespaces in the full profile (excludes config-gated extras).
pub const FULL_STREAM_COUNT: usize = 5;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamProfile {
    /// `site_daily` only (discover / dev).
    Minimal,
    /// Core daily reports.
    Standard,
    /// Full catalog including page snapshot and run aggregate.
    #[default]
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BingStreamKind {
    /// Daily rows with a `date` dimension from the API payload.
    DatedReport,
    /// Current page traffic snapshot partitioned by sync run date.
    PageSnapshot,
    /// Per-run sync aggregate.
    SiteRunAggregate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BingApiMethod {
    GetRankAndTrafficStats,
    GetQueryStats,
    GetPageStats,
    GetCrawlStats,
}

#[derive(Clone, Copy, Debug)]
pub struct BingStreamDef {
    pub namespace: &'static str,
    pub method: BingApiMethod,
    pub kind: BingStreamKind,
    /// Primary-key dimension field names beyond `site_url` and `date`.
    pub dimension_fields: &'static [&'static str],
}

pub const CURATED_STREAMS: &[BingStreamDef] = &[
    BingStreamDef {
        namespace: "bing_webmaster_tools.site_daily",
        method: BingApiMethod::GetRankAndTrafficStats,
        kind: BingStreamKind::DatedReport,
        dimension_fields: &[],
    },
    BingStreamDef {
        namespace: "bing_webmaster_tools.query_daily",
        method: BingApiMethod::GetQueryStats,
        kind: BingStreamKind::DatedReport,
        dimension_fields: &["Query"],
    },
    BingStreamDef {
        namespace: "bing_webmaster_tools.page_daily",
        method: BingApiMethod::GetPageStats,
        kind: BingStreamKind::PageSnapshot,
        dimension_fields: &["Url"],
    },
    BingStreamDef {
        namespace: "bing_webmaster_tools.crawl_daily",
        method: BingApiMethod::GetCrawlStats,
        kind: BingStreamKind::DatedReport,
        dimension_fields: &[],
    },
    BingStreamDef {
        namespace: "bing_webmaster_tools.site_run_daily",
        method: BingApiMethod::GetRankAndTrafficStats,
        kind: BingStreamKind::SiteRunAggregate,
        dimension_fields: &[],
    },
];

const MINIMAL_NAMESPACES: &[&str] = &["bing_webmaster_tools.site_daily"];

const STANDARD_EXCLUDED: &[&str] = &[
    "bing_webmaster_tools.page_daily",
    "bing_webmaster_tools.site_run_daily",
];

pub fn streams_for_profile(profile: StreamProfile) -> Vec<&'static BingStreamDef> {
    CURATED_STREAMS
        .iter()
        .filter(|stream| stream_in_profile(stream, profile))
        .collect()
}

fn stream_in_profile(stream: &BingStreamDef, profile: StreamProfile) -> bool {
    match profile {
        StreamProfile::Full => true,
        StreamProfile::Standard => !STANDARD_EXCLUDED.contains(&stream.namespace),
        StreamProfile::Minimal => MINIMAL_NAMESPACES.contains(&stream.namespace),
    }
}

pub fn resolve_streams(
    profile: StreamProfile,
    explicit: Option<Vec<String>>,
) -> Vec<&'static BingStreamDef> {
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
    fn standard_profile_is_three_streams() {
        let streams = streams_for_profile(StreamProfile::Standard);
        assert_eq!(streams.len(), 3);
        for name in STANDARD_EXCLUDED {
            assert!(!streams.iter().any(|s| s.namespace == *name));
        }
    }

    #[test]
    fn minimal_profile_is_site_daily_only() {
        let streams = streams_for_profile(StreamProfile::Minimal);
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].namespace, "bing_webmaster_tools.site_daily");
    }

    #[test]
    fn explicit_streams_filter_overrides_profile() {
        let streams = resolve_streams(
            StreamProfile::Full,
            Some(vec!["bing_webmaster_tools.query_daily".into()]),
        );
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].namespace, "bing_webmaster_tools.query_daily");
    }
}
