use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime};
use std::{fs, process};

use skipprd::cli::{Cli, Mode};

extern crate clap;
extern crate core;

use clap::Parser;

use std::string::ToString;

use skipprd::helpers::configuration::{Config, PIPELINE_NAME};
use skipprd::helpers::logging::init_logging;
use tracing::{error, info};

use skipprd::metrics::Metrics;
use skipprd::METRICS;

use skipprd::benchmark::PerformanceBenchmark;
use skipprd::sqlrt::doc_parser::SqlDocParser;
use skipprd::sqlrt::docs::{get_docs_in_format, DocFormat};
use skipprd::sqlrt::query::{query_with_options, QueryExecutionMode, QueryExecutionOptions};

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

async fn run_sync_or_exit(output_mode: &str, source_once: bool) {
    if let Err(err) = skipprd::engine::run_sync(output_mode, source_once).await {
        error!(
            "Pipeline '{}' sync failed: {}",
            Config::get_pipeline_name(),
            err
        );
        process::exit(1);
    }
}

async fn run_discover_or_exit(output_mode: &str) {
    if let Err(err) = skipprd::engine::run_discover(output_mode).await {
        error!(
            "Pipeline '{}' discover failed: {}",
            Config::get_pipeline_name(),
            err
        );
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

    // Initialize logging if --log is provided; default level is 'info', '--log debug' enables debug
    init_logging(cli.log.clone());

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

                run_sync_or_exit(&output_mode, run_once).await;
            } else {
                let pipeline_name = Config::getenv("PIPELINE_NAME", "");
                if !pipeline_name.is_empty() {
                    PIPELINE_NAME.write().clear();
                    PIPELINE_NAME.write().push_str(&pipeline_name.clone());
                    Config::init().await;

                    run_sync_or_exit(&output_mode, run_once).await;
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

                            run_sync_or_exit(&output_mode, run_once).await;
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
                query_with_options(
                    &sql,
                    QueryExecutionOptions {
                        mode: QueryExecutionMode::Query,
                        plain: options.plain,
                        watch: options.watch,
                    },
                )
                .await;
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
                    query_with_options(
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
            Config::build_config();
            // println!("Command schema");
            // Config::init().await;
            skipprd::engine::run_schema(&options.pipeline).await;
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
