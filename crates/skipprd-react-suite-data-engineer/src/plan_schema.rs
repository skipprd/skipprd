use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub fn strict_schema_for<T: JsonSchema>() -> Result<Value, String> {
    react_core::schema_registry::strict_json_schema_for::<T>().map_err(|e| e.to_string())
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

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CleansePlanEnrichmentItemV1 {
    pub task_id: String,
    pub implementation_spec: super::plan_types::CleanseImplementationSpec,
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
    pub implementation_spec: super::plan_types::ModelImplementationSpec,
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

#[derive(
    Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord,
)]
#[serde(rename_all = "snake_case")]
pub enum PlanDesignBlockerCodeV1 {
    MissingGroundedTasks,
    MissingTaskSpecs,
    MissingWorkGroupCoverage,
    InvalidChecklistProgress,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanDesignFixActionV1 {
    RegenerateTasks,
    EnrichTaskSpecs,
    RepairWorkGroups,
    RepairChecklistCoverage,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn collect_ref_sibling_violations(v: &Value, path: &str, out: &mut Vec<String>) {
        match v {
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    collect_ref_sibling_violations(item, &format!("{path}[{i}]"), out);
                }
            }
            Value::Object(map) => {
                if map.contains_key("$ref") && map.len() > 1 {
                    let mut keys: Vec<String> = map.keys().cloned().collect();
                    keys.sort();
                    out.push(format!("{path}: {:?}", keys));
                }
                for (k, child) in map {
                    collect_ref_sibling_violations(child, &format!("{path}.{k}"), out);
                }
            }
            _ => {}
        }
    }

    fn collect_unsupported_one_of_keywords(v: &Value, path: &str, out: &mut Vec<String>) {
        match v {
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    collect_unsupported_one_of_keywords(item, &format!("{path}[{i}]"), out);
                }
            }
            Value::Object(map) => {
                if map.contains_key("oneOf") {
                    out.push(format!("{path}.oneOf"));
                }
                for (k, child) in map {
                    collect_unsupported_one_of_keywords(child, &format!("{path}.{k}"), out);
                }
            }
            _ => {}
        }
    }

    fn assert_openai_ref_compat(schema: &Value, name: &str) {
        let mut violations: Vec<String> = Vec::new();
        collect_ref_sibling_violations(schema, "$", &mut violations);
        assert!(
            violations.is_empty(),
            "{name} has OpenAI-incompatible $ref sibling nodes:\n{}",
            violations.join("\n")
        );
        let mut union_violations: Vec<String> = Vec::new();
        collect_unsupported_one_of_keywords(schema, "$", &mut union_violations);
        assert!(
            union_violations.is_empty(),
            "{name} has OpenAI-incompatible oneOf keywords:\n{}",
            union_violations.join("\n")
        );
    }

    #[test]
    fn strict_schema_cleanse_plan_enrichment_is_openai_compatible() {
        let schema = strict_schema_for::<CleansePlanEnrichmentV1>().expect("schema");
        assert_openai_ref_compat(&schema, "CleansePlanEnrichmentV1");
    }

    #[test]
    fn strict_schema_model_plan_enrichment_is_openai_compatible() {
        let schema = strict_schema_for::<ModelPlanEnrichmentV1>().expect("schema");
        assert_openai_ref_compat(&schema, "ModelPlanEnrichmentV1");
    }

    #[test]
    fn strict_schema_design_critique_is_openai_compatible() {
        let schema = strict_schema_for::<PlanDesignCritiqueV1>().expect("schema");
        assert_openai_ref_compat(&schema, "PlanDesignCritiqueV1");
    }
}
