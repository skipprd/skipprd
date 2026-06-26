pub const NAMESPACE_SITE_RUN_DAILY: &str = "upfoundry_backlinks.site_run_daily";
pub const NAMESPACE_BACKLINK_DAILY: &str = "upfoundry_backlinks.backlink_daily";
pub const NAMESPACE_SUMMARY_DAILY: &str = "upfoundry_backlinks.summary_daily";
pub const NAMESPACE_REFERRING_DOMAIN_DAILY: &str = "upfoundry_backlinks.referring_domain_daily";
pub const NAMESPACE_ANCHOR_DAILY: &str = "upfoundry_backlinks.anchor_daily";
pub const NAMESPACE_HISTORY_DAILY: &str = "upfoundry_backlinks.history_daily";
pub const NAMESPACE_OUTBOUND_CONTEXT_DAILY: &str = "upfoundry_backlinks.outbound_context_daily";

/// Legacy DFS-only namespace; not emitted when writing unified `upfoundry_backlinks_*` tables.
pub const NAMESPACE_PAGE_INTERSECTION_DAILY: &str = "upfoundry_backlinks.page_intersection_daily";

pub const ALL_NAMESPACES: &[&str] = &[
    NAMESPACE_SITE_RUN_DAILY,
    NAMESPACE_BACKLINK_DAILY,
    NAMESPACE_SUMMARY_DAILY,
    NAMESPACE_REFERRING_DOMAIN_DAILY,
    NAMESPACE_ANCHOR_DAILY,
    NAMESPACE_HISTORY_DAILY,
    NAMESPACE_OUTBOUND_CONTEXT_DAILY,
];

pub const DFS_INGEST_NAMESPACES: &[&str] = &[
    NAMESPACE_SITE_RUN_DAILY,
    NAMESPACE_BACKLINK_DAILY,
    NAMESPACE_SUMMARY_DAILY,
    NAMESPACE_REFERRING_DOMAIN_DAILY,
    NAMESPACE_ANCHOR_DAILY,
    NAMESPACE_HISTORY_DAILY,
];
