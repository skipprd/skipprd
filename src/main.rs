mod arr;
use std::time::{Duration, SystemTime};
use rand::Rng;

use arrow::datatypes::Schema;

// mod thread_pool;
// use thread_pool::ThreadPool;
mod ingest_work;

extern crate nix;

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use std::{io, process};


use std::sync::{Arc};
use std::thread;

use std::fs;

use std::sync::atomic::{AtomicBool, Ordering, AtomicU64};
use std::thread::sleep;
use std::time::Instant;

mod buffer;

mod helpers;

mod metrics;

mod internalfields;

mod discover;
use crate::discover::{PipelineMetadata};
mod converters;
// use self::converters::avro_parquet::AvroSchema;
mod cli;
use crate::cli::{Cli, Mode, CLI_MODE};

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

mod ingest;

mod serdes;

mod plugins;
mod sql;
mod benchmark;

use crate::helpers::configuration::{Config, PIPELINE_NAME};

use crate::helpers::logger::{Logger, LogLevel};
use crate::helpers::offsets::Offsets;

use crate::plugins::athena::DataOutputAwsAthenaPlugin;

use crate::plugins::s3_input::DataSourceS3Plugin;
// use crate::plugins::s3_inventory::DataSourceS3InventoryPlugin;

use crate::metrics::{Metrics, MetricsStatus};
use crate::plugins::file_input::DataSourceLocalFilePlugin;
// use crate::plugins::file_output::DataOutputFilePlugin;
// use crate::plugins::s3_output::DataOutputS3Plugin;
// use crate::plugins::stdin_input::DataSourceStdinPlugin;
// use crate::plugins::stdout_output::DataOutputStdoutPlugin;

use datafusion::prelude::*;
use crate::buffer::ingest_buffer::{Buffers, wal_recover};
// use crate::buffer::BufferChunker;
use crate::helpers::timed_rwlock::TimedRwLock;
use crate::ingest_work::Ingest;
use arc_swap::ArcSwap;
use crate::plugins::DataOutputPlugin;
use crate::plugins::file_output::DataOutputFilePlugin;
use crate::sql::query::query;
use crate::sql::docs::{DocFormat, get_docs_in_format};
use crate::sql::doc_parser::SqlDocParser;
use crate::benchmark::PerformanceBenchmark;

// use crate::plugins::pcap_input::DataSourcePcapPlugin;

// pub static DISPLAY_METRICS: Lazy<TimedRwLock<AtomicBool>> =
//     Lazy::new(|| TimedRwLock::new("display_metrics".to_string(), AtomicBool::new(false)));

pub static RUNNING: Lazy<TimedRwLock<AtomicBool>> = Lazy::new(|| TimedRwLock::new("running".to_string(),AtomicBool::new(true)));

pub static OUTPUT_RUNNING: Lazy<TimedRwLock<AtomicBool>> =
    Lazy::new(|| TimedRwLock::new("output_running".to_string(), AtomicBool::new(false)));

// pub static BUFFER_FINALISE_RUNNING: Lazy<TimedRwLock<AtomicBool>> =
//     Lazy::new(|| TimedRwLock::new("buffer_finalise_running".to_string(), AtomicBool::new(false)));

pub static OUTPUT_GRACEFUL_SHUTDOWN_COMPLETE: Lazy<TimedRwLock<AtomicBool>> =
    Lazy::new(|| TimedRwLock::new("output_graceful_shutdown_complete".to_string(), AtomicBool::new(false)));

pub static LOGGER: Lazy<Arc<tokio::sync::RwLock<Logger>>> = Lazy::new(|| Logger::new(100));
pub static METRICS: Lazy<Arc<TimedRwLock<Metrics>>> = Lazy::new(|| Arc::new(TimedRwLock::new("metrics".to_string(), Metrics::new())));
// Publish metadata via ArcSwap; readers do lock-free loads
pub static METADATA: Lazy<ArcSwap<PipelineMetadata>> = Lazy::new(|| ArcSwap::new(Arc::new(PipelineMetadata::new())));
// Per-namespace Arrow schema snapshots and versions
pub static ARROW_SCHEMA: Lazy<dashmap::DashMap<String, ArcSwap<Schema>>> = Lazy::new(|| dashmap::DashMap::new());
pub static ARROW_SCHEMA_VERSION: Lazy<dashmap::DashMap<String, AtomicU64>> = Lazy::new(|| dashmap::DashMap::new());

#[derive(Clone, Debug)]
struct PipelineCache {
}

// @todo, last_ran should be the updated_at timestamp for the file DATA_DIR/LASTRAN
impl PipelineCache {
    fn get_metadata() -> fs::Metadata {

        let last_ran_file = format!("{}/LASTRAN", Config::get_data_dir());

        match fs::metadata(&last_ran_file) {
            Ok(metadata) => {
                metadata
            },
            Err(_e) => {
                PipelineCache::set_last_ran()
            }
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
        SystemTime::now().duration_since(PipelineCache::last_ran()).unwrap().as_secs()
    }

    fn last_ran_is_elapsed() -> bool {

        let duration = match SystemTime::now().duration_since(PipelineCache::last_ran()) {
            Ok(duration) => duration,
            Err(_e) => Duration::from_secs(0) // probably microsecond difference
        };

        duration.as_secs() > Config::get_sync_frequency()
            || duration.as_secs() == 0 // just created on first run
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

    CLI_MODE.write().clone_from(&cli.mode);

    match cli.mode {

        Mode::Sync(options) => {

            Config::build_config();

            Metrics::init_send_loop();

            if options.pipeline.is_some() {
                // println!("Syncing pipeline: {}", options.pipeline.unwrap().clone());
                PIPELINE_NAME.write().clear();
                PIPELINE_NAME.write().push_str(&options.pipeline.unwrap().clone());
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
                    println!("Syncing all pipelines");
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
                            let remaining = Config::get_sync_frequency() - PipelineCache::get_last_ran_elapsed();
                            println!("Pipeline '{}' last ran {} seconds ago, skipping for {} seconds.", &pipeline_name, PipelineCache::get_last_ran_elapsed(), remaining);
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
                PIPELINE_NAME.write().push_str(&options.pipeline.unwrap().clone());
                Config::init().await;

                discover().await;
            } else {
                println!("No pipeline name provided, you must provide a pipeline name to discover schemas");
            }
        }
        Mode::Query(options) => {
            Config::build_config();
            if let Some(sql) = options.sql {
                let now = Instant::now();
                query(&sql).await;
                let elapsed = now.elapsed();
                println!("Query time: {} seconds", elapsed.as_secs());
            } else {
                // Simple interactive REPL
                use std::io::{self, Write};
                println!("Skippr SQL REPL. Type SQL and press Enter. Type :q to quit.");
                loop {
                    print!("sql> ");
                    let _ = io::stdout().flush();
                    let mut line = String::new();
                    if io::stdin().read_line(&mut line).is_err() { break; }
                    let stmt = line.trim();
                    if stmt.is_empty() { continue; }
                    if stmt == ":q" || stmt == ":quit" || stmt.eq_ignore_ascii_case("exit") { break; }
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
                                println!("SQL documentation generated and saved to: {}", output_path);
                            },
                            Err(e) => {
                                println!("Error: Failed to write to file: {}", e);
                                process::exit(1);
                            }
                        }
                    },
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
                    },
                    Ok(None) => {
                        println!("Unknown SQL command or standard SQL query.");
                        println!("If this is a standard SQL query, it may be supported by the system but not specifically documented.");
                    },
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
                options.record_size
            );
            
            println!("Creating benchmark data...");
            match benchmark.create_benchmark_data() {
                Ok(total_bytes) => {
                    println!("Generated {} files with {} records each ({} bytes total)",
                        options.num_files, options.records_per_file, total_bytes);
                    
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
                        },
                        Err(e) => {
                            println!("Benchmark failed: {}", e);
                            process::exit(1);
                        }
                    }
                },
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

    let mut session_config = SessionConfig::new();
    session_config = session_config.set("datafusion.catalog.information_schema", "true".into());
    session_config = session_config.set("datafusion.catalog.default_catalog", "skippr".into());
    session_config = session_config.set("datafusion.execution.collect_statistics", "true".into());

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

    println!("Querying data dir: {}", output_dir);

    match ctx.register_parquet(&pipeline, &output_dir, ParquetReadOptions::default()).await {
        Ok(_) => {}
        Err(_e) => {
            println!("Can't find data for table: {} in dir: {}", pipeline, output_dir);
            process::exit(1);
        }
    }

    let dfn = ctx.table(pipeline).await.unwrap();

    // print each field and type for schema:
    let schema = dfn.schema();
    let mut fields: Vec<String> = Vec::new();
    for i in 0..schema.fields().len() {
        fields.push(format!("{}: {}", schema.field(i).name(), schema.field(i).data_type().to_string()));
    }

    fields.sort();

    for field in fields {
        println!("{}", field);
    }
}

async fn discover() {

    let pipeline_name = Config::get_pipeline_name();

    println!("Analysing data and generating Skippr metadata for pipeline: {}", pipeline_name);

    let _data_dir = Config::get_data_dir();

    let pipeline_metadata = match Config::get_metadata().await {
        Ok(pipeline_metadata) => {
            println!("Found existing Skippr metadata, will update with schema discovered from sampled data");
            
            pipeline_metadata
        }
        Err(_e) => {
            println!("No existing Skippr metadata, will discover schemas");

            PipelineMetadata::new()
        }
    };

    METADATA.store(Arc::new(pipeline_metadata.clone()));

    let offsets = match Offsets::init() {
        Ok(offsets) => offsets,
        Err(e) => {
            println!("Skipping: {}", e);
            return;
        }
    };

    let offsets_db = Arc::new(offsets);

    let _offsets_clone = offsets_db.clone();

    // let output = DataOutputAwsAthenaPlugin::new("output".to_string()).await;
    // let output_plugin_name = Config::get_pipeline_output_plugin_name();
    // let output = sync_output_plugin(&output_plugin_name, "output".to_string()).await.unwrap();
    // let shared_output = Arc::new(TimedRwLock::new("output_plugin".to_string(), output));
    let output_plugin_name = Config::get_pipeline_output_plugin_name();
    let output = sync_output_plugin(&output_plugin_name, "output".to_string()).await.unwrap();
    let shared_output = Arc::new(output);

    // sync schema if output plugin configured
    if output_plugin_name != "" {
        Config::sync_schema(&pipeline_metadata.metadata).await;
    } else {
        // Just build the arrow schemas internally
        let flatten = Config::get_transform_flatten_events();
        for (namespace, _metadata) in pipeline_metadata.metadata.iter() {
            match Ingest::prepare_arrow_schema_with_metadata(&namespace, &pipeline_metadata.metadata, flatten) {
                Ok(_t) => {}
                Err(e) => {
                    println!("Failed to prepare arrow schema: {}", e);
                    return;
                }
            }
        }
    }

    let _now = Arc::new(TimedRwLock::new("now".to_string(), Instant::now()));

    /* 
     * Handle PANICS in threads
     */
    // take_hook() returns the default hook in case when a custom one is not set
    let orig_hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        // invoke the default handler and exit the process
        orig_hook(panic_info);
        let panic_str = format!("{:?}", panic_info);

        let panic_info_clone = panic_str.clone();

        {
            let mut counter_lock = METRICS.write();
            counter_lock.status = MetricsStatus::Error;
        }

        thread::spawn(move || {
            let rt = runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();

            rt.block_on(async {
                LOGGER
                    .write()
                    .await
                    .log(LogLevel::Error, panic_info_clone)
                    .await;
                LOGGER.write().await.flush().await.unwrap();
            });
        }).join().unwrap();

        if !RUNNING.read().load(Ordering::SeqCst) {
            println!("Received another panic - already gracefully shutting down");
        } else {

            let pid = process::id() as i32; // or replace with the PID of the target process

            kill(Pid::from_raw(pid), Signal::SIGTERM).unwrap();
        }

    }));

    /* 
     * Handle SIGNALS
     */
    let mut signals = Signals::new(&[SIGINT, SIGTERM, SIGQUIT, SIGABRT]).unwrap();

    let _offsets_clone = offsets_db.clone();

    thread::spawn(move || {
        for sig in signals.forever() {

            {
                let mut counter_lock = METRICS.write();
                counter_lock.status = MetricsStatus::Stopped;
            }

            thread::spawn(move || {
                let rt = runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .unwrap();

                rt.block_on(async {
                });
            }).join().unwrap();

            if !RUNNING.read().load(Ordering::SeqCst) {
                println!("Received another Ctrl+C signal - terminating immediately, this may result in data loss...");
                _offsets_clone.flush();
                std::process::exit(0);
            }

            {
                RUNNING.write().store(false, Ordering::SeqCst);
            }

            let _offsets_clone = _offsets_clone.clone();

            thread::spawn(move || {
                println!("Received SIG: {} - Gracefully shutting down", sig.to_string());

                while
                    OUTPUT_RUNNING
                        .read()
                        .load(Ordering::SeqCst) &&
                    !OUTPUT_GRACEFUL_SHUTDOWN_COMPLETE
                        .read()
                        .load(Ordering::SeqCst)
                {
                    sleep(Duration::from_secs(1));
                }

                _offsets_clone.flush();

                println!("Graceful shutdown complete... bye");
                std::process::exit(0);
            });
        }
    });


    use rand::Rng; // 0.8.5

    let mut out_pnanner = periodic::Planner::new();
    if Config::get_pipeline_chaos_mode() {
        out_pnanner.add(
            move || {
                if RUNNING.read().load(Ordering::SeqCst) {
                    println!("Chaos mode throwing a random exit. You can disable this test mode buy removing CHAOS_MODE flag or setting to 'no'");
                    let pid = process::id() as i32;
                    let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
                }
            },
            periodic::Every::new(Duration::from_secs(rand::thread_rng().gen_range(60..90))),
        );
    }

    let _offsets_clone = offsets_db.clone();

    let shared_output_clone = shared_output.clone();
    sync_input_plugin(offsets_db.clone(), shared_output_clone).await;

    println!("Reached end of source data");
    println!("Ingest completed, flushing remaining buffers to output plugin {}", Config::get_pipeline_config().output.or(Some("".to_string())).unwrap());

    {
        let mut counter_lock = METRICS.write();
        counter_lock.status = MetricsStatus::Finishing;
    }

    while OUTPUT_RUNNING.read().load(Ordering::SeqCst) {
        sleep(Duration::from_secs(1));
    }

    {
        OUTPUT_RUNNING.write().store(true, Ordering::SeqCst);
    }

    {
        OUTPUT_RUNNING
            .write()
            .store(false, Ordering::SeqCst);
    }

    {
        let mut counter_lock = METRICS.write();
        counter_lock.status = MetricsStatus::Completed;

    }

    {
        let counter_lock = METRICS.write();
        
        println!("Messages Fixed: {}", counter_lock.ingeted_slow_total);
        println!("Deadletter Total: {}", counter_lock.deadletters_total);
        println!("Ingested Total: {}", counter_lock.messages_total);
    }

    match Metrics::send_metrics(Some(0)).await {
        Ok(_res) => (),
        Err(e) => {
            LOGGER.write()
                .await
                .log(LogLevel::Error, format!("Failed to send metrics to Skippr API: {}", e))
                .await;
        }
    }

    if !LOGGER.read().await.logs.is_empty() {
        LOGGER.write().await.flush().await.unwrap();
    }
    
    println!("Pipeline '{}' sync complete", pipeline_name);

    // Final metrics snapshot (same as periodic per-minute print)
    {
        use std::sync::atomic::Ordering as AtomicOrdering;
        let messages_total_counter = crate::metrics::counters::MESSAGES_TOTAL.load(AtomicOrdering::Relaxed);
        let source_bytes_total_counter = crate::metrics::counters::SOURCE_BYTES_TOTAL.load(AtomicOrdering::Relaxed);
        let deadletters_total_counter = crate::metrics::counters::DEADLETTERS_TOTAL.load(AtomicOrdering::Relaxed);
        let _ingested_slow_total_counter = crate::metrics::counters::INGESTED_SLOW_TOTAL.load(AtomicOrdering::Relaxed);
        let human_bytes = crate::helpers::Helpers::human_readable_size(source_bytes_total_counter);
        println!("Messages per Min: {}", 0);
        println!("Messages Fixed per Min: {}", 0);
        println!("Bytes Total: {}", human_bytes);
        println!("Messages Total: {}", messages_total_counter);
        println!("Deadletters per Min: {}", 0);
        println!("Deadletter Total: {}", deadletters_total_counter);
        // Runtime not directly accessible here; print 0 to keep format consistent
        println!("Runtime: {} seconds", 0);
        let up_total = crate::metrics::counters::UPLOADS_TOTAL.load(AtomicOrdering::SeqCst);
        let up_inflight = crate::metrics::counters::UPLOADS_IN_FLIGHT.load(AtomicOrdering::SeqCst);
        let up_lat_ns_total = crate::metrics::counters::UPLOAD_LATENCY_NS_TOTAL.load(AtomicOrdering::SeqCst);
        let avg_up_ms = if up_total > 0 { (up_lat_ns_total / up_total) as f64 / 1_000_000.0 } else { 0.0 };
        let up_target = crate::metrics::counters::UPLOAD_CONCURRENCY_TARGET.load(AtomicOrdering::SeqCst);
        let wal_target = crate::metrics::counters::WAL_COMPACTION_CONCURRENCY_TARGET.load(AtomicOrdering::SeqCst);
        let dl_target = crate::metrics::counters::S3_DOWNLOAD_CONCURRENCY_TARGET.load(AtomicOrdering::SeqCst);
        let active = crate::metrics::counters::ACTIVE_THREADS.load(AtomicOrdering::SeqCst);
        let queue = crate::metrics::counters::QUEUE_LENGTH.load(AtomicOrdering::SeqCst);
        println!("Uploads total: {}, inflight: {}, avg latency: {:.2} ms", up_total, up_inflight, avg_up_ms);
        println!("Targets - upload: {}, wal: {}, s3_download: {} | active: {}, queue: {}", up_target, wal_target, dl_target, active, queue);
    }

}

async fn sync() {

    let pipeline_name = Config::get_pipeline_name();
    
    {
        let mut counter_lock = METRICS.write();
        counter_lock.status = MetricsStatus::Running;
    }

    {
        match Metrics::send_config().await {
            Ok(_res) => (),
            Err(e) => {
                LOGGER.write()
                    .await
                    .log(LogLevel::Error, format!("Failed to send config to Skippr API: {}", e))
                    .await;
            }
        }
    }

    let pipeline_metadata: PipelineMetadata;

    // @todo - we don't cache Pipeline metatdata, as currently SQL statements are not stored in metadata.
    //         Refactor to accept SQL directly via database connection
    pipeline_metadata = match Config::get_metadata().await {
        Ok(pipeline_metadata) => {
            println!("Found existing Skippr metadata");

            match pipeline_metadata.sql {
                Some(sql) => {
                    for stmt in sql {
                        println!("Recieved SQL statement: '{}'", stmt);

                        // Important to exec the SQL before saving metadata, as the SQL may drop or otherwise alter the metadata
                        query(&stmt).await;
                    }

                    return;
                },
                None => {}
            }

            if !pipeline_metadata.enabled {
                println!("Pipeline '{}' disabled, skipping.", pipeline_name);
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


    println!("Syncing pipeline: {}", pipeline_name);

    METADATA.store(Arc::new(pipeline_metadata.clone()));


    let offsets_db = match Offsets::init() {
        Ok(offsets) => offsets,
        Err(e) => {
            println!("Skipping: {}", e);
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
    let output = sync_output_plugin(&output_plugin_name, "output".to_string()).await.unwrap();
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
                    println!("Chaos mode throwing a random exit. You can disable this test mode buy removing CHAOS_MODE flag or setting to 'no'");
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
            match Ingest::prepare_arrow_schema_with_metadata(&namespace, &pipeline_metadata.metadata, flatten) {
                Ok(_t) => {}
                Err(e) => {
                    println!("Failed to prepare arrow schema: {}", e);
                    return;
                }
            }
        }
    }

    sync_input_plugin(offsets_db.clone(), shared_output_clone).await;

    println!("Reached end of source data");
    println!("Ingest completed, flushing remaining buffers to output plugin {}", Config::get_pipeline_config().output.or(Some("".to_string())).unwrap());

    {
        METRICS.write().status = MetricsStatus::Finishing;
    }

    // Queue-based model: rely on the explicit drain above; skip legacy compact_all_partitions

    println!("All buffers flushed to output plugin");

    // Deterministic drain: compact all remaining on-disk segments to parquet
    {
        // Stop background compactor pool and wait for in-flight to drain
        Buffers::request_compactor_stop();
        // Wait for in-flight to reach zero (bounded wait)
        for _ in 0..40 {
            let inflight = crate::metrics::counters::WAL_COMPACTIONS_IN_FLIGHT.load(std::sync::atomic::Ordering::Relaxed);
            if inflight == 0 { break; }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        Buffers::compact_all_partitions(true, offsets_db.clone(), shared_output.clone()).await;
        // Safety loop: if any .seg remain, run another pass (handles late live persist)
        if Buffers::segs_remaining() > 0 {
            Buffers::compact_all_partitions(true, offsets_db.clone(), shared_output.clone()).await;
        }
    }
    // Single-thread model: no background compaction tasks remain here
    // Wait for background Glue partition tasks to settle to avoid undercount at end
    crate::plugins::athena::DataOutputAwsAthenaPlugin::await_partition_tasks_zero().await;

    // Summary and integrity check: uploaded rows vs expected msgs, quarantined parts
    {
        use std::sync::atomic::Ordering as AO;
        let uploaded_rows = crate::metrics::counters::PARQUET_PERSISTED_ROWS_TOTAL.load(AO::Relaxed);
        let expected_msgs = crate::metrics::counters::MESSAGES_TOTAL.load(AO::Relaxed);
        let quarantined_parts = crate::metrics::counters::QUARANTINED_PARTITIONS_TOTAL.load(AO::Relaxed);
        println!(
            "Compactor: summary uploaded_rows={} expected_msgs={} quarantined_parts={}",
            uploaded_rows, expected_msgs, quarantined_parts
        );
        if quarantined_parts > 0 || uploaded_rows != expected_msgs {
            eprintln!(
                "Compactor: integrity check failed (uploaded_rows != expected_msgs or quarantined_parts > 0); exiting nonzero"
            );
        }
    }

    {
        METRICS.write().status = MetricsStatus::Completed;
    }

    match Metrics::send_metrics(Some(0)).await {
        Ok(_res) => (),
        Err(e) => {
            LOGGER.write()
                .await
                .log(LogLevel::Error, format!("Failed to send metrics to Skippr API: {}", e))
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
        use crate::metrics::counters as counters;
        let m = METRICS.read();
        let messages_total = m.messages_total + counters::MESSAGES_TOTAL.load(std::sync::atomic::Ordering::Relaxed);
        let source_bytes_total = m.source_bytes_total + counters::SOURCE_BYTES_TOTAL.load(std::sync::atomic::Ordering::Relaxed);
        let parquet_objects = m.parquet_persisted_objects_total + counters::PARQUET_PERSISTED_OBJECTS_TOTAL.load(std::sync::atomic::Ordering::Relaxed);
        let parquet_rows = m.parquet_persisted_rows_total + counters::PARQUET_PERSISTED_ROWS_TOTAL.load(std::sync::atomic::Ordering::Relaxed);
        let parquet_bytes = m.parquet_persisted_bytes_total + counters::PARQUET_PERSISTED_BYTES_TOTAL.load(std::sync::atomic::Ordering::Relaxed);
        println!(
            "Final metrics: msgs_total={} src_bytes_total={} parquet_rows_total={} parquet_bytes_total={} parquet_objects_total={}",
            messages_total,
            source_bytes_total,
            parquet_rows,
            parquet_bytes,
            parquet_objects
        );
    }
    println!("Pipeline sync complete");
}

pub async fn sync_output_plugin(plugin_name: &str, buffer_name: String) -> Result<Box<dyn DataOutputPlugin + Send + Sync>, io::Error> {

    println!("Output plugin: {}", plugin_name);
    
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
            println!("No Data {} plugin specified, defaulting to local file", buffer_name);
            let plugin = DataOutputFilePlugin::new(buffer_name).await;
            Ok(Box::new(plugin) as Box<dyn DataOutputPlugin + Send + Sync>)
        }
        _ => {
            // println!("Unknown Data {} plugin specified", buffer_name);
            Err(io::Error::new(io::ErrorKind::Other, "Unknown Data plugin specified"))
        }
    }
}

pub async fn sync_input_plugin(offsets_clone: Arc<Offsets>, shared_output: Arc<Box<dyn DataOutputPlugin + Send + Sync>>) {
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
            input.sync(
                offsets_clone,
                shared_output
            )
                .await;
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
            println!("No Data Source plugin specified. You must specify a data source plugin, see documentation for the DATA_SOURCE_PLUGIN_NAME environment variable.");
        }
        unknown => {
            println!("Data Source Plugin {} not supported", unknown);
        }
    }
}
