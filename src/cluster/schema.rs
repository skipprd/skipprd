use skippr_lease::{PipelineKey, PipelinePaths, PromoteError};

use crate::buffer::durable::log::MutationLog;
use crate::discover::PipelineMetadata;
use crate::helpers::configuration::Config;
use crate::runtime_plugins::schema_state::install_from_pipeline_metadata;

pub async fn load_and_install_pipeline_schema(
    key: &PipelineKey,
    paths: &PipelinePaths,
    log: &MutationLog,
    flatten: bool,
    initialized: bool,
) -> Result<(), PromoteError> {
    let required = live_wal_namespaces(paths, log, key)?;
    let mut metadata = match Config::load_pipeline_metadata(key).await {
        Ok(Some(metadata)) => metadata,
        Ok(None) => {
            if missing_metadata_is_fatal(initialized, &required) {
                return Err(PromoteError::SchemaMissing("metadata.json".into()));
            }
            return Ok(());
        }
        Err(err) => return Err(PromoteError::Other(err)),
    };
    metadata.flattened = flatten;
    let _ = metadata.migrate_persisted_metadata();
    install_from_pipeline_metadata(&metadata);
    ensure_live_wal_namespaces_in_metadata(&required, &metadata)
}

pub fn missing_metadata_is_fatal(initialized: bool, live_namespaces: &[String]) -> bool {
    initialized || !live_namespaces.is_empty()
}

pub fn ensure_live_wal_namespaces_in_metadata(
    required: &[String],
    metadata: &PipelineMetadata,
) -> Result<(), PromoteError> {
    for namespace in required {
        if !metadata.metadata.contains_key(namespace) {
            return Err(PromoteError::SchemaMissing(namespace.clone()));
        }
    }
    Ok(())
}

pub fn live_wal_namespaces(
    paths: &PipelinePaths,
    log: &MutationLog,
    key: &PipelineKey,
) -> Result<Vec<String>, PromoteError> {
    let snapshot = crate::buffer::durable::snapshot::clustered_compaction_sot(paths, log, key)
        .map_err(|err| PromoteError::Other(err.to_string()))?;
    let mut out = Vec::new();
    for descriptor in &snapshot.segments {
        let id = skippr_lease::SegmentId::new(&descriptor.segment_id)
            .map_err(|err| PromoteError::Other(err.to_string()))?;
        let path = paths.segment(&id);
        let seg = crate::buffer::segment_file::SegmentFile { path };
        let meta = seg.read_metadata_durable().map_err(|err| {
            PromoteError::Other(format!(
                "live WAL decode failed for {}: {err}",
                descriptor.segment_id
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
        let mut pipeline = PipelineMetadata::new();
        pipeline.metadata = HashMap::from([(namespace.to_string(), root)]);
        pipeline.flattened = false;
        pipeline
    }

    #[test]
    fn missing_live_namespace_blocks_promotion() {
        let metadata = metadata_with_namespace("events");
        let err = ensure_live_wal_namespaces_in_metadata(&["other".into()], &metadata).unwrap_err();
        assert!(matches!(err, PromoteError::SchemaMissing(ns) if ns == "other"));
    }

    #[test]
    fn live_namespace_in_metadata_json_allows_promotion() {
        let metadata = metadata_with_namespace("events");
        ensure_live_wal_namespaces_in_metadata(&["events".into()], &metadata).unwrap();
        ensure_live_wal_namespaces_in_metadata(&[], &metadata).unwrap();
    }

    #[test]
    fn missing_metadata_json_is_fatal_once_initialized_or_live_wal_exists() {
        assert!(!missing_metadata_is_fatal(false, &[]));
        assert!(missing_metadata_is_fatal(true, &[]));
        assert!(missing_metadata_is_fatal(false, &["events".into()]));
        assert!(missing_metadata_is_fatal(true, &["events".into()]));
    }
}
