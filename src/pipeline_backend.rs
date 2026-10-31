//! Shared SkipprStore factory. Disk and S3 consume the same trait objects;
//! core WAL/engine code does not branch on backend.

use std::sync::Arc;

use skippr_lease::PipelineKey;

use crate::helpers::configuration::Config;
use crate::helpers::offsets::Offsets;
use crate::helpers::wal_storage::SkipprStoreKind;
use skippr_wal_catalog::{CatalogFence, WalCatalogRead, WalCatalogStore};

pub fn configured_kind(config: &Config) -> Result<SkipprStoreKind, String> {
    match config.configured_skippr_store() {
        Ok(Some(kind)) => Ok(kind),
        Ok(None) => Ok(SkipprStoreKind::default_for_wal(config.get_wal_storage())),
        Err(err) => Err(err.to_string()),
    }
}

/// Canonical SkipprStore type string injected as `SKIPPR_STORE_TYPE` for plugins.
pub fn skippr_store_type_value(config: &Config) -> String {
    match configured_kind(config) {
        Ok(kind) => kind.as_str().to_string(),
        Err(_) => String::new(),
    }
}

pub fn pipeline_key(config: &Config) -> Result<PipelineKey, String> {
    PipelineKey::new(
        config.get_tenant(),
        config.get_workspace_name(),
        config.get_pipeline_name(),
    )
    .map_err(|err| err.to_string())
}

pub fn open_wal_catalog(
    offsets: Option<&Offsets>,
    fence: CatalogFence,
) -> Result<Arc<dyn WalCatalogStore>, String> {
    let offsets = offsets.ok_or_else(|| "WAL catalog requires an open Skippr Store".to_string())?;
    offsets.open_wal_catalog(fence)
}

pub fn open_wal_catalog_for_config(
    config: &Config,
    offsets: Option<&Offsets>,
    fence: CatalogFence,
) -> Result<Arc<dyn WalCatalogStore>, String> {
    let _ = configured_kind(config)?;
    open_wal_catalog(offsets, fence)
}

pub fn open_wal_catalog_reader_for_config(
    config: &Config,
    offsets: Option<&Offsets>,
) -> Result<Arc<dyn WalCatalogRead>, String> {
    let _ = config;
    let offsets = offsets.ok_or_else(|| "WAL catalog requires an open Skippr Store".to_string())?;
    Ok(offsets.open_wal_catalog_read())
}

/// Discover reset: catalog rows (`wal#`) plus published offset/checkpoint items.
pub fn reset_pipeline_state(
    config: &Config,
    offsets: &Offsets,
    fence: CatalogFence,
) -> Result<(), String> {
    let catalog = open_wal_catalog_for_config(config, Some(offsets), fence)?;
    skippr_wal_catalog::reset_catalog(catalog.as_ref(), fence).map_err(|err| err.to_string())?;
    if let Some(store) = offsets.bytes_store() {
        store.reset_published_ingest()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helpers::wal_storage::SkipprStoreKind;
    use serde_json::json;
    use serial_test::serial;
    use skippr_wal_catalog::WalCatalogRead;

    #[test]
    #[serial]
    fn disk_without_offset_store_kind_is_fjall() {
        let old_store = std::env::var("SKIPPR_STORE_TYPE").ok();
        let old_wal = std::env::var("WAL_STORAGE").ok();
        std::env::remove_var("SKIPPR_STORE_TYPE");
        Config::set_evncache("SKIPPR_STORE_TYPE", "");
        Config::set_wal_storage("disk");
        let config: Config = serde_json::from_value(json!({
            "skippr": { "workspace": "ws-a", "tenant": "ten-a" },
            "pipelines": {
                "orders": { "data_source": "data_sources.sample" }
            },
            "data_sources": {
                "sample": { "S3": { "s3_bucket": "b", "s3_prefix": "p" } }
            }
        }))
        .unwrap();
        assert_eq!(configured_kind(&config).unwrap(), SkipprStoreKind::Fjall);
        if let Some(value) = old_store {
            Config::set_skippr_store_type(&value);
        } else {
            std::env::remove_var("SKIPPR_STORE_TYPE");
            Config::set_evncache("SKIPPR_STORE_TYPE", "");
        }
        if let Some(value) = old_wal {
            Config::set_wal_storage(&value);
        } else {
            std::env::remove_var("WAL_STORAGE");
            Config::set_evncache("WAL_STORAGE", "");
        }
    }

    #[test]
    fn fjall_wal_catalog_opens_from_the_same_store_as_offsets() {
        let offsets = Offsets::from_memory_for_test();
        let fence = offsets.writer_fence().expect("memory store fence");
        let catalog = open_wal_catalog(Some(&offsets), fence).unwrap();
        let mut txn = skippr_wal_catalog::CatalogTxn::new();
        txn.put(
            skippr_wal_catalog::CatalogKey::Format,
            skippr_wal_catalog::CatalogRow::Format(skippr_wal_catalog::FormatRow {
                version: skippr_wal_catalog::FORMAT_VERSION,
                phase: skippr_wal_catalog::FormatPhase::Clean,
                row_version: 0,
            }),
            skippr_wal_catalog::PutCond::NotExists,
        )
        .unwrap();
        catalog.commit(&txn, fence).unwrap();
        assert!(matches!(
            catalog
                .get(&skippr_wal_catalog::CatalogKey::Format)
                .unwrap(),
            Some(skippr_wal_catalog::CatalogRow::Format(_))
        ));
    }

    #[test]
    fn discover_reset_drops_catalog_and_offsets() {
        let offsets = Offsets::from_memory_for_test();
        let fence = CatalogFence::local();
        let catalog = offsets.open_wal_catalog(fence).unwrap();
        let mut txn = skippr_wal_catalog::CatalogTxn::new();
        txn.put(
            skippr_wal_catalog::CatalogKey::Format,
            skippr_wal_catalog::CatalogRow::Format(skippr_wal_catalog::FormatRow {
                version: skippr_wal_catalog::FORMAT_VERSION,
                phase: skippr_wal_catalog::FormatPhase::Clean,
                row_version: 0,
            }),
            skippr_wal_catalog::PutCond::NotExists,
        )
        .unwrap();
        catalog.commit(&txn, fence).unwrap();
        offsets
            .seed_published_offset(
                &crate::helpers::offsets::OffsetKey::new("ns", "p"),
                1,
                2,
                0,
                1,
            )
            .unwrap();
        skippr_wal_catalog::reset_catalog(&catalog, fence).unwrap();
        offsets
            .bytes_store()
            .unwrap()
            .reset_published_ingest()
            .unwrap();
        assert!(catalog
            .get(&skippr_wal_catalog::CatalogKey::Format)
            .unwrap()
            .is_none());
        assert!(offsets
            .get(&crate::helpers::offsets::OffsetKey::new("ns", "p"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn query_reader_opener_does_not_construct_a_commit_fence() {
        let src = include_str!("pipeline_backend.rs");
        let start = src
            .find("pub fn open_wal_catalog_reader_for_config")
            .expect("open_wal_catalog_reader_for_config");
        let body = src[start..]
            .split("pub fn reset_pipeline_state")
            .next()
            .expect("reader opener body");
        assert!(
            body.contains("open_wal_catalog_read"),
            "query open must use the reader facade"
        );
        assert!(
            !body.contains("CatalogFence"),
            "query opener must not build a commit fence"
        );
        assert!(
            !body.contains("open_wal_catalog("),
            "query opener must not call the writer factory"
        );
    }
}
