use skippr_runtime_sdk::plugins::source_contract::{
    FieldPath, SourceNamespaceContract, SourceSemantics, WritePolicy,
};

pub const NAMESPACE_TARGET_INDEX: &str = "cc_wat_source_pages_by_target_domain_index";
pub const NAMESPACE_RUN_DAILY: &str = "upfoundry_link_graph_wat_index.run_daily";
pub const NAMESPACE_AUDIT_SKIP: &str = "upfoundry_link_graph_wat_index.audit_skip";

pub fn all_namespace_contracts() -> Vec<SourceNamespaceContract> {
    vec![
        namespace_contract(NAMESPACE_TARGET_INDEX),
        namespace_contract(NAMESPACE_RUN_DAILY),
        namespace_contract(NAMESPACE_AUDIT_SKIP),
    ]
}

pub fn namespace_contract(namespace: &str) -> SourceNamespaceContract {
    match namespace {
        NAMESPACE_TARGET_INDEX => SourceNamespaceContract {
            namespace: namespace.to_string(),
            primary_key: vec![
                FieldPath::single("crawl_id"),
                FieldPath::single("target_domain_hash_bucket"),
                FieldPath::single("target_domain_id"),
                FieldPath::single("source_url_id"),
            ],
            cursor: Some(FieldPath::single("wat_path")),
            partition_key: vec![
                FieldPath::single("crawl_id"),
                FieldPath::single("target_domain_hash_bucket"),
            ],
            write_policy: WritePolicy::Append,
            refresh_window: None,
            description: "Common Crawl WAT source pages indexed by linked target domain".into(),
            semantics: Some(SourceSemantics::MutableReport),
        },
        NAMESPACE_RUN_DAILY => SourceNamespaceContract {
            namespace: namespace.to_string(),
            primary_key: vec![FieldPath::single("crawl_id"), FieldPath::single("run_date")],
            cursor: Some(FieldPath::single("run_date")),
            partition_key: vec![FieldPath::single("run_date")],
            write_policy: WritePolicy::ReplacePartition,
            refresh_window: None,
            description: "WAT index build run summary".into(),
            semantics: Some(SourceSemantics::MutableReport),
        },
        NAMESPACE_AUDIT_SKIP => SourceNamespaceContract {
            namespace: namespace.to_string(),
            primary_key: vec![
                FieldPath::single("crawl_id"),
                FieldPath::single("wat_path"),
                FieldPath::single("reason"),
            ],
            cursor: Some(FieldPath::single("run_date")),
            partition_key: vec![FieldPath::single("run_date")],
            write_policy: WritePolicy::Append,
            refresh_window: None,
            description: "WAT records skipped during index build".into(),
            semantics: Some(SourceSemantics::MutableReport),
        },
        _ => panic!("unknown namespace: {namespace}"),
    }
}
