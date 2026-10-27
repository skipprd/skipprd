pub use skippr_runtime_sdk::plugins::source_contract::*;

use std::collections::HashMap;

use crate::discover::PipelineMetadata;
use crate::helpers::configuration::Config;

/// Authoritatively replace pipeline source contracts with the provided set.
pub fn replace_source_contracts_authoritative(
    config: &Config,
    pipeline: &mut PipelineMetadata,
    contracts: impl IntoIterator<Item = SourceNamespaceContract>,
) -> Result<bool, SourceContractError> {
    let contracts: Vec<_> = contracts.into_iter().collect();
    validate_namespace_contracts(&contracts)?;
    let next: HashMap<String, SourceNamespaceContract> = contracts
        .into_iter()
        .map(|mut contract| {
            contract.namespace = crate::ingest_work::storage_namespace(config, &contract.namespace);
            (contract.namespace.clone(), contract)
        })
        .collect();
    if pipeline.source_contracts == next {
        return Ok(false);
    }
    pipeline.source_contracts = next;
    Ok(true)
}

pub fn merge_source_contracts_into_pipeline(
    config: &Config,
    pipeline: &mut PipelineMetadata,
    contracts: impl IntoIterator<Item = SourceNamespaceContract>,
) -> bool {
    replace_source_contracts_authoritative(config, pipeline, contracts).unwrap_or_else(|err| {
        panic!("invalid source namespace contracts: {err}");
    })
}

impl PipelineMetadata {
    pub fn source_contract_for_namespace(
        &self,
        namespace: &str,
    ) -> Option<SourceNamespaceContract> {
        self.source_contracts.get(namespace).cloned()
    }
}

pub async fn validate_active_sink_supports_contracts(
    config: &Config,
    contracts: &[SourceNamespaceContract],
) -> Result<(), SourceContractError> {
    use crate::runtime_plugins::discovery::resolve_runtime_plugin;
    use skippr_runtime_sdk::protocol::RuntimePluginKind;

    if contracts.is_empty() {
        return Ok(());
    }
    validate_namespace_contracts(contracts)?;
    let output_plugin_name = config.get_pipeline_output_plugin_name();
    let runtime_output_version = config.get_pipeline_output_plugin_version().ok().flatten();
    let sink_manifest = resolve_runtime_plugin(
        RuntimePluginKind::DataSink,
        &output_plugin_name,
        runtime_output_version.as_deref(),
    )
    .await
    .map_err(|err| {
        SourceContractError::SinkPolicyUnsupported(format!(
            "failed to resolve output sink manifest for write-policy validation: {err}"
        ))
    })?;
    let sink_cap = sink_manifest.manifest.sink_capability.as_ref().ok_or_else(|| {
        SourceContractError::SinkPolicyUnsupported(format!(
            "output sink '{}' does not declare sink_capability metadata required for write-policy validation",
            output_plugin_name
        ))
    })?;
    let support = SinkWritePolicySupport::from(sink_cap);
    for contract in contracts {
        validate_write_policy_for_sink(contract, &sink_cap.name, support)
            .map_err(|err| SourceContractError::SinkPolicyUnsupported(err.to_string()))?;
    }
    Ok(())
}

async fn persist_pipeline_metadata(config: &Config, pipeline: &PipelineMetadata) {
    use std::sync::Arc;
    crate::METADATA.store(Arc::new(pipeline.clone()));
    config.set_metadata(pipeline, false).await;
}

pub async fn apply_runtime_source_namespace_contracts(
    config: &Config,
    contracts: Vec<SourceNamespaceContract>,
) -> Result<bool, SourceContractError> {
    validate_active_sink_supports_contracts(config, &contracts).await?;
    let mut pipeline = crate::METADATA.load().as_ref().clone();
    let changed = replace_source_contracts_authoritative(config, &mut pipeline, contracts)?;
    if changed {
        if config.pipeline_type().ok() == Some(crate::pipeline_type::PipelineType::Cdc) {
            let key = crate::pipeline_backend::pipeline_key(config)
                .map_err(SourceContractError::CdcContracts)?;
            let cdc = config.get_pipeline_config().cdc.clone().unwrap_or_default();
            let resolved =
                cdc.resolve_contracts(&cdc_source_key_columns(&pipeline.source_contracts));
            crate::plugins::cdc::reinstall_resolved_contracts(key, resolved)
                .map_err(|err| SourceContractError::CdcContracts(err.to_string()))?;
        }
        persist_pipeline_metadata(config, &pipeline).await;
    }
    Ok(changed)
}

pub fn resolved_write_policy_for_namespace(
    namespace: &str,
    ctx_contract: Option<&SourceNamespaceContract>,
) -> (WritePolicy, Option<SourceNamespaceContract>) {
    if let Some(contract) = ctx_contract {
        return (contract.write_policy, Some(contract.clone()));
    }
    if let Some(stored) = namespace_source_contract(namespace) {
        return (stored.write_policy, Some(stored));
    }
    (WritePolicy::Append, None)
}

pub fn ensure_source_contract_for_policy(
    namespace: &str,
    policy: WritePolicy,
    contract: Option<&SourceNamespaceContract>,
) -> Result<(), std::io::Error> {
    match skippr_runtime_sdk::plugins::source_contract::ensure_source_contract_for_policy(
        namespace, policy, contract,
    ) {
        Ok(()) => Ok(()),
        Err(err) => {
            if namespace_source_contract(namespace).is_some() {
                Ok(())
            } else {
                Err(err)
            }
        }
    }
}

pub fn namespace_source_contract(namespace: &str) -> Option<SourceNamespaceContract> {
    crate::METADATA
        .load()
        .source_contract_for_namespace(namespace)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::{metadata_test_lock, METADATA};

    #[test]
    fn persisted_metadata_without_contracts_deserializes() {
        let json = serde_json::json!({
            "name": "pipeline",
            "metadata": {},
            "enabled": true,
            "metadata_version": 2,
            "schema_id": 1
        });
        let pipeline: crate::discover::PipelineMetadata = serde_json::from_value(json).unwrap();
        assert!(pipeline.source_contracts.is_empty());
    }

    #[test]
    fn empty_authoritative_replace_clears_all_contracts() {
        let mut pipeline = crate::discover::PipelineMetadata::new(&Config::new());
        pipeline.source_contracts.insert(
            "stale".into(),
            SourceNamespaceContract {
                namespace: "stale".into(),
                primary_key: vec![FieldPath::single("id")],
                cursor: None,
                partition_key: vec![],
                write_policy: WritePolicy::Append,
                refresh_window: None,
                description: String::new(),
                semantics: None,
            },
        );
        let changed =
            replace_source_contracts_authoritative(&Config::new(), &mut pipeline, Vec::new())
                .unwrap();
        assert!(changed);
        assert!(pipeline.source_contracts.is_empty());
    }

    #[test]
    fn authoritative_replace_removes_stale_namespaces() {
        let mut pipeline = crate::discover::PipelineMetadata::new(&Config::new());
        pipeline.source_contracts.insert(
            "stale".into(),
            SourceNamespaceContract {
                namespace: "stale".into(),
                primary_key: vec![FieldPath::single("id")],
                cursor: None,
                partition_key: vec![],
                write_policy: WritePolicy::Append,
                refresh_window: None,
                description: String::new(),
                semantics: None,
            },
        );
        let changed = replace_source_contracts_authoritative(
            &Config::new(),
            &mut pipeline,
            vec![SourceNamespaceContract {
                namespace: "active".into(),
                primary_key: vec![FieldPath::single("id")],
                cursor: None,
                partition_key: vec![],
                write_policy: WritePolicy::Append,
                refresh_window: None,
                description: String::new(),
                semantics: None,
            }],
        )
        .unwrap();
        assert!(changed);
        assert!(!pipeline.source_contracts.contains_key("stale"));
        assert!(pipeline.source_contracts.contains_key("active"));
    }

    fn sample_contract(namespace: &str, policy: WritePolicy) -> SourceNamespaceContract {
        SourceNamespaceContract {
            namespace: namespace.into(),
            primary_key: vec![FieldPath::single("id")],
            cursor: None,
            partition_key: if policy == WritePolicy::ReplacePartition {
                vec![FieldPath::single("date")]
            } else {
                vec![]
            },
            write_policy: policy,
            refresh_window: None,
            description: String::new(),
            semantics: None,
        }
    }

    #[test]
    fn authoritative_replace_unchanged_returns_false() {
        let contract = sample_contract("ns", WritePolicy::Append);
        let mut pipeline = crate::discover::PipelineMetadata::new(&Config::new());
        replace_source_contracts_authoritative(
            &Config::new(),
            &mut pipeline,
            vec![contract.clone()],
        )
        .unwrap();
        let changed =
            replace_source_contracts_authoritative(&Config::new(), &mut pipeline, vec![contract])
                .unwrap();
        assert!(!changed);
    }

    #[test]
    fn resolve_write_policy_prefers_context_contract() {
        let ctx = sample_contract("ctx", WritePolicy::ReplacePartition);
        let (policy, resolved) = resolved_write_policy_for_namespace("other", Some(&ctx));
        assert_eq!(policy, WritePolicy::ReplacePartition);
        assert_eq!(resolved.unwrap().namespace, "ctx");
    }

    #[test]
    fn ensure_source_contract_replace_uses_host_metadata_when_request_has_none() {
        let _guard = metadata_test_lock();
        let contract = sample_contract(
            "google_analytics.events_daily",
            WritePolicy::ReplacePartition,
        );
        let mut pipeline = crate::discover::PipelineMetadata::new(&Config::new());
        pipeline
            .source_contracts
            .insert(contract.namespace.clone(), contract);
        METADATA.store(Arc::new(pipeline));
        ensure_source_contract_for_policy(
            "google_analytics.events_daily",
            WritePolicy::ReplacePartition,
            None,
        )
        .unwrap();
        METADATA.store(Arc::new(crate::discover::PipelineMetadata::new(
            &Config::new(),
        )));
    }
}
