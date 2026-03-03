use serde::{Deserialize, Serialize};
use serde_json::Value;

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
}

impl Default for PlanProgress {
    fn default() -> Self {
        Self {
            last_applied_step_idx: 0,
            consecutive_batch_failures: 0,
            total_batch_failures: 0,
        }
    }
}

/// Audit trail for plan mutations (repairs, pruning, canonicalization).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanMutation {
    pub ts: String,
    /// Machine-readable reason code, e.g. "plan_repair_semantic".
    pub reason_code: String,
    /// Additional structured detail (best-effort; keep small).
    #[serde(default)]
    pub detail: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecklistOrigin {
    Initial,
    ReviewActionable,
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
    pub origin_step_idx: Option<usize>,
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

// -----------------------
// Design-first plan spec
// -----------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldKind {
    Raw,
    Clean,
    Derived,
    QualityFlag,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputFieldSpec {
    /// Output column name.
    pub name: String,
    /// The role of the field in the model interface.
    pub kind: FieldKind,
    /// Upstream source columns (or prior-stage columns) this field depends on.
    #[serde(default)]
    pub source_columns: Vec<String>,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JoinSpec {
    pub right_model: String,
    /// join type: inner|left|right|full (design intent)
    pub join_type: String,
    /// Join keys contract. Example: ["customer_id = customer_id"] or ["order_id = order_id"].
    pub on: Vec<String>,
    /// Optional cardinality expectation (e.g. "many_to_one", "one_to_many").
    #[serde(default)]
    pub cardinality: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricSpec {
    pub name: String,
    /// One-line definition (formula + inclusion/exclusion rules).
    pub definition: String,
    /// Optional caveats/assumptions (bounded).
    #[serde(default)]
    pub caveats: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelImplementationSpec {
    pub spec_version: i64,
    /// Grain contract, e.g. "1 row per order_id".
    pub grain: String,
    /// Inputs should align with task.inputs (stg_* only).
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
    pub implementation_spec: CleanseImplementationSpec,
    #[serde(default)]
    pub status: TaskStatus,
    #[serde(default)]
    pub checklist: Vec<PlanChecklistItem>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CleansePlan {
    /// Storage key where this plan is persisted.
    #[serde(default)]
    pub plan_key: String,
    pub status: PlanStatus,
    /// Optional snapshot of the DBT project state when this plan was created.
    #[serde(default)]
    pub project_snapshot: Value,
    pub tasks: Vec<CleanseTask>,
    /// Ordered batches of dataset_ids; each batch MUST have at most 5 items.
    pub batches: Vec<Vec<String>>,
    /// Ordered work groups (checklist-driven). This is the canonical execution plan for the UI.
    #[serde(default)]
    pub work_groups: Vec<PlanWorkGroup>,
    /// Audit trail for deterministic + LLM-backed repairs.
    #[serde(default)]
    pub mutations: Vec<PlanMutation>,
    #[serde(default)]
    pub progress: PlanProgress,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelTask {
    pub name: String,
    #[serde(default)]
    pub folder: String, // "marts" | "core"
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
    pub implementation_spec: ModelImplementationSpec,
    #[serde(default)]
    pub status: TaskStatus,
    #[serde(default)]
    pub checklist: Vec<PlanChecklistItem>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPlan {
    /// Storage key where this plan is persisted.
    #[serde(default)]
    pub plan_key: String,
    pub status: PlanStatus,
    /// Optional snapshot of the DBT project state when this plan was created.
    #[serde(default)]
    pub project_snapshot: Value,
    pub tasks: Vec<ModelTask>,
    /// Ordered batches of model names; each batch MUST have at most 5 items.
    pub batches: Vec<Vec<String>>,
    /// Ordered work groups (checklist-driven). This is the canonical execution plan for the UI.
    #[serde(default)]
    pub work_groups: Vec<PlanWorkGroup>,
    /// Audit trail for deterministic + LLM-backed repairs.
    #[serde(default)]
    pub mutations: Vec<PlanMutation>,
    #[serde(default)]
    pub progress: PlanProgress,
}

#[derive(Clone, Debug)]
pub enum PlanIRDraft {
    Cleanse(CleansePlan),
    Model(ModelPlan),
}

#[derive(Clone, Debug)]
pub struct GroundedCleansePlan(pub CleansePlan);

#[derive(Clone, Debug)]
pub struct GroundedModelPlan(pub ModelPlan);

#[derive(Clone, Debug)]
pub enum PersistableCleansePlan {
    Grounded(GroundedCleansePlan),
    Terminal(CleansePlan),
}

#[derive(Clone, Debug)]
pub enum PersistableModelPlan {
    Grounded(GroundedModelPlan),
    Terminal(ModelPlan),
}
