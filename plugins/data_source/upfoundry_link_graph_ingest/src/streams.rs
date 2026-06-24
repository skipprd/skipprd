use skippr_runtime_sdk::plugins::source_contract::{
    FieldPath, SourceNamespaceContract, SourceSemantics, WritePolicy,
};

pub const NAMESPACE_SITE_RUN_DAILY: &str = "upfoundry_link_graph_ingest.site_run_daily";
pub const NAMESPACE_AUDIT_SKIP: &str = "upfoundry_link_graph_ingest.audit_skip";

pub fn all_namespace_contracts() -> Vec<SourceNamespaceContract> {
    vec![
        namespace_contract(NAMESPACE_SITE_RUN_DAILY),
        namespace_contract(NAMESPACE_AUDIT_SKIP),
    ]
}

pub fn namespace_contract(namespace: &str) -> SourceNamespaceContract {
    let run_date = FieldPath::single("run_date");
    let (primary_key, description) = match namespace {
        NAMESPACE_SITE_RUN_DAILY => (
            vec![FieldPath::single("cc_crawl_id"), run_date.clone()],
            "Global link graph ingest run rollup",
        ),
        NAMESPACE_AUDIT_SKIP => (
            vec![
                FieldPath::single("cc_crawl_id"),
                FieldPath::single("url"),
                run_date.clone(),
            ],
            "Skipped URL audit rows",
        ),
        _ => panic!("unknown namespace: {namespace}"),
    };
    SourceNamespaceContract {
        namespace: namespace.to_string(),
        primary_key,
        cursor: Some(run_date.clone()),
        partition_key: vec![run_date],
        write_policy: WritePolicy::ReplacePartition,
        refresh_window: None,
        description: description.into(),
        semantics: Some(SourceSemantics::MutableReport),
    }
}
