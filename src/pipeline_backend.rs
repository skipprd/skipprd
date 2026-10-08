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
}
