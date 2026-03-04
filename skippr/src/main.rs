use rand::Rng;
use std::time::{Duration, SystemTime};

use arrow::datatypes::Schema;

// mod thread_pool;
// use thread_pool::ThreadPool;
// Note: Skippr now builds both a library (`skippr`) and a binary (`src/main.rs`).
// This binary should prefer importing functionality from the library to avoid duplicating modules.

extern crate nix;

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use std::{io, process};

use std::sync::Arc;
use std::thread;

use std::fs;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::sleep;
use std::time::Instant;

use skippr::cli::{Cli, Mode, CLI_MODE};
use skippr::discover::PipelineMetadata;

extern crate clap;
extern crate core;

use clap::Parser;

use signal_hook::iterator::Signals;

use std::panic;
use std::string::ToString;
// use datafusion::common::ExprSchema;

use once_cell::sync::Lazy;
use signal_hook::consts::{SIGABRT, SIGINT, SIGQUIT, SIGTERM};
use tokio::runtime;

// All modules are provided by the library crate `skippr`.

use skippr::helpers::configuration::{Config, PIPELINE_NAME};
use skippr::helpers::logging::init_logging;
use skippr::helpers::progress::ProgressUi;
use tracing::{error, info, warn};

use skippr::helpers::logger::{LogLevel, Logger};
use skippr::helpers::offsets::Offsets;

use skippr::plugins::athena::DataOutputAwsAthenaPlugin;

use skippr::plugins::s3_input::DataSourceS3Plugin;
// use crate::plugins::s3_inventory::DataSourceS3InventoryPlugin;

use skippr::metrics::{Metrics, MetricsStatus};
use skippr::plugins::file_input::DataSourceLocalFilePlugin;
use skippr::{
    ARROW_SCHEMA, ARROW_SCHEMA_VERSION, LOGGER, METADATA, METRICS,
    OUTPUT_GRACEFUL_SHUTDOWN_COMPLETE, OUTPUT_RUNNING, RUNNING,
};
// use crate::plugins::file_output::DataOutputFilePlugin;
// use crate::plugins::s3_output::DataOutputS3Plugin;
// use crate::plugins::stdin_input::DataSourceStdinPlugin;
// use crate::plugins::stdout_output::DataOutputStdoutPlugin;

use datafusion::prelude::*;
use skippr::buffer::ingest_buffer::{wal_recover, Buffers};
// use crate::buffer::BufferChunker;
use arc_swap::ArcSwap;
use skippr::benchmark::PerformanceBenchmark;
use skippr::helpers::timed_rwlock::TimedRwLock;
use skippr::ingest_work::Ingest;
use skippr::plugins::file_output::DataOutputFilePlugin;
use skippr::plugins::DataOutputPlugin;
use skippr::sqlrt::doc_parser::SqlDocParser;
use skippr::sqlrt::docs::{get_docs_in_format, DocFormat};
use skippr::sqlrt::query::query;
use std::io::IsTerminal as _;

// use crate::plugins::pcap_input::DataSourcePcapPlugin;

// pub static DISPLAY_METRICS: Lazy<TimedRwLock<AtomicBool>> =
//     Lazy::new(|| TimedRwLock::new("display_metrics".to_string(), AtomicBool::new(false)));

// Global runtime state now lives in the library crate (see `src/globals.rs`).

#[derive(Clone, Debug)]
struct PipelineCache {}

// @todo, last_ran should be the updated_at timestamp for the file DATA_DIR/LASTRAN
impl PipelineCache {
    fn get_metadata() -> fs::Metadata {
        let last_ran_file = format!("{}/LASTRAN", Config::get_data_dir());

        match fs::metadata(&last_ran_file) {
            Ok(metadata) => metadata,
            Err(_e) => PipelineCache::set_last_ran(),
        }
    }

    fn last_ran() -> SystemTime {
        PipelineCache::get_metadata().modified().unwrap()
    }

    fn set_last_ran() -> fs::Metadata {
        let last_ran_file = format!("{}/LASTRAN", Config::get_data_dir());

        fs::write(&last_ran_file, "").expect("Failed to write LASTRAN file");
        fs::metadata(&last_ran_file).expect("Failed to create LASTRAN file")
    }

    fn get_last_ran_elapsed() -> u64 {
        SystemTime::now()
            .duration_since(PipelineCache::last_ran())
            .unwrap()
            .as_secs()
    }

    fn last_ran_is_elapsed() -> bool {
        let duration = match SystemTime::now().duration_since(PipelineCache::last_ran()) {
            Ok(duration) => duration,
            Err(_e) => Duration::from_secs(0), // probably microsecond difference
        };

        duration.as_secs() > Config::get_sync_frequency() || duration.as_secs() == 0
        // just created on first run
    }
}

#[tokio::main]
async fn main() {
    // let now = Instant::now();

    // lazy_static! {
    //     static ref metadata: Mutex<HashMap<String, Metadata>> = Mutex::new(HashMap::new());
    //     static ref arrowSchema: Mutex<Result<Schema, ArrowError>> = Mutex::new(Ok(Schema::empty()));
    //     // static ref my_mutex: Mutex<i32> = Mutex::new(0i32);
    // }

    let cli: Cli = Cli::parse();

    // Initialize logging if --log is provided; default level is 'info', '--log debug' enables debug
    init_logging(cli.log.clone());

    CLI_MODE.write().clone_from(&cli.mode);

    match cli.mode {
        Mode::Sync(options) => {
            Config::build_config();

            Metrics::init_send_loop();

            if options.pipeline.is_some() {
                // println!("Syncing pipeline: {}", options.pipeline.unwrap().clone());
                PIPELINE_NAME.write().clear();
                PIPELINE_NAME
                    .write()
                    .push_str(&options.pipeline.unwrap().clone());
                Config::init().await;

                sync().await;
            } else {
                let pipeline_name = Config::getenv("PIPELINE_NAME", "");
                if !pipeline_name.is_empty() {
                    // println!("Syncing pipeline: {}", Config::getenv("PIPELINE_NAME").unwrap());
                    PIPELINE_NAME.write().clear();
                    PIPELINE_NAME.write().push_str(&pipeline_name.clone());
                    Config::init().await;

                    sync().await;
                } else {
                    info!("Syncing all pipelines");
                    let pipelines = Config::get_pipelines();
                    // loop {
                    for pipeline_name in pipelines {
                        Config::reset_envcache();
                        {
                            PIPELINE_NAME.write().clear();
                            PIPELINE_NAME.write().push_str(&pipeline_name.clone());
                        }
                        Config::init().await;

                        if !PipelineCache::last_ran_is_elapsed() {
                            let remaining = Config::get_sync_frequency()
                                - PipelineCache::get_last_ran_elapsed();
                            info!(
                                "Pipeline '{}' last ran {} seconds ago, skipping for {} seconds.",
                                &pipeline_name,
                                PipelineCache::get_last_ran_elapsed(),
                                remaining
                            );
                            continue;
                        }

                        PipelineCache::set_last_ran();

                        {
                            let mut counter_lock = METRICS.write();
                            counter_lock.reset();
                        }

                        sync().await;
                    }

                    // sleep(Duration::from_secs(10));
                    // }
                }
                // PIPELINE_NAME.write().unwrap().clear();
                // PIPELINE_NAME.write().unwrap().push_str(&options.pipeline.unwrap().clone());
                // Config::init().await;
                // sync().await;
            }
        }
        Mode::Discover(options) => {
            Config::build_config();

            if options.pipeline.is_some() {
                // println!("Syncing pipeline: {}", options.pipeline.unwrap().clone());
                PIPELINE_NAME.write().clear();
                PIPELINE_NAME
                    .write()
                    .push_str(&options.pipeline.unwrap().clone());
                Config::init().await;

                discover(options.log).await;
            } else {
                error!("No pipeline name provided, you must provide a pipeline name to discover schemas");
            }
        }
        Mode::Query(options) => {
            Config::build_config();
            if let Some(sql) = options.sql {
                let now = Instant::now();
                query(&sql).await;
                let elapsed = now.elapsed();
                if !options.plain {
                    println!("Query time: {} seconds", elapsed.as_secs());
                }
            } else {
                // Simple interactive REPL
                use std::io::{self, Write};
                println!("Skippr SQL REPL. Type SQL and press Enter. Type :q to quit.");
                loop {
                    print!("sql> ");
                    let _ = io::stdout().flush();
                    let mut line = String::new();
                    if io::stdin().read_line(&mut line).is_err() {
                        break;
                    }
                    let stmt = line.trim();
                    if stmt.is_empty() {
                        continue;
                    }
                    if stmt == ":q" || stmt == ":quit" || stmt.eq_ignore_ascii_case("exit") {
                        break;
                    }
                    let now = Instant::now();
                    query(stmt).await;
                    let elapsed = now.elapsed();
                    println!("Query time: {} seconds", elapsed.as_secs());
                }
            }
        }
        Mode::Schema(options) => {
            Config::build_config();
            // println!("Command schema");
            // Config::init().await;
            schema(&options.pipeline).await;
        }
        Mode::SqlHelp(options) => {
            // Handle SQL help and documentation
            if options.output.is_some() {
                // Generate documentation and save to file
                let output_path = options.output.unwrap();
                let format = match options.format.as_deref() {
                    Some("html") => DocFormat::Html,
                    Some("json") => DocFormat::Json,
                    _ => DocFormat::Markdown,
                };

                let content = get_docs_in_format(format);

                match std::fs::File::create(&output_path) {
                    Ok(mut file) => {
                        match std::io::Write::write_all(&mut file, content.as_bytes()) {
                            Ok(_) => {
                                println!(
                                    "SQL documentation generated and saved to: {}",
                                    output_path
                                );
                            }
                            Err(e) => {
                                println!("Error: Failed to write to file: {}", e);
                                process::exit(1);
                            }
                        }
                    }
                    Err(e) => {
                        println!("Error: Failed to create file: {}", e);
                        process::exit(1);
                    }
                }
            } else if options.command.is_some() {
                // Explain specific SQL command
                let command = options.command.unwrap();
                match SqlDocParser::parse_and_document(&command) {
                    Ok(Some(doc)) => {
                        println!("SQL Command: {}", doc.name);
                        println!();
                        println!("Description: {}", doc.description);
                        println!();
                        println!("Syntax: {}", doc.syntax);
                        println!();
                        println!("Example: {}", doc.example);
                    }
                    Ok(None) => {
                        println!("Unknown SQL command or standard SQL query.");
                        println!("If this is a standard SQL query, it may be supported by the system but not specifically documented.");
                    }
                    Err(e) => {
                        println!("Error: {}", e);
                    }
                }
            } else {
                // Show all SQL commands
                println!("Supported SQL Commands:");
                println!();

                // Group by category for better readability
                let mut schema_cmds = Vec::new();
                let mut pipeline_cmds = Vec::new();
                let mut data_cmds = Vec::new();
                let mut query_cmds = Vec::new();

                for doc in SqlDocParser::list_all_statements() {
                    if doc.name.contains("SCHEMA") {
                        schema_cmds.push(doc);
                    } else if doc.name.contains("PIPELINE") {
                        pipeline_cmds.push(doc);
                    } else if doc.name.contains("TABLE") || doc.name.contains("DATABASE") {
                        data_cmds.push(doc);
                    } else {
                        query_cmds.push(doc);
                    }
                }

                if !schema_cmds.is_empty() {
                    println!("Schema Operations:");
                    println!("-----------------");
                    for doc in schema_cmds {
                        println!("  {} - {}", doc.name, doc.description);
                    }
                    println!();
                }

                if !pipeline_cmds.is_empty() {
                    println!("Pipeline Operations:");
                    println!("-------------------");
                    for doc in pipeline_cmds {
                        println!("  {} - {}", doc.name, doc.description);
                    }
                    println!();
                }

                if !data_cmds.is_empty() {
                    println!("Data Operations:");
                    println!("---------------");
                    for doc in data_cmds {
                        println!("  {} - {}", doc.name, doc.description);
                    }
                    println!();
                }

                if !query_cmds.is_empty() {
                    println!("Query Operations:");
                    println!("----------------");
                    for doc in query_cmds {
                        println!("  {} - {}", doc.name, doc.description);
                    }
                    println!();
                }

                println!("For more details on a specific command, use:");
                println!("  skippr sql-help --command \"<SQL COMMAND>\"");
                println!();
                println!("To generate documentation, use:");
                println!("  skippr sql-help --output <FILE_PATH> [--format md|html|json]");
            }
        }
        Mode::Benchmark(options) => {
            Config::build_config();
            // Setup default pipeline name for benchmarking
            PIPELINE_NAME.write().clear();
            PIPELINE_NAME.write().push_str("benchmark");
            Config::init().await;

            // Initialize benchmark
            let benchmark = PerformanceBenchmark::new(
                options.num_files,
                options.records_per_file,
                options.record_size,
            );

            println!("Creating benchmark data...");
            match benchmark.create_benchmark_data() {
                Ok(total_bytes) => {
                    println!(
                        "Generated {} files with {} records each ({} bytes total)",
                        options.num_files, options.records_per_file, total_bytes
                    );

                    println!("Running benchmark '{}'...", options.name);

                    // Get description or use a default
                    let description = options.description.unwrap_or_else(|| {
                        if options.name == "baseline" {
                            "Baseline performance measurement".to_string()
                        } else {
                            format!("Performance test: {}", options.name)
                        }
                    });

                    match benchmark.run_benchmark(&options.name, &description).await {
                        Ok(_) => {
                            println!("Benchmark completed successfully");
                        }
                        Err(e) => {
                            println!("Benchmark failed: {}", e);
                            process::exit(1);
                        }
                    }
                }
                Err(e) => {
                    println!("Failed to create benchmark data: {}", e);
                    process::exit(1);
                }
            }
        }
    }
}

async fn schema(pipeline: &str) {
    // register the table
    // let mut options = ConfigOptions::default();
    // options.catalog.information_schema = true;

    let session_config = SessionConfig::new();
    let ctx = SessionContext::new_with_config(session_config);

    PIPELINE_NAME.write().clear();
    PIPELINE_NAME.write().push_str(&pipeline);
    Config::init().await;
    let workspace = Config::get_workspace_name();
    // Config::setenv("PIPELINE_NAME", &table_name);
    let _full_table_name = format!("{}.{}", workspace, pipeline);

    // @todo - check dir exists for provided table name, otherwise we end up creating erroneous dirs

    // itterate over data dir output buffers
    let data_dir = Config::get_data_dir();
    let output_dir = format!("{}/output_buffer", data_dir);

    info!("Querying data dir: {}", output_dir);

    // Use ListingTable for local output_buffer to inspect schema
    {
        use datafusion::datasource::file_format::parquet::ParquetFormat;
        use datafusion::datasource::listing::{
            ListingOptions, ListingTable, ListingTableConfig, ListingTableUrl,
        };
        let url = ListingTableUrl::parse(&output_dir).expect("invalid dir");
        let fmt = ParquetFormat::default();
        let opts = ListingOptions::new(Arc::new(fmt)).with_file_extension(".parquet");
        let cfg = ListingTableConfig::new(url).with_listing_options(opts);
        let table = ListingTable::try_new(cfg).expect("listing table");
        ctx.register_table(pipeline, Arc::new(table))
            .expect("register table");
    }
    let dfn = ctx.table(pipeline).await.unwrap();

    // print each field and type for schema:
    let schema = dfn.schema();
    let mut fields: Vec<String> = Vec::new();
    for i in 0..schema.fields().len() {
        fields.push(format!(
            "{}: {}",
            schema.field(i).name(),
            schema.field(i).data_type().to_string()
        ));
    }

    fields.sort();

    for field in fields {
        println!("{}", field);
    }
}

async fn discover(log: bool) {
    let pipeline_name = Config::get_pipeline_name();
    let _start_time = Instant::now();

    let stdout_is_tty = std::io::stdout().is_terminal();
    let progress = ProgressUi::new(stdout_is_tty && !log);
    if progress.enabled() {
        progress.add_tasks(&[
            "Ingesting",
            "Finalising",
            "Building stats",
            "Building catalog",
            "Creating embeddings",
        ]);
    }

    info!(
        "Analysing data and generating Skippr metadata for pipeline: {}",
        pipeline_name
    );

    // Stats tailer removed; catalogs built at end-of-run only

    let _data_dir = Config::get_data_dir();

    let pipeline_metadata = match Config::get_metadata().await {
        Ok(pipeline_metadata) => {
            info!("Found existing Skippr metadata, will update with schema discovered from sampled data");

            pipeline_metadata
        }
        Err(_e) => {
            info!("No existing Skippr metadata, will discover schemas");

            PipelineMetadata::new()
        }
    };

    METADATA.store(Arc::new(pipeline_metadata.clone()));

    let offsets = match Offsets::init() {
        Ok(offsets) => offsets,
        Err(e) => {
            error!("Skipping: {}", e);
            return;
        }
    };

    let offsets_db = Arc::new(offsets);

    // let output = DataOutputAwsAthenaPlugin::new("output".to_string()).await;
    // let output_plugin_name = Config::get_pipeline_output_plugin_name();
    // let output = sync_output_plugin(&output_plugin_name, "output".to_string()).await.unwrap();
    // let shared_output = Arc::new(TimedRwLock::new("output_plugin".to_string(), output));
    let output_plugin_name = Config::get_pipeline_output_plugin_name();
    let output = sync_output_plugin(&output_plugin_name, "output".to_string())
        .await
        .unwrap();
    let shared_output = Arc::new(output);

    // sync schema if output plugin configured
    if output_plugin_name != "" {
        Config::sync_schema(&pipeline_metadata.metadata).await;
    } else {
        // Just build the arrow schemas internally
        let flatten = Config::get_transform_flatten_events();
        for (namespace, _metadata) in pipeline_metadata.metadata.iter() {
            match Ingest::prepare_arrow_schema_with_metadata(
                &namespace,
                &pipeline_metadata.metadata,
                flatten,
            ) {
                Ok(_t) => {}
                Err(e) => {
                    error!("Failed to prepare arrow schema: {}", e);
                    return;
                }
            }
        }
    }

    let shared_output_clone = shared_output.clone();
    if progress.enabled() {
        progress.start("Ingesting");
    }
    sync_input_plugin(offsets_db.clone(), shared_output_clone).await;
    if progress.enabled() {
        progress.complete("Ingesting");
    }

    info!("Reached end of source data");
    info!(
        "Discover completed, syncing schema to output plugin {}",
        Config::get_pipeline_config()
            .output
            .or(Some("".to_string()))
            .unwrap()
    );

    // Stats tailer removed; stats computed by orchestrator from DataFusion at end-of-run

    // Late rebuild from existing S3 parquet if no new data (bounded)
    // (legacy catalog module removed; this is now provider-driven)
    // Stats tailer disabled

    // NOTE: Catalog build is handled by the dedicated runtime service.

    // Final metrics snapshot (same as periodic per-minute print)
    // if log {
    use std::sync::atomic::Ordering as AtomicOrdering;
    let messages_total_counter =
        skippr::metrics::counters::MESSAGES_TOTAL.load(AtomicOrdering::Relaxed);
    let source_bytes_total_counter =
        skippr::metrics::counters::SOURCE_BYTES_TOTAL.load(AtomicOrdering::Relaxed);
    let deadletters_total_counter =
        skippr::metrics::counters::DEADLETTERS_TOTAL.load(AtomicOrdering::Relaxed);
    let _ingested_slow_total_counter =
        skippr::metrics::counters::INGESTED_SLOW_TOTAL.load(AtomicOrdering::Relaxed);
    let human_bytes = skippr::helpers::Helpers::human_readable_size(source_bytes_total_counter);
    info!("Messages per Min: {}", 0);
    info!("Messages Fixed per Min: {}", 0);
    info!("Bytes Total: {}", human_bytes);
    info!("Messages Total: {}", messages_total_counter);
    info!("Deadletters per Min: {}", 0);
    info!("Deadletter Total: {}", deadletters_total_counter);
    // Runtime not directly accessible here; print 0 to keep format consistent
    info!("Runtime: {} seconds", 0);
    let up_total = skippr::metrics::counters::UPLOADS_TOTAL.load(AtomicOrdering::SeqCst);
    let up_inflight = skippr::metrics::counters::UPLOADS_IN_FLIGHT.load(AtomicOrdering::SeqCst);
    let up_lat_ns_total =
        skippr::metrics::counters::UPLOAD_LATENCY_NS_TOTAL.load(AtomicOrdering::SeqCst);
    let avg_up_ms = if up_total > 0 {
        (up_lat_ns_total / up_total) as f64 / 1_000_000.0
    } else {
        0.0
    };
    let up_target =
        skippr::metrics::counters::UPLOAD_CONCURRENCY_TARGET.load(AtomicOrdering::SeqCst);
    let wal_target =
        skippr::metrics::counters::WAL_COMPACTION_CONCURRENCY_TARGET.load(AtomicOrdering::SeqCst);
    let dl_target =
        skippr::metrics::counters::S3_DOWNLOAD_CONCURRENCY_TARGET.load(AtomicOrdering::SeqCst);
    let active = skippr::metrics::counters::ACTIVE_THREADS.load(AtomicOrdering::SeqCst);
    let queue = skippr::metrics::counters::QUEUE_LENGTH.load(AtomicOrdering::SeqCst);
    info!(
        "Uploads total: {}, inflight: {}, avg latency: {:.2} ms",
        up_total, up_inflight, avg_up_ms
    );
    info!(
        "Targets - upload: {}, wal: {}, s3_download: {} | active: {}, queue: {}",
        up_target, wal_target, dl_target, active, queue
    );
    // }

    // LLM enrichment is run via the catalog provider (see provider.run_llm_enrichment_all above).

    // Insightful LLM summary based on Catalog Stats (not ingest counters)
    {
        use chrono::{TimeZone, Utc};
        use std::collections::HashMap;
        // Collect namespaces from registry (preferred) or metadata
        let pipeline = Config::get_pipeline_name();
        let mut namespaces = skippr::sqlrt::registry::list_namespaces(&pipeline).await;
        if namespaces.is_empty() {
            namespaces = pipeline_metadata
                .metadata
                .keys()
                .cloned()
                .collect::<Vec<_>>();
        }

        // Gather per-namespace stats and descriptions
        #[derive(Clone, Default)]
        struct NsSummary {
            approx_rows: u64,
            desc: Option<String>,
            earliest_ts: Option<i64>,
            latest_ts: Option<i64>,
        }
        let mut by_ns: HashMap<String, NsSummary> = HashMap::new();

        fn parse_epoch_to_secs(x: f64) -> Option<i64> {
            let v = x as i64;
            if v <= 0 {
                return None;
            }
            if v > 1_000_000_000_000_000 {
                // micros
                Some(v / 1_000_000)
            } else if v > 1_000_000_000_000 {
                // millis
                Some(v / 1_000)
            } else if v > 1_000_000_000 {
                // seconds
                Some(v)
            } else {
                None
            }
        }

        for ns in namespaces.iter() {
            // Stats → approx rows and date range heuristic
            if let Some(v) = Config::read_namespace_stats_async(ns).await {
                if let Ok(stats) =
                    serde_json::from_value::<skippr::discover::stats::NamespaceStats>(v)
                {
                    let mut approx_rows: u64 = 0;
                    let mut min_ts: Option<i64> = None;
                    let mut max_ts: Option<i64> = None;
                    for (fname, fs) in stats.fields.iter() {
                        approx_rows = approx_rows.max(fs.sample_total.unwrap_or(fs.total));
                        // Name-agnostic: infer time window only when numeric epoch-like stats are present
                        if let Some(min_num) = fs.min_numeric {
                            if let Some(s) = parse_epoch_to_secs(min_num) {
                                min_ts = Some(min_ts.map(|m| m.min(s)).unwrap_or(s));
                            }
                        }
                        if let Some(max_num) = fs.max_numeric {
                            if let Some(s) = parse_epoch_to_secs(max_num) {
                                max_ts = Some(max_ts.map(|m| m.max(s)).unwrap_or(s));
                            }
                        }
                    }
                    by_ns.insert(
                        ns.clone(),
                        NsSummary {
                            approx_rows,
                            desc: None,
                            earliest_ts: min_ts,
                            latest_ts: max_ts,
                        },
                    );
                }
            }
            // Catalog → description
            if let Some(entry) = skippr::sqlrt::registry::find_entry(&pipeline, ns).await {
                if !entry.catalog_key.is_empty() {
                    if let Ok(val) = skippr::helpers::s3::get_json(&entry.catalog_key).await {
                        if let Some(s) = val.get("description").and_then(|x| x.as_str()) {
                            by_ns.entry(ns.clone()).or_default().desc = Some(s.to_string());
                        }
                    }
                }
            }
        }

        let tables = namespaces.len();
        let approx_total: u64 = by_ns.values().map(|s| s.approx_rows).sum();
        let earliest_any: Option<i64> = by_ns.values().filter_map(|s| s.earliest_ts).min();
        let latest_any: Option<i64> = by_ns.values().filter_map(|s| s.latest_ts).max();
        let period_str = match (earliest_any, latest_any) {
            (Some(a), Some(b)) => {
                let a_dt = Utc.timestamp_opt(a, 0).single();
                let b_dt = Utc.timestamp_opt(b, 0).single();
                match (a_dt, b_dt) {
                    (Some(x), Some(y)) => format!("{} → {}", x.date_naive(), y.date_naive()),
                    _ => String::new(),
                }
            }
            (Some(a), None) => Utc
                .timestamp_opt(a, 0)
                .single()
                .map(|d| d.date_naive().to_string())
                .unwrap_or_default(),
            _ => String::new(),
        };

        // Build a compact digest of namespaces
        let mut digest_lines: Vec<String> = Vec::new();
        for ns in namespaces.iter() {
            let s = by_ns.get(ns).cloned().unwrap_or_default();
            let d = s.desc.unwrap_or_else(|| String::from(""));
            let line = if d.is_empty() {
                format!("{} (≈{} rows)", ns, s.approx_rows)
            } else {
                format!("{}: {} (≈{} rows)", ns, d, s.approx_rows)
            };
            digest_lines.push(line);
            if digest_lines.len() >= 6 {
                break;
            } // cap prompt size
        }

        if period_str.is_empty() {
            println!(
                "Warehouse spans {} table(s) with ≈{} rows in total.",
                tables, approx_total
            );
        } else {
            println!(
                "Warehouse spans {} table(s) with ≈{} rows from {}.",
                tables, approx_total, period_str
            );
        }
    }

    // NOTE: Embeddings sync is handled by the dedicated runtime service.

    {
        let mut counter_lock = METRICS.write();
        counter_lock.status = MetricsStatus::Finishing;
    }

    if progress.enabled() {
        progress.start("Finalising");
    }
    while OUTPUT_RUNNING.read().load(Ordering::SeqCst) {
        sleep(Duration::from_secs(1));
    }

    {
        OUTPUT_RUNNING.write().store(true, Ordering::SeqCst);
    }

    {
        OUTPUT_RUNNING.write().store(false, Ordering::SeqCst);
    }

    {
        let mut counter_lock = METRICS.write();
        counter_lock.status = MetricsStatus::Completed;
    }

    {
        let counter_lock = METRICS.write();

        info!("Messages Fixed: {}", counter_lock.ingeted_slow_total);
        info!("Deadletter Total: {}", counter_lock.deadletters_total);
        info!("Ingested Total: {}", counter_lock.messages_total);
    }

    match Metrics::send_metrics(Some(0)).await {
        Ok(_res) => (),
        Err(e) => {
            LOGGER
                .write()
                .await
                .log(
                    LogLevel::Error,
                    format!("Failed to send metrics to Skippr API: {}", e),
                )
                .await;
        }
    }

    if log {
        if !LOGGER.read().await.logs.is_empty() {
            LOGGER.write().await.flush().await.unwrap();
        }
    }

    if progress.enabled() {
        progress.complete("Finalising");
        progress.finish();
    }
}

async fn sync() {
    let pipeline_name = Config::get_pipeline_name();

    let stdout_is_tty = std::io::stdout().is_terminal();
    let progress = ProgressUi::new(stdout_is_tty && !skippr::helpers::logging::cli_logs_enabled());
    if progress.enabled() {
        progress.add_tasks(&[
            "Ingesting",
            "Finalising",
            "Building stats",
            "Building catalog",
            "Creating embeddings",
        ]);
    }

    {
        let mut counter_lock = METRICS.write();
        counter_lock.status = MetricsStatus::Running;
    }

    {
        match Metrics::send_config().await {
            Ok(_res) => (),
            Err(e) => {
                LOGGER
                    .write()
                    .await
                    .log(
                        LogLevel::Error,
                        format!("Failed to send config to Skippr API: {}", e),
                    )
                    .await;
            }
        }
    }

    let pipeline_metadata: PipelineMetadata;

    // @todo - we don't cache Pipeline metatdata, as currently SQL statements are not stored in metadata.
    //         Refactor to accept SQL directly via database connection
    pipeline_metadata = match Config::get_metadata().await {
        Ok(pipeline_metadata) => {
            info!("Found existing Skippr metadata");

            match pipeline_metadata.sql {
                Some(sql) => {
                    for stmt in sql {
                        info!("Recieved SQL statement: '{}'", stmt);

                        // Important to exec the SQL before saving metadata, as the SQL may drop or otherwise alter the metadata
                        query(&stmt).await;
                    }

                    return;
                }
                None => {}
            }

            if !pipeline_metadata.enabled {
                info!("Pipeline '{}' disabled, skipping.", pipeline_name);
                return;
            }

            pipeline_metadata
        }
        Err(_e) => {
            // println!("No existing Skippr metadata, skipping pipeline '{}'. Init the pipeline with 'skippr discover' to create metadata.", pipeline_name);

            // return;
            PipelineMetadata::new()
        }
    };

    info!("Syncing pipeline: {}", pipeline_name);
    // Stats tailer removed; catalogs built at end-of-run only

    METADATA.store(Arc::new(pipeline_metadata.clone()));

    let offsets_db = match Offsets::init() {
        Ok(offsets) => offsets,
        Err(e) => {
            error!("Skipping: {}", e);
            return;
        }
    };

    let offsets_db = Arc::new(offsets_db);

    let _offsets_clone = offsets_db.clone();

    // One-time migration: backfill .seg.commit and cleanup legacy segs before WAL recovery
    Buffers::migrate_segs_once();

    wal_recover(offsets_db.clone()).expect("Failed to recover WAL index");

    {
        METRICS.write().status = MetricsStatus::Running;
    }

    let output_plugin_name = Config::get_pipeline_output_plugin_name();
    let output = sync_output_plugin(&output_plugin_name, "output".to_string())
        .await
        .unwrap();
    let shared_output = Arc::new(output);

    // Start background WAL compactor pool after WAL recovery
    Buffers::start_single_consumer(shared_output.clone(), offsets_db.clone());

    let shared_output_clone = shared_output.clone();

    // Arm chaos interrupt for sync runs using the old planner (deterministic tick)
    let mut out_pnanner = periodic::Planner::new();
    if Config::get_pipeline_chaos_mode() {
        out_pnanner.add(
            move || {
                if RUNNING.read().load(Ordering::SeqCst) {
                    warn!("Chaos mode throwing a random exit. You can disable this test mode buy removing CHAOS_MODE flag or setting to 'no'");
                    let pid = process::id() as i32;
                    let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
                }
            },
            periodic::Every::new(Duration::from_secs(rand::thread_rng().gen_range(60..90))),
        );
    }

    // sync schema if output plugin configured
    if output_plugin_name != "" {
        Config::sync_schema(&pipeline_metadata.metadata).await;
    } else {
        // Just build the arrow schemas internally
        let flatten = Config::get_transform_flatten_events();
        for (namespace, _metadata) in pipeline_metadata.metadata.iter() {
            match Ingest::prepare_arrow_schema_with_metadata(
                &namespace,
                &pipeline_metadata.metadata,
                flatten,
            ) {
                Ok(_t) => {}
                Err(e) => {
                    error!("Failed to prepare arrow schema: {}", e);
                    return;
                }
            }
        }
    }

    if progress.enabled() {
        progress.start("Ingesting");
    }
    sync_input_plugin(offsets_db.clone(), shared_output_clone).await;
    if progress.enabled() {
        progress.complete("Ingesting");
    }

    info!("Reached end of source data");
    info!(
        "Ingest completed, flushing remaining buffers to output plugin {}",
        Config::get_pipeline_config()
            .output
            .or(Some("".to_string()))
            .unwrap()
    );

    {
        METRICS.write().status = MetricsStatus::Finishing;
    }

    // Queue-based model: rely on the explicit drain above; skip legacy compact_all_partitions

    info!("All buffers flushed to output plugin");

    // Deterministic drain: compact all remaining on-disk segments to parquet
    {
        if progress.enabled() {
            progress.start("Finalising");
        }
        let finalising_started = std::time::Instant::now();
        info!("Finalising: stopping background compactor");
        // Stop background compactor pool and wait for in-flight to drain
        Buffers::request_compactor_stop();
        info!("Finalising: waiting for in-flight compactions to drain");
        // Wait for in-flight to reach zero (bounded wait)
        for i in 0..40 {
            let inflight = skippr::metrics::counters::WAL_COMPACTIONS_IN_FLIGHT
                .load(std::sync::atomic::Ordering::Relaxed);
            if inflight == 0 {
                info!("Finalising: in-flight compactions drained after {} checks", i + 1);
                break;
            }
            if i % 10 == 9 {
                info!(
                    "Finalising: waiting for in-flight compactions (inflight={})",
                    inflight
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        let remaining_inflight = skippr::metrics::counters::WAL_COMPACTIONS_IN_FLIGHT
            .load(std::sync::atomic::Ordering::Relaxed);
        if remaining_inflight > 0 {
            warn!(
                "Finalising: compactor drain timeout reached with {} in-flight tasks",
                remaining_inflight
            );
        }
        info!("Finalising: running forced compaction pass 1");
        Buffers::compact_all_partitions(true, offsets_db.clone(), shared_output.clone()).await;
        // Safety loop: if any .seg remain, run another pass (handles late live persist)
        if Buffers::segs_remaining() > 0 {
            info!("Finalising: running forced compaction pass 2");
            Buffers::compact_all_partitions(true, offsets_db.clone(), shared_output.clone()).await;
        }
        let (scanned_commits, removed_orphans, orphan_errors) =
            Buffers::cleanup_orphan_seg_commits(200_000);
        info!(
            "Finalising: orphan commit cleanup scanned={} removed={} errors={}",
            scanned_commits, removed_orphans, orphan_errors
        );
        info!(
            "Finalising: compaction+cleanup finished in {:?}",
            finalising_started.elapsed()
        );
    }
    // Single-thread model: no background compaction tasks remain here
    // Wait for background Glue partition tasks to settle to avoid undercount at end
    info!("Finalising: waiting for Athena partition tasks to drain");
    skippr::plugins::athena::DataOutputAwsAthenaPlugin::await_partition_tasks_zero().await;
    info!("Finalising: Athena partition tasks drained");
    if progress.enabled() {
        progress.complete("Finalising");
    }

    // Summary and integrity check: uploaded rows vs expected msgs, quarantined parts
    {
        use std::sync::atomic::Ordering as AO;
        let uploaded_rows =
            skippr::metrics::counters::PARQUET_PERSISTED_ROWS_TOTAL.load(AO::Relaxed);
        let expected_msgs = skippr::metrics::counters::MESSAGES_TOTAL.load(AO::Relaxed);
        let quarantined_parts =
            skippr::metrics::counters::QUARANTINED_PARTITIONS_TOTAL.load(AO::Relaxed);
        info!(
            "Compactor: summary uploaded_rows={} expected_msgs={} quarantined_parts={}",
            uploaded_rows, expected_msgs, quarantined_parts
        );
        if quarantined_parts > 0 || uploaded_rows != expected_msgs {
            warn!("Compactor: integrity check mismatch (uploaded_rows != expected_msgs or quarantined_parts > 0). Proceeding; this may occur when compacting pre-existing WAL.");
        }
    }

    {
        METRICS.write().status = MetricsStatus::Completed;
    }

    match Metrics::send_metrics(Some(0)).await {
        Ok(_res) => (),
        Err(e) => {
            LOGGER
                .write()
                .await
                .log(
                    LogLevel::Error,
                    format!("Failed to send metrics to Skippr API: {}", e),
                )
                .await;
        }
    }

    // Note: sync_license is not implemented in the Config struct
    // Commenting out the license sync call
    // if let Err(err) = Config::sync_license().await {
    //     LOGGER
    //         .write()
    //         .await
    //         .log(LogLevel::Error, format!("Failed to sync license: {}", err))
    //         .await;
    // }

    // Final concise metrics
    {
        use skippr::metrics::counters;
        let m = METRICS.read();
        let messages_total =
            m.messages_total + counters::MESSAGES_TOTAL.load(std::sync::atomic::Ordering::Relaxed);
        let source_bytes_total = m.source_bytes_total
            + counters::SOURCE_BYTES_TOTAL.load(std::sync::atomic::Ordering::Relaxed);
        let parquet_objects = m.parquet_persisted_objects_total
            + counters::PARQUET_PERSISTED_OBJECTS_TOTAL.load(std::sync::atomic::Ordering::Relaxed);
        let parquet_rows = m.parquet_persisted_rows_total
            + counters::PARQUET_PERSISTED_ROWS_TOTAL.load(std::sync::atomic::Ordering::Relaxed);
        let parquet_bytes = m.parquet_persisted_bytes_total
            + counters::PARQUET_PERSISTED_BYTES_TOTAL.load(std::sync::atomic::Ordering::Relaxed);
        info!(
            "Final metrics: msgs_total={} src_bytes_total={} parquet_rows_total={} parquet_bytes_total={} parquet_objects_total={}",
            messages_total,
            source_bytes_total,
            parquet_rows,
            parquet_bytes,
            parquet_objects
        );
    }
    info!("Pipeline sync complete");
    // NOTE: Catalog/semantic/embeddings work is handled by the dedicated runtime service.
    if progress.enabled() {
        progress.finish();
    }
}

// legacy no-op; replaced by catalog::orchestrator
async fn build_catalog(_pipeline_metadata: &PipelineMetadata) {}

pub async fn sync_output_plugin(
    plugin_name: &str,
    buffer_name: String,
) -> Result<Box<dyn DataOutputPlugin + Send + Sync>, io::Error> {
    info!("Output plugin: {}", plugin_name);

    match plugin_name {
        // "stdout" => {
        //     let output = DataOutputStdoutPlugin::new(buffer_name).await;
        //     output
        //         .sync()
        //         .await;
        // }
        "File" => {
            let plugin = DataOutputFilePlugin::new(buffer_name).await;
            Ok(Box::new(plugin) as Box<dyn DataOutputPlugin + Send + Sync>)
        }
        // "s3" => {
        //     if *HAS_LICENSE.read() {
        //         let output = DataOutputS3Plugin::new(buffer_name).await;
        //         output
        //             .sync()
        //             .await;
        //     } else {
        //         println!("No license found for S3 output plugin. Visit https://skippr.io to get a license.");
        //     }
        //
        // }
        "Athena" => {
            let plugin = DataOutputAwsAthenaPlugin::new(buffer_name).await;
            Ok(Box::new(plugin) as Box<dyn DataOutputPlugin + Send + Sync>)
        }
        "" => {
            info!(
                "No Data {} plugin specified, defaulting to local file",
                buffer_name
            );
            let plugin = DataOutputFilePlugin::new(buffer_name).await;
            Ok(Box::new(plugin) as Box<dyn DataOutputPlugin + Send + Sync>)
        }
        _ => {
            // println!("Unknown Data {} plugin specified", buffer_name);
            Err(io::Error::new(
                io::ErrorKind::Other,
                "Unknown Data plugin specified",
            ))
        }
    }
}

pub async fn sync_input_plugin(
    offsets_clone: Arc<Offsets>,
    shared_output: Arc<Box<dyn DataOutputPlugin + Send + Sync>>,
) {
    match Config::get_pipeline_input_plugin_name().as_str() {
        // "pcap" => {
        //     panic!("PCAP input plugin not installed, please contact support")
        //     // let mut input = DataSourcePcapPlugin::new().await;
        //     // input
        //     //     .sync(
        //     //         offsets_clone,
        //     //     )
        //     //     .await;
        // }
        // "stdin" => {
        //     let mut input = DataSourceStdinPlugin::new().await;
        //     input
        //         .sync(
        //             offsets_clone,
        //         )
        //         .await;
        // }
        "File" => {
            let mut input = DataSourceLocalFilePlugin::new().await;
            input.sync(offsets_clone, shared_output).await;
        }
        "S3" => {
            let mut input = DataSourceS3Plugin::new().await;
            input.sync(offsets_clone, shared_output).await;
        }
        // "s3_inventory" => {
        //     if *HAS_LICENSE.read() {
        //         let mut input = DataSourceS3InventoryPlugin::new().await;
        //         input.sync(
        //             offsets_clone,
        //         )
        //             .await;
        //     } else {
        //         println!("No license found for S3 Inventory input plugin. Visit https://skippr.io to get a license.");
        //     }
        // }
        "" => {
            error!("No Data Source plugin specified. You must specify a data source plugin, see documentation for the DATA_SOURCE_PLUGIN_NAME environment variable.");
        }
        unknown => {
            error!("Data Source Plugin {} not supported", unknown);
        }
    }
}
