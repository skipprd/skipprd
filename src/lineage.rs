use serde_derive::{Deserialize, Serialize};

use crate::discover::{OutputMetadata, SkipprDataType};

/// Stable Skippr-owned field identity.
///
/// Iceberg field ids can be derived from this model, but the lineage model is
/// intentionally broader: it tracks fields across source payloads, Skippr's
/// logical schema, physical sink columns, and downstream model assets.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct FieldLineage {
    pub field_id: i32,
    pub source_system: String,
    pub source_namespace: String,
    pub source_path: Vec<String>,
    pub skippr_namespace: String,
    pub output_path: Vec<String>,
    #[serde(default)]
    pub sink_refs: Vec<SinkLineageRef>,
    #[serde(default)]
    pub downstream_refs: Vec<DownstreamLineageRef>,
    pub data_type: SkipprDataType,
    #[serde(default = "default_nullable")]
    pub nullable: bool,
    #[serde(default)]
    pub aliases: Vec<FieldAlias>,
    #[serde(default)]
    pub parent_field_id: Option<i32>,
    #[serde(default)]
    pub created_in_schema_version: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct FieldAlias {
    pub path: Vec<String>,
    pub reason: FieldAliasReason,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum FieldAliasReason {
    SourceRename,
    OutputRename,
    Flattening,
    TypeEvolution,
    Manual,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SinkLineageRef {
    pub sink_ref: String,
    pub catalog: Option<String>,
    pub table: String,
    pub field_path: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct DownstreamLineageRef {
    pub asset_kind: DownstreamAssetKind,
    pub asset_name: String,
    pub field_path: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum DownstreamAssetKind {
    Model,
    View,
    Metric,
    Report,
    External,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct NamespaceLineage {
    pub namespace: String,
    pub schema_version: u64,
    pub fields: Vec<FieldLineage>,
}

pub fn default_nullable() -> bool {
    true
}

/// Derive a deterministic positive 31-bit field id from a namespace and path.
///
/// This is suitable for initial field-id assignment. If a source rename is
/// later detected, the persisted `FieldLineage` record should preserve the old
/// id and record the new path as an alias instead of recomputing it.
pub fn deterministic_field_id(namespace: &str, output_path: &[String]) -> i32 {
    let mut input = String::with_capacity(namespace.len() + output_path.len() * 16);
    input.push_str(namespace);
    input.push('\0');
    input.push_str(&output_path.join("."));
    let digest = md5::compute(input.as_bytes());
    let raw = u32::from_be_bytes([digest.0[0], digest.0[1], digest.0[2], digest.0[3]]);
    // Iceberg field ids are positive signed integers.
    (raw & 0x7fff_ffff) as i32
}

pub fn derive_namespace_lineage(
    source_system: impl Into<String>,
    source_namespace: impl Into<String>,
    skippr_namespace: impl Into<String>,
    schema_version: u64,
    metadata: &OutputMetadata,
) -> NamespaceLineage {
    let source_system = source_system.into();
    let source_namespace = source_namespace.into();
    let skippr_namespace = skippr_namespace.into();
    let mut fields = Vec::new();
    for (_, field) in metadata.child_fields() {
        derive_field_lineage(
            &source_system,
            &source_namespace,
            &skippr_namespace,
            schema_version,
            field,
            Vec::new(),
            None,
            &mut fields,
        );
    }
    NamespaceLineage {
        namespace: skippr_namespace,
        schema_version,
        fields,
    }
}

#[allow(clippy::too_many_arguments)]
fn derive_field_lineage(
    source_system: &str,
    source_namespace: &str,
    skippr_namespace: &str,
    schema_version: u64,
    field: &OutputMetadata,
    mut output_path: Vec<String>,
    parent_field_id: Option<i32>,
    out: &mut Vec<FieldLineage>,
) {
    output_path.push(field.out_field_name().to_string());
    let field_id = deterministic_field_id(skippr_namespace, &output_path);
    out.push(FieldLineage {
        field_id,
        source_system: source_system.to_string(),
        source_namespace: source_namespace.to_string(),
        source_path: output_path.clone(),
        skippr_namespace: skippr_namespace.to_string(),
        output_path: output_path.clone(),
        sink_refs: Vec::new(),
        downstream_refs: Vec::new(),
        data_type: field.determined_type().clone(),
        nullable: field.nullable(),
        aliases: Vec::new(),
        parent_field_id,
        created_in_schema_version: schema_version,
    });
    for (_, child) in field.child_fields() {
        derive_field_lineage(
            source_system,
            source_namespace,
            skippr_namespace,
            schema_version,
            child,
            output_path.clone(),
            Some(field_id),
            out,
        );
    }
}
