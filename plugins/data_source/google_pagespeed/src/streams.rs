pub const NAMESPACE_SITE_RUN_DAILY: &str = "google_pagespeed.site_run_daily";
pub const NAMESPACE_PAGE_DAILY: &str = "google_pagespeed.page_daily";
pub const NAMESPACE_FIELD_ORIGIN_DAILY: &str = "google_pagespeed.field_origin_daily";
pub const NAMESPACE_AUDIT_DAILY: &str = "google_pagespeed.audit_daily";
pub const NAMESPACE_ISSUE: &str = "google_pagespeed.issue";

pub const ALL_NAMESPACES: &[&str] = &[
    NAMESPACE_SITE_RUN_DAILY,
    NAMESPACE_PAGE_DAILY,
    NAMESPACE_FIELD_ORIGIN_DAILY,
    NAMESPACE_AUDIT_DAILY,
    NAMESPACE_ISSUE,
];

pub const NAMESPACE_COUNT: usize = ALL_NAMESPACES.len();
