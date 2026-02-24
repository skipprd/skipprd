use once_cell::sync::OnceCell;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Stable identifiers for JSON Schemas enforced at runtime.
///
/// These IDs are the single source of truth across:
/// - prompt contracts (what the LLM must emit)
/// - provider adapters (transport-level schema when available)
/// - runtime parsing/validation (fallback when transport can’t enforce)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SchemaId {
    AgentStepV1,
    PatchSingleFileV1,
    CleansePlanSkeletonV1,
    ModelPlanSkeletonV1,
    CleansePlanEnrichmentV1,
    ModelPlanEnrichmentV1,
    PlanDesignCritiqueV1,
    ModelPlanCandidatesV1,
}

impl SchemaId {
    pub fn name(&self) -> &'static str {
        match self {
            SchemaId::AgentStepV1 => "agent.step.v1",
            SchemaId::PatchSingleFileV1 => "patch_protocol.single_file.v1",
            SchemaId::CleansePlanSkeletonV1 => "data_engineer.cleanse_plan_skeleton.v1",
            SchemaId::ModelPlanSkeletonV1 => "data_engineer.model_plan_skeleton.v1",
            SchemaId::CleansePlanEnrichmentV1 => "data_engineer.cleanse_plan_enrichment.v1",
            SchemaId::ModelPlanEnrichmentV1 => "data_engineer.model_plan_enrichment.v1",
            SchemaId::PlanDesignCritiqueV1 => "data_engineer.plan_design_critique.v1",
            SchemaId::ModelPlanCandidatesV1 => "data_engineer.model_plan_candidates.v1",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FinalEnvelopeV1 {
    pub kind: String,
    /// JSON-encoded payload.
    ///
    /// OpenAI Structured Outputs cannot represent arbitrary JSON objects (`additionalProperties`
    /// must be false on objects), so we transport arbitrary payloads as a JSON string and parse it
    /// at runtime.
    pub payload: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentStepTypeV1 {
    Tool,
    Final,
}

/// Exactly one agent step result (wire format).
///
/// This is intentionally a single object schema (no `oneOf`), because OpenAI Structured Outputs
/// rejects schemas containing `oneOf` (and disallows open-ended objects).
///
/// We emulate optional fields by making them nullable and requiring all fields (OpenAI constraint).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentStepV1 {
    #[serde(rename = "type")]
    pub type_: AgentStepTypeV1,

    /// Tool name when `type` is `"tool"`, otherwise null.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// JSON-encoded args when `type` is `"tool"`, otherwise null.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<String>,

    /// Final envelope when `type` is `"final"`, otherwise null.
    #[serde(rename = "final", default, skip_serializing_if = "Option::is_none")]
    pub final_: Option<FinalEnvelopeV1>,
}

/// Patch protocol schema for a single expected file.
///
/// The suite enforces “EXACTLY ONE primitive” in logic, but this schema provides
/// a provider-compatible contract and a validator target for fallback backends.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PatchSingleFileV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace_file: Option<PatchReplaceFileV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace_range: Option<PatchReplaceRangeV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace_list: Option<PatchReplaceListV1>,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PatchReplaceFileV1 {
    pub path: String,
    pub new_text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PatchReplaceRangeV1 {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub new_text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PatchReplaceListEditV1 {
    pub start_line: usize,
    pub end_line: usize,
    pub new_text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PatchReplaceListV1 {
    pub path: String,
    pub edits: Vec<PatchReplaceListEditV1>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CleansePlanSkeletonTaskV1 {
    pub dataset_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CleansePlanSkeletonV1 {
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
    /// Relative business value score (0-100).
    pub value_score: i32,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelPlanCandidatesV1 {
    pub candidates: Vec<ModelPlanCandidateV1>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanDesignCritiqueV1 {
    pub ok: bool,
    pub blockers: Vec<String>,
    pub fixes: Vec<String>,
}

fn schema_for_id(id: SchemaId) -> Value {
    let schema = match id {
        SchemaId::AgentStepV1 => schemars::schema_for!(AgentStepV1),
        SchemaId::PatchSingleFileV1 => schemars::schema_for!(PatchSingleFileV1),
        SchemaId::CleansePlanSkeletonV1 => schemars::schema_for!(CleansePlanSkeletonV1),
        SchemaId::ModelPlanSkeletonV1 => schemars::schema_for!(ModelPlanSkeletonV1),
        SchemaId::CleansePlanEnrichmentV1 => schemars::schema_for!(CleansePlanEnrichmentV1),
        SchemaId::ModelPlanEnrichmentV1 => schemars::schema_for!(ModelPlanEnrichmentV1),
        SchemaId::PlanDesignCritiqueV1 => schemars::schema_for!(PlanDesignCritiqueV1),
        SchemaId::ModelPlanCandidatesV1 => schemars::schema_for!(ModelPlanCandidatesV1),
    };
    let root_v = serde_json::to_value(&schema).expect("schema serialization must succeed");
    root_schema_json_to_json_schema_value(root_v)
}

/// Convert `schemars::schema_for!()` JSON into a standard JSON Schema document.
///
/// `schemars::schema_for!()` yields a wrapper like:
/// `{ "$schema": "...", "schema": { ...actual schema... }, "definitions": { ... } }`
///
/// OpenAI expects `text.format.schema` to be the schema itself (top-level `type:"object"`),
/// not wrapped under `schema`.
fn root_schema_json_to_json_schema_value(root_v: Value) -> Value {
    // Schemars versions differ:
    // - Some emit a wrapper: { "$schema": "...", "schema": { ... }, "definitions": { ... } }
    // - Newer versions emit a full JSON Schema doc directly.
    let mut out = if let Some(schema_v) = root_v.get("schema") {
        let mut inner = schema_v.clone();
        // Preserve root metadata / defs alongside the inner schema.
        for defs_key in ["definitions", "$defs"] {
            if let Some(defs) = root_v.get(defs_key) {
                if defs.as_object().is_some_and(|m| !m.is_empty()) {
                    if let Some(obj) = inner.as_object_mut() {
                        obj.insert(defs_key.to_string(), defs.clone());
                    }
                }
            }
        }
        if let Some(meta) = root_v.get("$schema") {
            if let Some(obj) = inner.as_object_mut() {
                obj.insert("$schema".to_string(), meta.clone());
            }
        }
        inner
    } else {
        root_v
    };

    // OpenAI requires the top-level schema be an object schema for structured output.
    // Schemars sometimes emits `oneOf`/`anyOf` without an explicit `type`.
    if let Some(obj) = out.as_object_mut() {
        obj.entry("type".to_string())
            .or_insert_with(|| Value::String("object".to_string()));
    }
    openai_structured_outputs_strictify_schema(&mut out);
    out
}

/// OpenAI Structured Outputs has additional JSON Schema constraints beyond Draft 2020-12.
///
/// In particular, for any object schema with `properties`, OpenAI requires:
/// - `required` MUST be present
/// - `required` MUST include *every* key present in `properties`
/// - (Practically) `additionalProperties: false` is expected for strict schemas
fn openai_structured_outputs_strictify_schema(v: &mut Value) {
    fn visit(node: &mut Value) {
        match node {
            Value::Array(a) => {
                for x in a.iter_mut() {
                    visit(x);
                }
            }
            Value::Object(m) => {
                // First recurse into all children so nested schemas (including $defs maps)
                // are strictified too.
                for v in m.values_mut() {
                    visit(v);
                }

                // Then enforce object constraints when properties exist.
                let props_keys: Option<Vec<String>> = m
                    .get("properties")
                    .and_then(|p| p.as_object())
                    .map(|props| props.keys().cloned().collect());
                if let Some(mut keys) = props_keys {
                    // Ensure additionalProperties is false (strict).
                    m.entry("additionalProperties".to_string())
                        .or_insert(Value::Bool(false));

                    keys.sort();

                    let req = m
                        .entry("required".to_string())
                        .or_insert_with(|| Value::Array(Vec::new()));

                    let mut req_keys: Vec<String> = req
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();

                    // Required must include *every* property key.
                    for k in keys.iter() {
                        if !req_keys.iter().any(|rk| rk == k) {
                            req_keys.push(k.clone());
                        }
                    }
                    req_keys.sort();
                    req_keys.dedup();
                    *req = Value::Array(req_keys.into_iter().map(Value::String).collect());
                }
            }
            _ => {}
        }
    }
    visit(v);
}

static SCHEMAS: OnceCell<std::collections::HashMap<SchemaId, Value>> = OnceCell::new();
pub fn json_schema(id: SchemaId) -> Value {
    let map = SCHEMAS.get_or_init(|| {
        use std::collections::HashMap;
        let mut m: HashMap<SchemaId, Value> = HashMap::new();
        m.insert(SchemaId::AgentStepV1, schema_for_id(SchemaId::AgentStepV1));
        m.insert(
            SchemaId::PatchSingleFileV1,
            schema_for_id(SchemaId::PatchSingleFileV1),
        );
        m.insert(
            SchemaId::CleansePlanSkeletonV1,
            schema_for_id(SchemaId::CleansePlanSkeletonV1),
        );
        m.insert(
            SchemaId::ModelPlanSkeletonV1,
            schema_for_id(SchemaId::ModelPlanSkeletonV1),
        );
        m.insert(
            SchemaId::CleansePlanEnrichmentV1,
            schema_for_id(SchemaId::CleansePlanEnrichmentV1),
        );
        m.insert(
            SchemaId::ModelPlanEnrichmentV1,
            schema_for_id(SchemaId::ModelPlanEnrichmentV1),
        );
        m.insert(
            SchemaId::PlanDesignCritiqueV1,
            schema_for_id(SchemaId::PlanDesignCritiqueV1),
        );
        m.insert(
            SchemaId::ModelPlanCandidatesV1,
            schema_for_id(SchemaId::ModelPlanCandidatesV1),
        );
        m
    });
    map.get(&id)
        .cloned()
        .unwrap_or_else(|| schema_for_id(id))
}

pub fn validate(id: SchemaId, instance: &Value) -> Result<(), String> {
    let schema_json = json_schema(id);
    let validator = jsonschema::validator_for(&schema_json)
        .map_err(|e| format!("failed to compile {}: {}", id.name(), e))?;
    if let Err(first) = validator.validate(instance) {
        // Keep the first error concise; callers can decide whether to retry with more detail.
        return Err(format!("{} validation error: {}", id.name(), first));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_step_schema_accepts_tool_and_final() {
        let tool = serde_json::json!({
            "type": "tool",
            "name": "file",
            "args": "{\"op\":\"list\",\"prefix\":\"models/\"}",
            "final": null
        });
        validate(SchemaId::AgentStepV1, &tool).expect("tool should validate");

        let fin = serde_json::json!({
            "type": "final",
            "name": null,
            "args": null,
            "final": { "kind": "generic", "payload": "{\"text\":\"ok\"}", "display": "ok" }
        });
        validate(SchemaId::AgentStepV1, &fin).expect("final should validate");
    }

    #[test]
    fn agent_step_schema_rejects_legacy_action_envelope() {
        let legacy = serde_json::json!({ "action": "file", "args": { "op": "list" } });
        let err = validate(SchemaId::AgentStepV1, &legacy).expect_err("should reject legacy");
        assert!(err.contains("validation error"), "err={err}");
    }

    #[test]
    fn schema_documents_have_object_type_at_top_level() {
        for id in [SchemaId::AgentStepV1, SchemaId::PatchSingleFileV1] {
            let s = json_schema(id);
            assert_eq!(
                s.get("type").and_then(|v| v.as_str()),
                Some("object"),
                "schema {} must be a top-level object schema for OpenAI; schema={}",
                id.name()
                , s
            );
        }
    }

    #[test]
    fn final_envelope_required_includes_display_for_openai() {
        let s = json_schema(SchemaId::AgentStepV1);
        let defs = s.get("$defs").and_then(|v| v.as_object()).expect("$defs");
        let env = defs.get("FinalEnvelopeV1").expect("FinalEnvelopeV1");
        let req = env
            .get("required")
            .and_then(|v| v.as_array())
            .expect("required");
        let mut got: Vec<String> = req
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();
        got.sort();
        assert!(
            got.contains(&"display".to_string()),
            "expected display required (nullable) for OpenAI strict schema; got={got:?}"
        );
    }
}

