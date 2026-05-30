//! Bronze namespace contracts for `seo_crawl.*`.

use skippr_runtime_sdk::plugins::source_contract::{
    FieldPath, SourceNamespaceContract, SourceSemantics, WritePolicy,
};

pub const NAMESPACE_SITE_RUN_DAILY: &str = "seo_crawl.site_run_daily";
pub const NAMESPACE_PAGE_DAILY: &str = "seo_crawl.page_daily";
pub const NAMESPACE_LINK_EDGE: &str = "seo_crawl.link_edge";
pub const NAMESPACE_ROBOTS_TXT: &str = "seo_crawl.robots_txt";
pub const NAMESPACE_SITEMAP_URL: &str = "seo_crawl.sitemap_url";
pub const NAMESPACE_ISSUE: &str = "seo_crawl.issue";
pub const NAMESPACE_CONTENT_BLOCK: &str = "seo_crawl.content_block";

pub const ALL_NAMESPACES: &[&str] = &[
    NAMESPACE_SITE_RUN_DAILY,
    NAMESPACE_PAGE_DAILY,
    NAMESPACE_LINK_EDGE,
    NAMESPACE_ROBOTS_TXT,
    NAMESPACE_SITEMAP_URL,
    NAMESPACE_ISSUE,
    NAMESPACE_CONTENT_BLOCK,
];

pub const NAMESPACE_COUNT: usize = ALL_NAMESPACES.len();

pub fn all_namespace_contracts() -> Vec<SourceNamespaceContract> {
    let contracts = vec![
        site_run_daily_contract(),
        page_daily_contract(),
        link_edge_contract(),
        robots_txt_contract(),
        sitemap_url_contract(),
        issue_contract(),
        content_block_contract(),
    ];
    for contract in &contracts {
        contract
            .validate()
            .expect("invalid seo_crawl namespace contract");
    }
    contracts
}

fn base_contract(namespace: &str, primary_key: Vec<FieldPath>) -> SourceNamespaceContract {
    SourceNamespaceContract {
        namespace: namespace.to_string(),
        primary_key,
        cursor: Some(FieldPath::single("crawl_date")),
        partition_key: vec![FieldPath::single("crawl_date")],
        write_policy: WritePolicy::ReplacePartition,
        refresh_window: None,
        description: "SEO crawl daily snapshot".into(),
        semantics: Some(SourceSemantics::MutableReport),
    }
}

pub fn site_run_daily_contract() -> SourceNamespaceContract {
    let mut c = base_contract(
        NAMESPACE_SITE_RUN_DAILY,
        vec![
            FieldPath::single("site"),
            FieldPath::single("crawl_date"),
        ],
    );
    c.description = "Per-run site crawl aggregate".into();
    c
}

pub fn page_daily_contract() -> SourceNamespaceContract {
    base_contract(
        NAMESPACE_PAGE_DAILY,
        vec![
            FieldPath::single("site"),
            FieldPath::single("canonical_url"),
            FieldPath::single("crawl_date"),
        ],
    )
}

pub fn link_edge_contract() -> SourceNamespaceContract {
    base_contract(
        NAMESPACE_LINK_EDGE,
        vec![
            FieldPath::single("site"),
            FieldPath::single("source_url"),
            FieldPath::single("target_url"),
            FieldPath::single("link_kind"),
            FieldPath::single("crawl_date"),
        ],
    )
}

pub fn robots_txt_contract() -> SourceNamespaceContract {
    base_contract(
        NAMESPACE_ROBOTS_TXT,
        vec![
            FieldPath::single("site"),
            FieldPath::single("crawl_date"),
        ],
    )
}

pub fn sitemap_url_contract() -> SourceNamespaceContract {
    base_contract(
        NAMESPACE_SITEMAP_URL,
        vec![
            FieldPath::single("site"),
            FieldPath::single("page_url"),
            FieldPath::single("sitemap_file"),
            FieldPath::single("crawl_date"),
        ],
    )
}

pub fn issue_contract() -> SourceNamespaceContract {
    base_contract(
        NAMESPACE_ISSUE,
        vec![
            FieldPath::single("site"),
            FieldPath::single("page_url"),
            FieldPath::single("issue_code"),
            FieldPath::single("crawl_date"),
        ],
    )
}

pub fn content_block_contract() -> SourceNamespaceContract {
    base_contract(
        NAMESPACE_CONTENT_BLOCK,
        vec![
            FieldPath::single("site"),
            FieldPath::single("page_url"),
            FieldPath::single("block_id"),
            FieldPath::single("crawl_date"),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_namespaces_have_contracts() {
        assert_eq!(all_namespace_contracts().len(), NAMESPACE_COUNT);
        assert_eq!(ALL_NAMESPACES.len(), NAMESPACE_COUNT);
    }
}
