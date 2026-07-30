use std::collections::{BTreeMap, HashMap};
use std::io;
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use aws_sdk_glue::error::SdkError;
use aws_sdk_glue::operation::get_partition::GetPartitionError;
use aws_sdk_glue::types::{Column, PartitionInput, SerDeInfo, StorageDescriptor};
use aws_sdk_glue::Client as GlueClient;
use aws_types::region::Region;
use futures::{stream, StreamExt};
use once_cell::sync::Lazy;
use rand::Rng;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::catalog_budget::{
    process_catalog_operation_budget, refresh_process_catalog_operation_budget,
    CatalogOperationBudget,
};
use crate::catalog_outbox::{CatalogOutbox, ConditionalMutationResult, PendingCatalogIntent};
use crate::runtime_plugins::protocol::{CatalogIntent, CatalogIntentKind};

const GLUE_BATCH_CREATE_LIMIT: usize = 100;
const RECOVERY_SCAN_LIMIT: usize = 10_000;

static CATALOG_COORDINATORS: Lazy<std::sync::Mutex<HashMap<String, Weak<CatalogCoordinator>>>> =
    Lazy::new(|| std::sync::Mutex::new(HashMap::new()));
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct GlueColumnIntent {
    name: String,
    r#type: String,
    comment: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
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

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct GlueTarget {
    region: Option<String>,
    catalog_id: Option<String>,
    database: String,
    table: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GlueTableLayout {
    partition_columns: Vec<GlueColumnIntent>,
    storage_columns: Vec<GlueColumnIntent>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GluePartitionSpec {
    values: Vec<String>,
    location: String,
    storage_columns: Vec<GlueColumnIntent>,
    input_format: String,
    output_format: String,
    serde_library: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum GlueBatchCreateOutcome {
    Created,
    AlreadyExists,
    Failed(GlueApiError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GlueApiErrorKind {
    Transient,
    NotFound,
    AlreadyExists,
    Terminal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GlueApiError {
    kind: GlueApiErrorKind,
    message: String,
}

impl GlueApiError {
    fn new(kind: GlueApiErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

#[async_trait]
trait GlueExecutor: Send + Sync {
    async fn table_layout(&self, target: &GlueTarget) -> Result<GlueTableLayout, GlueApiError>;

    async fn get_partition(
        &self,
        target: &GlueTarget,
        values: &[String],
    ) -> Result<Option<String>, GlueApiError>;

    async fn batch_create_partitions(
        &self,
        target: &GlueTarget,
        partitions: &[GluePartitionSpec],
    ) -> Result<Vec<GlueBatchCreateOutcome>, GlueApiError>;

    async fn update_partition(
        &self,
        target: &GlueTarget,
        partition: &GluePartitionSpec,
    ) -> Result<(), GlueApiError>;
}

#[derive(Default)]
struct AwsGlueExecutor {
    clients: tokio::sync::Mutex<HashMap<Option<String>, GlueClient>>,
}

impl AwsGlueExecutor {
    async fn client(&self, region: &Option<String>) -> GlueClient {
        if let Some(client) = self.clients.lock().await.get(region).cloned() {
            return client;
        }
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
        if let Some(region) = region.as_ref() {
            loader = loader.region(Region::new(region.clone()));
        }
        let client = GlueClient::new(&loader.load().await);
        self.clients
            .lock()
            .await
            .insert(region.clone(), client.clone());
        client
    }
}

#[async_trait]
impl GlueExecutor for AwsGlueExecutor {
    async fn table_layout(&self, target: &GlueTarget) -> Result<GlueTableLayout, GlueApiError> {
        let client = self.client(&target.region).await;
        let mut request = client
            .get_table()
            .database_name(&target.database)
            .name(&target.table);
        if let Some(catalog_id) = target.catalog_id.as_ref() {
            request = request.catalog_id(catalog_id);
        }
        let output = request
            .send()
            .await
            .map_err(|err| classify_glue_error(err.to_string()))?;
        let table = output.table().ok_or_else(|| {
            GlueApiError::new(
                GlueApiErrorKind::Transient,
                "GetTable returned no table definition",
            )
        })?;
        let to_column = |column: &Column| GlueColumnIntent {
            name: column.name().to_string(),
            r#type: column.r#type().unwrap_or("string").to_string(),
            comment: column.comment().map(str::to_string),
        };
        Ok(GlueTableLayout {
            partition_columns: table
                .partition_keys
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(to_column)
                .collect(),
            storage_columns: table
                .storage_descriptor()
                .and_then(|descriptor| descriptor.columns.as_deref())
                .unwrap_or_default()
                .iter()
                .map(to_column)
                .collect(),
        })
    }

    async fn get_partition(
        &self,
        target: &GlueTarget,
        values: &[String],
    ) -> Result<Option<String>, GlueApiError> {
        let client = self.client(&target.region).await;
        let mut request = client
            .get_partition()
            .database_name(&target.database)
            .table_name(&target.table)
            .set_partition_values(Some(values.to_vec()));
        if let Some(catalog_id) = target.catalog_id.as_ref() {
            request = request.catalog_id(catalog_id);
        }
        match request.send().await {
            Ok(output) => Ok(Some(
                output
                    .partition()
                    .and_then(|partition| partition.storage_descriptor())
                    .and_then(|descriptor| descriptor.location())
                    .unwrap_or_default()
                    .to_string(),
            )),
            Err(SdkError::ServiceError(err))
                if matches!(err.err(), GetPartitionError::EntityNotFoundException(_)) =>
            {
                Ok(None)
            }
            Err(err) => Err(classify_glue_error(err.to_string())),
        }
    }

    async fn batch_create_partitions(
        &self,
        target: &GlueTarget,
        partitions: &[GluePartitionSpec],
    ) -> Result<Vec<GlueBatchCreateOutcome>, GlueApiError> {
        let client = self.client(&target.region).await;
        let inputs = partitions
            .iter()
            .map(partition_input)
            .collect::<io::Result<Vec<_>>>()
            .map_err(|err| GlueApiError::new(GlueApiErrorKind::Terminal, err.to_string()))?;
        let mut request = client
            .batch_create_partition()
            .database_name(&target.database)
            .table_name(&target.table)
            .set_partition_input_list(Some(inputs));
        if let Some(catalog_id) = target.catalog_id.as_ref() {
            request = request.catalog_id(catalog_id);
        }
        let output = request
            .send()
            .await
            .map_err(|err| classify_glue_error(err.to_string()))?;
        let mut outcomes = vec![GlueBatchCreateOutcome::Created; partitions.len()];
        let indexes = partitions
            .iter()
            .enumerate()
            .map(|(index, partition)| (partition.values.clone(), index))
            .collect::<HashMap<_, _>>();
        for error in output.errors() {
            let Some(index) = indexes.get(error.partition_values()).copied() else {
                return Err(GlueApiError::new(
                    GlueApiErrorKind::Transient,
                    format!(
                        "BatchCreatePartition returned an error for unknown values {:?}",
                        error.partition_values()
                    ),
                ));
            };
            let code = error
                .error_detail()
                .and_then(|detail| detail.error_code())
                .unwrap_or("UnknownGlueError");
            let message = error
                .error_detail()
                .and_then(|detail| detail.error_message())
                .unwrap_or(code);
            outcomes[index] = if code.contains("AlreadyExists") {
                GlueBatchCreateOutcome::AlreadyExists
            } else {
                GlueBatchCreateOutcome::Failed(classify_glue_error(format!("{code}: {message}")))
            };
        }
        Ok(outcomes)
    }

    async fn update_partition(
        &self,
        target: &GlueTarget,
        partition: &GluePartitionSpec,
    ) -> Result<(), GlueApiError> {
        let client = self.client(&target.region).await;
        let mut request =
            client
                .update_partition()
                .database_name(&target.database)
                .table_name(&target.table)
                .set_partition_value_list(Some(partition.values.clone()))
                .partition_input(partition_input(partition).map_err(|err| {
                    GlueApiError::new(GlueApiErrorKind::Terminal, err.to_string())
                })?);
        if let Some(catalog_id) = target.catalog_id.as_ref() {
            request = request.catalog_id(catalog_id);
        }
        request
            .send()
            .await
            .map(|_| ())
            .map_err(|err| classify_glue_error(err.to_string()))
    }
}

type GlueIntentGroupKey = (Option<String>, Option<String>, String, String);
type GlueIntentGroups = BTreeMap<GlueIntentGroupKey, Vec<DecodedIntent>>;

pub struct CatalogCoordinator {
    outbox: Arc<CatalogOutbox>,
    executor: Arc<dyn GlueExecutor>,
    catalog_budget: Arc<CatalogOperationBudget>,
    refresh_budget: bool,
    notify: Notify,
    table_layout_cache: tokio::sync::RwLock<HashMap<String, GlueTableLayout>>,
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
            executor: Arc::new(AwsGlueExecutor::default()),
            catalog_budget: process_catalog_operation_budget(),
            refresh_budget: true,
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
        let target = GlueTarget {
            region,
            catalog_id,
            database,
            table,
        };
        let concurrency = if self.refresh_budget {
            refresh_process_catalog_operation_budget()
        } else {
            self.catalog_budget.target()
        }
        .max(1);
        let mut valid = Vec::with_capacity(intents.len());
        for intent in intents {
            match self.ensure_table_layout(&target, &intent).await {
                Ok(()) => valid.push(intent),
                Err(error) => self.record_glue_failure(&intent.pending, &error)?,
            }
        }
        let executor = Arc::clone(&self.executor);
        let budget = Arc::clone(&self.catalog_budget);
        let target_for_get = target.clone();
        let inspected = stream::iter(valid)
            .map(move |intent| {
                let executor = Arc::clone(&executor);
                let budget = Arc::clone(&budget);
                let target = target_for_get.clone();
                async move {
                    let _permit = budget.acquire().await;
                    let result = executor
                        .get_partition(&target, &intent.payload.partition_values)
                        .await;
                    (intent, result)
                }
            })
            .buffer_unordered(concurrency)
            .collect::<Vec<_>>()
            .await;
        let mut missing = Vec::new();
        for (intent, result) in inspected {
            match result {
                Ok(Some(current)) => {
                    if current == intent.payload.location {
                        self.outbox.mark_delivered_if(&intent.pending)?;
                    } else {
                        self.update_partition(&target, &intent).await?;
                    }
                }
                Ok(None) => missing.push(intent),
                Err(error) => self.record_glue_failure(&intent.pending, &error)?,
            }
        }

        for batch in missing.chunks(GLUE_BATCH_CREATE_LIMIT) {
            let specs = batch
                .iter()
                .map(|intent| partition_spec(&intent.payload))
                .collect::<Vec<_>>();
            let result = {
                let _permit = self.catalog_budget.acquire().await;
                self.executor.batch_create_partitions(&target, &specs).await
            };
            match result {
                Ok(outcomes) if outcomes.len() == batch.len() => {
                    for (intent, outcome) in batch.iter().zip(outcomes) {
                        match outcome {
                            GlueBatchCreateOutcome::Created => {
                                self.outbox.mark_delivered_if(&intent.pending)?;
                            }
                            GlueBatchCreateOutcome::AlreadyExists => self.record_glue_failure(
                                &intent.pending,
                                &GlueApiError::new(
                                    GlueApiErrorKind::AlreadyExists,
                                    "AlreadyExistsException: recheck partition",
                                ),
                            )?,
                            GlueBatchCreateOutcome::Failed(error) => {
                                self.record_glue_failure(&intent.pending, &error)?
                            }
                        }
                    }
                    crate::metrics::counters::add_catalog_successful_batch(1);
                }
                Ok(outcomes) => {
                    let error = GlueApiError::new(
                        GlueApiErrorKind::Transient,
                        format!(
                            "BatchCreatePartition returned {} outcomes for {} inputs",
                            outcomes.len(),
                            batch.len()
                        ),
                    );
                    for intent in batch {
                        self.record_glue_failure(&intent.pending, &error)?;
                    }
                }
                Err(error) => {
                    for intent in batch {
                        self.record_glue_failure(&intent.pending, &error)?;
                    }
                }
            }
        }
        Ok(())
    }

    async fn ensure_table_layout(
        &self,
        target: &GlueTarget,
        intent: &DecodedIntent,
    ) -> Result<(), GlueApiError> {
        let cache_key = format!(
            "{}\0{}\0{}\0{}\0{}\0{}",
            target.region.as_deref().unwrap_or_default(),
            target.catalog_id.as_deref().unwrap_or_default(),
            target.database,
            target.table,
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
        let actual = {
            let _permit = self.catalog_budget.acquire().await;
            self.executor.table_layout(target).await?
        };
        let expected = GlueTableLayout {
            partition_columns: intent.payload.partition_columns.clone(),
            storage_columns: intent.payload.storage_columns.clone(),
        };
        if !same_column_layout(&actual.partition_columns, &expected.partition_columns)
            || !same_column_layout(&actual.storage_columns, &expected.storage_columns)
        {
            return Err(GlueApiError::new(
                GlueApiErrorKind::Terminal,
                format!(
                    "Glue table layout mismatch for '{}.{}': actual={actual:?} expected={expected:?}",
                    target.database, target.table
                ),
            ));
        }
        let namespace_prefix = format!(
            "{}\0{}\0{}\0{}\0{}\0",
            target.region.as_deref().unwrap_or_default(),
            target.catalog_id.as_deref().unwrap_or_default(),
            target.database,
            target.table,
            intent.payload.schema_namespace
        );
        let mut cache = self.table_layout_cache.write().await;
        cache.retain(|key, _| !key.starts_with(&namespace_prefix));
        cache.insert(cache_key, actual);
        Ok(())
    }

    async fn update_partition(
        &self,
        target: &GlueTarget,
        intent: &DecodedIntent,
    ) -> io::Result<()> {
        let spec = partition_spec(&intent.payload);
        let result = {
            let _permit = self.catalog_budget.acquire().await;
            self.executor.update_partition(target, &spec).await
        };
        match result {
            Ok(_) => {
                if self.outbox.mark_delivered_if(&intent.pending)?
                    == ConditionalMutationResult::Applied
                {
                    crate::metrics::counters::add_catalog_location_update(1);
                }
            }
            Err(error) => self.record_glue_failure(&intent.pending, &error)?,
        }
        Ok(())
    }

    fn record_glue_failure(
        &self,
        pending: &PendingCatalogIntent,
        error: &GlueApiError,
    ) -> io::Result<()> {
        let transient = matches!(
            error.kind,
            GlueApiErrorKind::Transient
                | GlueApiErrorKind::NotFound
                | GlueApiErrorKind::AlreadyExists
        );
        let delay = transient.then(|| retry_delay(pending.attempts));
        if self
            .outbox
            .record_failure_if(pending, &error.message, delay, !transient)?
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

fn partition_spec(payload: &GluePartitionCatalogIntentV1) -> GluePartitionSpec {
    GluePartitionSpec {
        values: payload.partition_values.clone(),
        location: payload.location.clone(),
        storage_columns: payload.storage_columns.clone(),
        input_format: payload.input_format.clone(),
        output_format: payload.output_format.clone(),
        serde_library: payload.serde_library.clone(),
    }
}

fn partition_input(partition: &GluePartitionSpec) -> io::Result<PartitionInput> {
    let columns = partition
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
        .set_values(Some(partition.values.clone()))
        .parameters("parquet.compression", "SNAPPY")
        .storage_descriptor(
            StorageDescriptor::builder()
                .set_columns(Some(columns))
                .compressed(true)
                .input_format(&partition.input_format)
                .location(&partition.location)
                .output_format(&partition.output_format)
                .serde_info(
                    SerDeInfo::builder()
                        .parameters("serialization.format", "1")
                        .serialization_library(&partition.serde_library)
                        .build(),
                )
                .stored_as_sub_directories(true)
                .build(),
        )
        .build())
}

fn same_column_layout(left: &[GlueColumnIntent], right: &[GlueColumnIntent]) -> bool {
    left.iter()
        .map(|column| (&column.name, &column.r#type))
        .eq(right.iter().map(|column| (&column.name, &column.r#type)))
}

fn classify_glue_error(error: String) -> GlueApiError {
    let kind = if error.to_ascii_lowercase().contains("entitynotfound") {
        GlueApiErrorKind::NotFound
    } else if error.to_ascii_lowercase().contains("alreadyexist") {
        GlueApiErrorKind::AlreadyExists
    } else if is_transient(&error) {
        GlueApiErrorKind::Transient
    } else {
        GlueApiErrorKind::Terminal
    };
    GlueApiError::new(kind, error)
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
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;

    #[derive(Default)]
    struct MockGlueState {
        layout: Option<Result<GlueTableLayout, GlueApiError>>,
        layout_calls: usize,
        get_results: HashMap<Vec<String>, VecDeque<Result<Option<String>, GlueApiError>>>,
        batch_results: VecDeque<Result<Vec<GlueBatchCreateOutcome>, GlueApiError>>,
        batch_sizes: Vec<usize>,
        update_results: VecDeque<Result<(), GlueApiError>>,
        updates: Vec<GluePartitionSpec>,
    }

    #[derive(Default)]
    struct MockGlueExecutor {
        state: StdMutex<MockGlueState>,
        get_delay_ms: AtomicU64,
        active_gets: AtomicUsize,
        max_active_gets: AtomicUsize,
    }

    struct ActiveGet<'a>(&'a AtomicUsize);

    impl Drop for ActiveGet<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl GlueExecutor for MockGlueExecutor {
        async fn table_layout(
            &self,
            _target: &GlueTarget,
        ) -> Result<GlueTableLayout, GlueApiError> {
            let mut state = self.state.lock().unwrap();
            state.layout_calls += 1;
            state
                .layout
                .clone()
                .unwrap_or_else(|| Ok(expected_layout("bigint")))
        }

        async fn get_partition(
            &self,
            _target: &GlueTarget,
            values: &[String],
        ) -> Result<Option<String>, GlueApiError> {
            let active = self.active_gets.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active_gets.fetch_max(active, Ordering::SeqCst);
            let _active = ActiveGet(&self.active_gets);
            let delay = self.get_delay_ms.load(Ordering::SeqCst);
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            self.state
                .lock()
                .unwrap()
                .get_results
                .get_mut(values)
                .and_then(VecDeque::pop_front)
                .unwrap_or(Ok(None))
        }

        async fn batch_create_partitions(
            &self,
            _target: &GlueTarget,
            partitions: &[GluePartitionSpec],
        ) -> Result<Vec<GlueBatchCreateOutcome>, GlueApiError> {
            let mut state = self.state.lock().unwrap();
            state.batch_sizes.push(partitions.len());
            state
                .batch_results
                .pop_front()
                .unwrap_or_else(|| Ok(vec![GlueBatchCreateOutcome::Created; partitions.len()]))
        }

        async fn update_partition(
            &self,
            _target: &GlueTarget,
            partition: &GluePartitionSpec,
        ) -> Result<(), GlueApiError> {
            let mut state = self.state.lock().unwrap();
            state.updates.push(partition.clone());
            state.update_results.pop_front().unwrap_or(Ok(()))
        }
    }

    fn column(name: &str, r#type: &str) -> GlueColumnIntent {
        GlueColumnIntent {
            name: name.to_string(),
            r#type: r#type.to_string(),
            comment: None,
        }
    }

    fn expected_layout(storage_type: &str) -> GlueTableLayout {
        GlueTableLayout {
            partition_columns: vec![column("day", "string")],
            storage_columns: vec![column("id", storage_type)],
        }
    }

    fn intent(day: &str, location: &str, schema_version: u64) -> CatalogIntent {
        let payload = GluePartitionCatalogIntentV1 {
            version: 1,
            region: Some("eu-west-1".into()),
            catalog_id: Some("123456789012".into()),
            database: "analytics".into(),
            table: "events".into(),
            partition_values: vec![day.into()],
            location: location.into(),
            storage_columns: vec![column("id", "bigint")],
            partition_columns: vec![column("day", "string")],
            input_format: "parquet-input".into(),
            output_format: "parquet-output".into(),
            serde_library: "parquet-serde".into(),
            schema_namespace: "events".into(),
            schema_version,
        };
        CatalogIntent {
            version: CATALOG_INTENT_VERSION,
            identity: CatalogIntentIdentity {
                sink_ref: "primary".into(),
                namespace: "events".into(),
                kind: CatalogIntentKind::UpsertPartition,
                key: serde_json::to_string(&payload.partition_values).unwrap(),
            },
            payload_json: serde_json::to_string(&payload).unwrap(),
        }
    }

    fn coordinator(
        temp: &tempfile::TempDir,
        executor: Arc<MockGlueExecutor>,
        budget: Arc<CatalogOperationBudget>,
    ) -> CatalogCoordinator {
        CatalogCoordinator {
            outbox: Arc::new(CatalogOutbox::open(temp.path()).unwrap()),
            executor,
            catalog_budget: budget,
            refresh_budget: false,
            notify: Notify::new(),
            table_layout_cache: tokio::sync::RwLock::new(HashMap::new()),
            worker_abort: std::sync::Mutex::new(None),
        }
    }

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
        let coordinator = coordinator(
            &temp,
            Arc::new(MockGlueExecutor::default()),
            CatalogOperationBudget::new(1),
        );
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

    #[tokio::test]
    async fn partial_batch_only_retries_failed_partition() {
        let temp = tempfile::tempdir().unwrap();
        let executor = Arc::new(MockGlueExecutor::default());
        executor
            .state
            .lock()
            .unwrap()
            .batch_results
            .push_back(Ok(vec![
                GlueBatchCreateOutcome::Created,
                GlueBatchCreateOutcome::Failed(GlueApiError::new(
                    GlueApiErrorKind::Transient,
                    "ThrottlingException",
                )),
                GlueBatchCreateOutcome::Created,
            ]));
        let coordinator = coordinator(&temp, Arc::clone(&executor), CatalogOperationBudget::new(3));
        coordinator
            .persist(&[
                intent("2026-07-30", "s3://bucket/day=30/", 1),
                intent("2026-07-31", "s3://bucket/day=31/", 1),
                intent("2026-08-01", "s3://bucket/day=01/", 1),
            ])
            .unwrap();

        coordinator.drain_once().await.unwrap();

        let pending = coordinator.outbox.scan_pending(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].intent.identity.key.contains("2026-07-31"));
        assert_eq!(pending[0].attempts, 1);
        assert!(pending[0].next_attempt_at_ms > now_ms());
        assert_eq!(executor.state.lock().unwrap().batch_sizes, vec![3]);
    }

    #[tokio::test]
    async fn throttling_backoff_persists_and_retry_recovery_delivers() {
        let temp = tempfile::tempdir().unwrap();
        let executor = Arc::new(MockGlueExecutor::default());
        executor.state.lock().unwrap().get_results.insert(
            vec!["2026-07-30".into()],
            VecDeque::from([
                Err(GlueApiError::new(
                    GlueApiErrorKind::Transient,
                    "ThrottlingException",
                )),
                Ok(Some("s3://bucket/day=30/".into())),
            ]),
        );
        let coordinator = coordinator(&temp, Arc::clone(&executor), CatalogOperationBudget::new(1));
        coordinator
            .persist(&[intent("2026-07-30", "s3://bucket/day=30/", 1)])
            .unwrap();

        coordinator.drain_once().await.unwrap();
        let pending = coordinator.outbox.scan_pending(1).unwrap().remove(0);
        assert_eq!(pending.attempts, 1);
        assert!(pending.next_attempt_at_ms > now_ms());

        tokio::time::sleep(Duration::from_millis(350)).await;
        coordinator.drain_once().await.unwrap();
        assert!(coordinator.outbox.scan_pending(1).unwrap().is_empty());
    }

    #[tokio::test]
    async fn same_location_delivers_without_update() {
        let temp = tempfile::tempdir().unwrap();
        let executor = Arc::new(MockGlueExecutor::default());
        executor.state.lock().unwrap().get_results.insert(
            vec!["2026-07-30".into()],
            VecDeque::from([Ok(Some("s3://bucket/day=30/".into()))]),
        );
        let coordinator = coordinator(&temp, Arc::clone(&executor), CatalogOperationBudget::new(1));
        coordinator
            .persist(&[intent("2026-07-30", "s3://bucket/day=30/", 1)])
            .unwrap();

        coordinator.drain_once().await.unwrap();

        assert!(coordinator.outbox.scan_pending(1).unwrap().is_empty());
        assert!(executor.state.lock().unwrap().updates.is_empty());
    }

    #[tokio::test]
    async fn changed_location_updates_then_delivers() {
        let temp = tempfile::tempdir().unwrap();
        let executor = Arc::new(MockGlueExecutor::default());
        executor.state.lock().unwrap().get_results.insert(
            vec!["2026-07-30".into()],
            VecDeque::from([Ok(Some("s3://bucket/old/".into()))]),
        );
        let coordinator = coordinator(&temp, Arc::clone(&executor), CatalogOperationBudget::new(1));
        coordinator
            .persist(&[intent("2026-07-30", "s3://bucket/day=30/", 1)])
            .unwrap();

        coordinator.drain_once().await.unwrap();

        assert!(coordinator.outbox.scan_pending(1).unwrap().is_empty());
        assert_eq!(
            executor.state.lock().unwrap().updates[0].location,
            "s3://bucket/day=30/"
        );
    }

    #[tokio::test]
    async fn entity_not_found_is_batched_for_create() {
        let temp = tempfile::tempdir().unwrap();
        let executor = Arc::new(MockGlueExecutor::default());
        executor
            .state
            .lock()
            .unwrap()
            .get_results
            .insert(vec!["2026-07-30".into()], VecDeque::from([Ok(None)]));
        let coordinator = coordinator(&temp, Arc::clone(&executor), CatalogOperationBudget::new(1));
        coordinator
            .persist(&[intent("2026-07-30", "s3://bucket/day=30/", 1)])
            .unwrap();

        coordinator.drain_once().await.unwrap();

        assert!(coordinator.outbox.scan_pending(1).unwrap().is_empty());
        assert_eq!(executor.state.lock().unwrap().batch_sizes, vec![1]);
    }

    #[tokio::test]
    async fn already_exists_remains_pending_for_safe_recheck() {
        let temp = tempfile::tempdir().unwrap();
        let executor = Arc::new(MockGlueExecutor::default());
        {
            let mut state = executor.state.lock().unwrap();
            state.get_results.insert(
                vec!["2026-07-30".into()],
                VecDeque::from([Ok(None), Ok(Some("s3://bucket/day=30/".into()))]),
            );
            state
                .batch_results
                .push_back(Ok(vec![GlueBatchCreateOutcome::AlreadyExists]));
        }
        let coordinator = coordinator(&temp, Arc::clone(&executor), CatalogOperationBudget::new(1));
        coordinator
            .persist(&[intent("2026-07-30", "s3://bucket/day=30/", 1)])
            .unwrap();

        coordinator.drain_once().await.unwrap();
        assert_eq!(coordinator.outbox.scan_pending(1).unwrap()[0].attempts, 1);
        tokio::time::sleep(Duration::from_millis(350)).await;
        coordinator.drain_once().await.unwrap();
        assert!(coordinator.outbox.scan_pending(1).unwrap().is_empty());
    }

    #[tokio::test]
    async fn full_layout_mismatch_is_terminal() {
        let temp = tempfile::tempdir().unwrap();
        let executor = Arc::new(MockGlueExecutor::default());
        executor.state.lock().unwrap().layout = Some(Ok(expected_layout("string")));
        let coordinator = coordinator(&temp, Arc::clone(&executor), CatalogOperationBudget::new(1));
        coordinator
            .persist(&[intent("2026-07-30", "s3://bucket/day=30/", 1)])
            .unwrap();

        coordinator.drain_once().await.unwrap();

        let pending = coordinator.outbox.scan_pending(1).unwrap().remove(0);
        assert!(pending.terminal);
        assert!(pending
            .last_error
            .as_deref()
            .unwrap()
            .contains("table layout mismatch"));
    }

    #[tokio::test]
    async fn schema_version_change_revalidates_and_replaces_layout_cache() {
        let temp = tempfile::tempdir().unwrap();
        let executor = Arc::new(MockGlueExecutor::default());
        executor.state.lock().unwrap().get_results.insert(
            vec!["2026-07-30".into()],
            VecDeque::from([Ok(Some("s3://bucket/day=30/".into()))]),
        );
        let coordinator = coordinator(&temp, Arc::clone(&executor), CatalogOperationBudget::new(1));
        coordinator
            .persist(&[intent("2026-07-30", "s3://bucket/day=30/", 1)])
            .unwrap();
        coordinator.drain_once().await.unwrap();

        executor.state.lock().unwrap().get_results.insert(
            vec!["2026-07-31".into()],
            VecDeque::from([Ok(Some("s3://bucket/day=31/".into()))]),
        );
        coordinator
            .persist(&[intent("2026-07-31", "s3://bucket/day=31/", 2)])
            .unwrap();
        coordinator.drain_once().await.unwrap();

        assert_eq!(executor.state.lock().unwrap().layout_calls, 2);
        let cache = coordinator.table_layout_cache.read().await;
        assert_eq!(cache.len(), 1);
        assert!(cache.keys().next().unwrap().ends_with("\u{0}2"));
    }

    #[tokio::test]
    async fn partition_inspection_uses_bounded_parallel_budget() {
        let temp = tempfile::tempdir().unwrap();
        let executor = Arc::new(MockGlueExecutor::default());
        executor.get_delay_ms.store(50, Ordering::SeqCst);
        {
            let mut state = executor.state.lock().unwrap();
            for day in ["30", "31", "01", "02"] {
                state.get_results.insert(
                    vec![day.into()],
                    VecDeque::from([Ok(Some(format!("s3://bucket/day={day}/")))]),
                );
            }
        }
        let coordinator = coordinator(&temp, Arc::clone(&executor), CatalogOperationBudget::new(3));
        coordinator
            .persist(&[
                intent("30", "s3://bucket/day=30/", 1),
                intent("31", "s3://bucket/day=31/", 1),
                intent("01", "s3://bucket/day=01/", 1),
                intent("02", "s3://bucket/day=02/", 1),
            ])
            .unwrap();

        coordinator.drain_once().await.unwrap();

        assert!(coordinator.outbox.scan_pending(1).unwrap().is_empty());
        assert_eq!(executor.max_active_gets.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn batch_create_never_exceeds_aws_limit() {
        let temp = tempfile::tempdir().unwrap();
        let executor = Arc::new(MockGlueExecutor::default());
        let coordinator = coordinator(&temp, Arc::clone(&executor), CatalogOperationBudget::new(8));
        let intents = (0..101)
            .map(|index| {
                intent(
                    &format!("{index:03}"),
                    &format!("s3://bucket/day={index:03}/"),
                    1,
                )
            })
            .collect::<Vec<_>>();
        coordinator.persist(&intents).unwrap();

        coordinator.drain_once().await.unwrap();

        assert!(coordinator.outbox.scan_pending(1).unwrap().is_empty());
        assert_eq!(executor.state.lock().unwrap().batch_sizes, vec![100, 1]);
    }
}
