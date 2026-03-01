use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub fn strict_schema_for<T: JsonSchema>() -> Value {
    react_core::schema_registry::strict_json_schema_for::<T>()
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CleansePlanSkeletonTaskV1 {
    pub dataset_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CleansePlanSkeletonV1 {
    #[schemars(length(min = 1))]
    pub tasks: Vec<CleansePlanSkeletonTaskV1>,
    pub batches: Vec<Vec<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelPlanSkeletonTaskV1 {
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelPlanSkeletonV1 {
    pub tasks: Vec<ModelPlanSkeletonTaskV1>,
    pub batches: Vec<Vec<String>>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FieldKindV1 {
    Raw,
    Clean,
    Derived,
    QualityFlag,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutputFieldSpecV1 {
    pub name: String,
    pub kind: FieldKindV1,
    #[serde(default)]
    pub source_columns: Vec<String>,
    pub expression: String,
    #[serde(default)]
    pub data_type: Option<String>,
    #[serde(default)]
    pub nullable: bool,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CleanseImplementationSpecV1 {
    pub spec_version: i64,
    pub row_preserving: bool,
    pub output_fields: Vec<OutputFieldSpecV1>,
    #[serde(default)]
    pub prohibited_ops: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JoinSpecV1 {
    pub right_model: String,
    pub join_type: String,
    pub on: Vec<String>,
    #[serde(default)]
    pub cardinality: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MetricSpecV1 {
    pub name: String,
    pub definition: String,
    #[serde(default)]
    pub caveats: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelImplementationSpecV1 {
    pub spec_version: i64,
    pub grain: String,
    #[serde(default)]
    pub inputs: Vec<String>,
    #[serde(default)]
    pub joins: Vec<JoinSpecV1>,
    #[serde(default)]
    pub metrics: Vec<MetricSpecV1>,
    #[serde(default)]
    pub output_fields: Vec<OutputFieldSpecV1>,
    #[serde(default)]
    pub assumptions: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CleansePlanEnrichmentItemV1 {
    pub task_id: String,
    pub implementation_spec: CleanseImplementationSpecV1,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CleansePlanEnrichmentV1 {
    pub items: Vec<CleansePlanEnrichmentItemV1>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelPlanEnrichmentItemV1 {
    pub task_id: String,
    pub implementation_spec: ModelImplementationSpecV1,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelPlanEnrichmentV1 {
    pub items: Vec<ModelPlanEnrichmentItemV1>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelPlanCandidateV1 {
    pub name: String,
    pub insight: String,
    pub observation: String,
    pub value_score: i32,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelPlanCandidatesV1 {
    pub candidates: Vec<ModelPlanCandidateV1>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanDesignBlockerV1 {
    pub code: PlanDesignBlockerCodeV1,
    #[serde(default)]
    pub target_id: Option<String>,
    #[serde(default)]
    pub severity: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanDesignFixV1 {
    pub action: PlanDesignFixActionV1,
    #[serde(default)]
    pub blocker_code: Option<PlanDesignBlockerCodeV1>,
    #[serde(default)]
    pub target_id: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanDesignBlockerCodeV1 {
    MissingGroundedTasks,
    MissingTaskSpecs,
    MissingWorkGroupCoverage,
    InvalidChecklistProgress,
    Other,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanDesignFixActionV1 {
    RegenerateTasks,
    EnrichTaskSpecs,
    RepairWorkGroups,
    RepairChecklistCoverage,
    Other,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanDesignCritiqueV1 {
    pub ok: bool,
    #[serde(default)]
    pub blockers: Vec<PlanDesignBlockerV1>,
    #[serde(default)]
    pub fixes: Vec<PlanDesignFixV1>,
}
