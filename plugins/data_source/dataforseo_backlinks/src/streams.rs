pub const NAMESPACE_SITE_RUN_DAILY: &str = "dataforseo_backlinks.site_run_daily";
pub const NAMESPACE_BACKLINK_DAILY: &str = "dataforseo_backlinks.backlink_daily";
pub const NAMESPACE_PAGE_INTERSECTION_DAILY: &str =
    "dataforseo_backlinks.page_intersection_daily";

pub const ALL_NAMESPACES: &[&str] = &[
    NAMESPACE_SITE_RUN_DAILY,
    NAMESPACE_BACKLINK_DAILY,
    NAMESPACE_PAGE_INTERSECTION_DAILY,
];

pub const NAMESPACE_COUNT: usize = ALL_NAMESPACES.len();
