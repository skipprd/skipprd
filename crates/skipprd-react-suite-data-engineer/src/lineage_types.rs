use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::providers::EvidenceStatus;

pub const LINEAGE_GRAPH_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[serde(transparent)]
pub struct LineageNodeId(pub String);

impl LineageNodeId {
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into().trim().to_string();
        if value.is_empty() {
            return None;
        }
        Some(Self(value))
    }

    pub fn generated(value: impl Into<String>) -> Self {
        Self::new(value).expect("generated lineage node id must be non-empty")
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for LineageNodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[serde(transparent)]
pub struct LineageEdgeId(pub String);

impl LineageEdgeId {
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into().trim().to_string();
        if value.is_empty() {
            return None;
        }
        Some(Self(value))
    }

    pub fn generated(value: impl Into<String>) -> Self {
        Self::new(value).expect("generated lineage edge id must be non-empty")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LineageNodeKind {
    RawSource,
    Pipeline,
    IngestTable,
    DbtSource,
    DbtModel,
    WarehouseTable,
    Field,
    Query,
    Dashboard,
    Metric,
    ExternalSystem,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LineageEdgeKind {
    ContainsField,
    Ingests,
    Materializes,
    SelectsFrom,
    FieldDerivesFrom,
    AggregatesFrom,
    JoinsOn,
    FiltersOn,
    Feeds,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LineageEvidenceSource {
    SkipprdMetadata,
    Catalog,
    DbtManifest,
    DbtPlanContract,
    WarehouseQueryHistory,
    Manual,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LineageDirection {
    Upstream,
    Downstream,
    Both,
}

impl Default for LineageDirection {
    fn default() -> Self {
        Self::Both
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LineageFieldRef {
    pub dataset_id: String,
    pub field_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field_id: Option<i32>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LineageProvenance {
    pub source: LineageEvidenceSource,
    pub status: EvidenceStatus,
    /// 0-100 deterministic confidence. `100` means observed or explicit.
    #[serde(default)]
    pub confidence: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at_epoch_secs: Option<u64>,
}

impl LineageProvenance {
    pub fn observed(source: LineageEvidenceSource, source_ref: impl Into<Option<String>>) -> Self {
        Self {
            source,
            status: EvidenceStatus::Observed,
            confidence: 100,
            source_ref: source_ref.into(),
            run_id: None,
            thread_id: None,
            observed_at_epoch_secs: now_epoch_secs(),
        }
    }

    pub fn unverified(
        source: LineageEvidenceSource,
        source_ref: impl Into<Option<String>>,
        confidence: u8,
    ) -> Self {
        Self {
            source,
            status: EvidenceStatus::Unverified,
            confidence: confidence.min(100),
            source_ref: source_ref.into(),
            run_id: None,
            thread_id: None,
            observed_at_epoch_secs: now_epoch_secs(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LineageNode {
    pub id: LineageNodeId,
    pub label: String,
    pub kind: LineageNodeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<LineageFieldRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LineageEdge {
    pub id: LineageEdgeId,
    pub from_node_id: LineageNodeId,
    pub to_node_id: LineageNodeId,
    pub kind: LineageEdgeKind,
    pub provenance: LineageProvenance,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LineageDiagnostic {
    pub severity: LineageDiagnosticSeverity,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<LineageEvidenceSource>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LineageDiagnosticSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LineageGraphSnapshot {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub built_at_epoch_secs: Option<u64>,
    #[serde(default)]
    pub nodes: Vec<LineageNode>,
    #[serde(default)]
    pub edges: Vec<LineageEdge>,
    #[serde(default)]
    pub diagnostics: Vec<LineageDiagnostic>,
}

impl Default for LineageGraphSnapshot {
    fn default() -> Self {
        Self {
            version: LINEAGE_GRAPH_VERSION,
            built_at_epoch_secs: now_epoch_secs(),
            nodes: Vec::new(),
            edges: Vec::new(),
            diagnostics: Vec::new(),
        }
    }
}

impl LineageGraphSnapshot {
    pub fn validate(&self) -> Result<(), String> {
        let mut node_ids = BTreeSet::new();
        for node in &self.nodes {
            if node.id.as_str().trim().is_empty() {
                return Err("lineage graph contains a node with an empty id".to_string());
            }
            if node.label.trim().is_empty() {
                return Err(format!("lineage node '{}' has an empty label", node.id));
            }
            if !node_ids.insert(node.id.clone()) {
                return Err(format!("duplicate lineage node id '{}'", node.id));
            }
            if let Some(field) = node.field.as_ref() {
                if field.dataset_id.trim().is_empty() || field.field_path.trim().is_empty() {
                    return Err(format!(
                        "lineage field node '{}' has an incomplete field reference",
                        node.id
                    ));
                }
            }
        }

        let mut edge_ids = BTreeSet::new();
        for edge in &self.edges {
            if edge.id.0.trim().is_empty() {
                return Err("lineage graph contains an edge with an empty id".to_string());
            }
            if !edge_ids.insert(edge.id.clone()) {
                return Err(format!("duplicate lineage edge id '{}'", edge.id.0));
            }
            if !node_ids.contains(&edge.from_node_id) {
                return Err(format!(
                    "lineage edge '{}' references missing from_node_id '{}'",
                    edge.id.0, edge.from_node_id
                ));
            }
            if !node_ids.contains(&edge.to_node_id) {
                return Err(format!(
                    "lineage edge '{}' references missing to_node_id '{}'",
                    edge.id.0, edge.to_node_id
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LineageResourceKind {
    Metadata,
    Config,
    SourceFile,
    StorageLocation,
    DatasetId,
    NodeId,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LineageResourceRef {
    pub kind: LineageResourceKind,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

pub const LINEAGE_META_RESOURCES: &str = "lineage_resources";

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LineageGraphQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field_node_id: Option<String>,
    #[serde(default)]
    pub direction: LineageDirection,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LineageRefreshResult {
    pub ok: bool,
    pub node_count: usize,
    pub edge_count: usize,
    pub diagnostic_count: usize,
    #[serde(default)]
    pub query_history: QueryHistoryImportSummary,
    pub graph: LineageGraphSnapshot,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct QueryHistoryImportSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_error: Option<String>,
    pub queries_seen: usize,
    pub queries_imported: usize,
    pub queries_skipped: usize,
    pub parse_warnings: usize,
}

pub fn now_epoch_secs() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
}

pub fn dataset_node_id(dataset_id: &str, kind: LineageNodeKind) -> LineageNodeId {
    let prefix = match kind {
        LineageNodeKind::RawSource => "raw",
        LineageNodeKind::Pipeline => "pipeline",
        LineageNodeKind::IngestTable => "ingest",
        LineageNodeKind::DbtSource => "dbt_source",
        LineageNodeKind::DbtModel => "dbt_model",
        LineageNodeKind::WarehouseTable => "warehouse",
        LineageNodeKind::Metric => "metric",
        LineageNodeKind::Query => "query",
        LineageNodeKind::Dashboard => "dashboard",
        LineageNodeKind::ExternalSystem => "external",
        LineageNodeKind::Field => "field",
    };
    LineageNodeId::generated(format!(
        "{}:{}",
        prefix,
        canonical_dataset_id_for_kind(dataset_id, &kind)
    ))
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CanonicalRelationId(String);

impl CanonicalRelationId {
    pub fn new(value: impl AsRef<str>) -> Option<Self> {
        let value = canonical_dataset_id(value.as_ref());
        (!value.is_empty()).then_some(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::fmt::Display for CanonicalRelationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CanonicalFieldPath(String);

impl CanonicalFieldPath {
    pub fn new(value: impl AsRef<str>) -> Option<Self> {
        let value = canonical_field_path(value.as_ref());
        (!value.is_empty()).then_some(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::fmt::Display for CanonicalFieldPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub fn field_node_id(dataset_id: &str, field_path: &str) -> LineageNodeId {
    LineageNodeId::generated(format!(
        "field:{}#{}",
        canonical_dataset_id(dataset_id),
        canonical_field_path(field_path)
    ))
}

pub fn canonical_dataset_id(dataset_id: &str) -> String {
    dataset_id
        .trim()
        .trim_matches('"')
        .split('.')
        .map(|part| part.trim().trim_matches('"').to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(".")
}

pub fn canonical_field_path(field_path: &str) -> String {
    field_path
        .trim()
        .split('.')
        .map(|part| {
            part.trim()
                .trim_matches('`')
                .trim_matches('"')
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_ascii_lowercase()
        })
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(".")
}

fn canonical_dataset_id_for_kind(dataset_id: &str, kind: &LineageNodeKind) -> String {
    match kind {
        LineageNodeKind::WarehouseTable
        | LineageNodeKind::IngestTable
        | LineageNodeKind::DbtSource => canonical_dataset_id(dataset_id),
        _ => dataset_id.trim().to_string(),
    }
}

pub fn edge_id(
    kind: LineageEdgeKind,
    from_node_id: &LineageNodeId,
    to_node_id: &LineageNodeId,
) -> LineageEdgeId {
    LineageEdgeId::generated(format!(
        "{:?}:{}->{}",
        kind,
        from_node_id.as_str(),
        to_node_id.as_str()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_missing_edge_endpoint() {
        let graph = LineageGraphSnapshot {
            nodes: vec![LineageNode {
                id: LineageNodeId::generated("a"),
                label: "A".to_string(),
                kind: LineageNodeKind::WarehouseTable,
                dataset_id: None,
                field: None,
                path: None,
                metadata: BTreeMap::new(),
            }],
            edges: vec![LineageEdge {
                id: LineageEdgeId::generated("e"),
                from_node_id: LineageNodeId::generated("a"),
                to_node_id: LineageNodeId::generated("b"),
                kind: LineageEdgeKind::SelectsFrom,
                provenance: LineageProvenance::observed(LineageEvidenceSource::Catalog, None),
                metadata: BTreeMap::new(),
            }],
            ..Default::default()
        };
        assert!(graph.validate().is_err());
    }

    #[test]
    fn generated_field_ids_are_stable() {
        assert_eq!(
            field_node_id("db.schema.orders", "order_id"),
            field_node_id("db.schema.orders", "order_id")
        );
    }

    #[test]
    fn dataset_ids_are_canonical_for_warehouse_relations() {
        assert_eq!(
            dataset_node_id("ANALYTICS.RAW.BIKE_HIRE", LineageNodeKind::WarehouseTable),
            dataset_node_id("analytics.raw.bike_hire", LineageNodeKind::WarehouseTable)
        );
        assert_eq!(
            field_node_id("ANALYTICS.RAW.BIKE_HIRE", "BIKE_ID"),
            field_node_id("analytics.raw.bike_hire", "BIKE_ID")
        );
    }
}
