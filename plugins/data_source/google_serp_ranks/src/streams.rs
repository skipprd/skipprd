use skippr_runtime_sdk::plugins::source_contract::{
    FieldPath, SourceNamespaceContract, SourceSemantics, WritePolicy,
};

pub const NAMESPACE_RUN_DAILY: &str = "google_serp_ranks.run_daily";
pub const NAMESPACE_TARGET_RANK_DAILY: &str = "google_serp_ranks.target_rank_daily";
pub const NAMESPACE_RESULT_DAILY: &str = "google_serp_ranks.result_daily";

pub fn active_namespaces(capture_results: bool) -> Vec<&'static str> {
    let mut out = vec![NAMESPACE_RUN_DAILY, NAMESPACE_TARGET_RANK_DAILY];
    if capture_results {
        out.push(NAMESPACE_RESULT_DAILY);
    }
    out
}

pub fn namespace_contract(namespace: &str) -> SourceNamespaceContract {
    match namespace {
        NAMESPACE_RUN_DAILY => SourceNamespaceContract {
            namespace: namespace.to_string(),
            primary_key: vec![
                FieldPath::single("keyword"),
                FieldPath::single("country"),
                FieldPath::single("language"),
                FieldPath::single("device"),
                FieldPath::single("run_date"),
            ],
            cursor: Some(FieldPath::single("run_date")),
            partition_key: vec![FieldPath::single("run_date")],
            write_policy: WritePolicy::ReplacePartition,
            refresh_window: None,
            description: "Google SERP rank query run summary".into(),
            semantics: Some(SourceSemantics::MutableReport),
        },
        NAMESPACE_TARGET_RANK_DAILY => SourceNamespaceContract {
            namespace: namespace.to_string(),
            primary_key: vec![
                FieldPath::single("keyword"),
                FieldPath::single("country"),
                FieldPath::single("language"),
                FieldPath::single("device"),
                FieldPath::single("target_site"),
                FieldPath::single("run_date"),
            ],
            cursor: Some(FieldPath::single("run_date")),
            partition_key: vec![FieldPath::single("run_date")],
            write_policy: WritePolicy::ReplacePartition,
            refresh_window: None,
            description: "Target site rank position per keyword and locale".into(),
            semantics: Some(SourceSemantics::MutableReport),
        },
        NAMESPACE_RESULT_DAILY => SourceNamespaceContract {
            namespace: namespace.to_string(),
            primary_key: vec![
                FieldPath::single("keyword"),
                FieldPath::single("country"),
                FieldPath::single("language"),
                FieldPath::single("device"),
                FieldPath::single("position"),
                FieldPath::single("run_date"),
            ],
            cursor: Some(FieldPath::single("run_date")),
            partition_key: vec![FieldPath::single("run_date")],
            write_policy: WritePolicy::ReplacePartition,
            refresh_window: None,
            description: "Organic SERP results captured during rank checks".into(),
            semantics: Some(SourceSemantics::MutableReport),
        },
        other => panic!("unknown google_serp_ranks namespace: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contracts_validate() {
        for ns in active_namespaces(true) {
            namespace_contract(ns).validate().expect("valid contract");
        }
    }

    #[test]
    fn result_namespace_optional() {
        let without = active_namespaces(false);
        assert_eq!(without.len(), 2);
        assert!(!without.contains(&NAMESPACE_RESULT_DAILY));
    }
}
