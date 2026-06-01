use skippr_runtime_sdk::plugins::source_contract::{
    FieldPath, SourceNamespaceContract, SourceSemantics, WritePolicy,
};

pub const NAMESPACE_RUN_DAILY: &str = "ai_citations.run_daily";
pub const NAMESPACE_PROMPT_RESPONSE_DAILY: &str = "ai_citations.prompt_response_daily";
pub const NAMESPACE_MENTION: &str = "ai_citations.mention";
pub const NAMESPACE_CITATION: &str = "ai_citations.citation";
pub const NAMESPACE_LINK: &str = "ai_citations.link";
pub const NAMESPACE_CHECK_DAILY: &str = "ai_citations.check_daily";

pub const NAMESPACE_COUNT: usize = 6;

pub const ALL_NAMESPACES: &[&str] = &[
    NAMESPACE_RUN_DAILY,
    NAMESPACE_PROMPT_RESPONSE_DAILY,
    NAMESPACE_MENTION,
    NAMESPACE_CITATION,
    NAMESPACE_LINK,
    NAMESPACE_CHECK_DAILY,
];

pub fn namespace_contract(namespace: &str) -> SourceNamespaceContract {
    let site = FieldPath::single("site");
    let run_date = FieldPath::single("run_date");
    let (primary_key, description) = match namespace {
        NAMESPACE_RUN_DAILY => (
            vec![site.clone(), run_date.clone()],
            "AI citations daily run summary",
        ),
        NAMESPACE_PROMPT_RESPONSE_DAILY => (
            vec![
                site.clone(),
                FieldPath::single("prompt_id"),
                FieldPath::single("model"),
                run_date.clone(),
            ],
            "AI model response per tracked prompt",
        ),
        NAMESPACE_MENTION => (
            vec![
                site.clone(),
                FieldPath::single("prompt_id"),
                FieldPath::single("model"),
                FieldPath::single("mention_id"),
                run_date.clone(),
            ],
            "Brand or alias mention span in model response",
        ),
        NAMESPACE_CITATION => (
            vec![
                site.clone(),
                FieldPath::single("prompt_id"),
                FieldPath::single("model"),
                FieldPath::single("citation_id"),
                run_date.clone(),
            ],
            "Cited source URL in model response",
        ),
        NAMESPACE_LINK => (
            vec![
                site.clone(),
                FieldPath::single("prompt_id"),
                FieldPath::single("model"),
                FieldPath::single("link_id"),
                run_date.clone(),
            ],
            "URL link extracted from model response",
        ),
        NAMESPACE_CHECK_DAILY => (
            vec![
                site.clone(),
                FieldPath::single("prompt_id"),
                FieldPath::single("model"),
                FieldPath::single("check_code"),
                run_date.clone(),
            ],
            "AI visibility check outcomes per prompt and model",
        ),
        other => panic!("unknown ai_citations namespace: {other}"),
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
