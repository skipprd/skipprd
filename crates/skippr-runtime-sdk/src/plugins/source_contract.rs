//! Per-namespace source contracts for API/SaaS connectors.
//!
//! These describe logical keys, cursors, partition keys, and sink write policies.
//! They are independent of CDC but can supply `business_key_columns` when CDC is enabled.

use serde_derive::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

use crate::plugins::cdc::NamespaceContract;
use crate::protocol::RuntimeSinkCapabilityDescriptor;

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
    CdcContracts(String),
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
            Self::CdcContracts(msg) => write!(f, "{msg}"),
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

/// The only map from persisted source contracts into `resolve_business_keys`.
pub fn cdc_source_key_columns(
    source_contracts: &HashMap<String, SourceNamespaceContract>,
) -> HashMap<String, Vec<String>> {
    source_contracts
        .iter()
        .map(|(namespace, contract)| (namespace.clone(), contract.business_key_column_names()))
        .collect()
}

/// Fail closed when a non-append write has no contract on the request.
/// Child plugins must not look up host METADATA.
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
    Err(Error::new(
        ErrorKind::InvalidInput,
        format!(
            "namespace '{namespace}' requires a source namespace contract for write policy {policy:?}"
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cdc_source_key_columns_uses_primary_key_names() {
        let mut contracts = HashMap::new();
        contracts.insert(
            "orders".into(),
            SourceNamespaceContract {
                namespace: "orders".into(),
                primary_key: vec![FieldPath::single("src_id")],
                cursor: None,
                partition_key: vec![],
                write_policy: WritePolicy::Append,
                refresh_window: None,
                description: String::new(),
                semantics: None,
            },
        );
        let keys = cdc_source_key_columns(&contracts);
        assert_eq!(keys.get("orders").unwrap(), &vec!["src_id".to_string()]);
    }

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

    #[test]
    fn ensure_source_contract_append_ok_without_contract() {
        ensure_source_contract_for_policy("ns", WritePolicy::Append, None).unwrap();
    }

    #[test]
    fn ensure_source_contract_replace_partition_requires_request_contract() {
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
    fn ensure_source_contract_accepts_request_contract() {
        let contract = sample_contract("ns", WritePolicy::ReplacePartition);
        ensure_source_contract_for_policy("ns", WritePolicy::ReplacePartition, Some(&contract))
            .unwrap();
    }
}
