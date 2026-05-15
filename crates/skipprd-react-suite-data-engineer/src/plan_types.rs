use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Maximum number of `StrippedArtifact` entries held on a [`PlanSnapshot`]. The buffer is FIFO:
/// once full, the oldest entry is evicted on push. Keeping this small bounds prompt size when
/// the agent persistently authors stripped content.
pub const MAX_STRIPPED_ARTIFACTS: usize = 5;

/// One LLM-authored entry that the sanitizer removed from a system-shared file.
///
/// The sanitizer emits a `StrippedArtifact` each time it strips a top-level key from
/// `dbt_project.yml` (or any future `Shared` file). The artifact is persisted on the active
/// plan's [`PlanSnapshot::stripped_artifacts`] buffer and rendered into the author/repair prompt
/// on the next turn, so the agent can decide whether to re-author the intent in a sanctioned
/// location — see [`crate::file_ownership`] for the source-of-truth ownership table.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrippedArtifact {
    /// Relative path within the dbt project (e.g. `dbt_project.yml`).
    pub file: String,
    /// Nested key path that was removed. For top-level strips this is a single-element vec
    /// (e.g. `["on-run-start"]`).
    pub key_path: Vec<String>,
    /// Truncated YAML of the removed value, suitable for rendering verbatim into the prompt.
    pub value_summary: String,
    /// Human-readable rationale shown to the agent; comes from
    /// [`crate::file_ownership`]'s key tables.
    pub reason: String,
    /// Optional relocation hint shown to the agent; comes from
    /// [`crate::file_ownership::relocation_hint`]. `None` means "no sanctioned alternative
    /// location" and the prompt renders the corresponding fallback line.
    pub relocation_hint: Option<String>,
}

/// Typed container for `Plan.project_snapshot`. Known fields are directly
/// accessible; all other audit/diagnostic data falls through to `extra`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PlanSnapshot {
    #[serde(default)]
    pub pruned_task_count: Option<usize>,
    #[serde(default)]
    pub validate_fail_facts: Vec<crate::facts::FactsBundle>,
    /// Bounded FIFO buffer (cap = [`MAX_STRIPPED_ARTIFACTS`]) of content the sanitizer removed
    /// from system-shared files since the last author turn. The author/repair prompts read this
    /// to surface a "stripped-content" notice so intent is not silently lost.
    #[serde(default)]
    pub stripped_artifacts: Vec<StrippedArtifact>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

impl PlanSnapshot {
    /// Convenience: insert a key-value pair into the `extra` bag.
    pub fn insert(&mut self, key: impl Into<String>, value: Value) {
        self.extra.insert(key.into(), value);
    }

    /// Construct from a JSON Value (deserializes into typed + extra).
    pub fn from_value(v: Value) -> Self {
        serde_json::from_value(v).unwrap_or_default()
    }

    /// Push a stripped-artifact entry, evicting the oldest if the buffer is at capacity.
    pub fn push_stripped_artifact(&mut self, artifact: StrippedArtifact) {
        self.stripped_artifacts.push(artifact);
        while self.stripped_artifacts.len() > MAX_STRIPPED_ARTIFACTS {
            self.stripped_artifacts.remove(0);
        }
    }

    /// Push multiple stripped-artifact entries in order. Evicts oldest entries past capacity.
    pub fn extend_stripped_artifacts<I: IntoIterator<Item = StrippedArtifact>>(
        &mut self,
        artifacts: I,
    ) {
        for a in artifacts {
            self.push_stripped_artifact(a);
        }
    }
}

/// Compact column definition captured from the catalog at enrichment time.
/// Stored on each task so the plan explicitly records what source columns it was
/// built against — auditable, persistent, and available to authoring/review
/// without re-fetching.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourceColumnDef {
    pub name: String,
    pub data_type: String,
}

/// Map from dataset_id (or staging model name) to its column list.
/// Required wherever the planning pipeline needs column context — making
/// "enrichment without column schemas" a compile error.
pub type SourceSchema = BTreeMap<String, Vec<SourceColumnDef>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Draft,
    Approved,
    Completed,
    Cancelled,
}

impl Default for PlanStatus {
    fn default() -> Self {
        PlanStatus::Draft
    }
}

impl PlanStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, PlanStatus::Completed | PlanStatus::Cancelled)
    }

    pub fn try_approve(self) -> Result<PlanStatus, String> {
        match self {
            PlanStatus::Draft => Ok(PlanStatus::Approved),
            other => Err(format!("cannot approve plan in {:?} state", other)),
        }
    }

    pub fn try_cancel(self) -> Result<PlanStatus, String> {
        if self.is_terminal() {
            return Err(format!("cannot cancel plan in terminal {:?} state", self));
        }
        Ok(PlanStatus::Cancelled)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Done,
    Blocked,
    /// Task was completed but requires correction / updates before the plan can proceed.
    NeedsUpdate,
}

impl Default for TaskStatus {
    fn default() -> Self {
        TaskStatus::Pending
    }
}

/// Trait abstracting over cleanse/model task types so `Plan<T>` can be generic.
pub trait PlanTask:
    Clone + std::fmt::Debug + Serialize + serde::de::DeserializeOwned + Send + Sync
{
    fn task_id(&self) -> &str;
    fn expected_model_path(&self) -> Option<&str>;
    fn set_status(&mut self, status: TaskStatus);
    fn checklist(&self) -> &[PlanChecklistItem];
    fn checklist_mut(&mut self) -> &mut Vec<PlanChecklistItem>;
    /// Prefix used when generating canonical work_group IDs (e.g. "cleanse", "model").
    fn work_group_prefix() -> &'static str;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlanProgress {
    /// Thread step index cursor: when updating from the thread log, only consider steps after this
    /// index (exclusive). This keeps progress updates idempotent and cheap.
    #[serde(default)]
    pub last_applied_step_idx: usize,

    /// Consecutive failed batch attempts (resets on a fully successful attempt).
    /// Used to avoid infinite remediation loops in plan-batched authoring.
    #[serde(default)]
    pub consecutive_batch_failures: usize,

    /// Total failed batch attempts across the plan lifetime (diagnostics only).
    #[serde(default)]
    pub total_batch_failures: usize,

    /// Consecutive infra_transient failures (resets on success). When this
    /// reaches `MAX_CONSECUTIVE_INFRA_TRANSIENT`, the failure is promoted to a
    /// regular batch failure so the batch-lock circuit-breaker can fire.
    #[serde(default)]
    pub consecutive_infra_transient_failures: usize,
}

impl Default for PlanProgress {
    fn default() -> Self {
        Self {
            last_applied_step_idx: 0,
            consecutive_batch_failures: 0,
            total_batch_failures: 0,
            consecutive_infra_transient_failures: 0,
        }
    }
}

/// Audit trail for plan mutations (repairs, pruning, canonicalization).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanMutation {
    pub ts: String,
    pub reason_code: MutationReasonCode,
    /// Additional structured detail (best-effort; keep small).
    #[serde(default)]
    pub detail: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecklistOrigin {
    Initial,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecklistItemStatus {
    Pending,
    InProgress,
    Done,
    Blocked,
    NeedsUpdate,
}

impl Default for ChecklistItemStatus {
    fn default() -> Self {
        ChecklistItemStatus::Pending
    }
}

// TODO(item-91): Convert ChecklistEvidence.kind from bare String to a typed enum once all
// concrete kind values are catalogued. Values are currently passed as free-form strings via
// plan_progress::make_checklist_evidence.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChecklistEvidence {
    pub kind: String,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_id: Option<String>,
    pub step_idx: usize,
    #[serde(default)]
    pub ts: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanChecklistItem {
    pub checklist_item_id: String,
    pub label: String,
    #[serde(default)]
    pub details: Option<String>,
    #[serde(default)]
    pub status: ChecklistItemStatus,
    pub origin: ChecklistOrigin,
    #[serde(default)]
    pub evidence: Vec<ChecklistEvidence>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkGroupKind {
    AuthorSql,
    AuthorSchema,
    Validate,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkGroupItemRef {
    pub task_id: String,
    pub checklist_item_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanWorkGroup {
    pub group_id: String,
    pub label: String,
    pub kind: WorkGroupKind,
    pub items: Vec<WorkGroupItemRef>,
    #[serde(default)]
    pub depends_on_group_ids: Option<Vec<String>>,
}

// TODO(item-90): Convert MutationReasonCode from a string newtype to an enum with known
// variants + `Other(String)` fallback once the concrete reason codes are stabilized.
// Currently only constructed via deserialization; audit JSON payloads for known values first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MutationReasonCode(pub String);

// -----------------------
// Design-first plan spec
// -----------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelFolder {
    Marts,
    Core,
}

impl Default for ModelFolder {
    fn default() -> Self {
        ModelFolder::Marts
    }
}

impl ModelFolder {
    pub fn as_str(self) -> &'static str {
        match self {
            ModelFolder::Marts => "marts",
            ModelFolder::Core => "core",
        }
    }
}

impl std::fmt::Display for ModelFolder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JoinType {
    Inner,
    Left,
    Right,
    Full,
}

impl std::fmt::Display for JoinType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JoinType::Inner => f.write_str("inner"),
            JoinType::Left => f.write_str("left"),
            JoinType::Right => f.write_str("right"),
            JoinType::Full => f.write_str("full"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Cardinality {
    ManyToOne,
    OneToMany,
    OneToOne,
    ManyToMany,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FieldKind {
    Raw,
    Clean,
    Derived,
    QualityFlag,
}

/// Canonical `lineage[].role` values when `lineage_kind` is `column` (plain strings — not a
/// JSON-Schema `oneOf` enum — for OpenAI strict structured output).
pub mod lineage_role {
    pub const PASSTHROUGH: &str = "passthrough";
    pub const NORMALIZED: &str = "normalized";
    pub const PARSED: &str = "parsed";
    pub const DERIVED_INPUT: &str = "derived_input";
    pub const QUALITY_INPUT: &str = "quality_input";
}

/// JSON / OpenAI structured-output values for [`FieldLineage::lineage_kind`].
pub mod field_lineage_kind {
    pub const COLUMN: &str = "column";
    pub const SYSTEM: &str = "system";
    pub const CONSTANT: &str = "constant";
}

/// Identifies a column on an upstream relation. For cleanse tasks `relation` is always omitted.
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceFieldRef {
    /// Grounded model input name (e.g. `stg_orders`) when the task has multiple inputs.
    #[serde(default)]
    #[schemars(with = "String")]
    pub relation: Option<String>,
    /// Warehouse-reported column name on that relation (may contain dots for nested paths).
    pub name: String,
}

/// One mapping row in `output_fields[].lineage`. `lineage_kind` is a plain string (not a JSON-Schema
/// `oneOf` enum) so strict OpenAI structured-output schemas stay compatible.
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FieldLineage {
    pub lineage_kind: String,
    /// Set when `lineage_kind` is `column`.
    #[serde(default)]
    pub source: Option<SourceFieldRef>,
    /// Set when `lineage_kind` is `column` (snake_case string: `passthrough`, `normalized`, `parsed`,
    /// `derived_input`, `quality_input` — not a JSON-schema enum, for OpenAI strict mode).
    #[serde(default)]
    pub role: Option<String>,
    /// Set when `lineage_kind` is `system` (stable key for tooling/prompts).
    #[serde(default)]
    pub system_key: Option<String>,
    /// Set when `lineage_kind` is `constant` (literal value contract).
    #[serde(default)]
    pub constant_value: Option<String>,
}

impl FieldLineage {
    #[allow(dead_code)]
    pub fn column(source: SourceFieldRef, role: impl Into<String>) -> Self {
        Self {
            lineage_kind: field_lineage_kind::COLUMN.to_string(),
            source: Some(source),
            role: Some(role.into()),
            system_key: None,
            constant_value: None,
        }
    }

    #[allow(dead_code)]
    pub fn system(system_key: impl Into<String>) -> Self {
        Self {
            lineage_kind: field_lineage_kind::SYSTEM.to_string(),
            source: None,
            role: None,
            system_key: Some(system_key.into()),
            constant_value: None,
        }
    }

    #[allow(dead_code)]
    pub fn constant(constant_value: impl Into<String>) -> Self {
        Self {
            lineage_kind: field_lineage_kind::CONSTANT.to_string(),
            source: None,
            role: None,
            system_key: None,
            constant_value: Some(constant_value.into()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutputFieldSpec {
    /// Output column name.
    pub name: String,
    pub kind: FieldKind,
    /// Explicit lineage: every output field MUST have at least one entry.
    #[serde(default)]
    pub lineage: Vec<FieldLineage>,
    /// A concise, imperative expression or transformation contract.
    /// This is not required to be dialect-perfect SQL; it is the design contract that authoring
    /// should implement faithfully.
    pub expression: String,
    /// Optional intended type (best-effort). Prefer empty over guessing.
    #[serde(default)]
    pub data_type: Option<String>,
    /// Whether the field is allowed to be NULL in the output.
    #[serde(default)]
    pub nullable: bool,
    /// Optional one-line meaning / usage guidance.
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CleanseImplementationSpec {
    /// Schema version for the spec itself (not the overall plan). Enables future evolution.
    pub spec_version: i64,
    /// Row-preserving is mandatory for SILVER (models/staging/).
    pub row_preserving: bool,
    /// Explicit output field design for this staging model.
    pub output_fields: Vec<OutputFieldSpec>,
    /// Optional explicit guardrails to prevent accidental grain enforcement (e.g. "no filtering", "no dedup").
    #[serde(default)]
    pub prohibited_ops: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JoinSpec {
    pub right_model: String,
    pub join_type: JoinType,
    pub on: Vec<String>,
    #[serde(default)]
    pub cardinality: Option<Cardinality>,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MetricSpec {
    pub name: String,
    /// One-line definition (formula + inclusion/exclusion rules).
    pub definition: String,
    /// Exact output/input fields used by the metric. This keeps metric plans
    /// auditable and lets authoring require field-level parse/aggregate evidence.
    #[serde(default)]
    pub source_fields: Vec<String>,
    /// Optional caveats/assumptions (bounded).
    #[serde(default)]
    pub caveats: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelImplementationSpec {
    pub spec_version: i64,
    /// Grain contract, e.g. "1 row per order_id".
    pub grain: String,
    /// Inputs should align with task.inputs (stg_* or intra-plan gold models).
    #[serde(default)]
    pub inputs: Vec<String>,
    /// Join contract for composing the model.
    #[serde(default)]
    pub joins: Vec<JoinSpec>,
    /// Metric definitions for business use.
    #[serde(default)]
    pub metrics: Vec<MetricSpec>,
    /// Expected output schema contract (columns + semantics).
    #[serde(default)]
    pub output_fields: Vec<OutputFieldSpec>,
    /// Assumptions that require validation probes before downstream reliance.
    #[serde(default)]
    pub assumptions: Vec<String>,
    /// Typed evidence references for grain, key, cardinality, parse, and aggregate claims.
    #[serde(default)]
    pub evidence_claim_refs: Vec<crate::providers::SemanticClaimRef>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroundedModelInput {
    /// Canonical input model name — staging (`stg_orders`) or intra-plan gold
    /// (`fct_orders`).
    pub input_name: String,
    /// Expected dbt model file path for this grounded input.
    pub model_rel_path: String,
    /// Warehouse FQN used for deterministic validation queries.
    pub relation_fqn: String,
    /// Authoritative output columns for this relation (empty when warehouse
    /// schema is not yet available, e.g. intra-plan gold deps before authoring).
    #[serde(default)]
    pub source_schema: Vec<SourceColumnDef>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CleanseTask {
    pub dataset_id: String,
    /// Expected DBT model file path for this dataset (project-relative).
    /// Typically models/staging/stg_<schema>_<table>.sql
    #[serde(default)]
    pub expected_model_path: Option<String>,
    /// High-signal planning output: inferred grain, keys, time fields, and any known hazards.
    /// This should be grounded in schema + stats + sample probes.
    #[serde(default)]
    pub invariants: Vec<String>,
    /// Design-first implementation contract for this staging model.
    /// `None` means the spec has not yet been enriched from the skeleton plan.
    #[serde(default)]
    pub implementation_spec: Option<CleanseImplementationSpec>,
    /// Authoritative source columns from the catalog, captured at enrichment time.
    /// The enrichment LLM sees these; the grounding gate rejects plans where this is empty.
    #[serde(default)]
    pub source_schema: Vec<SourceColumnDef>,
    #[serde(default)]
    pub status: TaskStatus,
    #[serde(default)]
    pub checklist: Vec<PlanChecklistItem>,
}

impl PlanTask for CleanseTask {
    fn task_id(&self) -> &str {
        &self.dataset_id
    }
    fn expected_model_path(&self) -> Option<&str> {
        self.expected_model_path.as_deref()
    }
    fn set_status(&mut self, status: TaskStatus) {
        self.status = status;
    }
    fn checklist(&self) -> &[PlanChecklistItem] {
        &self.checklist
    }
    fn checklist_mut(&mut self) -> &mut Vec<PlanChecklistItem> {
        &mut self.checklist
    }
    fn work_group_prefix() -> &'static str {
        "cleanse"
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelTask {
    pub name: String,
    #[serde(default)]
    pub folder: ModelFolder,
    #[serde(default)]
    pub goal: String,
    #[serde(default)]
    pub inputs: Vec<String>,
    /// Expected DBT model file path for this model (project-relative).
    #[serde(default)]
    pub expected_model_path: Option<String>,
    /// High-signal planning output: grain, join keys, time semantics, uniqueness expectations.
    #[serde(default)]
    pub invariants: Vec<String>,
    /// Design-first implementation contract for this model.
    /// `None` means the spec has not yet been enriched from the skeleton plan.
    #[serde(default)]
    pub implementation_spec: Option<ModelImplementationSpec>,
    /// Authoritative input columns from staging models, captured at enrichment time.
    /// For model tasks, this merges the columns of all input staging models.
    #[serde(default)]
    pub source_schema: Vec<SourceColumnDef>,
    /// Grounded per-input relation facts for authoring/validation. This removes
    /// the need for downstream tools to re-derive staging relation identities.
    #[serde(default)]
    pub grounded_inputs: Vec<GroundedModelInput>,
    #[serde(default)]
    pub status: TaskStatus,
    #[serde(default)]
    pub checklist: Vec<PlanChecklistItem>,
}

impl ModelTask {
    /// Re-stamp `source_schema` from the given `SourceSchema` map by merging
    /// columns from all `self.inputs`. Called after discovery records staging
    /// model output schemas on the plan.
    pub fn apply_source_schema_from(&mut self, schemas: &SourceSchema) {
        let mut merged: Vec<SourceColumnDef> = Vec::new();
        for inp in &self.inputs {
            if let Some(cols) = schemas.get(inp.trim()) {
                merged.extend(cols.iter().cloned());
            }
        }
        if !merged.is_empty() {
            self.source_schema = merged;
        }
    }

    /// Build grounded input relations for **staging** inputs only.
    /// Intra-plan gold deps are handled separately by
    /// [`apply_intra_plan_grounded_inputs`].
    pub fn apply_grounded_inputs_from(
        &mut self,
        schemas: &SourceSchema,
        staging_prefix: Option<&str>,
    ) {
        let Some(prefix) = staging_prefix.map(str::trim).filter(|p| !p.is_empty()) else {
            return;
        };
        let mut grounded: Vec<GroundedModelInput> = Vec::new();
        for inp in &self.inputs {
            let input_name = inp.trim();
            if input_name.is_empty() || !input_name.starts_with("stg_") {
                continue;
            }
            let source_schema = schemas.get(input_name).cloned().unwrap_or_default();
            grounded.push(GroundedModelInput {
                input_name: input_name.to_string(),
                model_rel_path: format!("models/staging/{}.sql", input_name),
                relation_fqn: format!("{}.{}", prefix, input_name),
                source_schema,
            });
        }
        self.grounded_inputs = grounded;
    }
}

impl PlanTask for ModelTask {
    fn task_id(&self) -> &str {
        &self.name
    }
    fn expected_model_path(&self) -> Option<&str> {
        self.expected_model_path.as_deref()
    }
    fn set_status(&mut self, status: TaskStatus) {
        self.status = status;
    }
    fn checklist(&self) -> &[PlanChecklistItem] {
        &self.checklist
    }
    fn checklist_mut(&mut self) -> &mut Vec<PlanChecklistItem> {
        &mut self.checklist
    }
    fn work_group_prefix() -> &'static str {
        "model"
    }
}

/// Populate [`GroundedModelInput`] entries for intra-plan gold dependencies
/// (inputs that reference another task in the same plan, not a staging model).
/// Must be called **after** [`ModelTask::apply_grounded_inputs_from`] so that
/// staging entries are already in place.
pub fn apply_intra_plan_grounded_inputs(tasks: &mut [ModelTask], gold_prefix: Option<&str>) {
    let Some(prefix) = gold_prefix.map(str::trim).filter(|p| !p.is_empty()) else {
        return;
    };
    let task_paths: std::collections::BTreeMap<String, String> = tasks
        .iter()
        .filter(|t| !t.name.trim().is_empty())
        .map(|t| {
            let name = t.name.trim().to_string();
            let path = t
                .expected_model_path
                .clone()
                .unwrap_or_else(|| format!("models/{}/{}.sql", t.folder.as_str(), &name));
            (name, path)
        })
        .collect();
    let task_schemas: std::collections::BTreeMap<String, Vec<SourceColumnDef>> = tasks
        .iter()
        .filter(|t| !t.name.trim().is_empty())
        .map(|t| {
            let schema = t
                .implementation_spec
                .as_ref()
                .map(|spec| {
                    spec.output_fields
                        .iter()
                        .filter_map(|field| {
                            let name = field.name.trim();
                            if name.is_empty() {
                                return None;
                            }
                            Some(SourceColumnDef {
                                name: name.to_string(),
                                data_type: field
                                    .data_type
                                    .as_deref()
                                    .map(str::trim)
                                    .filter(|ty| !ty.is_empty())
                                    .unwrap_or("unknown")
                                    .to_string(),
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            (t.name.trim().to_string(), schema)
        })
        .collect();
    for t in tasks.iter_mut() {
        for inp in &t.inputs {
            let input_name = inp.trim();
            if input_name.is_empty() || input_name.starts_with("stg_") {
                continue;
            }
            if t.grounded_inputs.iter().any(|g| g.input_name == input_name) {
                continue;
            }
            if let Some(path) = task_paths.get(input_name) {
                t.grounded_inputs.push(GroundedModelInput {
                    input_name: input_name.to_string(),
                    model_rel_path: path.clone(),
                    relation_fqn: format!("{}.{}", prefix, input_name),
                    source_schema: task_schemas.get(input_name).cloned().unwrap_or_default(),
                });
            }
        }
    }
}

// ---------- Generic Plan ----------

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(bound(
    serialize = "T: serde::Serialize",
    deserialize = "T: serde::de::DeserializeOwned"
))]
pub struct Plan<T: PlanTask> {
    #[serde(default)]
    pub plan_key: String,
    pub status: PlanStatus,
    #[serde(default)]
    pub project_snapshot: PlanSnapshot,
    pub tasks: Vec<T>,
    pub batches: Vec<Vec<String>>,
    #[serde(default)]
    pub work_groups: Vec<PlanWorkGroup>,
    #[serde(default)]
    pub mutations: Vec<PlanMutation>,
    #[serde(default)]
    pub progress: PlanProgress,
}

impl<T: PlanTask> Plan<T> {
    /// Regenerate work_groups from the current batches. Must be called after any
    /// mutation that adds/removes tasks or batches to maintain internal consistency.
    pub fn reconcile_work_groups(&mut self) {
        self.work_groups = crate::plan_progress::canonical_work_groups_from_batches(
            &self.batches,
            T::work_group_prefix(),
        );
    }
}

pub fn reconcile_model_batches_and_work_groups(plan: &mut ModelPlan) {
    plan.batches = crate::plan_progress::canonical_model_batches_from_tasks(&plan.tasks);
    plan.work_groups =
        crate::plan_progress::canonical_model_work_groups_from_batches(&plan.batches);
}

pub type CleansePlan = Plan<CleanseTask>;
pub type ModelPlan = Plan<ModelTask>;

// ---------- TrackPlan ----------

pub trait TrackPlan {
    fn plan_key(&self) -> &str;
    fn status(&self) -> PlanStatus;
    fn set_status(&mut self, status: PlanStatus);
    fn tasks_len(&self) -> usize;
    fn batches_len(&self) -> usize;
    fn progress_mut(&mut self) -> &mut PlanProgress;
    fn executable_plan_issues(&self) -> Vec<String>;
    fn is_empty(&self) -> bool {
        self.tasks_len() == 0 || self.batches_len() == 0
    }

    fn approve(&mut self) -> Result<(), String> {
        let next = self.status().try_approve()?;
        self.set_status(next);
        Ok(())
    }

    fn cancel(&mut self) -> Result<(), String> {
        let next = self.status().try_cancel()?;
        self.set_status(next);
        Ok(())
    }
}

impl<T: PlanTask> TrackPlan for Plan<T> {
    fn plan_key(&self) -> &str {
        &self.plan_key
    }
    fn status(&self) -> PlanStatus {
        self.status
    }
    fn set_status(&mut self, status: PlanStatus) {
        self.status = status;
    }
    fn tasks_len(&self) -> usize {
        self.tasks.len()
    }
    fn batches_len(&self) -> usize {
        self.batches.len()
    }
    fn progress_mut(&mut self) -> &mut PlanProgress {
        &mut self.progress
    }
    fn executable_plan_issues(&self) -> Vec<String> {
        crate::plan_progress::executable_plan_issues(self)
    }
}

// ---------- Grounded / Persistable newtypes ----------

#[derive(Clone, Debug)]
pub struct GroundedPlan<T: PlanTask>(pub(crate) Plan<T>);

pub type GroundedCleansePlan = GroundedPlan<CleanseTask>;
pub type GroundedModelPlan = GroundedPlan<ModelTask>;

impl<T: PlanTask> GroundedPlan<T> {
    #[cfg(test)]
    pub fn into_inner(self) -> Plan<T> {
        self.0
    }
}

#[cfg(test)]
#[derive(Clone, Debug)]
pub enum PersistablePlan<T: PlanTask> {
    Grounded(GroundedPlan<T>),
    Terminal(Plan<T>),
}

#[cfg(test)]
pub type PersistableCleansePlan = PersistablePlan<CleanseTask>;
#[cfg(test)]
pub type PersistableModelPlan = PersistablePlan<ModelTask>;

#[cfg(test)]
impl<T: PlanTask> PersistablePlan<T> {
    pub fn into_inner(self) -> Plan<T> {
        match self {
            Self::Grounded(v) => v.into_inner(),
            Self::Terminal(v) => v,
        }
    }
}

#[cfg(test)]
mod stripped_artifact_tests {
    use super::*;

    fn make_artifact(key: &str) -> StrippedArtifact {
        StrippedArtifact {
            file: "dbt_project.yml".to_string(),
            key_path: vec![key.to_string()],
            value_summary: format!("- removed: {key}"),
            reason: "test".to_string(),
            relocation_hint: None,
        }
    }

    #[test]
    fn push_stripped_artifact_appends() {
        let mut snap = PlanSnapshot::default();
        snap.push_stripped_artifact(make_artifact("on-run-start"));
        assert_eq!(snap.stripped_artifacts.len(), 1);
        assert_eq!(snap.stripped_artifacts[0].key_path, vec!["on-run-start"]);
    }

    #[test]
    fn push_stripped_artifact_evicts_oldest_past_capacity() {
        let mut snap = PlanSnapshot::default();
        for i in 0..(MAX_STRIPPED_ARTIFACTS + 3) {
            snap.push_stripped_artifact(make_artifact(&format!("key_{i}")));
        }
        assert_eq!(snap.stripped_artifacts.len(), MAX_STRIPPED_ARTIFACTS);
        // FIFO: the first three were evicted, so the oldest remaining is `key_3`.
        assert_eq!(
            snap.stripped_artifacts[0].key_path,
            vec![format!("key_{}", 3)]
        );
        // The newest is `key_(MAX+2)`.
        let last_index = MAX_STRIPPED_ARTIFACTS + 2;
        assert_eq!(
            snap.stripped_artifacts.last().unwrap().key_path,
            vec![format!("key_{last_index}")]
        );
    }

    #[test]
    fn extend_stripped_artifacts_preserves_order_and_bounded() {
        let mut snap = PlanSnapshot::default();
        let batch: Vec<_> = (0..(MAX_STRIPPED_ARTIFACTS * 2))
            .map(|i| make_artifact(&format!("k{i}")))
            .collect();
        snap.extend_stripped_artifacts(batch);
        assert_eq!(snap.stripped_artifacts.len(), MAX_STRIPPED_ARTIFACTS);
        let first_kept = MAX_STRIPPED_ARTIFACTS;
        assert_eq!(
            snap.stripped_artifacts[0].key_path,
            vec![format!("k{first_kept}")]
        );
    }

    #[test]
    fn stripped_artifact_round_trips_through_json() {
        let a = StrippedArtifact {
            file: "dbt_project.yml".to_string(),
            key_path: vec!["on-run-start".to_string()],
            value_summary: "- '{{ x() }}'".to_string(),
            reason: "system-owned".to_string(),
            relocation_hint: Some("use config(pre_hook=[...])".to_string()),
        };
        let json = serde_json::to_string(&a).expect("serialize");
        let back: StrippedArtifact = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.file, a.file);
        assert_eq!(back.key_path, a.key_path);
        assert_eq!(back.relocation_hint, a.relocation_hint);
    }
}
