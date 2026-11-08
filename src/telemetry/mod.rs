//! Anonymous usage rollups posted to skippr.io.
//!
//! Distinct from [`crate::metrics`]: those JSON files stay on the customer's
//! object store and are keyed by tenant / workspace / pipeline. This cube never
//! carries those names.

use std::io::ErrorKind;
#[cfg(test)]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use once_cell::sync::Lazy;
use serde::Serialize;
use skippr_wal_catalog::{CatalogFence, KvOp, SkipprStore, SkipprStoreRead, StoreFamily};

use crate::connect::{DataSink, DataSource, SchemaSink};
use crate::helpers::configuration::Config;
use crate::helpers::offsets::live_kv;

/// One hour must elapse between successful POSTs (scheduled or exit).
pub const POST_INTERVAL_SECS: u64 = 3600;

/// Product ingest URL. Not an operator knob.
pub const INGEST_URL: &str = "https://skippr.io/api/telemetry";

const INSTALL_ID_SUFFIX: &[u8] = b"telemetry/install_id";
const LAST_POST_SUFFIX: &[u8] = b"telemetry/last_post_unix";

#[cfg(test)]
const PAYLOAD_KEYS: &[&str] = &[
    "schema",
    "install_id",
    "day",
    "version",
    "os",
    "command",
    "pipeline_type",
    "source",
    "sink",
    "schema_sink",
    "flatten",
    "input_format",
    "output_format",
    "error_class",
    "runs",
    "rows",
    "bytes",
    "duration_ms",
    "namespaces",
    "fields",
    "evolutions",
    "deadletters",
    "flight_fail",
    "iceberg_cas",
    "compact_duration_ns",
    "compact_cycles",
    "apply_duration_ns",
    "apply_calls",
];

static PROCESS_START: Lazy<Instant> = Lazy::new(Instant::now);
static LAST_INTERVAL: Mutex<Option<IntervalSnap>> = Mutex::new(None);
static LAST_POST_INSTANT: Mutex<Option<Instant>> = Mutex::new(None);

#[derive(Debug)]
pub enum TelemetryError {
    Store(String),
    Sink(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostDecision {
    Posted,
    SkippedDisabled,
    SkippedTooSoon,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    Discover,
    Sync,
    Query,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    Ok,
    Auth,
    Config,
    Source,
    Sink,
    Schema,
    Decode,
    Compact,
    Query,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginRole {
    Source,
    Sink,
    SchemaSink,
}

pub trait TelemetryClock {
    fn unix_secs(&self) -> u64;
}

pub trait TelemetrySink {
    fn post(&self, payload: &TelemetryPayload) -> Result<(), TelemetryError>;
}

pub struct SystemClock;

impl TelemetryClock for SystemClock {
    fn unix_secs(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}

pub struct HttpTelemetrySink;

impl TelemetrySink for HttpTelemetrySink {
    fn post(&self, payload: &TelemetryPayload) -> Result<(), TelemetryError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .map_err(|err| TelemetryError::Sink(err.to_string()))?;
        let response = client
            .post(INGEST_URL)
            .header(
                "user-agent",
                format!("skipprd/{}", env!("CARGO_PKG_VERSION")),
            )
            .json(payload)
            .send()
            .map_err(|err| TelemetryError::Sink(err.to_string()))?;
        if !response.status().is_success() {
            return Err(TelemetryError::Sink(format!("http {}", response.status())));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TelemetryPayload {
    pub schema: u8,
    pub install_id: String,
    pub day: String,
    pub version: String,
    pub os: String,
    pub command: Command,
    pub pipeline_type: String,
    pub source: String,
    pub sink: String,
    pub schema_sink: String,
    pub flatten: u8,
    pub input_format: String,
    pub output_format: String,
    pub error_class: ErrorClass,
    pub runs: u64,
    pub rows: u64,
    pub bytes: u64,
    pub duration_ms: u64,
    pub namespaces: u64,
    pub fields: u64,
    pub evolutions: u64,
    pub deadletters: u64,
    pub flight_fail: u64,
    pub iceberg_cas: u64,
    pub compact_duration_ns: u64,
    pub compact_cycles: u64,
    pub apply_duration_ns: u64,
    pub apply_calls: u64,
}

#[derive(Clone, Debug)]
pub struct TelemetryDraft {
    pub command: Command,
    pub pipeline_type: String,
    pub source: String,
    pub sink: String,
    pub schema_sink: String,
    pub flatten: u8,
    pub input_format: String,
    pub output_format: String,
    pub error_class: ErrorClass,
    pub namespaces: u64,
    pub fields: u64,
    pub evolutions: u64,
}

#[derive(Clone, Copy, Default)]
struct IntervalSnap {
    rows: u64,
    bytes: u64,
    deadletters: u64,
    flight_fail: u64,
    iceberg_cas: u64,
    compact_duration_ns: u64,
    compact_cycles: u64,
    apply_duration_ns: u64,
    apply_calls: u64,
}

/// YAML wins over env. `CI=true` forces off so GitHub e2e never hits skippr.io.
pub fn telemetry_enabled(yaml: Option<bool>, env_telemetry: Option<&str>, ci: bool) -> bool {
    if ci {
        return false;
    }
    match yaml {
        Some(value) => value,
        None => match env_telemetry {
            Some(raw) => crate::helpers::configuration::parse_flag(raw).unwrap_or(false),
            None => true,
        },
    }
}

pub fn ci_forced_off() -> bool {
    std::env::var("CI")
        .ok()
        .as_deref()
        .and_then(crate::helpers::configuration::parse_flag)
        .unwrap_or(false)
}

pub fn clamp_catalog_stem(raw: &str, role: PluginRole) -> String {
    if raw.trim().is_empty() {
        return String::new();
    }
    let name = match role {
        PluginRole::Source => DataSource::parse(raw).map(DataSource::plugin_name),
        PluginRole::Sink => DataSink::parse(raw).map(DataSink::plugin_name),
        PluginRole::SchemaSink => SchemaSink::parse(raw).map(SchemaSink::plugin_name),
    };
    match name {
        Some(stem) => stem.to_ascii_lowercase(),
        None => "other".to_string(),
    }
}

pub fn classify_io(err: &std::io::Error) -> ErrorClass {
    match err.kind() {
        ErrorKind::PermissionDenied => ErrorClass::Auth,
        ErrorKind::InvalidInput | ErrorKind::InvalidData => ErrorClass::Config,
        ErrorKind::NotFound => ErrorClass::Source,
        _ => ErrorClass::Other,
    }
}

pub fn maybe_post(
    enabled: bool,
    store: &dyn SkipprStore,
    fence: CatalogFence,
    clock: &dyn TelemetryClock,
    sink: &dyn TelemetrySink,
    draft: &TelemetryDraft,
) -> Result<PostDecision, TelemetryError> {
    if !enabled {
        return Ok(PostDecision::SkippedDisabled);
    }
    let now = clock.unix_secs();
    if let Some(last) = read_last_post_unix(store)? {
        if now.saturating_sub(last) < POST_INTERVAL_SECS {
            return Ok(PostDecision::SkippedTooSoon);
        }
    }
    let install_id = ensure_install_id(store, fence)?;
    let snap = read_interval_snap();
    let baseline = {
        let last_snap = LAST_INTERVAL.lock().unwrap_or_else(|err| err.into_inner());
        *last_snap
    }
    .unwrap_or_default();
    let duration_ms = {
        let last_instant = LAST_POST_INSTANT
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        last_instant
            .map(|started| started.elapsed().as_millis() as u64)
            .unwrap_or_else(|| PROCESS_START.elapsed().as_millis() as u64)
    };
    let payload = TelemetryPayload {
        schema: 1,
        install_id,
        day: utc_day(now),
        version: env!("CARGO_PKG_VERSION").to_string(),
        os: os_label(),
        command: draft.command,
        pipeline_type: draft.pipeline_type.clone(),
        source: clamp_catalog_stem(&draft.source, PluginRole::Source),
        sink: clamp_catalog_stem(&draft.sink, PluginRole::Sink),
        schema_sink: clamp_catalog_stem(&draft.schema_sink, PluginRole::SchemaSink),
        flatten: if draft.flatten == 0 { 0 } else { 1 },
        input_format: draft.input_format.clone(),
        output_format: draft.output_format.clone(),
        error_class: draft.error_class,
        runs: 1,
        rows: snap.rows.saturating_sub(baseline.rows),
        bytes: snap.bytes.saturating_sub(baseline.bytes),
        duration_ms,
        namespaces: draft.namespaces,
        fields: draft.fields,
        evolutions: draft.evolutions,
        deadletters: snap.deadletters.saturating_sub(baseline.deadletters),
        flight_fail: snap.flight_fail.saturating_sub(baseline.flight_fail),
        iceberg_cas: snap.iceberg_cas.saturating_sub(baseline.iceberg_cas),
        compact_duration_ns: snap
            .compact_duration_ns
            .saturating_sub(baseline.compact_duration_ns),
        compact_cycles: snap.compact_cycles.saturating_sub(baseline.compact_cycles),
        apply_duration_ns: snap
            .apply_duration_ns
            .saturating_sub(baseline.apply_duration_ns),
        apply_calls: snap.apply_calls.saturating_sub(baseline.apply_calls),
    };
    sink.post(&payload)?;
    write_last_post_unix(store, fence, now)?;
    *LAST_INTERVAL.lock().unwrap_or_else(|err| err.into_inner()) = Some(snap);
    *LAST_POST_INSTANT
        .lock()
        .unwrap_or_else(|err| err.into_inner()) = Some(Instant::now());
    Ok(PostDecision::Posted)
}

pub fn maybe_post_from_config(config: &Config, command: Command, error_class: ErrorClass) {
    if cfg!(test) {
        return;
    }
    if !config.telemetry_enabled() {
        return;
    }
    let Some((store, fence)) = live_kv() else {
        return;
    };
    let draft = TelemetryDraft::from_config(config, command, error_class);
    let post = || {
        maybe_post(
            true,
            store.as_ref(),
            fence,
            &SystemClock,
            &HttpTelemetrySink,
            &draft,
        )
    };
    let _ = if tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::block_in_place(post)
    } else {
        post()
    };
}

pub fn spawn_maybe_post(config: Config, command: Command, error_class: ErrorClass) {
    if cfg!(test) {
        return;
    }
    let _ = tokio::task::spawn_blocking(move || {
        maybe_post_from_config(&config, command, error_class);
    });
}

impl TelemetryDraft {
    pub fn from_config(config: &Config, command: Command, error_class: ErrorClass) -> Self {
        let metadata = crate::METADATA.load();
        let namespaces = metadata.metadata.len() as u64;
        let fields = metadata
            .metadata
            .values()
            .map(|ns| ns.fields.len() as u64)
            .sum();
        let source_format = config
            .get_pipeline_input_plugin_config()
            .ok()
            .map(|entry| entry.input_format());
        let output_format = config
            .get_pipeline_output_plugin_config()
            .ok()
            .and_then(|entry| {
                let pipeline_type = config.pipeline_type().ok()?;
                entry
                    .resolved_output_format(pipeline_type, source_format)
                    .ok()
                    .flatten()
            })
            .map(|format| format.as_str().to_string())
            .unwrap_or_default();
        Self {
            command,
            pipeline_type: config.pipeline_type_log_label().to_string(),
            source: config.get_pipeline_input_plugin_name(),
            sink: config.get_pipeline_output_plugin_name(),
            schema_sink: config.get_pipeline_schema_plugin_name(),
            flatten: u8::from(config.get_transform_flatten_events()),
            input_format: source_format
                .map(|format| format.as_str().to_string())
                .unwrap_or_default(),
            output_format,
            error_class,
            namespaces,
            fields,
            evolutions: metadata.schema_id,
        }
    }
}

fn os_label() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

fn utc_day(unix_secs: u64) -> String {
    let days = unix_secs / 86_400;
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}")
}

fn install_id_key() -> Vec<u8> {
    StoreFamily::StoreMeta.key(INSTALL_ID_SUFFIX)
}

fn last_post_key() -> Vec<u8> {
    StoreFamily::StoreMeta.key(LAST_POST_SUFFIX)
}

fn ensure_install_id(
    store: &dyn SkipprStore,
    fence: CatalogFence,
) -> Result<String, TelemetryError> {
    let key = install_id_key();
    if let Some(bytes) =
        SkipprStoreRead::get(store, &key).map_err(|err| TelemetryError::Store(err.to_string()))?
    {
        if let Ok(existing) = String::from_utf8(bytes) {
            if uuid::Uuid::parse_str(existing.trim()).is_ok() {
                return Ok(existing.trim().to_string());
            }
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    store
        .commit(
            &[KvOp::Put {
                key,
                value: id.as_bytes().to_vec(),
            }],
            fence,
        )
        .map_err(|err| TelemetryError::Store(err.to_string()))?;
    Ok(id)
}

fn read_last_post_unix(store: &dyn SkipprStore) -> Result<Option<u64>, TelemetryError> {
    let Some(bytes) = SkipprStoreRead::get(store, &last_post_key())
        .map_err(|err| TelemetryError::Store(err.to_string()))?
    else {
        return Ok(None);
    };
    let text = String::from_utf8(bytes).unwrap_or_default();
    Ok(text.trim().parse().ok())
}

fn write_last_post_unix(
    store: &dyn SkipprStore,
    fence: CatalogFence,
    now: u64,
) -> Result<(), TelemetryError> {
    store
        .commit(
            &[KvOp::Put {
                key: last_post_key(),
                value: now.to_string().into_bytes(),
            }],
            fence,
        )
        .map_err(|err| TelemetryError::Store(err.to_string()))
}

fn read_interval_snap() -> IntervalSnap {
    use crate::metrics::counters;
    IntervalSnap {
        rows: counters::WAL_WRITE_ROWS_TOTAL.load(Ordering::Relaxed),
        bytes: counters::SOURCE_BYTES_TOTAL.load(Ordering::Relaxed),
        deadletters: counters::DEADLETTERS_TOTAL.load(Ordering::Relaxed),
        flight_fail: counters::FLIGHT_FAIL_TOTAL.load(Ordering::Relaxed),
        iceberg_cas: counters::ICEBERG_CAS_CONFLICT_TOTAL.load(Ordering::Relaxed),
        compact_duration_ns: counters::COMPACTION_PLANNER_DURATION_NS_TOTAL.load(Ordering::Relaxed),
        compact_cycles: counters::COMPACTION_PLANNER_CYCLES_TOTAL.load(Ordering::Relaxed),
        apply_duration_ns: counters::SINK_APPLY_DURATION_NS_TOTAL.load(Ordering::Relaxed),
        apply_calls: counters::SINK_APPLY_CALLS_TOTAL.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
pub(crate) fn reset_interval_state() {
    *LAST_INTERVAL.lock().unwrap() = None;
    *LAST_POST_INSTANT.lock().unwrap() = None;
}

#[cfg(test)]
struct FixedClock {
    now: AtomicU64,
}

#[cfg(test)]
impl TelemetryClock for FixedClock {
    fn unix_secs(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
struct RecordingSink {
    posts: Mutex<Vec<TelemetryPayload>>,
    fail: bool,
}

#[cfg(test)]
impl TelemetrySink for RecordingSink {
    fn post(&self, payload: &TelemetryPayload) -> Result<(), TelemetryError> {
        if self.fail {
            return Err(TelemetryError::Sink("injected".into()));
        }
        self.posts.lock().unwrap().push(payload.clone());
        Ok(())
    }
}

#[cfg(test)]
fn test_store() -> (skippr_wal_catalog::MemorySkipprStore, CatalogFence) {
    let fence = CatalogFence::local();
    (skippr_wal_catalog::MemorySkipprStore::open(fence), fence)
}

#[cfg(test)]
fn sample_draft() -> TelemetryDraft {
    TelemetryDraft {
        command: Command::Sync,
        pipeline_type: "ingest".into(),
        source: "Postgres".into(),
        sink: "AthenaIceberg".into(),
        schema_sink: "Glue".into(),
        flatten: 1,
        input_format: "json".into(),
        output_format: "native".into(),
        error_class: ErrorClass::Ok,
        namespaces: 2,
        fields: 11,
        evolutions: 3,
    }
}

#[test]
fn telemetry_default_on_env_zero_off_yaml_wins_ci_forces_off() {
    assert!(telemetry_enabled(None, None, false));
    assert!(!telemetry_enabled(None, Some("0"), false));
    assert!(!telemetry_enabled(None, Some("false"), false));
    assert!(telemetry_enabled(None, Some("1"), false));
    assert!(!telemetry_enabled(Some(false), Some("1"), false));
    assert!(telemetry_enabled(Some(true), Some("0"), false));
    assert!(!telemetry_enabled(None, Some("1"), true));
    assert!(!telemetry_enabled(Some(true), Some("1"), true));
}

#[test]
fn maybe_post_skips_when_disabled_and_does_not_call_sink() {
    reset_interval_state();
    let (store, fence) = test_store();
    let sink = RecordingSink {
        posts: Mutex::new(Vec::new()),
        fail: false,
    };
    let clock = FixedClock {
        now: AtomicU64::new(1_700_000_000),
    };
    let decision = maybe_post(false, &store, fence, &clock, &sink, &sample_draft()).unwrap();
    assert_eq!(decision, PostDecision::SkippedDisabled);
    assert!(sink.posts.lock().unwrap().is_empty());
}

#[test]
fn maybe_post_allows_first_then_blocks_inside_one_hour() {
    reset_interval_state();
    let (store, fence) = test_store();
    let sink = RecordingSink {
        posts: Mutex::new(Vec::new()),
        fail: false,
    };
    let clock = FixedClock {
        now: AtomicU64::new(1_700_000_000),
    };
    let first = maybe_post(true, &store, fence, &clock, &sink, &sample_draft()).unwrap();
    assert_eq!(first, PostDecision::Posted);
    clock.now.store(1_700_000_000 + 3_599, Ordering::SeqCst);
    let second = maybe_post(true, &store, fence, &clock, &sink, &sample_draft()).unwrap();
    assert_eq!(second, PostDecision::SkippedTooSoon);
    assert_eq!(sink.posts.lock().unwrap().len(), 1);
    clock.now.store(1_700_000_000 + 3_600, Ordering::SeqCst);
    let third = maybe_post(true, &store, fence, &clock, &sink, &sample_draft()).unwrap();
    assert_eq!(third, PostDecision::Posted);
    assert_eq!(sink.posts.lock().unwrap().len(), 2);
}

#[test]
fn maybe_post_persists_install_id_and_allowlisted_payload() {
    reset_interval_state();
    let (store, fence) = test_store();
    let sink = RecordingSink {
        posts: Mutex::new(Vec::new()),
        fail: false,
    };
    let clock = FixedClock {
        now: AtomicU64::new(1_700_000_000),
    };
    maybe_post(true, &store, fence, &clock, &sink, &sample_draft()).unwrap();
    let posted = sink.posts.lock().unwrap()[0].clone();
    uuid::Uuid::parse_str(&posted.install_id).expect("install uuid");
    assert_eq!(posted.schema, 1);
    assert_eq!(posted.source, "postgres");
    assert_eq!(posted.sink, "athenaiceberg");
    assert_eq!(posted.schema_sink, "glue");
    assert_eq!(posted.command, Command::Sync);
    assert_eq!(posted.error_class, ErrorClass::Ok);
    assert_eq!(posted.runs, 1);
    let json = serde_json::to_value(&posted).unwrap();
    let object = json.as_object().expect("object");
    assert!(!object.contains_key("pipeline_name"));
    assert!(!object.contains_key("workspace"));
    assert!(!object.contains_key("tenant"));
    assert!(!object.contains_key("data_dir"));
    assert!(!object.contains_key("null_fill_rows"));
    assert!(!object.contains_key("schema_id_missing"));
    for key in object.keys() {
        assert!(
            PAYLOAD_KEYS.contains(&key.as_str()),
            "unexpected telemetry key {key}"
        );
    }
    let again = RecordingSink {
        posts: Mutex::new(Vec::new()),
        fail: false,
    };
    clock.now.store(1_700_003_600, Ordering::SeqCst);
    maybe_post(true, &store, fence, &clock, &again, &sample_draft()).unwrap();
    assert_eq!(again.posts.lock().unwrap()[0].install_id, posted.install_id);
}

#[test]
fn unknown_plugin_stem_clamps_to_other() {
    assert_eq!(
        clamp_catalog_stem("bikehire_prod", PluginRole::Source),
        "other"
    );
    assert_eq!(
        clamp_catalog_stem("Postgres", PluginRole::Source),
        "postgres"
    );
    assert_eq!(clamp_catalog_stem("glue", PluginRole::SchemaSink), "glue");
    assert_eq!(clamp_catalog_stem("", PluginRole::Sink), "");
}

#[test]
fn telemetry_payload_has_no_pipeline_name_field() {
    let payload = TelemetryPayload {
        schema: 1,
        install_id: "00000000-0000-4000-8000-000000000001".into(),
        day: "2026-11-08".into(),
        version: "0.1.0".into(),
        os: "macos-aarch64".into(),
        command: Command::Discover,
        pipeline_type: "ingest".into(),
        source: "postgres".into(),
        sink: "snowflake".into(),
        schema_sink: "glue".into(),
        flatten: 0,
        input_format: "json".into(),
        output_format: "native".into(),
        error_class: ErrorClass::Ok,
        runs: 1,
        rows: 0,
        bytes: 0,
        duration_ms: 0,
        namespaces: 0,
        fields: 0,
        evolutions: 0,
        deadletters: 0,
        flight_fail: 0,
        iceberg_cas: 0,
        compact_duration_ns: 0,
        compact_cycles: 0,
        apply_duration_ns: 0,
        apply_calls: 0,
    };
    let encoded = serde_json::to_string(&payload).unwrap();
    assert!(
        !encoded.contains("pipeline_name"),
        "pipeline names are unrepresentable: {encoded}"
    );
}

#[test]
fn failed_sink_does_not_advance_semaphore() {
    reset_interval_state();
    let (store, fence) = test_store();
    let sink = RecordingSink {
        posts: Mutex::new(Vec::new()),
        fail: true,
    };
    let clock = FixedClock {
        now: AtomicU64::new(1_700_000_000),
    };
    assert!(maybe_post(true, &store, fence, &clock, &sink, &sample_draft()).is_err());
    let ok = RecordingSink {
        posts: Mutex::new(Vec::new()),
        fail: false,
    };
    let decision = maybe_post(true, &store, fence, &clock, &ok, &sample_draft()).unwrap();
    assert_eq!(decision, PostDecision::Posted);
}

#[test]
fn classify_io_does_not_use_display_strings() {
    assert_eq!(
        classify_io(&std::io::Error::new(ErrorKind::PermissionDenied, "secret")),
        ErrorClass::Auth
    );
    assert_eq!(
        classify_io(&std::io::Error::new(ErrorKind::InvalidInput, "skippr.yml")),
        ErrorClass::Config
    );
}
