//! Per-namespace source contracts for API/SaaS connectors.
//!
//! These describe logical keys, cursors, partition keys, and sink write policies.
//! They are independent of CDC but can supply `business_key_columns` when CDC is enabled.

use serde_derive::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

use crate::discover::PipelineMetadata;
use crate::plugins::cdc::NamespaceContract;
use crate::runtime_plugins::protocol::RuntimeSinkCapabilityDescriptor;

/// Dot-path segments for nested logical keys (e.g. `["user", "id"]`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FieldPath(pub Vec<String>);

impl FieldPath {
    pub fn single(column: impl Into<String>) -> Self {
        Self(vec![column.into()])
    }

    pub fn dotted(&self) -> String {
        self.0.join(".")
    }

    pub fn validate(&self) -> Result<(), SourceContractError> {
        if self.0.is_empty() {
            return Err(SourceContractError::EmptyFieldPath);
        }
        for segment in &self.0 {
            if segment.trim().is_empty() {
                return Err(SourceContractError::EmptyFieldPathSegment);
            }
        }
        Ok(())
    }
}

/// How a sink should materialize batches for a namespace.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WritePolicy {
    #[default]
    Append,
    MergeByKey,
    ReplaceTable,
    ReplacePartition,
}

/// Optional descriptive metadata; sinks do not branch on this.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceSemantics {
    EventStream,
    EntityState,
    MutableReport,
}

/// Declared extraction and landing semantics for one emitted namespace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceNamespaceContract {
    pub namespace: String,
    #[serde(default)]
    pub primary_key: Vec<FieldPath>,
    #[serde(default)]
    pub cursor: Option<FieldPath>,
    #[serde(default)]
    pub partition_key: Vec<FieldPath>,
    #[serde(default)]
    pub write_policy: WritePolicy,
    #[serde(default)]
    pub refresh_window: Option<u32>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub semantics: Option<SourceSemantics>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceContractError {
    EmptyNamespace,
    EmptyFieldPath,
    EmptyFieldPathSegment,
    MergeByKeyRequiresPrimaryKey,
    ReplacePartitionRequiresPartitionKey,
    DuplicateNamespace(String),
    SinkPolicyUnsupported(String),
}

/// Declared sink support for native write policies (from runtime manifest metadata).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SinkWritePolicySupport {
    pub supports_merge_by_key: bool,
    pub supports_replace_partition: bool,
    pub supports_replace_table: bool,
}

impl From<&RuntimeSinkCapabilityDescriptor> for SinkWritePolicySupport {
    fn from(value: &RuntimeSinkCapabilityDescriptor) -> Self {
        Self {
            supports_merge_by_key: value.supports_merge_by_key,
            supports_replace_partition: value.supports_replace_partition,
            supports_replace_table: value.supports_replace_table,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WritePolicyUnsupportedError {
    pub namespace: String,
    pub policy: WritePolicy,
    pub sink_name: String,
}

impl fmt::Display for WritePolicyUnsupportedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "sink '{}' does not support write policy {:?} for namespace '{}'",
            self.sink_name, self.policy, self.namespace
        )
    }
}

impl std::error::Error for WritePolicyUnsupportedError {}

/// Fail when the active sink cannot implement the namespace's declared write policy.
pub fn validate_write_policy_for_sink(
    contract: &SourceNamespaceContract,
    sink_name: &str,
    support: SinkWritePolicySupport,
) -> Result<(), WritePolicyUnsupportedError> {
    let unsupported = match contract.write_policy {
        WritePolicy::Append => false,
        WritePolicy::MergeByKey => !support.supports_merge_by_key,
        WritePolicy::ReplacePartition => !support.supports_replace_partition,
        WritePolicy::ReplaceTable => !support.supports_replace_table,
    };
    if unsupported {
        return Err(WritePolicyUnsupportedError {
            namespace: contract.namespace.clone(),
            policy: contract.write_policy,
            sink_name: sink_name.to_string(),
        });
    }
    Ok(())
}

impl fmt::Display for SourceContractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyNamespace => write!(f, "namespace must not be empty"),
            Self::EmptyFieldPath => write!(f, "field path must not be empty"),
            Self::EmptyFieldPathSegment => write!(f, "field path segment must not be empty"),
            Self::MergeByKeyRequiresPrimaryKey => {
                write!(f, "MergeByKey requires a non-empty primary_key")
            }
            Self::ReplacePartitionRequiresPartitionKey => {
                write!(f, "ReplacePartition requires a non-empty partition_key")
            }
            Self::DuplicateNamespace(ns) => write!(f, "duplicate namespace contract: {ns}"),
            Self::SinkPolicyUnsupported(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for SourceContractError {}

impl SourceNamespaceContract {
    pub fn validate(&self) -> Result<(), SourceContractError> {
        if self.namespace.trim().is_empty() {
            return Err(SourceContractError::EmptyNamespace);
        }
        for path in self
            .primary_key
            .iter()
            .chain(self.partition_key.iter())
            .chain(self.cursor.iter())
        {
            path.validate()?;
        }
        match self.write_policy {
            WritePolicy::MergeByKey if self.primary_key.is_empty() => {
                return Err(SourceContractError::MergeByKeyRequiresPrimaryKey);
            }
            WritePolicy::ReplacePartition if self.partition_key.is_empty() => {
                return Err(SourceContractError::ReplacePartitionRequiresPartitionKey);
            }
            _ => {}
        }
        Ok(())
    }

    /// Flat column names for CDC `business_key_columns` derivation.
    pub fn business_key_column_names(&self) -> Vec<String> {
        self.primary_key.iter().map(FieldPath::dotted).collect()
    }

    /// Build a CDC namespace contract from general source metadata when CDC is enabled.
    pub fn to_cdc_namespace_contract(
        &self,
        effective_guarantee: crate::plugins::cdc::EffectiveGuarantee,
        order_token_semantics: crate::plugins::cdc::OrderTokenSemantics,
        null_key_policy: crate::plugins::cdc::NullKeyPolicy,
        requires_skippr_system_columns: bool,
    ) -> NamespaceContract {
        NamespaceContract {
            namespace: self.namespace.clone(),
            business_key_columns: self.business_key_column_names(),
            effective_guarantee,
            order_token_semantics,
            null_key_policy,
            requires_skippr_system_columns,
        }
    }
}

/// Validate a batch of contracts (unique namespaces, per-contract rules).
pub fn validate_namespace_contracts(
    contracts: &[SourceNamespaceContract],
) -> Result<(), SourceContractError> {
    let mut seen = HashMap::new();
    for contract in contracts {
        contract.validate()?;
        if seen.insert(contract.namespace.clone(), ()).is_some() {
            return Err(SourceContractError::DuplicateNamespace(
                contract.namespace.clone(),
            ));
        }
    }
    Ok(())
}

/// Authoritatively replace pipeline source contracts with the provided set.
///
/// Namespaces not present in `contracts` are removed. Used for runtime `ContractsUpdate`
/// events that publish the complete contract set for the active source.
pub fn replace_source_contracts_authoritative(
    pipeline: &mut PipelineMetadata,
    contracts: impl IntoIterator<Item = SourceNamespaceContract>,
) -> Result<bool, SourceContractError> {
    let contracts: Vec<_> = contracts.into_iter().collect();
    validate_namespace_contracts(&contracts)?;
    let next: HashMap<String, SourceNamespaceContract> = contracts
        .into_iter()
        .map(|mut contract| {
            contract.namespace = crate::ingest_work::storage_namespace(&contract.namespace);
            (contract.namespace.clone(), contract)
        })
        .collect();
    if pipeline.source_contracts == next {
        return Ok(false);
    }
    pipeline.source_contracts = next;
    Ok(true)
}

/// Merge runtime-published contracts into pipeline metadata (in-memory).
///
/// Prefer [`replace_source_contracts_authoritative`] for runtime `ContractsUpdate` events.
pub fn merge_source_contracts_into_pipeline(
    pipeline: &mut PipelineMetadata,
    contracts: impl IntoIterator<Item = SourceNamespaceContract>,
) -> bool {
    replace_source_contracts_authoritative(pipeline, contracts).unwrap_or_else(|err| {
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

/// Validate that the configured output sink supports every contract write policy.
pub async fn validate_active_sink_supports_contracts(
    contracts: &[SourceNamespaceContract],
) -> Result<(), SourceContractError> {
    use crate::helpers::configuration::Config;
    use crate::runtime_plugins::discovery::resolve_runtime_plugin;
    use crate::runtime_plugins::protocol::RuntimePluginKind;

    if contracts.is_empty() {
        return Ok(());
    }
    validate_namespace_contracts(contracts)?;
    let output_plugin_name = Config::get_pipeline_output_plugin_name();
    let runtime_output_version = Config::get_pipeline_output_plugin_version().ok().flatten();
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

async fn persist_pipeline_metadata(pipeline: &PipelineMetadata) {
    use std::sync::Arc;

    use crate::helpers::configuration::Config;
    use crate::METADATA;

    METADATA.store(Arc::new(pipeline.clone()));
    Config::set_metadata(pipeline, false).await;
}

/// Authoritatively apply runtime-published contracts (empty vec clears all).
pub async fn apply_runtime_source_namespace_contracts(
    contracts: Vec<SourceNamespaceContract>,
) -> Result<bool, SourceContractError> {
    validate_active_sink_supports_contracts(&contracts).await?;

    let mut pipeline = crate::METADATA.load().as_ref().clone();
    let changed = replace_source_contracts_authoritative(&mut pipeline, contracts)?;
    if changed {
        persist_pipeline_metadata(&pipeline).await;
    }
    Ok(changed)
}

/// Resolve write policy for a namespace, using the persisted contract when the sink context omits it.
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

/// Fail when a non-append policy is declared for a namespace but no contract reached the sink.
pub fn ensure_source_contract_for_policy(
    namespace: &str,
    policy: WritePolicy,
    contract: Option<&SourceNamespaceContract>,
) -> Result<(), std::io::Error> {
    use std::io::{Error, ErrorKind};

    if policy == WritePolicy::Append {
        return Ok(());
    }
    if contract.is_some() {
        return Ok(());
    }
    if namespace_source_contract(namespace).is_some() {
        return Ok(());
    }
    Err(Error::new(
        ErrorKind::InvalidInput,
        format!(
            "namespace '{}' requires a source namespace contract for write policy {:?}",
            namespace, policy
        ),
    ))
}

/// Resolve the persisted contract for a namespace during compaction or sink writes.
pub fn namespace_source_contract(namespace: &str) -> Option<SourceNamespaceContract> {
    crate::METADATA
        .load()
        .source_contract_for_namespace(namespace)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_path_serializes_as_segment_array() {
        let path = FieldPath(vec!["property_id".into(), "date".into()]);
        let json = serde_json::to_value(&path).unwrap();
        assert_eq!(json, serde_json::json!(["property_id", "date"]));
        let roundtrip: FieldPath = serde_json::from_value(json).unwrap();
        assert_eq!(roundtrip, path);
    }

    #[test]
    fn write_policy_defaults_to_append_on_missing_field() {
        let contract: SourceNamespaceContract = serde_json::from_value(serde_json::json!({
            "namespace": "events",
        }))
        .unwrap();
        assert_eq!(contract.write_policy, WritePolicy::Append);
    }

    #[test]
    fn replace_partition_requires_partition_key() {
        let contract = SourceNamespaceContract {
            namespace: "reports".into(),
            primary_key: vec![FieldPath::single("id")],
            cursor: None,
            partition_key: vec![],
            write_policy: WritePolicy::ReplacePartition,
            refresh_window: Some(7),
            description: String::new(),
            semantics: Some(SourceSemantics::MutableReport),
        };
        assert_eq!(
            contract.validate().unwrap_err(),
            SourceContractError::ReplacePartitionRequiresPartitionKey
        );
    }

    #[test]
    fn merge_by_key_requires_primary_key() {
        let contract = SourceNamespaceContract {
            namespace: "users".into(),
            primary_key: vec![],
            cursor: None,
            partition_key: vec![],
            write_policy: WritePolicy::MergeByKey,
            refresh_window: None,
            description: String::new(),
            semantics: None,
        };
        assert_eq!(
            contract.validate().unwrap_err(),
            SourceContractError::MergeByKeyRequiresPrimaryKey
        );
    }

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
        let mut pipeline = crate::discover::PipelineMetadata::new();
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
        let changed = replace_source_contracts_authoritative(&mut pipeline, Vec::new()).unwrap();
        assert!(changed);
        assert!(pipeline.source_contracts.is_empty());
    }

    #[test]
    fn authoritative_replace_removes_stale_namespaces() {
        let mut pipeline = crate::discover::PipelineMetadata::new();
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

    #[test]
    fn write_policy_validation_fails_for_unsupported_sink() {
        let contract = SourceNamespaceContract {
            namespace: "orders".into(),
            primary_key: vec![FieldPath::single("id")],
            cursor: None,
            partition_key: vec![],
            write_policy: WritePolicy::MergeByKey,
            refresh_window: None,
            description: String::new(),
            semantics: None,
        };
        let err = validate_write_policy_for_sink(
            &contract,
            "Athena",
            SinkWritePolicySupport {
                supports_merge_by_key: false,
                supports_replace_partition: true,
                supports_replace_table: true,
            },
        )
        .unwrap_err();
        assert_eq!(err.policy, WritePolicy::MergeByKey);
    }

    #[test]
    fn business_key_columns_derived_from_primary_key() {
        let contract = SourceNamespaceContract {
            namespace: "orders".into(),
            primary_key: vec![
                FieldPath::single("property_id"),
                FieldPath(vec!["dimensions".into(), "date".into()]),
            ],
            cursor: Some(FieldPath::single("date")),
            partition_key: vec![FieldPath::single("date")],
            write_policy: WritePolicy::ReplacePartition,
            refresh_window: Some(3),
            description: "daily report".into(),
            semantics: Some(SourceSemantics::MutableReport),
        };
        contract.validate().unwrap();
        assert_eq!(
            contract.business_key_column_names(),
            vec!["property_id", "dimensions.date"]
        );
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
    fn validate_namespace_contracts_accepts_valid_batch() {
        let contracts = vec![
            sample_contract("a", WritePolicy::Append),
            sample_contract("b", WritePolicy::ReplacePartition),
        ];
        validate_namespace_contracts(&contracts).unwrap();
    }

    #[test]
    fn authoritative_replace_unchanged_returns_false() {
        let contract = sample_contract("ns", WritePolicy::Append);
        let mut pipeline = crate::discover::PipelineMetadata::new();
        replace_source_contracts_authoritative(&mut pipeline, vec![contract.clone()]).unwrap();
        let changed =
            replace_source_contracts_authoritative(&mut pipeline, vec![contract]).unwrap();
        assert!(!changed);
    }

    #[test]
    fn duplicate_namespace_in_batch_is_rejected() {
        let contracts = vec![
            sample_contract("dup", WritePolicy::Append),
            sample_contract("dup", WritePolicy::Append),
        ];
        assert!(matches!(
            validate_namespace_contracts(&contracts).unwrap_err(),
            SourceContractError::DuplicateNamespace(ref ns) if ns == "dup"
        ));
    }

    #[test]
    fn empty_namespace_is_rejected() {
        let contract = SourceNamespaceContract {
            namespace: "  ".into(),
            primary_key: vec![],
            cursor: None,
            partition_key: vec![],
            write_policy: WritePolicy::Append,
            refresh_window: None,
            description: String::new(),
            semantics: None,
        };
        assert_eq!(
            contract.validate().unwrap_err(),
            SourceContractError::EmptyNamespace
        );
    }

    #[test]
    fn empty_field_path_segment_is_rejected() {
        let path = FieldPath(vec!["ok".into(), "  ".into()]);
        assert_eq!(
            path.validate().unwrap_err(),
            SourceContractError::EmptyFieldPathSegment
        );
    }

    #[test]
    fn resolve_write_policy_prefers_context_contract() {
        let ctx = sample_contract("ctx", WritePolicy::ReplacePartition);
        let (policy, resolved) = resolved_write_policy_for_namespace("other", Some(&ctx));
        assert_eq!(policy, WritePolicy::ReplacePartition);
        assert_eq!(resolved.unwrap().namespace, "ctx");
    }

    #[test]
    fn ensure_source_contract_append_ok_without_contract() {
        ensure_source_contract_for_policy("ns", WritePolicy::Append, None).unwrap();
    }

    #[test]
    fn ensure_source_contract_replace_partition_requires_contract() {
        let err = ensure_source_contract_for_policy(
            "google_analytics.events_daily",
            WritePolicy::ReplacePartition,
            None,
        )
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("requires a source namespace contract"));
    }

    #[test]
    fn validate_append_always_supported() {
        let contract = sample_contract("events", WritePolicy::Append);
        validate_write_policy_for_sink(
            &contract,
            "Athena",
            SinkWritePolicySupport {
                supports_merge_by_key: false,
                supports_replace_partition: false,
                supports_replace_table: false,
            },
        )
        .unwrap();
    }

    #[test]
    fn validate_replace_table_unsupported_on_athena_manifest_defaults() {
        let contract = sample_contract("snapshot", WritePolicy::ReplaceTable);
        let err = validate_write_policy_for_sink(
            &contract,
            "Athena",
            SinkWritePolicySupport {
                supports_merge_by_key: false,
                supports_replace_partition: true,
                supports_replace_table: false,
            },
        )
        .unwrap_err();
        assert_eq!(err.policy, WritePolicy::ReplaceTable);
    }
}
