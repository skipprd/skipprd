use skippr_lease::{PipelineKey, PromoteError};

use crate::buffer::wal_handle;
use crate::discover::PipelineMetadata;
use crate::helpers::configuration::Config;
use crate::runtime_plugins::schema_state::install_from_pipeline_metadata;
use skippr_wal_catalog::{query_all, CatalogPrefix, CatalogRow};

pub async fn load_and_install_pipeline_schema(
    config: &Config,
    key: &PipelineKey,
    flatten: bool,
    initialized: bool,
) -> Result<(), PromoteError> {
    let required = live_wal_namespaces(key)?;
    let pipeline = config.pipeline_type().map_err(PromoteError::Other)?;
    let mut metadata = match config.load_pipeline_metadata(key).await {
        Ok(Some(metadata)) => metadata,
        Ok(None) => {
            if missing_metadata_is_fatal(initialized, &required, pipeline) {
                return Err(PromoteError::SchemaMissing("metadata.json".into()));
            }
            match promote_metadata_when_file_missing(config, pipeline) {
                Some(metadata) => metadata,
                None => return Ok(()),
            }
        }
        Err(err) => return Err(PromoteError::Other(err)),
    };
    metadata.flattened = flatten;
    let _ = metadata.migrate_persisted_metadata();
    install_from_pipeline_metadata(config, &metadata);
    ensure_live_wal_namespaces_in_metadata(&required, &metadata, pipeline)
}

pub fn promote_metadata_when_file_missing(
    config: &Config,
    pipeline: crate::pipeline_type::PipelineType,
) -> Option<PipelineMetadata> {
    match pipeline {
        crate::pipeline_type::PipelineType::Copy => Some(PipelineMetadata::new(config)),
        crate::pipeline_type::PipelineType::Ingest | crate::pipeline_type::PipelineType::Cdc => {
            None
        }
    }
}

pub fn missing_metadata_is_fatal(
    initialized: bool,
    live_namespaces: &[String],
    pipeline: crate::pipeline_type::PipelineType,
) -> bool {
    if pipeline == crate::pipeline_type::PipelineType::Copy {
        return false;
    }
    initialized || !live_namespaces.is_empty()
}

pub fn ensure_live_wal_namespaces_in_metadata(
    required: &[String],
    metadata: &PipelineMetadata,
    pipeline: crate::pipeline_type::PipelineType,
) -> Result<(), PromoteError> {
    if pipeline == crate::pipeline_type::PipelineType::Copy {
        return Ok(());
    }
    for namespace in required {
        if !metadata.metadata.contains_key(namespace) {
            return Err(PromoteError::SchemaMissing(namespace.clone()));
        }
    }
    Ok(())
}

pub fn live_wal_namespaces(key: &PipelineKey) -> Result<Vec<String>, PromoteError> {
    let Some(handle) = wal_handle::wal_handle_for(key) else {
        return Ok(Vec::new());
    };
    let rows = query_all(handle.catalog(), CatalogPrefix::Segment)
        .map_err(|err| PromoteError::Other(err.to_string()))?;
    let mut out = Vec::new();
    for (_, row) in rows {
        let CatalogRow::Segment(seg) = row else {
            continue;
        };
        let meta = crate::buffer::wal_handle::admit_payload(&seg.location, &seg.payload_sha256)
            .map_err(|err| {
                PromoteError::Other(format!(
                    "live WAL decode failed for {}: {err}",
                    seg.segment_id.as_str()
                ))
            })?;
        for idx in meta.index {
            if !idx.key.namespace.is_empty() {
                out.push(idx.key.namespace);
            }
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover::{Metadata, SkipprDataType};
    use std::collections::HashMap;

    fn metadata_with_namespace(namespace: &str) -> PipelineMetadata {
        let mut root = Metadata::new_with_type(SkipprDataType::Record, "");
        root.fields.insert(
            "id".to_string(),
            Metadata::new_with_type(SkipprDataType::String, "id"),
        );
        let mut pipeline = PipelineMetadata::new(&Config::new());
        pipeline.metadata = HashMap::from([(namespace.to_string(), root)]);
        pipeline.flattened = false;
        pipeline
    }

    #[test]
    fn missing_live_namespace_blocks_promotion() {
        let metadata = metadata_with_namespace("events");
        let err = ensure_live_wal_namespaces_in_metadata(
            &["other".into()],
            &metadata,
            crate::pipeline_type::PipelineType::Ingest,
        )
        .unwrap_err();
        assert!(matches!(err, PromoteError::SchemaMissing(ns) if ns == "other"));
    }

    #[test]
    fn live_namespace_in_metadata_json_allows_promotion() {
        let metadata = metadata_with_namespace("events");
        ensure_live_wal_namespaces_in_metadata(
            &["events".into()],
            &metadata,
            crate::pipeline_type::PipelineType::Ingest,
        )
        .unwrap();
    }
}
