use skippr_runtime_sdk::plugins::source_contract::{
    FieldPath, SourceNamespaceContract, SourceSemantics, WritePolicy,
};

pub const NAMESPACE_SITE_RUN_DAILY: &str = "content_quality.site_run_daily";
pub const NAMESPACE_PAGE_DAILY: &str = "content_quality.page_daily";
pub const NAMESPACE_CONTENT_BLOCK: &str = "content_quality.content_block";
pub const NAMESPACE_VECTOR_CHUNK: &str = "content_quality.vector_chunk";
pub const NAMESPACE_CHECK_DAILY: &str = "content_quality.check_daily";

pub const NAMESPACE_COUNT: usize = 5;

pub const ALL_NAMESPACES: &[&str] = &[
    NAMESPACE_SITE_RUN_DAILY,
    NAMESPACE_PAGE_DAILY,
    NAMESPACE_CONTENT_BLOCK,
    NAMESPACE_VECTOR_CHUNK,
    NAMESPACE_CHECK_DAILY,
];

pub fn namespace_contract(namespace: &str) -> SourceNamespaceContract {
    let site = FieldPath::single("site");
    let run_date = FieldPath::single("run_date");
    let (primary_key, partition_key) = match namespace {
        NAMESPACE_SITE_RUN_DAILY => (vec![site.clone(), run_date.clone()], vec![run_date.clone()]),
        NAMESPACE_PAGE_DAILY => (
            vec![
                site.clone(),
                FieldPath::single("canonical_url"),
                run_date.clone(),
            ],
            vec![run_date.clone()],
        ),
        NAMESPACE_CONTENT_BLOCK | NAMESPACE_VECTOR_CHUNK => (
            vec![
                site.clone(),
                FieldPath::single("page_url"),
                FieldPath::single("block_id"),
                run_date.clone(),
            ],
            vec![run_date.clone()],
        ),
        NAMESPACE_CHECK_DAILY => (
            vec![
                site.clone(),
                FieldPath::single("page_url"),
                FieldPath::single("issue_code"),
                run_date.clone(),
            ],
            vec![run_date.clone()],
        ),
        _ => panic!("unknown content_quality namespace: {namespace}"),
    };
    SourceNamespaceContract {
        namespace: namespace.to_string(),
        primary_key,
        cursor: Some(run_date.clone()),
        partition_key,
        write_policy: WritePolicy::ReplacePartition,
        refresh_window: None,
        description: "Content quality crawl daily snapshot".into(),
        semantics: Some(SourceSemantics::MutableReport),
    }
}

pub fn all_namespace_contracts() -> Vec<SourceNamespaceContract> {
    ALL_NAMESPACES
        .iter()
        .map(|ns| namespace_contract(ns))
        .collect()
}
