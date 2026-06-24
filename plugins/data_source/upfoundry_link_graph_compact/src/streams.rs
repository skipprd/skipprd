use skippr_runtime_sdk::plugins::source_contract::{
    FieldPath, SourceNamespaceContract, SourceSemantics, WritePolicy,
};

pub const NAMESPACE_COMPACT_RUN: &str = "upfoundry_link_graph_compact.compact_run_daily";

pub fn all_namespace_contracts() -> Vec<SourceNamespaceContract> {
    vec![namespace_contract(NAMESPACE_COMPACT_RUN)]
}

pub fn namespace_contract(namespace: &str) -> SourceNamespaceContract {
    let run_date = FieldPath::single("run_date");
    SourceNamespaceContract {
        namespace: namespace.to_string(),
        primary_key: vec![FieldPath::single("corpus_snapshot_id"), run_date.clone()],
        cursor: Some(run_date.clone()),
        partition_key: vec![run_date],
        write_policy: WritePolicy::ReplacePartition,
        refresh_window: None,
        description: "Compaction snapshot manifest summary".into(),
        semantics: Some(SourceSemantics::MutableReport),
    }
}
