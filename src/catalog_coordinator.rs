use std::collections::{BTreeMap, HashMap};
use std::io;
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aws_sdk_glue::error::SdkError;
use aws_sdk_glue::operation::get_partition::GetPartitionError;
use aws_sdk_glue::types::{Column, PartitionInput, SerDeInfo, StorageDescriptor};
use aws_sdk_glue::Client as GlueClient;
use aws_types::region::Region;
use once_cell::sync::Lazy;
use rand::Rng;
use serde::Deserialize;
use tokio::sync::{Notify, Semaphore};

use crate::catalog_outbox::{CatalogOutbox, ConditionalMutationResult, PendingCatalogIntent};
use crate::runtime_plugins::protocol::{CatalogIntent, CatalogIntentKind};

const GLUE_BATCH_CREATE_LIMIT: usize = 100;
const RECOVERY_SCAN_LIMIT: usize = 10_000;

static CATALOG_COORDINATORS: Lazy<std::sync::Mutex<HashMap<String, Weak<CatalogCoordinator>>>> =
    Lazy::new(|| std::sync::Mutex::new(HashMap::new()));
static CATALOG_OPERATION_BUDGET: Lazy<Arc<Semaphore>> = Lazy::new(|| {
    Arc::new(Semaphore::new(
        crate::ingest::tuner::current_flush_budget()
            .catalog_operations
            .max(1),
    ))
});

#[derive(Clone, Debug, Deserialize)]
struct GlueColumnIntent {
    name: String,
    r#type: String,
    comment: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct GluePartitionCatalogIntentV1 {
    version: u32,
    region: Option<String>,
    catalog_id: Option<String>,
    database: String,
    table: String,
    partition_values: Vec<String>,
    location: String,
    storage_columns: Vec<GlueColumnIntent>,
    partition_columns: Vec<GlueColumnIntent>,
    input_format: String,
    output_format: String,
    serde_library: String,
    schema_namespace: String,
    schema_version: u64,
}

#[derive(Clone)]
struct DecodedIntent {
    pending: PendingCatalogIntent,
    payload: GluePartitionCatalogIntentV1,
}

type GlueIntentGroupKey = (Option<String>, Option<String>, String, String);
type GlueIntentGroups = BTreeMap<GlueIntentGroupKey, Vec<DecodedIntent>>;

pub struct CatalogCoordinator {
    outbox: Arc<CatalogOutbox>,
    notify: Notify,
    table_layout_cache: tokio::sync::RwLock<HashMap<String, Vec<String>>>,
    worker_abort: std::sync::Mutex<Option<tokio::task::AbortHandle>>,
}

impl CatalogCoordinator {
    pub fn for_pipeline_data_dir(path: impl AsRef<std::path::Path>) -> io::Result<Arc<Self>> {
        let key = path.as_ref().to_string_lossy().into_owned();
        let mut coordinators = CATALOG_COORDINATORS
            .lock()
            .expect("catalog coordinator registry poisoned");
        if let Some(existing) = coordinators.get(&key).and_then(Weak::upgrade) {
            return Ok(existing);
        }
        let coordinator = Arc::new(Self {
            outbox: Arc::new(CatalogOutbox::open(path)?),
            notify: Notify::new(),
            table_layout_cache: tokio::sync::RwLock::new(HashMap::new()),
            worker_abort: std::sync::Mutex::new(None),
        });
        coordinator.start_worker();
        coordinators.insert(key, Arc::downgrade(&coordinator));
        Ok(coordinator)
    }

    pub fn persist(&self, intents: &[CatalogIntent]) -> io::Result<()> {
        let summary = self.outbox.persist(intents)?;
        if summary.inserted + summary.updated > 0 {
            self.notify.notify_one();
        }
        crate::metrics::counters::refresh_catalog_outbox_metrics(&self.outbox)?;
        Ok(())
    }

    pub async fn drain_until_idle(&self, timeout: Duration) -> io::Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.outbox.scan_pending(1)?.is_empty() {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "catalog outbox drain timed out with pending intents",
                ));
            }
            self.drain_once().await?;
            tokio::time::sleep(remaining.min(Duration::from_millis(100))).await;
        }
    }

    async fn drain_once(&self) -> io::Result<()> {
        let now = now_ms();
        let mut decoded = Vec::new();
        for pending in self
            .outbox
            .scan_eligible_pending(now, RECOVERY_SCAN_LIMIT)?
        {
            if pending.intent.identity.kind != CatalogIntentKind::UpsertPartition {
                self.record_terminal_invalid(&pending, "unsupported catalog intent kind")?;
                continue;
            }
            let payload: GluePartitionCatalogIntentV1 =
                match serde_json::from_str(&pending.intent.payload_json) {
                    Ok(payload) => payload,
                    Err(err) => {
                        self.record_terminal_invalid(
                            &pending,
                            format!("invalid durable Glue partition intent payload: {err}"),
                        )?;
                        continue;
                    }
                };
            if payload.version != 1
                || payload.database.trim().is_empty()
                || payload.table.trim().is_empty()
                || payload.partition_values.len() != payload.partition_columns.len()
                || payload.schema_namespace != pending.intent.identity.namespace
                || payload.schema_version == 0
            {
                self.record_terminal_invalid(&pending, "terminal invalid Glue partition intent")?;
                continue;
            }
            decoded.push(DecodedIntent { pending, payload });
        }

        let mut groups: GlueIntentGroups = BTreeMap::new();
        for intent in decoded {
            groups
                .entry((
                    intent.payload.region.clone(),
                    intent.payload.catalog_id.clone(),
                    intent.payload.database.clone(),
                    intent.payload.table.clone(),
                ))
                .or_default()
                .push(intent);
        }
        for ((region, catalog_id, database, table), intents) in groups {
            self.drain_group(region, catalog_id, database, table, intents)
                .await?;
        }
        crate::metrics::counters::refresh_catalog_outbox_metrics(&self.outbox)?;
        Ok(())
    }

    async fn drain_group(
        &self,
        region: Option<String>,
        catalog_id: Option<String>,
        database: String,
        table: String,
        intents: Vec<DecodedIntent>,
    ) -> io::Result<()> {
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
        if let Some(region) = region {
            loader = loader.region(Region::new(region));
        }
        let client = GlueClient::new(&loader.load().await);
        if let Some(first) = intents.first() {
            if let Err(error) = self
                .ensure_table_layout(&client, catalog_id.as_deref(), &database, &table, first)
                .await
            {
                for intent in &intents {
                    self.record_aws_failure(&intent.pending, error.clone())?;
                }
                return Ok(());
            }
        }
        let mut missing = Vec::new();
        for intent in intents {
            let mut get = client
                .get_partition()
                .database_name(&database)
                .table_name(&table)
                .set_partition_values(Some(intent.payload.partition_values.clone()));
            if let Some(catalog_id) = catalog_id.as_ref() {
                get = get.catalog_id(catalog_id);
            }
            let get_result = {
                let _permit = CATALOG_OPERATION_BUDGET
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|_| io::Error::other("catalog operation budget closed"))?;
                get.send().await
            };
            match get_result {
                Ok(existing) => {
                    let current = existing
                        .partition()
                        .and_then(|partition| partition.storage_descriptor())
                        .and_then(|descriptor| descriptor.location())
                        .unwrap_or_default();
                    if current == intent.payload.location {
                        self.outbox.mark_delivered_if(&intent.pending)?;
                    } else {
                        self.update_partition(
                            &client,
                            catalog_id.as_deref(),
                            &database,
                            &table,
                            &intent,
                        )
                        .await?;
                    }
                }
                Err(SdkError::ServiceError(err))
                    if matches!(err.err(), GetPartitionError::EntityNotFoundException(_)) =>
                {
                    missing.push(intent);
                }
                Err(err) => self.record_aws_failure(&intent.pending, err.to_string())?,
            }
        }

        for batch in missing.chunks(GLUE_BATCH_CREATE_LIMIT) {
            let _permit = CATALOG_OPERATION_BUDGET
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| io::Error::other("catalog operation budget closed"))?;
            let inputs = batch
                .iter()
                .map(|intent| partition_input(&intent.payload))
                .collect::<io::Result<Vec<_>>>()?;
            let mut create = client
                .batch_create_partition()
                .database_name(&database)
                .table_name(&table)
                .set_partition_input_list(Some(inputs));
            if let Some(catalog_id) = catalog_id.as_ref() {
                create = create.catalog_id(catalog_id);
            }
            match create.send().await {
                Ok(output) if output.errors().is_empty() => {
                    for intent in batch {
                        self.outbox.mark_delivered_if(&intent.pending)?;
                    }
                    crate::metrics::counters::add_catalog_successful_batch(1);
                }
                Ok(output) => {
                    let message = format!("BatchCreatePartition errors: {:?}", output.errors());
                    for intent in batch {
                        self.record_aws_failure(&intent.pending, message.clone())?;
                    }
                }
                Err(err) => {
                    for intent in batch {
                        self.record_aws_failure(&intent.pending, err.to_string())?;
                    }
                }
            }
        }
        Ok(())
    }

    async fn ensure_table_layout(
        &self,
        client: &GlueClient,
        catalog_id: Option<&str>,
        database: &str,
        table: &str,
        intent: &DecodedIntent,
    ) -> Result<(), String> {
        let cache_key = format!(
            "{}\0{}\0{}\0{}\0{}",
            catalog_id.unwrap_or_default(),
            database,
            table,
            intent.payload.schema_namespace,
            intent.payload.schema_version
        );
        if self
            .table_layout_cache
            .read()
            .await
            .contains_key(&cache_key)
        {
            return Ok(());
        }
        let _permit = CATALOG_OPERATION_BUDGET
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| "catalog operation budget closed".to_string())?;
        let mut request = client.get_table().database_name(database).name(table);
        if let Some(catalog_id) = catalog_id {
            request = request.catalog_id(catalog_id);
        }
        let table_output = request.send().await.map_err(|err| err.to_string())?;
        let actual = table_output
            .table()
            .and_then(|table| table.partition_keys.clone())
            .unwrap_or_default()
            .into_iter()
            .map(|column| column.name().to_string())
            .collect::<Vec<_>>();
        let expected = intent
            .payload
            .partition_columns
            .iter()
            .map(|column| column.name.clone())
            .collect::<Vec<_>>();
        if actual != expected {
            return Err(format!(
                "InvalidInputException: Glue partition layout mismatch for '{database}.{table}': actual={actual:?} expected={expected:?}"
            ));
        }
        let namespace_prefix = format!(
            "{}\0{}\0{}\0{}\0",
            catalog_id.unwrap_or_default(),
            database,
            table,
            intent.payload.schema_namespace
        );
        let mut cache = self.table_layout_cache.write().await;
        cache.retain(|key, _| !key.starts_with(&namespace_prefix));
        cache.insert(cache_key, actual);
        Ok(())
    }

    async fn update_partition(
        &self,
        client: &GlueClient,
        catalog_id: Option<&str>,
        database: &str,
        table: &str,
        intent: &DecodedIntent,
    ) -> io::Result<()> {
        let _permit = CATALOG_OPERATION_BUDGET
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| io::Error::other("catalog operation budget closed"))?;
        let mut update = client
            .update_partition()
            .database_name(database)
            .table_name(table)
            .set_partition_value_list(Some(intent.payload.partition_values.clone()))
            .partition_input(partition_input(&intent.payload)?);
        if let Some(catalog_id) = catalog_id {
            update = update.catalog_id(catalog_id);
        }
        match update.send().await {
            Ok(_) => {
                if self.outbox.mark_delivered_if(&intent.pending)?
                    == ConditionalMutationResult::Applied
                {
                    crate::metrics::counters::add_catalog_location_update(1);
                }
            }
            Err(err) => self.record_aws_failure(&intent.pending, err.to_string())?,
        }
        Ok(())
    }

    fn record_aws_failure(&self, pending: &PendingCatalogIntent, error: String) -> io::Result<()> {
        let transient = is_transient(&error);
        let delay = transient.then(|| retry_delay(pending.attempts));
        if self
            .outbox
            .record_failure_if(pending, &error, delay, !transient)?
            == ConditionalMutationResult::Applied
        {
            if transient {
                crate::metrics::counters::add_catalog_retry(1);
            } else {
                crate::metrics::counters::add_catalog_terminal_failure(1);
            }
        }
        Ok(())
    }

    fn record_terminal_invalid(
        &self,
        pending: &PendingCatalogIntent,
        error: impl Into<String>,
    ) -> io::Result<()> {
        if self.outbox.record_failure_if(pending, error, None, true)?
            == ConditionalMutationResult::Applied
        {
            crate::metrics::counters::add_catalog_terminal_failure(1);
        }
        Ok(())
    }

    fn start_worker(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let task = tokio::spawn(async move {
            loop {
                let Some(coordinator) = weak.upgrade() else {
                    return;
                };
                if let Err(err) = coordinator.drain_once().await {
                    tracing::error!("catalog outbox drain failed closed: {err}");
                }
                tokio::select! {
                    _ = coordinator.notify.notified() => {}
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
            }
        });
        *self
            .worker_abort
            .lock()
            .expect("catalog coordinator worker lock poisoned") = Some(task.abort_handle());
    }
}

impl Drop for CatalogCoordinator {
    fn drop(&mut self) {
        if let Some(abort) = self
            .worker_abort
            .lock()
            .expect("catalog coordinator worker lock poisoned")
            .take()
        {
            abort.abort();
        }
    }
}

pub async fn drain_catalog_outboxes(timeout: Duration) -> io::Result<()> {
    let coordinators = CATALOG_COORDINATORS
        .lock()
        .expect("catalog coordinator registry poisoned")
        .values()
        .filter_map(Weak::upgrade)
        .collect::<Vec<_>>();
    let deadline = tokio::time::Instant::now() + timeout;
    for coordinator in coordinators {
        coordinator
            .drain_until_idle(deadline.saturating_duration_since(tokio::time::Instant::now()))
            .await?;
    }
    Ok(())
}

fn partition_input(payload: &GluePartitionCatalogIntentV1) -> io::Result<PartitionInput> {
    let columns = payload
        .storage_columns
        .iter()
        .map(|column| {
            Column::builder()
                .name(&column.name)
                .set_type(Some(column.r#type.clone()))
                .set_comment(column.comment.clone())
                .build()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))
        })
        .collect::<io::Result<Vec<_>>>()?;
    Ok(PartitionInput::builder()
        .set_values(Some(payload.partition_values.clone()))
        .parameters("parquet.compression", "SNAPPY")
        .storage_descriptor(
            StorageDescriptor::builder()
                .set_columns(Some(columns))
                .compressed(true)
                .input_format(&payload.input_format)
                .location(&payload.location)
                .output_format(&payload.output_format)
                .serde_info(
                    SerDeInfo::builder()
                        .parameters("serialization.format", "1")
                        .serialization_library(&payload.serde_library)
                        .build(),
                )
                .stored_as_sub_directories(true)
                .build(),
        )
        .build())
}

fn is_transient(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    [
        "throttl",
        "timeout",
        "temporar",
        "service unavailable",
        "internalservice",
        "concurrentmodification",
        "connection",
        "alreadyexist",
        "entitynotfound",
    ]
    .iter()
    .any(|needle| error.contains(needle))
}

fn retry_delay(attempts: u32) -> Duration {
    let base_ms = 250u64.saturating_mul(1u64 << attempts.min(8));
    let jitter = rand::thread_rng().gen_range(0..=base_ms / 4);
    Duration::from_millis((base_ms + jitter).min(60_000))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_plugins::protocol::{CatalogIntentIdentity, CATALOG_INTENT_VERSION};

    #[test]
    fn glue_batch_limit_and_backoff_are_bounded() {
        assert_eq!(GLUE_BATCH_CREATE_LIMIT, 100);
        assert!(retry_delay(30) <= Duration::from_secs(60));
        assert!(is_transient("ThrottlingException"));
        assert!(is_transient("AlreadyExistsException"));
        assert!(is_transient("EntityNotFoundException"));
        assert!(!is_transient("InvalidInputException"));
    }

    #[tokio::test]
    async fn malformed_payload_is_marked_terminal_instead_of_bubbling() {
        let temp = tempfile::tempdir().unwrap();
        let coordinator = CatalogCoordinator {
            outbox: Arc::new(CatalogOutbox::open(temp.path()).unwrap()),
            notify: Notify::new(),
            table_layout_cache: tokio::sync::RwLock::new(HashMap::new()),
            worker_abort: std::sync::Mutex::new(None),
        };
        coordinator
            .persist(&[CatalogIntent {
                version: CATALOG_INTENT_VERSION,
                identity: CatalogIntentIdentity {
                    sink_ref: "primary".into(),
                    namespace: "events".into(),
                    kind: CatalogIntentKind::UpsertPartition,
                    key: "[\"2026-07-30\"]".into(),
                },
                payload_json: "not-json".into(),
            }])
            .unwrap();

        coordinator.drain_once().await.unwrap();

        let pending = coordinator.outbox.scan_pending(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].terminal);
        assert_eq!(pending[0].attempts, 1);
        assert!(pending[0]
            .last_error
            .as_deref()
            .unwrap()
            .contains("invalid durable Glue partition intent payload"));
    }
}
