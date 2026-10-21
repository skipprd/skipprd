//! Shared offset-store and pipeline-lease factory. Clustered and S3 consume
//! the same trait objects; core WAL/engine code does not branch on backend.

use std::sync::Arc;

use skippr_lease::{
    acquire_pipeline, renew_until_lost, FenceError, LeaseGuard, LeaseSession, NodeId, PipelineKey,
    PipelineLeaseStore, SystemClock, TokioSleeper,
};

use crate::helpers::configuration::Config;
use crate::helpers::offsets::Offsets;
use crate::helpers::wal_storage::SkipprStoreKind;
use skippr_wal_catalog::{CatalogFence, WalCatalogStore};

pub async fn open_pipeline_lease_store(
    kind: SkipprStoreKind,
    offsets: Option<&Offsets>,
    table: String,
) -> Result<Arc<dyn PipelineLeaseStore>, String> {
    match kind {
        SkipprStoreKind::Sled => {
            let offsets = offsets.ok_or_else(|| {
                "sled pipeline lease requires an open local offset database".to_string()
            })?;
            let store = offsets.open_sled_lease_store()?;
            Ok(Arc::new(store))
        }
        SkipprStoreKind::CloudTables => {
            #[cfg(feature = "offset-store-cloud-tables")]
            {
                let store = skippr_lease_store_cloud_tables::CloudTablesLeaseStore::connect(table)
                    .await
                    .map_err(|err| err.to_string())?;
                Ok(Arc::new(store))
            }
            #[cfg(not(feature = "offset-store-cloud-tables"))]
            {
                let _ = table;
                Err(
                    "skippr.store.type=cloud-tables requires --features offset-store-cloud-tables"
                        .into(),
                )
            }
        }
        SkipprStoreKind::DynamoDb => {
            #[cfg(feature = "offset-store-dynamodb")]
            {
                let store = skippr_lease_store_dynamodb::DynamoDbLeaseStore::connect(table)
                    .await
                    .map_err(|err| err.to_string())?;
                Ok(Arc::new(store))
            }
            #[cfg(not(feature = "offset-store-dynamodb"))]
            {
                let _ = table;
                Err("skippr.store.type=dynamodb requires --features offset-store-dynamodb".into())
            }
        }
    }
}

pub fn configured_kind(config: &Config) -> Result<SkipprStoreKind, String> {
    match config.configured_skippr_store() {
        Ok(Some(kind)) => Ok(kind),
        Ok(None) => Ok(SkipprStoreKind::default_for_wal(config.get_wal_storage())),
        Err(err) => Err(err.to_string()),
    }
}

/// Canonical SkipprStore type string. Host query and spawned plugins
/// both feed this to `SkipprCatalogBackend::from_store_type`.
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

pub struct AcquiredPipelineLease {
    pub store: Arc<dyn PipelineLeaseStore>,
    pub key: PipelineKey,
    pub guard: Arc<LeaseGuard>,
    pub session: LeaseSession,
}

pub async fn acquire_ingest_lease(
    config: &Config,
    offsets: &Offsets,
) -> Result<AcquiredPipelineLease, String> {
    let kind = configured_kind(config)?;
    let table = config.get_skippr_store_name();
    let store = open_pipeline_lease_store(kind, Some(offsets), table).await?;
    let key = pipeline_key(config)?;
    let clock = Arc::new(SystemClock::new());
    let sleeper = Arc::new(TokioSleeper::new(clock.clone()));
    let node = NodeId::generate();
    let session = acquire_pipeline(
        store.as_ref(),
        clock.as_ref(),
        sleeper.as_ref(),
        &key,
        &node,
    )
    .await
    .map_err(|err| format!("pipeline writer lease unavailable: {err}"))?;
    let guard = LeaseGuard::owner_elect(key.clone(), session.clone(), clock.clone());
    let renew_store = store.clone();
    let renew_key = key.clone();
    let renew_guard = guard.clone();
    let renew_clock = clock.clone();
    tokio::spawn(async move {
        renew_until_lost(
            renew_store.as_ref(),
            renew_clock.as_ref(),
            sleeper.as_ref(),
            &renew_key,
            renew_guard.as_ref(),
        )
        .await;
    });
    Ok(AcquiredPipelineLease {
        store,
        key,
        guard,
        session,
    })
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
        SkipprStoreKind::DynamoDb => {
            #[cfg(feature = "offset-store-dynamodb")]
            {
                let catalog = skippr_offset_store_dynamodb::DynamoWalCatalog::new(table, pk);
                Ok(Arc::new(catalog))
            }
            #[cfg(not(feature = "offset-store-dynamodb"))]
            {
                let _ = (table, pk);
                Err("skippr.store.type=dynamodb requires --features offset-store-dynamodb".into())
            }
        }
        SkipprStoreKind::CloudTables => {
            #[cfg(feature = "offset-store-cloud-tables")]
            {
                let catalog =
                    skippr_offset_store_cloud_tables::CloudTablesWalCatalog::new(table, pk);
                Ok(Arc::new(catalog))
            }
            #[cfg(not(feature = "offset-store-cloud-tables"))]
            {
                let _ = (table, pk);
                Err(
                    "skippr.store.type=cloud-tables requires --features offset-store-cloud-tables"
                        .into(),
                )
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

/// Discover reset: catalog rows (`wal#`) plus published offset/checkpoint
/// items. The pipeline lease row is untouched.
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

impl AcquiredPipelineLease {
    pub fn activate(&self) -> Result<(), FenceError> {
        self.guard.activate(self.session.clone())
    }

    pub async fn release_after_quiesce(&self, quiescent: bool) {
        self.guard.begin_drain();
        if quiescent {
            if let Some(session) = self.guard.leased_session() {
                let _ = self.store.release_after_drain(&self.key, &session).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helpers::wal_storage::SkipprStoreKind;
    use serde_json::json;
    use serial_test::serial;

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
    fn discover_reset_keeps_pipeline_leases_and_drops_catalog_and_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let db = sled::open(dir.path()).unwrap();
        let lease = db.open_tree("pipeline_leases").unwrap();
        lease.insert(b"lease-row", b"held").unwrap();
        let offsets = Offsets::from_local_db_for_test(db);
        let fence = CatalogFence {
            owner: skippr_lease::NodeId::single_node(),
            epoch: skippr_lease::LeaseEpoch::SINGLE_NODE,
        };
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
        assert_eq!(
            lease.get(b"lease-row").unwrap().as_deref(),
            Some(b"held".as_slice())
        );
    }
}
