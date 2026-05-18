use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Patch protocol schema for a single expected file.
///
/// The suite enforces "EXACTLY ONE primitive" in logic, but this schema provides
/// a provider-compatible contract and a validator target for fallback backends.
#[allow(dead_code)]
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

#[allow(dead_code)]
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PatchReplaceFileV1 {
    pub path: String,
    pub new_text: String,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PatchReplaceRangeV1 {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub new_text: String,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PatchReplaceListEditV1 {
    pub start_line: usize,
    pub end_line: usize,
    pub new_text: String,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PatchReplaceListV1 {
    pub path: String,
    pub edits: Vec<PatchReplaceListEditV1>,
}

#[cfg(test)]
pub fn patch_single_file_schema() -> Result<serde_json::Value, String> {
    react_core::schema_registry::strict_json_schema_for::<PatchSingleFileV1>()
        .map_err(|e| e.to_string())
}
