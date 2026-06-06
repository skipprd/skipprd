use skippr_runtime_sdk::plugins::source_contract::{
    FieldPath, SourceNamespaceContract, SourceSemantics, WritePolicy,
};

pub const NAMESPACE_PAGE_LAB_DAILY: &str = "site_quality.page_lab_daily";
pub const NAMESPACE_SITE_RUN_DAILY: &str = "site_quality.site_run_daily";
pub const NAMESPACE_A11Y_ISSUE: &str = "site_quality.a11y_issue";
pub const NAMESPACE_CHECK_DAILY: &str = "site_quality.check_daily";
pub const NAMESPACE_LIGHTHOUSE_AUDIT: &str = "site_quality.lighthouse_audit";

pub const FULL_NAMESPACE_COUNT: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SiteQualityNamespace {
    pub namespace: &'static str,
    pub enabled_when_lighthouse: bool,
    pub enabled_when_axe: bool,
}

pub const ALL_NAMESPACES: &[SiteQualityNamespace] = &[
    SiteQualityNamespace {
        namespace: NAMESPACE_SITE_RUN_DAILY,
        enabled_when_lighthouse: false,
        enabled_when_axe: false,
    },
    SiteQualityNamespace {
        namespace: NAMESPACE_PAGE_LAB_DAILY,
        enabled_when_lighthouse: false,
        enabled_when_axe: false,
    },
    SiteQualityNamespace {
        namespace: NAMESPACE_A11Y_ISSUE,
        enabled_when_lighthouse: false,
        enabled_when_axe: true,
    },
    SiteQualityNamespace {
        namespace: NAMESPACE_CHECK_DAILY,
        enabled_when_lighthouse: false,
        enabled_when_axe: false,
    },
    SiteQualityNamespace {
        namespace: NAMESPACE_LIGHTHOUSE_AUDIT,
        enabled_when_lighthouse: true,
        enabled_when_axe: false,
    },
];

pub fn active_namespaces(lighthouse_enabled: bool, axe_enabled: bool) -> Vec<&'static str> {
    ALL_NAMESPACES
        .iter()
        .filter(|ns| {
            if ns.enabled_when_lighthouse && !lighthouse_enabled {
                return false;
            }
            if ns.enabled_when_axe && !axe_enabled {
                return false;
            }
            true
        })
        .map(|ns| ns.namespace)
        .collect()
}

pub fn namespace_contract(namespace: &str) -> SourceNamespaceContract {
    match namespace {
        NAMESPACE_SITE_RUN_DAILY => SourceNamespaceContract {
            namespace: namespace.to_string(),
            primary_key: vec![FieldPath::single("site"), FieldPath::single("run_date")],
            cursor: Some(FieldPath::single("run_date")),
            partition_key: vec![FieldPath::single("run_date")],
            write_policy: WritePolicy::ReplacePartition,
            refresh_window: None,
            description: "Site Quality daily run summary".into(),
            semantics: Some(SourceSemantics::MutableReport),
        },
        NAMESPACE_PAGE_LAB_DAILY => SourceNamespaceContract {
            namespace: namespace.to_string(),
            primary_key: vec![
                FieldPath::single("site"),
                FieldPath::single("canonical_url"),
                FieldPath::single("device_profile"),
                FieldPath::single("run_date"),
            ],
            cursor: Some(FieldPath::single("run_date")),
            partition_key: vec![FieldPath::single("run_date")],
            write_policy: WritePolicy::ReplacePartition,
            refresh_window: None,
            description: "Site Quality lab metrics per URL and device".into(),
            semantics: Some(SourceSemantics::MutableReport),
        },
        NAMESPACE_A11Y_ISSUE => SourceNamespaceContract {
            namespace: namespace.to_string(),
            primary_key: vec![
                FieldPath::single("site"),
                FieldPath::single("page_url"),
                FieldPath::single("rule_id"),
                FieldPath::single("device_profile"),
                FieldPath::single("run_date"),
            ],
            cursor: Some(FieldPath::single("run_date")),
            partition_key: vec![FieldPath::single("run_date")],
            write_policy: WritePolicy::ReplacePartition,
            refresh_window: None,
            description: "Site Quality axe accessibility violations".into(),
            semantics: Some(SourceSemantics::MutableReport),
        },
        NAMESPACE_CHECK_DAILY => SourceNamespaceContract {
            namespace: namespace.to_string(),
            primary_key: vec![
                FieldPath::single("site"),
                FieldPath::single("page_url"),
                FieldPath::single("issue_code"),
                FieldPath::single("device_profile"),
                FieldPath::single("run_date"),
            ],
            cursor: Some(FieldPath::single("run_date")),
            partition_key: vec![FieldPath::single("run_date")],
            write_policy: WritePolicy::ReplacePartition,
            refresh_window: None,
            description: "Site Quality check outcomes per URL and device".into(),
            semantics: Some(SourceSemantics::MutableReport),
        },
        NAMESPACE_LIGHTHOUSE_AUDIT => SourceNamespaceContract {
            namespace: namespace.to_string(),
            primary_key: vec![
                FieldPath::single("site"),
                FieldPath::single("page_url"),
                FieldPath::single("audit_id"),
                FieldPath::single("device_profile"),
                FieldPath::single("run_date"),
            ],
            cursor: Some(FieldPath::single("run_date")),
            partition_key: vec![FieldPath::single("run_date")],
            write_policy: WritePolicy::ReplacePartition,
            refresh_window: None,
            description: "Site Quality Lighthouse failing audits".into(),
            semantics: Some(SourceSemantics::MutableReport),
        },
        other => panic!("unknown site_quality namespace: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_catalog_has_five_namespaces() {
        assert_eq!(ALL_NAMESPACES.len(), FULL_NAMESPACE_COUNT);
    }

    #[test]
    fn minimal_active_set_without_lighthouse_or_axe() {
        let active = active_namespaces(false, false);
        assert_eq!(active.len(), 3);
        assert!(active.contains(&NAMESPACE_PAGE_LAB_DAILY));
        assert!(!active.contains(&NAMESPACE_LIGHTHOUSE_AUDIT));
        assert!(!active.contains(&NAMESPACE_A11Y_ISSUE));
    }
}
