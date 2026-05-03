use serde::{Deserialize, Serialize};

/// Reserved dataset_id for the global semantic context artifact.
///
/// This is NOT a real dataset/table; it is a project-scope semantic summary inferred during preflight.
pub const GLOBAL_SEMANTIC_DATASET_ID: &str = "__global__";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStatus {
    Observed,
    UserProvided,
    Unverified,
    Contradicted,
}

impl EvidenceStatus {
    pub fn authoring_safe(&self) -> bool {
        matches!(self, Self::Observed | Self::UserProvided)
    }
}

#[derive(
    Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash, schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct ClaimId(pub String);

impl ClaimId {
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into().trim().to_string();
        if value.is_empty() {
            return None;
        }
        Some(Self(value))
    }

    pub fn generated(value: impl Into<String>) -> Self {
        Self::new(value).expect("generated claim id must be non-empty")
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for ClaimId {
    fn from(value: String) -> Self {
        Self::generated(value)
    }
}

impl From<&str> for ClaimId {
    fn from(value: &str) -> Self {
        Self::generated(value)
    }
}

impl std::fmt::Display for ClaimId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(
    Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SemanticClaimKind {
    CandidateKey,
    Relationship,
    Grain,
    NumericParse,
    TimeField,
    RowPreservation,
    AggregateSafety,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SemanticClaimRef {
    pub claim_id: ClaimId,
    pub kind: SemanticClaimKind,
    pub status: EvidenceStatus,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum StatsStatus {
    Collected,
    SchemaOnly,
    Failed { error: String },
}

impl Default for StatsStatus {
    fn default() -> Self {
        Self::SchemaOnly
    }
}

impl StatsStatus {
    pub fn observed_distribution(&self) -> bool {
        matches!(self, Self::Collected)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SemanticEvidenceProvenance {
    pub rule_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_sql_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl SemanticEvidenceProvenance {
    pub fn rule(rule_id: impl Into<String>) -> Self {
        Self {
            rule_id: rule_id.into(),
            probe_sql_hash: None,
            note: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProfileMetric {
    pub value: f64,
    #[serde(default)]
    pub exact: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FieldProfile {
    pub field_name: String,
    pub status: EvidenceStatus,
    #[serde(default)]
    pub stats_status: StatsStatus,
    #[serde(default)]
    pub total_count: Option<u64>,
    #[serde(default)]
    pub null_count: Option<u64>,
    #[serde(default)]
    pub approx_distinct_count: Option<u64>,
    #[serde(default)]
    pub distinct_count_exact: bool,
    #[serde(default)]
    pub null_ratio: Option<ProfileMetric>,
    #[serde(default)]
    pub distinct_ratio: Option<ProfileMetric>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct KeyCandidateProfile {
    pub claim_id: ClaimId,
    #[serde(default)]
    pub field_names: Vec<String>,
    pub status: EvidenceStatus,
    #[serde(default)]
    pub total_count: Option<u64>,
    #[serde(default)]
    pub null_count: Option<u64>,
    #[serde(default)]
    pub approx_distinct_count: Option<u64>,
    #[serde(default)]
    pub distinct_count_exact: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<SemanticEvidenceProvenance>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RelationshipCandidateProfile {
    pub claim_id: ClaimId,
    pub left_dataset_id: String,
    pub right_dataset_id: String,
    #[serde(default)]
    pub left_fields: Vec<String>,
    #[serde(default)]
    pub right_fields: Vec<String>,
    pub status: EvidenceStatus,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<SemanticEvidenceProvenance>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FieldClaimProfile {
    pub claim_id: ClaimId,
    pub field_name: String,
    pub status: EvidenceStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<SemanticEvidenceProvenance>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GrainCandidateProfile {
    pub claim_id: ClaimId,
    #[serde(default)]
    pub field_names: Vec<String>,
    pub status: EvidenceStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<SemanticEvidenceProvenance>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AggregateSafetyCandidateProfile {
    pub claim_id: ClaimId,
    #[serde(default)]
    pub field_names: Vec<String>,
    pub status: EvidenceStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<SemanticEvidenceProvenance>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RowPreservationCandidateProfile {
    pub claim_id: ClaimId,
    pub status: EvidenceStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<SemanticEvidenceProvenance>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DatasetProfile {
    pub dataset_id: String,
    #[serde(default)]
    pub row_count: Option<u64>,
    #[serde(default)]
    pub fields: Vec<FieldProfile>,
    #[serde(default)]
    pub key_candidates: Vec<KeyCandidateProfile>,
    #[serde(default)]
    pub relationship_candidates: Vec<RelationshipCandidateProfile>,
    #[serde(default)]
    pub numeric_parse_candidates: Vec<FieldClaimProfile>,
    #[serde(default)]
    pub time_field_candidates: Vec<FieldClaimProfile>,
    #[serde(default)]
    pub grain_candidates: Vec<GrainCandidateProfile>,
    #[serde(default)]
    pub aggregate_safety_candidates: Vec<AggregateSafetyCandidateProfile>,
    #[serde(default)]
    pub row_preservation_candidates: Vec<RowPreservationCandidateProfile>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SemanticProfile {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub built_at_epoch_secs: Option<u64>,
    #[serde(default)]
    pub dataset_profiles: Vec<DatasetProfile>,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum SemanticFieldRole {
    Id,
    Timestamp,
    Categorical,
    Metric,
    FreeText,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SemanticField {
    pub name: String,
    pub role: SemanticFieldRole,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FieldStatsLite {
    // Subset of stats safe and useful for catalog embedding
    pub total: u64,
    pub nulls: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_numeric: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_numeric: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_len: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_len: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approx_distinct: Option<u64>,
    #[serde(default)]
    pub distinct_count_exact: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub histogram_bins: Option<Vec<u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub histogram_min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub histogram_max: Option<f64>,
    pub last_updated_epoch_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DatasetStats {
    // Dataset-level summary counters and time window
    pub approx_total_rows: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub earliest_ts: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_ts: Option<i64>,
    // Quick map of null counts by field for convenience
    #[serde(default)]
    pub nulls_by_field: std::collections::HashMap<String, u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SemanticModel {
    /// Canonical dataset identifier: "<catalog>.<database>.<table>".
    pub dataset_id: String,
    #[serde(default)]
    pub catalog: String,
    #[serde(default)]
    pub database: String,
    #[serde(default)]
    pub table: String,
    pub fields: Vec<SemanticField>,
    #[serde(default)]
    pub dimensions: Vec<String>,
    #[serde(default)]
    pub metrics: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CatalogField {
    pub entity: String,
    pub name: String,
    /// Engine-native column type (e.g. Glue/Athena type string).
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub data_type: Option<String>,
    /// Top-level column name in the source table (when known).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_column: Option<String>,
    /// Field path for nested leaves (e.g. `context.session.id`) or the column name for scalars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field_path: Option<String>,
    /// High-level structure kind for this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structure_kind: Option<StructureKind>,
    /// Provider-agnostic structured access descriptor (not raw SQL).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_descriptor: Option<AccessDescriptor>,
    pub description: Option<String>,
    pub synonyms: Option<Vec<String>>,
    pub pii_sensitivity: Option<String>,
    pub units_or_format: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub stats_status: StatsStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<FieldStatsLite>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StructureKind {
    Scalar,
    NestedLeaf,
    Complex,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AccessDescriptor {
    /// A single top-level column name.
    ColumnName { name: String },
    /// A nested dereference path, represented as a root column and path segments.
    ///
    /// Example: root="context", path=["session","id"].
    NestedPath { root: String, path: Vec<String> },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DataCatalog {
    /// Canonical dataset identifier: "<catalog>.<database>.<table>".
    pub dataset_id: String,
    #[serde(default)]
    pub catalog: String,
    #[serde(default)]
    pub database: String,
    #[serde(default)]
    pub table: String,
    pub description: Option<String>,
    #[serde(default)]
    pub dimensions: Vec<String>,
    #[serde(default)]
    pub metrics: Vec<String>,
    pub fields: Vec<CatalogField>,
    // Shallow index of parent path -> immediate children for navigation (flat fields remain canonical)
    #[serde(default)]
    pub structure_index: std::collections::HashMap<String, Vec<String>>,
    // Dataset-level stats snapshot
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dataset_stats: Option<DatasetStats>,
    /// Epoch seconds when this catalog was built/refreshed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub built_at_epoch_secs: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct GlobalSemanticContext {
    /// Schema version.
    #[serde(default)]
    pub version: u32,
    /// Epoch seconds when this artifact was built/refreshed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub built_at_epoch_secs: Option<u64>,
    /// High-confidence likely audiences for this project/dataset collection.
    #[serde(default)]
    pub audiences: Vec<GlobalAudience>,
    /// High-confidence business context bullets.
    #[serde(default)]
    pub context_bullets: Vec<GlobalContextBullet>,
    /// Optional inferred logical groups of datasets and what they represent.
    #[serde(default)]
    pub dataset_groups: Vec<GlobalDatasetGroup>,
    /// Important assumptions and evidence gaps + suggested smallest probes.
    #[serde(default)]
    pub assumptions_and_gaps: Vec<GlobalAssumptionGap>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct GlobalAudience {
    pub audience: String,
    #[serde(default)]
    pub confidence: f32,
    #[serde(default)]
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct GlobalContextBullet {
    pub text: String,
    #[serde(default)]
    pub confidence: f32,
    #[serde(default)]
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct GlobalDatasetGroup {
    pub group_name: String,
    #[serde(default)]
    pub dataset_ids: Vec<String>,
    #[serde(default)]
    pub confidence: f32,
    #[serde(default)]
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct GlobalAssumptionGap {
    pub text: String,
    #[serde(default)]
    pub confidence: f32,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub suggested_probe: Option<String>,
}
