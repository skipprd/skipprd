//! Build namespace [`Metadata`] from flat field rows and persist via [`Config::set_metadata`].
//!
//! v1 supports top-level namespace fields only (same projection as `field_details()`).

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::discover::{Metadata, PipelineMetadata, SkipprDataType};
use crate::helpers::configuration::Config;
use crate::METADATA;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MetadataFieldRow {
    pub name: String,
    pub field_type: String,
    pub nullable: bool,
}

/// Build a namespace [`Metadata`] tree from flat `(name, type, nullable)` rows.
pub fn namespace_metadata_from_field_rows(
    fields: &[MetadataFieldRow],
    flatten: bool,
) -> Result<Metadata, String> {
    let mut metadata =
        Metadata::new().map_err(|e| format!("failed to initialize metadata: {e}"))?;
    for row in fields {
        let name = row.name.trim();
        if name.is_empty() {
            return Err("field name must not be empty".to_string());
        }
        let dt = SkipprDataType::from_str(row.field_type.trim());
        if dt == SkipprDataType::Unknown {
            return Err(format!(
                "unknown field type '{}' for field '{}'",
                row.field_type, name
            ));
        }
        let mut field_meta = Metadata::new_with_type(dt, name);
        field_meta.nullable = row.nullable;
        metadata.set_field(name, field_meta);
    }
    metadata.finalize_field_types(flatten);
    Ok(metadata)
}

/// Replace one namespace in pipeline metadata, update in-memory `METADATA`, and persist.
pub async fn apply_namespace_field_rows(
    namespace: &str,
    fields: &[MetadataFieldRow],
    evolved: bool,
) -> Result<usize, String> {
    let ns = namespace.trim();
    if ns.is_empty() {
        return Err("namespace must not be empty".to_string());
    }
    let flatten = Config::get_transform_flatten_events();
    let namespace_metadata = namespace_metadata_from_field_rows(fields, flatten)?;
    let fields_written = namespace_metadata.field_details().len();

    let mut pipeline_metadata = match Config::get_metadata().await {
        Ok(pm) => pm,
        Err(_) => PipelineMetadata::new(),
    };
    pipeline_metadata
        .metadata
        .insert(ns.to_string(), namespace_metadata);
    pipeline_metadata.enabled = true;

    METADATA.store(Arc::new(pipeline_metadata.clone()));
    Config::set_metadata(&pipeline_metadata, evolved).await;

    Ok(fields_written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover::SkipprDataType;

    #[test]
    fn field_rows_round_trip_field_details() {
        let rows = vec![
            MetadataFieldRow {
                name: "id".to_string(),
                field_type: "string".to_string(),
                nullable: false,
            },
            MetadataFieldRow {
                name: "amount".to_string(),
                field_type: "double".to_string(),
                nullable: true,
            },
            MetadataFieldRow {
                name: "created_at".to_string(),
                field_type: "date".to_string(),
                nullable: true,
            },
        ];
        let metadata = namespace_metadata_from_field_rows(&rows, false).expect("build metadata");
        let mut details = metadata.field_details();
        details.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(details.len(), 3);
        assert_eq!(
            details[0],
            ("amount".to_string(), "double".to_string(), true)
        );
        assert_eq!(
            details[1],
            ("created_at".to_string(), "date".to_string(), true)
        );
        assert_eq!(details[2], ("id".to_string(), "string".to_string(), false));

        let amount = metadata.fields.get("amount").expect("amount field");
        assert_eq!(amount.determined_type, SkipprDataType::Double);
    }

    #[test]
    fn unknown_field_type_is_rejected() {
        let rows = vec![MetadataFieldRow {
            name: "x".to_string(),
            field_type: "not_a_real_type".to_string(),
            nullable: true,
        }];
        let err = namespace_metadata_from_field_rows(&rows, false).unwrap_err();
        assert!(err.contains("unknown field type"));
    }
}
