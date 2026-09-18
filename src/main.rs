use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime};
use std::{fs, process};

use skipprd::cli::{Cli, Mode};
use skipprd::cluster::validation::{validate_clustered_cli, CliModeKind};
use skipprd::helpers::wal_storage::WalStorage;

extern crate clap;
extern crate core;

use clap::Parser;

use std::string::ToString;

use skipprd::api::{cli_named_pipeline, Session};
use skipprd::helpers::configuration::Config;
use skipprd::helpers::logging::init_logging;
use tracing::{error, info};

use skipprd::metrics::Metrics;
use skipprd::METRICS;

use serde_json;
use skipprd::benchmark::PerformanceBenchmark;
use skipprd::sqlrt::doc_parser::SqlDocParser;
use skipprd::sqlrt::docs::{get_docs_in_format, DocFormat};
use skipprd::sqlrt::query::{QueryExecutionMode, QueryExecutionOptions};

// pub static DISPLAY_METRICS: Lazy<TimedRwLock<AtomicBool>> =
//     Lazy::new(|| TimedRwLock::new("display_metrics".to_string(), AtomicBool::new(false)));

// Global runtime state now lives in the library crate (see `src/globals.rs`).

#[derive(Clone, Debug)]
struct PipelineCache {}

// @todo, last_ran should be the updated_at timestamp for the file DATA_DIR/LASTRAN
impl PipelineCache {
    fn get_metadata(config: &Config) -> fs::Metadata {
        let last_ran_file = format!("{}/LASTRAN", config.get_data_dir());

        match fs::metadata(&last_ran_file) {
            Ok(metadata) => metadata,
            Err(_e) => PipelineCache::set_last_ran(config),
        }
    }

    fn last_ran(config: &Config) -> SystemTime {
        PipelineCache::get_metadata(config).modified().unwrap()
    }

    fn set_last_ran(config: &Config) -> fs::Metadata {
        let last_ran_file = format!("{}/LASTRAN", config.get_data_dir());

        fs::write(&last_ran_file, "").expect("Failed to write LASTRAN file");
        fs::metadata(&last_ran_file).expect("Failed to create LASTRAN file")
    }

    fn get_last_ran_elapsed(config: &Config) -> u64 {
        SystemTime::now()
            .duration_since(PipelineCache::last_ran(config))
            .unwrap()
            .as_secs()
    }

    fn last_ran_is_elapsed(config: &Config) -> bool {
        let duration = match SystemTime::now().duration_since(PipelineCache::last_ran(config)) {
            Ok(duration) => duration,
            Err(_e) => Duration::from_secs(0), // probably microsecond difference
        };

        duration.as_secs() > config.get_sync_frequency() || duration.as_secs() == 0
        // just created on first run
    }
}

fn reject_invalid_wal_storage() -> WalStorage {
    match Config::parse_wal_storage() {
        Ok(storage) => storage,
        Err(err) => {
            error!("{err}");
            eprintln!("{err}");
            process::exit(1);
        }
    }
}

fn reject_invalid_clustered_mode(config: &Config, storage: WalStorage, mode: CliModeKind) {
    if let Err(err) = validate_clustered_cli(config, storage, mode) {
        error!("{err}");
        eprintln!("{err}");
        process::exit(1);
    }
}

fn main() {
    let stack_size = std::env::var("SKIPPR_MAIN_THREAD_STACK_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(32 * 1024 * 1024);

    let handle = std::thread::Builder::new()
        .name("skipprd-main".to_string())
        .stack_size(stack_size)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build Tokio runtime");
            runtime.block_on(async_main());
        })
        .expect("failed to spawn skipprd main thread");

    if let Err(panic) = handle.join() {
        std::panic::resume_unwind(panic);
    }
}

async fn async_main() {
    // let now = Instant::now();

    // lazy_static! {
    //     static ref metadata: Mutex<HashMap<String, Metadata>> = Mutex::new(HashMap::new());
    //     static ref arrowSchema: Mutex<Result<Schema, ArrowError>> = Mutex::new(Ok(Schema::empty()));
    //     // static ref my_mutex: Mutex<i32> = Mutex::new(0i32);
    // }

    let cli: Cli = Cli::parse();

    if let Some(config) = &cli.config {
        Config::setenv("SKIPPR_CONFIG_FILE", &config.to_string_lossy());
    }
    if let Some(wal_storage) = &cli.wal_storage {
        Config::set_wal_storage(wal_storage.as_str());
    }
    if let Some(bucket) = &cli.wal_s3_bucket {
        Config::set_wal_s3_bucket(bucket);
    }
    if let Some(store) = &cli.offset_store {
        Config::set_offset_store(store.as_str());
    }
    if let Some(table) = &cli.offset_dynamodb_table {
        Config::set_offset_dynamodb_table(table);
    }

    // Initialize logging if --log is provided; default level is 'info', '--log debug' enables debug
    init_logging(cli.log.clone());

    match cli.mode.clone() {
        Mode::Sync(options) => {
            let loaded = Config::build_config();
            let storage = reject_invalid_wal_storage();
            reject_invalid_clustered_mode(
                &loaded,
                storage,
                CliModeKind::Sync { once: options.once },
            );
            let output_mode = options.output.clone();
            let run_once = options.once;
            let named = cli_named_pipeline(options.pipeline.as_deref());
            if storage == WalStorage::Clustered {
                let session =
                    Session::from_config(loaded, named.as_deref()).unwrap_or_else(|err| {
                        error!("{err}");
                        process::exit(1);
                    });
                if let Err(err) = session.sync(run_once, &output_mode).await {
                    error!("clustered sync failed: {err}");
                    process::exit(1);
                }
                return;
            }

            Metrics::init_send_loop(&loaded);

            if let Some(pipeline_name) = named {
                let session =
                    Session::from_config(loaded, Some(&pipeline_name)).unwrap_or_else(|err| {
                        error!("{err}");
                        process::exit(1);
                    });
                if let Err(err) = session.sync(run_once, &output_mode).await {
                    error!("Pipeline '{}' sync failed: {}", pipeline_name, err);
                    process::exit(1);
                }
            } else {
                info!("Syncing all pipelines");
                let pipelines = loaded.get_pipelines();
                loop {
                    for pipeline_name in pipelines.iter() {
                        let session = Session::from_config(loaded.clone(), Some(pipeline_name))
                            .unwrap_or_else(|err| {
                                error!("{err}");
                                process::exit(1);
                            });
                        let bound = loaded.bind_pipeline(pipeline_name);
                        bound.init().await;

                        if !PipelineCache::last_ran_is_elapsed(&bound) {
                            let remaining = bound.get_sync_frequency()
                                - PipelineCache::get_last_ran_elapsed(&bound);
                            info!(
                                "Pipeline '{}' last ran {} seconds ago, skipping for {} seconds.",
                                &pipeline_name,
                                PipelineCache::get_last_ran_elapsed(&bound),
                                remaining
                            );
                            continue;
                        }

                        PipelineCache::set_last_ran(&bound);

                        {
                            let mut counter_lock = METRICS.write();
                            counter_lock.reset();
                        }

                        if let Err(err) = session.sync(run_once, &output_mode).await {
                            error!("Pipeline '{}' sync failed: {}", pipeline_name, err);
                            process::exit(1);
                        }
                    }

                    if run_once {
                        break;
                    }
                    sleep(Duration::from_secs(10));
                }
            }
        }
        Mode::Discover(options) => {
            let loaded = Config::build_config();
            let storage = reject_invalid_wal_storage();
            reject_invalid_clustered_mode(&loaded, storage, CliModeKind::Discover);

            let output_mode = options.output.clone();
            let named = cli_named_pipeline(options.pipeline.as_deref());

            if let Some(pipeline) = named {
                let session = Session::from_config(loaded, Some(&pipeline)).unwrap_or_else(|err| {
                    error!("{err}");
                    process::exit(1);
                });
                if let Err(err) = session.discover(&output_mode).await {
                    error!("Pipeline '{}' discover failed: {}", pipeline, err);
                    process::exit(1);
                }
            } else {
                error!("No pipeline name provided, you must provide a pipeline name to discover schemas");
            }
        }
        Mode::Metadata { action } => {
            let loaded = Config::build_config();
            let storage = reject_invalid_wal_storage();
            reject_invalid_clustered_mode(&loaded, storage, CliModeKind::Metadata);
            let pipeline = skipprd::cli::metadata::pipeline_for_action(&action);
            let bound = loaded.bind_pipeline(pipeline);
            bound.init().await;
            match &action {
                skipprd::cli::metadata::MetadataAction::Show(_) => {
                    skipprd::cli::metadata::run_metadata_show(&bound).await;
                }
                skipprd::cli::metadata::MetadataAction::Apply(args) => {
                    skipprd::cli::metadata::run_metadata_apply(&bound, args).await;
                }
            }
        }
        Mode::Query(options) => {
            let loaded = Config::build_config();
            let storage = reject_invalid_wal_storage();
            reject_invalid_clustered_mode(&loaded, storage, CliModeKind::Query);
            let session = Session::from_config(loaded, None).unwrap_or_else(|err| {
                error!("{err}");
                process::exit(1);
            });
            let collect_plain =
                storage == WalStorage::Clustered || (options.plain && options.watch.is_none());
            if let Some(sql) = options.sql {
                let now = Instant::now();
                if collect_plain {
                    match session.query(&sql).await {
                        Ok(batches) => {
                            for batch in &batches {
                                skipprd::sqlrt::query::print_batches_plain(batch);
                            }
                        }
                        Err(err) => {
                            error!("query failed: {err}");
                            eprintln!("query failed: {err}");
                            process::exit(1);
                        }
                    }
                } else {
                    session
                        .query_with_options(
                            &sql,
                            QueryExecutionOptions {
                                mode: QueryExecutionMode::Query,
                                plain: options.plain,
                                watch: options.watch,
                            },
                        )
                        .await;
                }
                let elapsed = now.elapsed();
                if !options.plain && storage != WalStorage::Clustered {
                    println!("Query time: {} seconds", elapsed.as_secs());
                }
            } else if storage == WalStorage::Clustered {
                error!("clustered query requires --sql");
                eprintln!("clustered query requires --sql");
                process::exit(1);
            } else {
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
                    session
                        .query_with_options(
                            stmt,
                            QueryExecutionOptions {
                                mode: QueryExecutionMode::Query,
                                plain: options.plain,
                                watch: options.watch,
                            },
                        )
                        .await;
                    let elapsed = now.elapsed();
                    println!("Query time: {} seconds", elapsed.as_secs());
                }
            }
        }
        Mode::Schema(options) => {
            let loaded = Config::build_config();
            let storage = reject_invalid_wal_storage();
            reject_invalid_clustered_mode(&loaded, storage, CliModeKind::Schema);
            skipprd::engine::run_schema(&loaded, &options.pipeline).await;
        }
        Mode::Doctor(options) => {
            let loaded = Config::build_config();
            let session = Session::from_config(loaded, None).unwrap_or_else(|err| {
                error!("{err}");
                process::exit(1);
            });
            let result = session.doctor().await;
            if options.output == "json" {
                println!("{}", serde_json::to_string_pretty(&result).unwrap());
            } else {
                result.print_text();
            }
            if !result.ok {
                process::exit(1);
            }
        }
        Mode::Df(options) => {
            let named = cli_named_pipeline(options.pipeline.as_deref());
            let loaded = Config::build_config();
            let storage = reject_invalid_wal_storage();
            reject_invalid_clustered_mode(&loaded, storage, CliModeKind::Query);
            let session = Session::from_config(loaded, named.as_deref()).unwrap_or_else(|err| {
                error!("{err}");
                process::exit(1);
            });
            match session.df(options.namespace.as_deref()).await {
                Ok(batches) => {
                    for batch in &batches {
                        skipprd::sqlrt::query::print_batches_plain(batch);
                    }
                }
                Err(err) => {
                    error!("df failed: {err}");
                    process::exit(1);
                }
            }
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
                println!("  skipprd sql-help --command \"<SQL COMMAND>\"");
                println!();
                println!("To generate documentation, use:");
                println!("  skipprd sql-help --output <FILE_PATH> [--format md|html|json]");
            }
        }
        Mode::Benchmark(options) => {
            let loaded = Config::build_config();
            let storage = reject_invalid_wal_storage();
            reject_invalid_clustered_mode(&loaded, storage, CliModeKind::Benchmark);
            let bound = loaded.bind_pipeline("benchmark");
            bound.init().await;

            // Initialize benchmark
            let benchmark = PerformanceBenchmark::new(
                &bound,
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
        Mode::Connect(args) => {
            let path = cli
                .config
                .clone()
                .unwrap_or_else(skipprd::connect::discover_config_path);
            if cli.workspace.is_some()
                || cli.storage_mode.is_some()
                || cli.wal_s3_bucket.is_some()
                || cli.offset_store.is_some()
                || cli.offset_dynamodb_table.is_some()
                || cli.skippr_s3_bucket.is_some()
                || cli.tenant.is_some()
            {
                if let Err(err) = skipprd::connect::persist_skippr_keys(
                    &path,
                    cli.workspace.as_deref(),
                    cli.storage_mode,
                    cli.wal_s3_bucket.as_deref(),
                    cli.offset_store,
                    cli.offset_dynamodb_table.as_deref(),
                    cli.skippr_s3_bucket.as_deref(),
                    cli.tenant.as_deref(),
                ) {
                    error!("{err}");
                    eprintln!("{err}");
                    process::exit(1);
                }
            }
            let plugin = args.role.plugin();
            let pipeline = match args.role.pipeline() {
                Some(value) if !value.is_empty() => value.to_string(),
                _ => match skipprd::connect::prompt_if_tty("pipeline") {
                    Ok(value) if !value.is_empty() => value,
                    Ok(_) | Err(_) => {
                        eprintln!("connect requires --pipeline");
                        process::exit(1);
                    }
                },
            };
            let name = match args.role.name() {
                Some(value) if !value.is_empty() => value.to_string(),
                _ => match skipprd::connect::prompt_if_tty("name") {
                    Ok(value) if !value.is_empty() => value,
                    Ok(_) | Err(_) => {
                        eprintln!("connect requires --name");
                        process::exit(1);
                    }
                },
            };
            if let Err(err) = skipprd::connect::persist_plugin(
                &path,
                &pipeline,
                plugin,
                &name,
                match skipprd::connect::yaml_path_fields(plugin, &args.role.to_yaml_map()) {
                    Ok(fields) => fields,
                    Err(err) => {
                        error!("{err}");
                        eprintln!("{err}");
                        process::exit(1);
                    }
                },
            ) {
                error!("{err}");
                eprintln!("{err}");
                process::exit(1);
            }
        }
    }
}
