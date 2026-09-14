//! Shared offset-store and pipeline-lease factory. Clustered and S3 consume
//! the same trait objects; core WAL/engine code does not branch on backend.

use std::sync::Arc;

use skippr_lease::{
    acquire_pipeline, renew_until_lost, FenceError, LeaseGuard, LeaseSession, NodeId, PipelineKey,
    PipelineLeaseStore, SystemClock, TokioSleeper,
};

use crate::helpers::configuration::Config;
use crate::helpers::offsets::Offsets;
use crate::helpers::wal_storage::OffsetStoreKind;

pub async fn open_pipeline_lease_store(
    kind: OffsetStoreKind,
    offsets: Option<&Offsets>,
    table: String,
) -> Result<Arc<dyn PipelineLeaseStore>, String> {
    match kind {
        OffsetStoreKind::Sled => {
            let offsets = offsets.ok_or_else(|| {
                "sled pipeline lease requires an open local offset database".to_string()
            })?;
            let store = offsets.open_sled_lease_store()?;
            Ok(Arc::new(store))
        }
        OffsetStoreKind::CloudTables => {
            #[cfg(feature = "offset-store-cloud-tables")]
            {
                let store = skippr_store_cloud_tables::CloudTablesLeaseStore::connect(table)
                    .await
                    .map_err(|err| err.to_string())?;
                Ok(Arc::new(store))
            }
            #[cfg(not(feature = "offset-store-cloud-tables"))]
            {
                let _ = table;
                Err(
                    "SKIPPR_OFFSET_STORE=cloud-tables requires --features offset-store-cloud-tables"
                        .into(),
                )
            }
        }
        OffsetStoreKind::DynamoDb => {
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
                Err("SKIPPR_OFFSET_STORE=dynamodb requires --features offset-store-dynamodb".into())
            }
        }
    }
}

pub fn configured_kind() -> Result<OffsetStoreKind, String> {
    match Config::configured_offset_store() {
        Ok(Some(kind)) => Ok(kind),
        Ok(None) => {
            if matches!(
                Config::get_wal_storage(),
                crate::helpers::wal_storage::WalStorage::Clustered
            ) {
                if cfg!(feature = "offset-store-dynamodb") {
                    Ok(OffsetStoreKind::DynamoDb)
                } else {
                    Ok(OffsetStoreKind::CloudTables)
                }
            } else {
                Ok(OffsetStoreKind::Sled)
            }
        }
        Err(err) => Err(err.to_string()),
    }
}

pub fn pipeline_key() -> Result<PipelineKey, String> {
    PipelineKey::new(
        Config::get_tenant(),
        Config::get_workspace_name(),
        Config::get_pipeline_name(),
    )
    .map_err(|err| err.to_string())
}

pub struct AcquiredPipelineLease {
    pub store: Arc<dyn PipelineLeaseStore>,
    pub key: PipelineKey,
    pub guard: Arc<LeaseGuard>,
    pub session: LeaseSession,
}

pub async fn acquire_ingest_lease(offsets: &Offsets) -> Result<AcquiredPipelineLease, String> {
    let kind = configured_kind()?;
    let table = Config::get_offset_dynamodb_table();
    let store = open_pipeline_lease_store(kind, Some(offsets), table).await?;
    let key = pipeline_key()?;
    let clock = Arc::new(SystemClock::new());
    let sleeper = TokioSleeper::new(clock.clone());
    let node = NodeId::generate();
    let session = acquire_pipeline(store.as_ref(), clock.as_ref(), &sleeper, &key, &node)
        .await
        .map_err(|err| format!("pipeline writer lease unavailable: {err}"))?;
    let guard = LeaseGuard::owner_elect(key.clone(), session.clone(), clock.clone());
    crate::buffer::wal_store::install_pipeline_lease(guard.clone());
    let renew_store = store.clone();
    let renew_key = key.clone();
    let renew_guard = guard.clone();
    let renew_clock = clock.clone();
    tokio::spawn(async move {
        renew_until_lost(
            renew_store.as_ref(),
            renew_clock.as_ref(),
            &sleeper,
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
        crate::buffer::wal_store::clear_pipeline_lease();
    }
}
