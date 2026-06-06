use skippr_runtime_sdk::plugins::source_contract::{
    FieldPath, SourceNamespaceContract, SourceSemantics, WritePolicy,
};

pub const NAMESPACE_SITE_RUN_DAILY: &str = "seo_crawl.site_run_daily";
pub const NAMESPACE_PAGE_DAILY: &str = "seo_crawl.page_daily";
pub const NAMESPACE_LINK_EDGE: &str = "seo_crawl.link_edge";
pub const NAMESPACE_ROBOTS_TXT: &str = "seo_crawl.robots_txt";
pub const NAMESPACE_SITEMAP_URL: &str = "seo_crawl.sitemap_url";
pub const NAMESPACE_CHECK_DAILY: &str = "seo_crawl.check_daily";

pub const NAMESPACE_COUNT: usize = 6;

pub const ALL_NAMESPACES: &[&str] = &[
    NAMESPACE_SITE_RUN_DAILY,
    NAMESPACE_PAGE_DAILY,
    NAMESPACE_LINK_EDGE,
    NAMESPACE_ROBOTS_TXT,
    NAMESPACE_SITEMAP_URL,
    NAMESPACE_CHECK_DAILY,
];

pub fn namespace_contract(namespace: &str) -> SourceNamespaceContract {
    let site = FieldPath::single("site");
    let crawl_date = FieldPath::single("crawl_date");
    let (primary_key, partition_key) = match namespace {
        NAMESPACE_SITE_RUN_DAILY => (
            vec![site.clone(), crawl_date.clone()],
            vec![crawl_date.clone()],
        ),
        NAMESPACE_PAGE_DAILY => (
            vec![
                site.clone(),
                FieldPath::single("canonical_url"),
                crawl_date.clone(),
            ],
            vec![crawl_date.clone()],
        ),
        NAMESPACE_LINK_EDGE => (
            vec![
                site.clone(),
                FieldPath::single("source_url"),
                FieldPath::single("target_url"),
                FieldPath::single("link_kind"),
                crawl_date.clone(),
            ],
            vec![crawl_date.clone()],
        ),
        NAMESPACE_ROBOTS_TXT => (
            vec![site.clone(), crawl_date.clone()],
            vec![crawl_date.clone()],
        ),
        NAMESPACE_SITEMAP_URL => (
            vec![
                site.clone(),
                FieldPath::single("page_url"),
                FieldPath::single("sitemap_file"),
                crawl_date.clone(),
            ],
            vec![crawl_date.clone()],
        ),
        NAMESPACE_CHECK_DAILY => (
            vec![
                site.clone(),
                FieldPath::single("page_url"),
                FieldPath::single("issue_code"),
                crawl_date.clone(),
            ],
            vec![crawl_date.clone()],
        ),
        _ => panic!("unknown seo_crawl namespace: {namespace}"),
    };
    SourceNamespaceContract {
        namespace: namespace.to_string(),
        primary_key,
        cursor: Some(crawl_date.clone()),
        partition_key,
        write_policy: WritePolicy::ReplacePartition,
        refresh_window: None,
        description: "SEO technical crawl daily snapshot".into(),
        semantics: Some(SourceSemantics::MutableReport),
    }
}

pub fn all_namespace_contracts() -> Vec<SourceNamespaceContract> {
    ALL_NAMESPACES
        .iter()
        .map(|ns| namespace_contract(ns))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_namespaces_use_replace_partition() {
        for contract in all_namespace_contracts() {
            assert_eq!(contract.write_policy, WritePolicy::ReplacePartition);
            contract.validate().expect("valid contract");
        }
        assert_eq!(ALL_NAMESPACES.len(), NAMESPACE_COUNT);
    }
}
