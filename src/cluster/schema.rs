use skippr_lease::{PipelineKey, PromoteError};

use crate::buffer::wal_reader;
use crate::discover::PipelineMetadata;
use crate::helpers::configuration::Config;
use crate::runtime_plugins::schema_state::install_from_pipeline_metadata;

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
    let Some(reader) = wal_reader::wal_reader_for(key) else {
        return Err(PromoteError::Other(format!(
            "WAL catalog for pipeline {} is not open",
            key.pipeline()
        )));
    };
    let mut out = reader
        .live_namespaces()
        .map_err(|err| PromoteError::Other(err.to_string()))?;
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

    #[test]
    fn live_wal_namespaces_use_catalog_index_not_payload_decode() {
        let src = include_str!("schema.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        assert!(
            !prod.contains("admit_payload"),
            "promote live namespaces must not decode payload bytes"
        );
        assert!(
            prod.contains("live_namespaces"),
            "promote live namespaces come from WalCatalogReader"
        );
        assert!(
            !prod.contains("Ok(Vec::new())"),
            "missing catalog must fail closed, not skip Skippr Store"
        );
    }

    #[test]
    #[serial_test::serial]
    fn live_wal_namespaces_fails_when_catalog_is_not_open() {
        use crate::buffer::wal_handle::remove_wal_handle;
        use crate::buffer::wal_reader::clear_query_readers;

        let key = PipelineKey::new("t", "w", "schema-no-catalog").unwrap();
        remove_wal_handle(&key);
        clear_query_readers();
        let err = live_wal_namespaces(&key).unwrap_err();
        assert!(
            err.to_string().contains("not open"),
            "missing catalog must fail closed, got {err}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn live_wal_namespaces_from_query_reader() {
        use crate::buffer::wal_handle::remove_wal_handle;
        use crate::buffer::wal_reader::{
            clear_query_readers, install_query_reader, reader_with_live_slice,
        };

        let key = PipelineKey::new("t", "w", "schema-query-ns").unwrap();
        remove_wal_handle(&key);
        clear_query_readers();
        install_query_reader(reader_with_live_slice(key.clone(), "events", "live-seg")).unwrap();
        assert_eq!(
            live_wal_namespaces(&key).unwrap(),
            vec!["events".to_string()]
        );
        clear_query_readers();
    }
}
