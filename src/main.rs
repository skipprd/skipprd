use rand::Rng;
use std::time::{Duration, SystemTime};

#[cfg(unix)]
extern crate nix;

#[cfg(unix)]
use nix::sys::signal::{kill, Signal};
#[cfg(unix)]
use nix::unistd::Pid;
use std::{io, process};

use std::sync::Arc;

use std::collections::HashMap;
use std::fs;

use std::sync::atomic::Ordering;
use std::thread::sleep;
use std::time::Instant;

use skippr::cli::{Cli, Mode, CLI_MODE};
use skippr::discover::PipelineMetadata;

extern crate clap;
extern crate core;

use clap::Parser;

use std::string::ToString;

use skippr::buffer::BufferChunker;
use skippr::helpers::configuration::{Config, PIPELINE_NAME};
use skippr::helpers::logging::init_logging;
use skippr::helpers::sync_reporter::SyncReporter;
use tracing::{error, info, warn};

use skippr::helpers::logger::LogLevel;
use skippr::helpers::offsets::Offsets;

use skippr::metrics::{Metrics, MetricsStatus};
use skippr::{LOGGER, METADATA, METRICS, RUNNING};

use datafusion::physical_plan::SendableRecordBatchStream;
use datafusion::prelude::*;
use skippr::benchmark::PerformanceBenchmark;
use skippr::buffer::ingest_buffer::{wal_recover, Buffers};
use skippr::ingest::deadletter;
use skippr::ingest_work::Ingest;
use skippr::plugins::DataSink;
use skippr::runtime_plugins::discovery::resolve_runtime_plugin;
#[cfg(unix)]
use skippr::runtime_plugins::host::terminate_runtime_plugin_children;
use skippr::runtime_plugins::host::{
    sync_runtime_input_plugin, ResolvedRuntimePlugin, RuntimeDataSinkPlugin,
};
use skippr::runtime_plugins::protocol::{RuntimeBinding, RuntimePluginKind, RuntimeSinkConfig};
use skippr::runtime_plugins::schema_state::clear_runtime_source_schema_state;
use skippr::sqlrt::doc_parser::SqlDocParser;
use skippr::sqlrt::docs::{get_docs_in_format, DocFormat};
use skippr::sqlrt::query::query;
use std::io::IsTerminal as _;

// pub static DISPLAY_METRICS: Lazy<TimedRwLock<AtomicBool>> =
//     Lazy::new(|| TimedRwLock::new("display_metrics".to_string(), AtomicBool::new(false)));

// Global runtime state now lives in the library crate (see `src/globals.rs`).

#[derive(Clone, Debug)]
struct PipelineCache {}

struct OutputRouter {
    primary_sink_ref: String,
    sinks: HashMap<String, Arc<Box<dyn DataSink + Send + Sync>>>,
}

#[async_trait::async_trait]
impl DataSink for OutputRouter {
    async fn sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
        cdc_ctx: Option<&skippr::plugins::cdc::SyncContext>,
    ) -> Result<(), std::io::Error> {
        let sink_ref = BufferChunker::decode_file_sink_ref(&filename);
        let target_sink_ref = if sink_ref.is_empty() {
            self.primary_sink_ref.clone()
        } else {
            sink_ref
        };
        let plugin = self.sinks.get(&target_sink_ref).ok_or_else(|| {
            std::io::Error::other(format!(
                "No output sink registered for persisted sink_ref '{}'",
                target_sink_ref
            ))
        })?;
        plugin.sync(stream, filename, cdc_ctx).await
    }

    async fn install_schema_state(
        &self,
        schema_version: u64,
        namespaces: &std::collections::BTreeMap<String, skippr::discover::OutputMetadata>,
    ) -> Result<(), std::io::Error> {
        for plugin in self.sinks.values() {
            plugin
                .install_schema_state(schema_version, namespaces)
                .await?;
        }
        Ok(())
    }
}

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

async fn run_sync_or_exit(output_mode: &str) {
    if let Err(err) = sync(output_mode).await {
        error!(
            "Pipeline '{}' sync failed: {}",
            Config::get_pipeline_name(),
            err
        );
        process::exit(1);
    }
}

async fn run_discover_or_exit(output_mode: &str) {
    if let Err(err) = discover(output_mode).await {
        error!(
            "Pipeline '{}' discover failed: {}",
            Config::get_pipeline_name(),
            err
        );
        process::exit(1);
    }
}

fn chaos_mode_delay() -> Duration {
    const DEFAULT_MIN_SECS: u64 = 60;
    const DEFAULT_MAX_SECS: u64 = 90;

    let min = std::env::var("SKIPPR_CHAOS_MIN_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_MIN_SECS)
        .max(1);
    let max = std::env::var("SKIPPR_CHAOS_MAX_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_MAX_SECS)
        .max(1);
    let (min, max) = if min <= max { (min, max) } else { (max, min) };
    let secs = if min == max {
        min
    } else {
        rand::thread_rng().gen_range(min..=max)
    };
    Duration::from_secs(secs)
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

            let output_mode = options.output.clone();
            let run_once = options.once;

            if options.pipeline.is_some() {
                PIPELINE_NAME.write().clear();
                PIPELINE_NAME
                    .write()
                    .push_str(&options.pipeline.unwrap().clone());
                Config::init().await;

                run_sync_or_exit(&output_mode).await;
            } else {
                let pipeline_name = Config::getenv("PIPELINE_NAME", "");
                if !pipeline_name.is_empty() {
                    PIPELINE_NAME.write().clear();
                    PIPELINE_NAME.write().push_str(&pipeline_name.clone());
                    Config::init().await;

                    run_sync_or_exit(&output_mode).await;
                } else {
                    info!("Syncing all pipelines");
                    let pipelines = Config::get_pipelines();
                    loop {
                        for pipeline_name in pipelines.iter() {
                            Config::reset_envcache();
                            {
                                PIPELINE_NAME.write().clear();
                                PIPELINE_NAME.write().push_str(pipeline_name);
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

                            run_sync_or_exit(&output_mode).await;
                        }

                        if run_once {
                            break;
                        }
                        sleep(Duration::from_secs(10));
                    }
                }
            }
        }
        Mode::Discover(options) => {
            Config::build_config();

            let output_mode = options.output.clone();

            if options.pipeline.is_some() {
                PIPELINE_NAME.write().clear();
                PIPELINE_NAME
                    .write()
                    .push_str(&options.pipeline.unwrap().clone());
                Config::init().await;

                run_discover_or_exit(&output_mode).await;
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
                println!("  skippr-el sql-help --command \"<SQL COMMAND>\"");
                println!();
                println!("To generate documentation, use:");
                println!("  skippr-el sql-help --output <FILE_PATH> [--format md|html|json]");
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

async fn discover(output_mode: &str) -> io::Result<()> {
    let pipeline_name = Config::get_pipeline_name();
    let start_time = Instant::now();

    let stdout_is_tty = std::io::stdout().is_terminal();
    let logs_enabled = skippr::helpers::logging::cli_logs_enabled();
    let reporter = SyncReporter::new(output_mode, stdout_is_tty, logs_enabled);
    if reporter.enabled() {
        reporter.add_tasks(&["Discovering"]);
    }
    reporter.discover_start(&pipeline_name);

    info!(
        "Analysing data and generating Skippr metadata for pipeline: {}",
        pipeline_name
    );

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
            return Err(io::Error::other(format!(
                "Failed to initialize offsets: {}",
                e
            )));
        }
    };

    let offsets_db = Arc::new(offsets);

    let noop_output: Box<dyn skippr::plugins::DataSink + Send + Sync> =
        Box::new(skippr::plugins::NoopOutputPlugin);
    let shared_output = Arc::new(noop_output);

    {
        let flatten = Config::get_transform_flatten_events();
        for (namespace, _metadata) in pipeline_metadata.metadata.iter() {
            match Ingest::prepare_arrow_schema_with_metadata(
                &namespace,
                &pipeline_metadata.metadata,
                flatten,
            ) {
                Ok(_t) => {}
                Err(e) => {
                    return Err(io::Error::other(format!(
                        "Failed to prepare arrow schema: {}",
                        e
                    )));
                }
            }
        }
    }

    if reporter.enabled() {
        reporter.start("Discovering");
    }
    let discover_result = sync_input_plugin(offsets_db.clone(), shared_output).await;
    if let Err(err) = discover_result {
        if reporter.enabled() {
            reporter.finish();
        }
        return Err(err);
    }
    if reporter.enabled() {
        reporter.complete("Discovering");
    }

    info!("Reached end of source data, persisting discovered metadata");

    let flatten = Config::truth_value(
        &Config::get_transform_config()
            .flatten_events
            .unwrap_or("false".to_string()),
    );

    let mut updated_metadata = METADATA.load().as_ref().clone();
    for (_namespace, metadata) in updated_metadata.metadata.iter_mut() {
        metadata.finalize_field_types(flatten);
    }
    updated_metadata.enabled = true;
    METADATA.store(Arc::new(updated_metadata.clone()));

    Config::set_metadata(&updated_metadata, true).await;

    let namespaces_discovered = updated_metadata.metadata.len();
    for (ns_name, ns_metadata) in updated_metadata.metadata.iter() {
        let fields: Vec<serde_json::Value> = ns_metadata
            .field_details()
            .into_iter()
            .map(|(name, type_name, _nullable)| {
                serde_json::json!({
                    "name": name,
                    "type": type_name,
                })
            })
            .collect();
        reporter.discover_namespace(ns_name, fields);
    }

    let elapsed_ms = start_time.elapsed().as_millis() as u64;
    reporter.discover_complete(&pipeline_name, namespaces_discovered, elapsed_ms);

    if reporter.enabled() {
        reporter.finish();
    }

    Ok(())
}

async fn sync(output_mode: &str) -> io::Result<()> {
    let pipeline_name = Config::get_pipeline_name();
    let sync_started = Instant::now();

    let stdout_is_tty = std::io::stdout().is_terminal();
    let reporter = SyncReporter::new(
        output_mode,
        stdout_is_tty,
        skippr::helpers::logging::cli_logs_enabled(),
    );
    if reporter.enabled() {
        reporter.add_tasks(&["Ingesting", "Finalising"]);
    }
    reporter.sync_start(&pipeline_name);

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

                    return Ok(());
                }
                None => {}
            }

            if !pipeline_metadata.enabled {
                info!("Pipeline '{}' disabled, skipping.", pipeline_name);
                return Ok(());
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

    clear_runtime_source_schema_state();
    METADATA.store(Arc::new(pipeline_metadata.clone()));
    if Config::get_pipeline_deadletters_ref().is_some() {
        deadletter::ensure_namespace_registered();
    }

    let offsets_db = match Offsets::init() {
        Ok(offsets) => offsets,
        Err(e) => {
            return Err(io::Error::other(format!(
                "Failed to initialize offsets: {}",
                e
            )));
        }
    };

    let offsets_db = Arc::new(offsets_db);

    let _offsets_clone = offsets_db.clone();

    wal_recover(offsets_db.clone())
        .await
        .expect("Failed to recover WAL index");

    {
        METRICS.write().status = MetricsStatus::Running;
    }

    let output_plugin_name = Config::get_pipeline_output_plugin_name();
    let output = sync_output_plugin(&output_plugin_name, "output".to_string()).await?;
    let shared_output = Arc::new(output);

    // CDC compatibility validation at startup
    {
        use skippr::plugins::cdc::{
            derive_and_validate, set_global_cdc_contract, CompatibilityResult,
        };
        let pipeline = Config::get_pipeline_config();
        if let Some(ref cdc_cfg) = pipeline.cdc {
            let input_name = Config::get_pipeline_input_plugin_name();
            let runtime_input_version =
                Config::get_pipeline_input_plugin_version().unwrap_or_else(|err| {
                    panic!("Runtime input plugin version lookup failed: {}", err)
                });
            let runtime_input_manifest = match Config::get_pipeline_runtime_input_plugin() {
                Ok(entry) => Some(
                    resolve_runtime_plugin(
                        entry,
                        RuntimePluginKind::DataSource,
                        &input_name,
                        runtime_input_version.as_deref(),
                    )
                    .await
                    .unwrap_or_else(|err| {
                        panic!("Runtime input manifest resolution failed: {}", err)
                    }),
                ),
                Err(err) => panic!("Runtime input manifest lookup failed: {}", err),
            };
            let runtime_output_version = Config::get_pipeline_output_plugin_version()
                .unwrap_or_else(|err| {
                    panic!("Runtime output plugin version lookup failed: {}", err)
                });
            let runtime_output_manifest = match Config::get_pipeline_runtime_output_plugin() {
                Ok(entry) => Some(
                    resolve_runtime_plugin(
                        entry,
                        RuntimePluginKind::DataSink,
                        &output_plugin_name,
                        runtime_output_version.as_deref(),
                    )
                    .await
                    .unwrap_or_else(|err| {
                        panic!("Runtime output manifest resolution failed: {}", err)
                    }),
                ),
                Err(err) => panic!("Runtime output manifest lookup failed: {}", err),
            };
            let src_cap = runtime_input_manifest
                .as_ref()
                .and_then(runtime_source_capability_for_manifest)
                .or_else(|| source_capability_for_plugin(&input_name).cloned());
            let sink_cap = runtime_output_manifest
                .as_ref()
                .and_then(runtime_sink_capability_for_manifest)
                .or_else(|| sink_capability_for_plugin(&output_plugin_name).cloned());
            if let (Some(src), Some(snk)) = (src_cap.as_ref(), sink_cap.as_ref()) {
                match derive_and_validate(src, snk, "default", &cdc_cfg.business_key_columns) {
                    CompatibilityResult::Compatible(guarantee) => {
                        info!(
                            "CDC validation passed: source={} sink={} enforced_guarantee={:?}",
                            src.name, snk.name, guarantee
                        );
                        let contract = build_cdc_contract(cdc_cfg, "default", guarantee);
                        set_global_cdc_contract(Some(contract));
                    }
                    CompatibilityResult::Incompatible(reasons) => {
                        for r in &reasons {
                            error!("CDC incompatibility: {}", r);
                        }
                        panic!(
                            "CDC validation failed: source={} sink={} — {} reason(s)",
                            src.name,
                            snk.name,
                            reasons.len()
                        );
                    }
                }
            } else {
                warn!(
                    "CDC config present but source '{}' or sink '{}' has not declared capabilities; \
                     validation skipped. The pipeline will run in append mode.",
                    input_name, output_plugin_name
                );
                set_global_cdc_contract(None);
            }
        } else {
            set_global_cdc_contract(None);
        }
    }

    Buffers::start_compactor_service(shared_output.clone(), offsets_db.clone());

    let shared_output_clone = shared_output.clone();

    // Arm chaos interrupt for sync runs using the old planner (deterministic tick)
    let mut out_pnanner = periodic::Planner::new();
    if Config::get_pipeline_chaos_mode() {
        out_pnanner.add(
            move || {
                if RUNNING.read().load(Ordering::SeqCst) {
                    warn!("Chaos mode throwing a random exit. You can disable this test mode buy removing CHAOS_MODE flag or setting to 'no'");
                    #[cfg(unix)]
                    {
                        terminate_runtime_plugin_children();
                        sleep(Duration::from_millis(250));
                        let pid = process::id() as i32;
                        let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
                    }
                    #[cfg(not(unix))]
                    {
                        process::exit(137);
                    }
                }
            },
            periodic::Every::new(chaos_mode_delay()),
        );
    }

    // Build Arrow schema cache for all known namespaces so that the ingest
    // hot-path does not treat first-seen records as "schema changes".  For
    // Athena output this also enqueues each namespace for Glue table sync
    // (one-by-one, via the worker), replacing the previous bulk re-send.
    {
        let flatten = Config::get_transform_flatten_events();
        for (namespace, _ns_metadata) in pipeline_metadata.metadata.iter() {
            match Ingest::prepare_arrow_schema_with_metadata(
                namespace,
                &pipeline_metadata.metadata,
                flatten,
            ) {
                Ok(schema) => {
                    reporter.namespace_discovered(namespace, schema.fields().len());
                }
                Err(e) => {
                    return Err(io::Error::other(format!(
                        "Failed to prepare arrow schema: {}",
                        e
                    )));
                }
            }
        }
    }

    if reporter.enabled() {
        reporter.start("Ingesting");
    }

    let (heartbeat_tx, mut heartbeat_rx) = tokio::sync::watch::channel(false);
    let heartbeat_pipeline = pipeline_name.clone();
    let heartbeat_started = sync_started;
    let is_json_mode = matches!(&reporter, SyncReporter::Json);
    if is_json_mode {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
            interval.tick().await;
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        use std::sync::atomic::Ordering::Relaxed;
                        let msgs = skippr::metrics::counters::MESSAGES_TOTAL.load(Relaxed);
                        let bytes = skippr::metrics::counters::SOURCE_BYTES_TOTAL.load(Relaxed);
                        let rows = skippr::metrics::counters::PARQUET_PERSISTED_ROWS_TOTAL.load(Relaxed);
                        let uploads = skippr::metrics::counters::UPLOADS_IN_FLIGHT.load(Relaxed) as u64;
                        let elapsed = heartbeat_started.elapsed().as_millis() as u64;
                        let r = SyncReporter::Json;
                        r.sync_status(&heartbeat_pipeline, msgs, bytes, rows, elapsed, uploads);
                    }
                    _ = heartbeat_rx.changed() => break,
                }
            }
        });
    }

    let source_sync_result = sync_input_plugin(offsets_db.clone(), shared_output_clone).await;
    match &source_sync_result {
        Ok(()) => {
            if reporter.enabled() {
                reporter.complete("Ingesting");
            }
            info!("Reached end of source data");
            info!(
                "Ingest completed, flushing remaining buffers to output plugin {}",
                Config::get_pipeline_config()
                    .data_sink
                    .or(Some("".to_string()))
                    .unwrap()
            );
        }
        Err(err) => {
            reporter.sync_error(&pipeline_name, &err.to_string(), None);
            warn!("Source ingest failed, finalizing pipeline state before exiting");
        }
    }

    {
        METRICS.write().status = MetricsStatus::Finishing;
    }

    info!("All buffers flushed to output plugin");

    {
        if reporter.enabled() {
            reporter.start("Finalising");
        }
        let finalising_started = std::time::Instant::now();
        info!("Finalising: draining and stopping compactor");
        let compactor_ok = Buffers::drain_and_stop_compactor(offsets_db.clone()).await;
        if compactor_ok {
            info!("Finalising: compactor drained and stopped");
        } else {
            panic!("Finalising: compactor drain/stop did not complete cleanly");
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
    info!("Finalising: draining schema sync worker");
    Config::drain_schema_sync_worker();
    info!("Finalising: schema sync worker drained");

    if reporter.enabled() {
        reporter.complete("Finalising");
    }

    let _ = heartbeat_tx.send(true);

    // Summary and integrity check: uploaded rows vs expected rows (normal + deadletters), quarantined parts
    {
        use std::sync::atomic::Ordering as AO;
        let uploaded_rows =
            skippr::metrics::counters::PARQUET_PERSISTED_ROWS_TOTAL.load(AO::Relaxed);
        let expected_msgs = skippr::metrics::counters::MESSAGES_TOTAL.load(AO::Relaxed);
        let expected_deadletters = skippr::metrics::counters::DEADLETTERS_TOTAL.load(AO::Relaxed);
        let expected_uploaded_rows = expected_msgs.saturating_add(expected_deadletters);
        let quarantined_parts =
            skippr::metrics::counters::QUARANTINED_PARTITIONS_TOTAL.load(AO::Relaxed);
        info!(
            "Compactor: summary uploaded_rows={} expected_msgs={} expected_deadletters={} expected_uploaded_rows={} quarantined_parts={}",
            uploaded_rows, expected_msgs, expected_deadletters, expected_uploaded_rows, quarantined_parts
        );
        if quarantined_parts > 0 || uploaded_rows != expected_uploaded_rows {
            warn!("Compactor: integrity check mismatch (uploaded_rows != expected_msgs + expected_deadletters or quarantined_parts > 0). Proceeding; this may occur when compacting pre-existing WAL.");
        }
    }

    skippr::converters::parquet_ordering::log_unmatched_order_fields();

    {
        METRICS.write().status = if source_sync_result.is_ok() {
            MetricsStatus::Completed
        } else {
            MetricsStatus::Error
        };
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

    if let Err(err) = source_sync_result {
        if reporter.enabled() {
            reporter.finish();
        }
        return Err(err);
    }

    info!("Pipeline sync complete");

    {
        use skippr::metrics::counters;
        let namespaces_synced = pipeline_metadata.metadata.len();
        let total_rows =
            counters::PARQUET_PERSISTED_ROWS_TOTAL.load(std::sync::atomic::Ordering::Relaxed);
        let elapsed_ms = sync_started.elapsed().as_millis() as u64;
        reporter.sync_complete(&pipeline_name, namespaces_synced, total_rows, elapsed_ms);
    }

    if reporter.enabled() {
        reporter.finish();
    }

    Ok(())
}

pub async fn sync_output_plugin(
    plugin_name: &str,
    _buffer_name: String,
) -> Result<Box<dyn DataSink + Send + Sync>, io::Error> {
    info!("Output plugin: {}", plugin_name);

    let primary_sink_ref = Config::get_pipeline_output_sink_ref();
    let runtime_entry = Config::get_pipeline_runtime_output_plugin().map_err(io::Error::other)?;
    let runtime_version = Config::get_pipeline_output_plugin_version().map_err(io::Error::other)?;
    let resolved = resolve_runtime_plugin(
        runtime_entry,
        RuntimePluginKind::DataSink,
        plugin_name,
        runtime_version.as_deref(),
    )
    .await?;
    let runtime_config = resolve_runtime_sink_config(RuntimeBinding::Primary)?;
    let primary_plugin = Box::new(
        RuntimeDataSinkPlugin::new(
            resolved,
            Config::get_pipeline_name(),
            RuntimeBinding::Primary,
            runtime_config,
        )
        .await?,
    ) as Box<dyn DataSink + Send + Sync>;

    let mut sinks: HashMap<String, Arc<Box<dyn DataSink + Send + Sync>>> = HashMap::new();
    sinks.insert(primary_sink_ref.clone(), Arc::new(primary_plugin));

    if let Some((deadletter_sink_ref, deadletter_plugin)) =
        sync_deadletter_plugin("deadletters".to_string()).await?
    {
        sinks.insert(deadletter_sink_ref, Arc::new(deadletter_plugin));
    }

    Ok(Box::new(OutputRouter {
        primary_sink_ref,
        sinks,
    }) as Box<dyn DataSink + Send + Sync>)
}

pub async fn sync_deadletter_plugin(
    _buffer_name: String,
) -> Result<Option<(String, Box<dyn DataSink + Send + Sync>)>, io::Error> {
    let sink_ref = match Config::get_pipeline_deadletters_ref() {
        Some(sink_ref) => sink_ref,
        None => return Ok(None),
    };
    let Some(plugin_name) =
        Config::get_pipeline_deadletter_plugin_name().map_err(io::Error::other)?
    else {
        return Ok(None);
    };
    let runtime_entry = Config::get_pipeline_runtime_output_plugin().map_err(io::Error::other)?;
    let runtime_version =
        Config::get_pipeline_deadletter_plugin_version().map_err(io::Error::other)?;
    let resolved = resolve_runtime_plugin(
        runtime_entry,
        RuntimePluginKind::DataSink,
        &plugin_name,
        runtime_version.as_deref(),
    )
    .await?;
    let runtime_config = resolve_runtime_sink_config(RuntimeBinding::Deadletter)?;
    let plugin = Box::new(
        RuntimeDataSinkPlugin::new(
            resolved,
            Config::get_pipeline_name(),
            RuntimeBinding::Deadletter,
            runtime_config,
        )
        .await?,
    ) as Box<dyn DataSink + Send + Sync>;
    Ok(Some((sink_ref, plugin)))
}

fn build_cdc_contract(
    cfg: &skippr::helpers::configuration::CdcPipelineConfig,
    namespace: &str,
    effective_guarantee: skippr::plugins::cdc::EffectiveGuarantee,
) -> skippr::plugins::cdc::NamespaceContract {
    skippr::plugins::cdc::NamespaceContract {
        namespace: namespace.to_string(),
        business_key_columns: cfg.business_key_columns.clone(),
        effective_guarantee,
    }
}

fn runtime_source_capability_for_manifest(
    resolved: &ResolvedRuntimePlugin,
) -> Option<skippr::plugins::cdc::SourceCapability> {
    resolved
        .manifest
        .source_capability
        .as_ref()
        .map(|capability| capability.to_cdc_capability())
}

fn runtime_sink_capability_for_manifest(
    resolved: &ResolvedRuntimePlugin,
) -> Option<skippr::plugins::cdc::SinkCapability> {
    resolved
        .manifest
        .sink_capability
        .as_ref()
        .map(|capability| capability.to_cdc_capability())
}

fn resolve_runtime_sink_config(binding: RuntimeBinding) -> Result<RuntimeSinkConfig, io::Error> {
    let config = match binding {
        RuntimeBinding::Primary => {
            Config::get_pipeline_output_plugin_config().map_err(io::Error::other)?
        }
        RuntimeBinding::Deadletter => Config::get_pipeline_deadletter_plugin_config()
            .map_err(io::Error::other)?
            .ok_or_else(|| io::Error::other("deadletter sink is not configured"))?,
    };
    RuntimeSinkConfig::try_from(config).map_err(io::Error::other)
}

fn source_capability_for_plugin(
    name: &str,
) -> Option<&'static skippr::plugins::cdc::SourceCapability> {
    use skippr::plugins::cdc::source_capabilities;
    match name {
        "Postgres" => Some(&source_capabilities::POSTGRES),
        "Mysql" => Some(&source_capabilities::MYSQL),
        "Mongodb" => Some(&source_capabilities::MONGODB),
        "Dynamodb" => Some(&source_capabilities::DYNAMODB),
        "Kafka" => Some(&source_capabilities::KAFKA),
        "S3" => Some(&source_capabilities::S3),
        "Kinesis" => Some(&source_capabilities::KINESIS),
        "Sqs" => Some(&source_capabilities::SQS),
        "File" => Some(&source_capabilities::FILE),
        "Mssql" => Some(&source_capabilities::MSSQL),
        "HttpClient" => Some(&source_capabilities::HTTP_CLIENT),
        "HttpServer" => Some(&source_capabilities::HTTP_SERVER),
        "Stdin" => Some(&source_capabilities::STDIN),
        "Eventbridge" => Some(&source_capabilities::EVENTBRIDGE),
        "Sns" => Some(&source_capabilities::SNS),
        "Mqtt" => Some(&source_capabilities::MQTT),
        "Sftp" => Some(&source_capabilities::SFTP),
        "Redshift" => Some(&source_capabilities::REDSHIFT),
        "Amqp" => Some(&source_capabilities::AMQP),
        "Websocket" => Some(&source_capabilities::WEBSOCKET),
        "Statsd" => Some(&source_capabilities::STATSD),
        "Socket" => Some(&source_capabilities::SOCKET),
        "Clickhouse" => Some(&source_capabilities::CLICKHOUSE),
        "DeltaLake" => Some(&source_capabilities::DELTA_LAKE),
        "Motherduck" => Some(&source_capabilities::MOTHERDUCK),
        _ => None,
    }
}

fn sink_capability_for_plugin(name: &str) -> Option<&'static skippr::plugins::cdc::SinkCapability> {
    use skippr::plugins::cdc::sink_capabilities;
    match name {
        "Postgres" => Some(&sink_capabilities::POSTGRES),
        "Snowflake" => Some(&sink_capabilities::SNOWFLAKE),
        "Bigquery" | "BigQuery" => Some(&sink_capabilities::BIGQUERY),
        "Redshift" => Some(&sink_capabilities::REDSHIFT),
        "Clickhouse" | "ClickHouse" => Some(&sink_capabilities::CLICKHOUSE),
        "Motherduck" | "MotherDuck" => Some(&sink_capabilities::MOTHERDUCK),
        "Synapse" => Some(&sink_capabilities::SYNAPSE),
        "Databricks" => Some(&sink_capabilities::DATABRICKS),
        "S3" => Some(&sink_capabilities::S3),
        "Gcs" | "GCS" => Some(&sink_capabilities::GCS),
        "AzureBlob" | "Azure" => Some(&sink_capabilities::AZURE_BLOB),
        "File" => Some(&sink_capabilities::FILE),
        "Sftp" | "SFTP" => Some(&sink_capabilities::SFTP),
        "Athena" => Some(&sink_capabilities::ATHENA),
        "Stdout" => Some(&sink_capabilities::STDOUT),
        "Amqp" | "AMQP" => Some(&sink_capabilities::AMQP),
        _ => None,
    }
}

pub async fn sync_input_plugin(
    offsets_clone: Arc<Offsets>,
    shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
) -> io::Result<()> {
    let plugin_name = Config::get_pipeline_input_plugin_name();
    let runtime_entry = Config::get_pipeline_runtime_input_plugin().map_err(|err| {
        io::Error::other(format!(
            "Failed to resolve runtime input manifest reference: {}",
            err
        ))
    })?;
    let runtime_version = Config::get_pipeline_input_plugin_version().map_err(|err| {
        io::Error::other(format!(
            "Failed to resolve runtime input plugin version: {}",
            err
        ))
    })?;
    let resolved = resolve_runtime_plugin(
        runtime_entry,
        RuntimePluginKind::DataSource,
        &plugin_name,
        runtime_version.as_deref(),
    )
    .await
    .map_err(|err| {
        io::Error::other(format!("Runtime input manifest resolution failed: {}", err))
    })?;
    sync_runtime_input_plugin(
        resolved,
        Config::get_pipeline_name(),
        offsets_clone,
        shared_output,
    )
    .await
    .map_err(|err| io::Error::other(format!("Runtime data source sync failed: {}", err)))
}
