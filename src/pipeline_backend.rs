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
    kind: SkipprStoreKind,
    offsets: Option<&Offsets>,
    table: String,
    pk: String,
    fence: CatalogFence,
) -> Result<Arc<dyn WalCatalogStore>, String> {
    match kind {
        SkipprStoreKind::Sled => {
            let _ = (table, pk);
            let offsets = offsets.ok_or_else(|| {
                "sled WAL catalog requires an open local offset database".to_string()
            })?;
            let catalog = offsets.open_sled_wal_catalog(fence)?;
            Ok(Arc::new(catalog))
        }
        SkipprStoreKind::DynamoDb => open_shared_wal_catalog(kind, table, pk),
    }
}

fn open_shared_wal_catalog(
    kind: SkipprStoreKind,
    table: String,
    pk: String,
) -> Result<Arc<dyn WalCatalogStore>, String> {
    match kind {
        SkipprStoreKind::Sled => Err("sled WAL catalog is local, not a shared store".into()),
        SkipprStoreKind::DynamoDb => {
            #[cfg(feature = "offset-store-dynamodb")]
            {
                Ok(Arc::new(
                    skippr_offset_store_dynamodb::DynamoWalCatalog::new(table, pk),
                ))
            }
            #[cfg(not(feature = "offset-store-dynamodb"))]
            {
                let _ = (table, pk);
                Err("skippr.store.type=dynamodb requires --features offset-store-dynamodb".into())
            }
        }
    }
}

pub fn open_wal_catalog_for_config(
    config: &Config,
    offsets: Option<&Offsets>,
    fence: CatalogFence,
) -> Result<Arc<dyn WalCatalogStore>, String> {
    open_wal_catalog(
        configured_kind(config)?,
        offsets,
        config.get_skippr_store_name(),
        config.offset_store_partition_key(),
        fence,
    )
}

/// Same Skippr Store kind/table as ingest. Sled is `open_read` (commits are
/// fenced). DynamoDB is constructed as `WalCatalogRead` only.
pub fn open_wal_catalog_reader_for_config(
    config: &Config,
    offsets: Option<&Offsets>,
) -> Result<Arc<dyn WalCatalogRead>, String> {
    match configured_kind(config)? {
        SkipprStoreKind::Sled => {
            let offsets = offsets.ok_or_else(|| {
                "sled WAL catalog requires an open local offset database".to_string()
            })?;
            Ok(Arc::new(offsets.open_sled_wal_catalog_read()?))
        }
        SkipprStoreKind::DynamoDb => {
            let _ = offsets;
            let store = open_shared_wal_catalog(
                SkipprStoreKind::DynamoDb,
                config.get_skippr_store_name(),
                config.offset_store_partition_key(),
            )?;
            Ok(store as Arc<dyn WalCatalogRead>)
        }
    }
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
    fn disk_without_offset_store_kind_is_sled() {
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
        assert_eq!(configured_kind(&config).unwrap(), SkipprStoreKind::Sled);
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
    fn sled_wal_catalog_opens_from_the_same_db_as_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let db = sled::open(dir.path()).unwrap();
        let offsets = Offsets::from_local_db_for_test(db);
        let fence = CatalogFence {
            owner: skippr_lease::NodeId::generate(),
            epoch: skippr_lease::LeaseEpoch::new(1),
        };
        let catalog = open_wal_catalog(
            SkipprStoreKind::Sled,
            Some(&offsets),
            String::new(),
            String::new(),
            fence,
        )
        .unwrap();
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
        let dir = tempfile::tempdir().unwrap();
        let db = sled::open(dir.path()).unwrap();
        let offsets = Offsets::from_local_db_for_test(db);
        let fence = CatalogFence::local();
        let catalog = offsets.open_sled_wal_catalog(fence).unwrap();
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
            body.contains("open_sled_wal_catalog_read"),
            "sled query open must use open_read"
        );
        assert!(
            !body.contains("CatalogFence"),
            "query opener must not build a commit fence"
        );
        assert!(
            !body.contains("open_wal_catalog("),
            "query opener must not call the writer factory"
        );
        assert!(
            body.contains("open_shared_wal_catalog"),
            "DynamoDB reader open shares the fence-free constructor"
        );
    }

    #[test]
    fn sled_query_catalog_read_rejects_commit() {
        let dir = tempfile::tempdir().unwrap();
        let db = sled::open(dir.path()).unwrap();
        let offsets = Offsets::from_local_db_for_test(db);
        let catalog = offsets.open_sled_wal_catalog_read().unwrap();
        let fence = CatalogFence {
            owner: skippr_lease::NodeId::generate(),
            epoch: skippr_lease::LeaseEpoch::new(1),
        };
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
        let err = catalog.commit(&txn, fence).unwrap_err();
        assert!(
            matches!(err, skippr_wal_catalog::CatalogError::Fenced),
            "query-opened sled catalog must not commit, got {err}"
        );
    }
}
