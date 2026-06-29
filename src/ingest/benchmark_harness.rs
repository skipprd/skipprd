//! Deterministic local ingest benchmarks without external services.
//!
//! Used by integration tests and `scripts/benchmark-ingest-local.sh`.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::discover::{
    date_formats::DateFormats, DateCandidate, DateParserKind, Metadata, PipelineMetadata,
    SkipprDataType,
};
use crate::helpers::configuration::{Config, PIPELINE_NAME};
use crate::helpers::offsets::{OffsetKey, Offsets};
use crate::ingest::fast_ingest::{create_default_nested_message, DEFAULT_NESTED_MESSAGE};
use crate::ingest_work::storage_namespace;
use crate::ingest_work::{Ingest, IngestBatch};
use crate::metrics::ingest_profile::{self, IngestProfileSnapshot};
use crate::plugins::NoopOutputPlugin;
use crate::runtime_plugins::protocol::RuntimeExecutionMode;
use crate::{METADATA, RUNNING};

#[derive(Debug, Clone, Copy)]
pub enum BenchmarkFixture {
    WatFlatExact,
    FlatExactDates,
    FallbackMixed,
    /// WAT-shaped rows with long URL/path strings (allocator + memcpy stress).
    StringsHeavy,
    /// Same few partition keys repeated (buffer batching / hash-map stress).
    PartitionSkewed,
}

impl BenchmarkFixture {
    pub fn name(self) -> &'static str {
        match self {
            Self::WatFlatExact => "wat_flat_exact",
            Self::FlatExactDates => "flat_exact_dates",
            Self::FallbackMixed => "fallback_mixed",
            Self::StringsHeavy => "strings_heavy",
            Self::PartitionSkewed => "partition_skewed",
        }
    }

    pub fn namespace(self) -> &'static str {
        match self {
            Self::WatFlatExact | Self::StringsHeavy | Self::PartitionSkewed => {
                "cc_wat_source_pages_by_target_domain_index"
            }
            Self::FlatExactDates => "flat_exact_dates",
            Self::FallbackMixed => "fallback_mixed",
        }
    }

    pub fn default_rows(self) -> usize {
        match self {
            Self::WatFlatExact => 50_000,
            Self::FlatExactDates => 50_000,
            Self::FallbackMixed => 50_000,
            Self::StringsHeavy => 50_000,
            Self::PartitionSkewed => 50_000,
        }
    }

    pub fn all() -> [Self; 5] {
        [
            Self::WatFlatExact,
            Self::FlatExactDates,
            Self::FallbackMixed,
            Self::StringsHeavy,
            Self::PartitionSkewed,
        ]
    }
}

#[derive(Debug, Clone)]
pub struct BenchmarkRunResult {
    pub fixture: BenchmarkFixture,
    pub rows: usize,
    pub input_bytes: usize,
    pub duration: Duration,
    pub records_per_sec: f64,
    pub profile: IngestProfileSnapshot,
    pub force_legacy: bool,
}

pub fn benchmark_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/benchmark/ingest")
}

pub fn fixture_input_path(fixture: BenchmarkFixture) -> PathBuf {
    benchmark_root()
        .join("fixtures")
        .join(format!("{}.ndjson", fixture.name()))
}

pub fn fixture_metadata_path(fixture: BenchmarkFixture) -> PathBuf {
    benchmark_root()
        .join("metadata")
        .join(format!("{}.json", fixture.name()))
}

fn wat_flat_row(i: usize) -> Value {
    json!({
        "crawl_id": "CC-MAIN-BENCH",
        "target_domain_hash_bucket": format!("{}", i % 32_768),
        "target_domain_id": format!("{}", 1_000_000 + (i % 10_000)),
        "target_domain": format!("target-{}.example", i % 10_000),
        "source_url_id": format!("{}", 2_000_000 + (i % 20_000)),
        "source_url": format!("https://source-{}.example/page", i % 20_000),
        "source_domain_id": "100",
        "source_domain": "source.example",
        "source_host": "source.example",
        "warc_filename": "warc.warc.gz",
        "warc_record_offset": i as i64,
        "warc_record_length": 1000,
        "wat_filename": "wat.wat.gz",
        "wat_record_offset": i as i64,
        "wat_record_length": 500,
        "fetch_status": 200,
        "content_mime_type": "text/html",
        "fetch_time": "2026-01-01T00:00:00Z",
        "link_count_to_target": 1,
        "wat_path": format!("s3://bucket/wat/{i}.wat.gz")
    })
}

fn flat_dates_row(i: usize) -> Value {
    let day = (i % 28) + 1;
    let hour = i % 24;
    json!({
        "id": format!("row-{i}"),
        "event_time": format!("2026-01-{day:02}T{hour:02}:00:00Z"),
        "value": i as i64
    })
}

fn strings_heavy_row(i: usize) -> Value {
    let pad = "x".repeat(180 + (i % 64));
    let mut row = wat_flat_row(i);
    row["source_url"] = json!(format!(
        "https://source-{}.example/{pad}/page?ref={pad}",
        i % 20_000
    ));
    row["wat_path"] = json!(format!("s3://bucket/wat/{i}/{pad}/wat.wat.gz"));
    row["target_domain"] = json!(format!("target-{}.{}.example", i % 10_000, &pad[..32]));
    row
}

fn partition_skewed_row(i: usize) -> Value {
    let bucket = i % 8;
    json!({
        "crawl_id": "CC-MAIN-BENCH",
        "target_domain_hash_bucket": format!("{bucket}"),
        "target_domain_id": format!("{}", 1_000_000 + bucket),
        "target_domain": format!("target-{bucket}.example"),
        "source_url_id": format!("{}", 2_000_000 + (bucket % 4)),
        "source_url": format!("https://source-{bucket}.example/page"),
        "source_domain_id": "100",
        "source_domain": "source.example",
        "source_host": "source.example",
        "warc_filename": "warc.warc.gz",
        "warc_record_offset": i as i64,
        "warc_record_length": 1000,
        "wat_filename": "wat.wat.gz",
        "wat_record_offset": i as i64,
        "wat_record_length": 500,
        "fetch_status": 200,
        "content_mime_type": "text/html",
        "fetch_time": "2026-01-01T00:00:00Z",
        "link_count_to_target": 1,
        "wat_path": format!("s3://bucket/wat/{bucket}.wat.gz")
    })
}

fn fallback_mixed_row(i: usize) -> Value {
    let mut row = flat_dates_row(i);
    if i % 97 == 0 {
        row["unexpected_field"] = json!("force-fallback");
    }
    if i % 211 == 0 {
        row["value"] = json!("not-an-int");
    }
    row
}

pub fn generate_row(fixture: BenchmarkFixture, i: usize) -> Value {
    match fixture {
        BenchmarkFixture::WatFlatExact => wat_flat_row(i),
        BenchmarkFixture::FlatExactDates => flat_dates_row(i),
        BenchmarkFixture::FallbackMixed => fallback_mixed_row(i),
        BenchmarkFixture::StringsHeavy => strings_heavy_row(i),
        BenchmarkFixture::PartitionSkewed => partition_skewed_row(i),
    }
}

pub fn write_fixture_ndjson(
    fixture: BenchmarkFixture,
    rows: usize,
    path: &Path,
) -> std::io::Result<usize> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::File::create(path)?;
    let mut bytes = 0usize;
    for i in 0..rows {
        let line = serde_json::to_string(&generate_row(fixture, i))?;
        bytes += line.len() + 1;
        writeln!(file, "{line}")?;
    }
    Ok(bytes)
}

fn date_field(name: &str, kind: DateParserKind, tz: bool) -> Metadata {
    let mut meta = Metadata::new_with_type(SkipprDataType::Date, name);
    meta.date_candidate = Some(DateCandidate {
        check_count: 10,
        valid_count: 10,
        field: name.to_string(),
        format: DateFormats::Iso8601_2.name().to_string(),
    });
    meta.date_parser_kind = Some(kind);
    meta.timezone = tz;
    meta
}

pub fn metadata_for_fixture(fixture: BenchmarkFixture) -> HashMap<String, Metadata> {
    match fixture {
        BenchmarkFixture::WatFlatExact
        | BenchmarkFixture::StringsHeavy
        | BenchmarkFixture::PartitionSkewed => {
            let mut fields = HashMap::new();
            for (name, ty) in [
                ("crawl_id", SkipprDataType::String),
                ("target_domain_hash_bucket", SkipprDataType::String),
                ("target_domain_id", SkipprDataType::String),
                ("target_domain", SkipprDataType::String),
                ("source_url_id", SkipprDataType::String),
                ("source_url", SkipprDataType::String),
                ("source_domain_id", SkipprDataType::String),
                ("source_domain", SkipprDataType::String),
                ("source_host", SkipprDataType::String),
                ("warc_filename", SkipprDataType::String),
                ("warc_record_offset", SkipprDataType::Long),
                ("warc_record_length", SkipprDataType::Long),
                ("wat_filename", SkipprDataType::String),
                ("wat_record_offset", SkipprDataType::Long),
                ("wat_record_length", SkipprDataType::Long),
                ("content_mime_type", SkipprDataType::String),
                ("wat_path", SkipprDataType::String),
            ] {
                fields.insert(name.to_string(), Metadata::new_with_type(ty, name));
            }
            fields.insert(
                "fetch_status".to_string(),
                Metadata::new_with_type(SkipprDataType::Integer, "fetch_status"),
            );
            fields.insert(
                "link_count_to_target".to_string(),
                Metadata::new_with_type(SkipprDataType::Integer, "link_count_to_target"),
            );
            fields.insert(
                "fetch_time".to_string(),
                date_field("fetch_time", DateParserKind::ZNoMsT, true),
            );
            fields
        }
        BenchmarkFixture::FlatExactDates | BenchmarkFixture::FallbackMixed => {
            let mut fields = HashMap::new();
            fields.insert(
                "id".to_string(),
                Metadata::new_with_type(SkipprDataType::String, "id"),
            );
            fields.insert(
                "event_time".to_string(),
                date_field("event_time", DateParserKind::ZNoMsT, true),
            );
            fields.insert(
                "value".to_string(),
                Metadata::new_with_type(SkipprDataType::Long, "value"),
            );
            fields
        }
    }
}

pub fn pipeline_metadata_for_fixture(fixture: BenchmarkFixture) -> PipelineMetadata {
    let namespace = fixture.namespace();
    let fields = metadata_for_fixture(fixture);
    let mut ns_meta = Metadata::new_with_type(SkipprDataType::Record, namespace);
    ns_meta.fields = Box::new(fields.clone());

    let mut pipeline = PipelineMetadata::new();
    pipeline.name = "ingest_benchmark".to_string();
    pipeline.enabled = true;
    pipeline.metadata.insert(namespace.to_string(), ns_meta);

    let template = create_default_nested_message(&fields);
    DEFAULT_NESTED_MESSAGE.insert(namespace.to_string(), Arc::new(template));

    pipeline
}

pub fn install_fixture_metadata(fixture: BenchmarkFixture) {
    let pipeline = pipeline_metadata_for_fixture(fixture);
    let namespace = fixture.namespace();
    let _ = Ingest::prepare_arrow_schema_with_metadata(namespace, &pipeline.metadata, false);
    METADATA.store(Arc::new(pipeline));
}

fn prepare_benchmark_runtime(data_dir: &Path) {
    let config_path = benchmark_root().join("skippr.yml");
    std::env::set_var("SKIPPR_CONFIG_FILE", &config_path);
    std::env::set_var("DATA_DIR", data_dir);
    std::env::set_var("SKIPPR_PIPELINE", "ingest_benchmark");
    std::env::set_var("SKIPPR_WORKSPACE", "benchmark");
    // Disable disk guards for temp-dir benchmarks (0 = no percentage watermark).
    std::env::set_var("DATA_DIR_MIN_FREE_BYTES", "0");
    std::env::set_var("DATA_DIR_HIGH_WATERMARK_PCT", "0");
    std::env::set_var("DATA_DIR_LOW_WATERMARK_PCT", "0");

    crate::reset_data_dir_capacity_state_for_test();

    PIPELINE_NAME
        .write()
        .clone_from(&"ingest_benchmark".to_string());

    Config::try_build_config().expect("benchmark skippr.yml");

    let data_dir_str = Config::get_data_dir();
    for sub in ["ingest_buffer", "ingest_buffer/done", "output_buffer"] {
        let _ = fs::create_dir_all(format!("{data_dir_str}/{sub}"));
    }
}

fn read_ndjson(path: &Path) -> std::io::Result<String> {
    fs::read_to_string(path)
}

fn benchmark_rows(default: usize) -> usize {
    std::env::var("BENCHMARK_ROWS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// Run ingest `process_batch` against a local NDJSON fixture with noop sink.
pub fn run_local_fixture_benchmark(
    fixture: BenchmarkFixture,
    rows: Option<usize>,
    data_dir: &Path,
) -> BenchmarkRunResult {
    run_local_fixture_benchmark_with_options(fixture, rows, data_dir, false)
}

/// Benchmark entry with optional legacy-path baseline (test harness only).
pub fn run_local_fixture_benchmark_with_options(
    fixture: BenchmarkFixture,
    rows: Option<usize>,
    data_dir: &Path,
    force_legacy: bool,
) -> BenchmarkRunResult {
    let _guard = crate::metadata_test_lock();
    ingest_profile::reset_profile_counters();
    ingest_profile::set_force_legacy_ingest_for_benchmark(force_legacy);

    fs::create_dir_all(data_dir).expect("data dir");
    prepare_benchmark_runtime(data_dir);

    let input_path = fixture_input_path(fixture);
    let row_count = rows.unwrap_or_else(|| fixture.default_rows());
    if rows.is_some() || !input_path.exists() {
        write_fixture_ndjson(fixture, row_count, &input_path).expect("write fixture");
    }

    install_fixture_metadata(fixture);
    RUNNING
        .write()
        .store(true, std::sync::atomic::Ordering::SeqCst);

    let payload = read_ndjson(&input_path).expect("read fixture");
    let input_bytes = payload.len();
    let namespace = storage_namespace(fixture.namespace());
    let offset_key = OffsetKey::new("file", fixture.name());
    let batch = IngestBatch::new(
        offset_key,
        payload,
        input_bytes,
        format!("file://{}", input_path.display()),
        Some(namespace),
        None,
    );

    let _ingest = Ingest::new_for_execution(RuntimeExecutionMode::Sync);
    let offsets = Arc::new(Offsets::init().expect("offsets"));
    let output: Arc<Box<dyn crate::plugins::DataSink + Send + Sync>> =
        Arc::new(Box::new(NoopOutputPlugin));

    let start = Instant::now();
    Ingest::run_process_batch_for_benchmark(&[batch], &offsets, output, 0);
    let duration = start.elapsed();
    let profile = ingest_profile::snapshot();
    let records_per_sec = row_count as f64 / duration.as_secs_f64().max(1e-9);

    ingest_profile::set_force_legacy_ingest_for_benchmark(false);

    BenchmarkRunResult {
        fixture,
        rows: row_count,
        input_bytes,
        duration,
        records_per_sec,
        profile,
        force_legacy,
    }
}

pub fn format_benchmark_report(result: &BenchmarkRunResult) -> String {
    format!(
        "fixture={} rows={} input_bytes={} duration_ms={:.2} records_per_sec={:.0} legacy_baseline={} profile={{decode_ms={:.2}, exact_plan_ms={:.2}, exact_append_ms={:.2}, exact_finish_ms={:.2}, partition_ms={:.2}, metadata_load_ms={:.2}, unwrap_ms={:.2}, buffer_offset_ms={:.2}, fast_ms={:.2}, slow_ms={:.2}, arrow_json_ms={:.2}, wal_ms={:.2}, exact_rows={}, exact_fallback={}, legacy_rows={}}}",
        result.fixture.name(),
        result.rows,
        result.input_bytes,
        result.duration.as_secs_f64() * 1000.0,
        result.records_per_sec,
        result.force_legacy,
        result.profile.decode_ns as f64 / 1_000_000.0,
        result.profile.exact_plan_ns as f64 / 1_000_000.0,
        result.profile.exact_append_ns as f64 / 1_000_000.0,
        result.profile.exact_finish_ns as f64 / 1_000_000.0,
        result.profile.partition_ns as f64 / 1_000_000.0,
        result.profile.metadata_load_ns as f64 / 1_000_000.0,
        result.profile.unwrap_ns as f64 / 1_000_000.0,
        result.profile.buffer_offset_ns as f64 / 1_000_000.0,
        result.profile.fast_path_ns as f64 / 1_000_000.0,
        result.profile.slow_path_ns as f64 / 1_000_000.0,
        result.profile.arrow_json_ns as f64 / 1_000_000.0,
        result.profile.wal_enqueue_ns as f64 / 1_000_000.0,
        result.profile.exact_arrow_rows,
        result.profile.exact_arrow_fallback_rows,
        result.profile.legacy_normalized_rows,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial]
    fn generates_fixture_files() {
        let dir =
            std::env::temp_dir().join(format!("skippr_ingest_fixture_{}", rand::random::<u64>()));
        let path = dir.join("wat.ndjson");
        let bytes = write_fixture_ndjson(BenchmarkFixture::WatFlatExact, 10, &path).unwrap();
        assert!(bytes > 0);
        let content = fs::read_to_string(path).unwrap();
        assert_eq!(content.lines().count(), 10);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    #[serial]
    fn local_wat_fixture_ingests_rows() {
        let dir =
            std::env::temp_dir().join(format!("skippr_ingest_smoke_{}", rand::random::<u64>()));
        let result = run_local_fixture_benchmark(BenchmarkFixture::WatFlatExact, Some(10), &dir);
        assert!(
            result.profile.exact_arrow_rows > 0,
            "profile={:?}",
            result.profile
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    #[ignore = "benchmark; run with `cargo test ingest_benchmark -- --ignored --nocapture`"]
    #[serial]
    fn bench_legacy_wat_flat_exact_baseline() {
        let dir =
            std::env::temp_dir().join(format!("skippr_ingest_bench_{}", rand::random::<u64>()));
        let result = run_local_fixture_benchmark_with_options(
            BenchmarkFixture::WatFlatExact,
            Some(10_000),
            &dir,
            true,
        );
        println!("{}", format_benchmark_report(&result));
        assert!(result.records_per_sec > 0.0);
        assert_eq!(result.profile.exact_arrow_rows, 0);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    #[ignore = "benchmark; run with `cargo test ingest_benchmark -- --ignored --nocapture`"]
    #[serial]
    fn bench_wat_flat_exact() {
        let dir =
            std::env::temp_dir().join(format!("skippr_ingest_bench_{}", rand::random::<u64>()));
        let result = run_local_fixture_benchmark(
            BenchmarkFixture::WatFlatExact,
            Some(benchmark_rows(10_000)),
            &dir,
        );
        println!("{}", format_benchmark_report(&result));
        assert!(result.profile.exact_arrow_rows > 0);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    #[ignore = "benchmark; run with `cargo test ingest_benchmark -- --ignored --nocapture`"]
    #[serial]
    fn bench_flat_exact_dates() {
        let dir =
            std::env::temp_dir().join(format!("skippr_ingest_bench_{}", rand::random::<u64>()));
        let result =
            run_local_fixture_benchmark(BenchmarkFixture::FlatExactDates, Some(10_000), &dir);
        println!("{}", format_benchmark_report(&result));
        assert!(result.profile.exact_arrow_rows > 0);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    #[ignore = "benchmark; run with `cargo test ingest_benchmark -- --ignored --nocapture`"]
    #[serial]
    fn bench_strings_heavy() {
        let dir =
            std::env::temp_dir().join(format!("skippr_ingest_bench_{}", rand::random::<u64>()));
        let result =
            run_local_fixture_benchmark(BenchmarkFixture::StringsHeavy, Some(10_000), &dir);
        println!("{}", format_benchmark_report(&result));
        assert!(result.profile.exact_arrow_rows > 0);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    #[ignore = "benchmark; run with `cargo test ingest_benchmark -- --ignored --nocapture`"]
    #[serial]
    fn bench_partition_skewed() {
        let dir =
            std::env::temp_dir().join(format!("skippr_ingest_bench_{}", rand::random::<u64>()));
        let result =
            run_local_fixture_benchmark(BenchmarkFixture::PartitionSkewed, Some(10_000), &dir);
        println!("{}", format_benchmark_report(&result));
        assert!(result.profile.exact_arrow_rows > 0);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    #[ignore = "benchmark; run with `cargo test ingest_benchmark -- --ignored --nocapture`"]
    #[serial]
    fn bench_fallback_mixed_fixture() {
        let dir =
            std::env::temp_dir().join(format!("skippr_ingest_bench_{}", rand::random::<u64>()));
        let result = run_local_fixture_benchmark(BenchmarkFixture::FallbackMixed, Some(2000), &dir);
        println!("{}", format_benchmark_report(&result));
        assert!(result.profile.exact_arrow_fallback_rows > 0);
        let _ = fs::remove_dir_all(dir);
    }
}
