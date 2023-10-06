mod arr;

use arrow::datatypes::Schema;
use arrow::error::ArrowError;

// mod thread_pool;
// use thread_pool::ThreadPool;
mod ingest_work;

extern crate nix;

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use std::process;

use std::collections::HashMap;

use std::fs::File;

use std::io::BufReader;
use std::ops::{Add, Deref};

use std::sync::{Arc, Mutex, RwLock};
use std::{env, thread};
use std::fmt::Debug;

use std::fs;



use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::{Instant};

use glob::glob_with;
use glob::MatchOptions;

mod buffer;

mod helpers;

mod metrics;

mod internalfields;

mod discover;
use crate::discover::AnalyseSchema;
use crate::discover::Metadata;
mod converters;
// use self::converters::avro_parquet::AvroSchema;
mod cli;
use crate::cli::{Cli, Mode};

extern crate clap;
extern crate core;

use clap::Parser;


use signal_hook::iterator::Signals;

use std::panic;
use std::process::abort;
use std::string::ToString;
use arrow_schema::DataType;
use datafusion::common::ExprSchema;
use datafusion::config::ConfigOptions;
use datafusion::physical_plan::Statistics;
use futures::TryFutureExt;

use once_cell::sync::Lazy;
use signal_hook::consts::{SIGABRT, SIGINT, SIGQUIT, SIGTERM};
use tokio::runtime;

mod ingest;

mod serdes;

use crate::serdes::parquet::SerdeParquet;

mod plugins;

use crate::discover::arrow_schema::convert_skippr_to_arrow;
use crate::helpers::configuration::{Config, PIPELINE_NAME};

use crate::buffer::BufferChunker;
use crate::helpers::logger::{LogLevel, Logger};
use crate::helpers::offsets::Offsets;
use crate::helpers::Helpers;
use crate::helpers::license::HAS_LICENSE;
use crate::plugins::athena::DataOutputAwsAthenaPlugin;

use crate::plugins::s3_input::DataSourceS3Plugin;
use crate::plugins::s3_inventory::DataSourceS3InventoryPlugin;

use crate::ingest_work::{Ingest, OUTPUT_FILES_STATIC};
use crate::metrics::{Metrics, MetricsStatus};
use crate::plugins::file_input::DataSourceLocalFilePlugin;
use crate::plugins::file_output::DataOutputFilePlugin;
use crate::plugins::s3_output::DataOutputS3Plugin;
use crate::plugins::stdin_input::DataSourceStdinPlugin;
use crate::plugins::stdout_output::DataOutputStdoutPlugin;

use datafusion::prelude::*;
use nix::libc::{exit, signal};
use crate::helpers::timed_rwlock::TimedRwLock;
// use crate::plugins::pcap_input::DataSourcePcapPlugin;

// pub static DISPLAY_METRICS: Lazy<TimedRwLock<AtomicBool>> =
//     Lazy::new(|| TimedRwLock::new("display_metrics".to_string(), AtomicBool::new(false)));

pub static RUNNING: Lazy<TimedRwLock<AtomicBool>> = Lazy::new(|| TimedRwLock::new("running".to_string(),AtomicBool::new(true)));
pub static OUTPUT_RUNNING: Lazy<TimedRwLock<AtomicBool>> =
    Lazy::new(|| TimedRwLock::new("output_running".to_string(), AtomicBool::new(false)));
pub static OUTPUT_GRACEFUL_SHUTDOWN_COMPLETE: Lazy<TimedRwLock<AtomicBool>> =
    Lazy::new(|| TimedRwLock::new("output_graceful_shutdown_complete".to_string(), AtomicBool::new(false)));

pub static LOGGER: Lazy<Arc<tokio::sync::RwLock<Logger>>> = Lazy::new(|| Logger::new(100));
pub static METRICS: Lazy<Arc<TimedRwLock<Metrics>>> = Lazy::new(|| Arc::new(TimedRwLock::new("metrics".to_string(),Metrics::new())));
pub static METADATA: Lazy<Arc<TimedRwLock<HashMap<String, Metadata>>>> = Lazy::new(|| Arc::new(TimedRwLock::new("metadata".to_string(), HashMap::new())));
pub static NEW_METADATA: Lazy<Arc<TimedRwLock<HashMap<String, Metadata>>>> = Lazy::new(|| Arc::new(TimedRwLock::new("metadata".to_string(), HashMap::new())));

#[tokio::main]
async fn main() {

    // let now = Instant::now();

    // lazy_static! {
    //     static ref metadata: Mutex<HashMap<String, Metadata>> = Mutex::new(HashMap::new());
    //     static ref arrowSchema: Mutex<Result<Schema, ArrowError>> = Mutex::new(Ok(Schema::empty()));
    //     // static ref my_mutex: Mutex<i32> = Mutex::new(0i32);
    // }

    let cli: Cli = Cli::parse();

    match cli.mode {

        Mode::Sync(options) => {

            Config::build_config();

            if options.pipeline.is_some() {
                // println!("Syncing pipeline: {}", options.pipeline.unwrap().clone());
                PIPELINE_NAME.write().unwrap().clear();
                PIPELINE_NAME.write().unwrap().push_str(&options.pipeline.unwrap().clone());
                Config::init().await;

                sync().await;
            } else {
                let pipeline_name = Config::getenv("PIPELINE_NAME", "");
                if !pipeline_name.is_empty() {
                    // println!("Syncing pipeline: {}", Config::getenv("PIPELINE_NAME").unwrap());
                    PIPELINE_NAME.write().unwrap().clear();
                    PIPELINE_NAME.write().unwrap().push_str(&pipeline_name);
                    Config::init().await;
                    sync().await;
                } else {
                    println!("No pipeline name provided, syncing all pipelines");
                    let pipelines = Config::get_pipelines();
                    for pipeline in pipelines {
                        println!("Syncing pipeline: {}", pipeline);
                        PIPELINE_NAME.write().unwrap().clear();
                        PIPELINE_NAME.write().unwrap().push_str(&pipeline);
                        Config::init().await;
                        sync().await;
                    }
                }
                // PIPELINE_NAME.write().unwrap().clear();
                // PIPELINE_NAME.write().unwrap().push_str(&options.pipeline.unwrap().clone());
                // Config::init().await;
                // sync().await;
            }


        }
        Mode::Discover => {
            // println!("Command discover");
            Config::init().await;
            discover().await;
        }
        Mode::Query(options) => {

            // Track and report query runtime in seconds
            let now = Instant::now();
            query(&options.query).await;
            let elapsed = now.elapsed();
            println!("Query time: {} seconds", elapsed.as_secs());
        }
        Mode::Schema(options) => {
            // println!("Command schema");
            Config::init().await;
            schema(&options.schema).await;
        }

    }
}

async fn schema(schema_name: &str) {
    // register the table
    // let mut options = ConfigOptions::default();
    // options.catalog.information_schema = true;

    let mut session_config = SessionConfig::new();
    session_config = session_config.set("datafusion.catalog.information_schema", "true".into());
    session_config = session_config.set("datafusion.catalog.default_catalog", "skippr".into());
    session_config = session_config.set("datafusion.execution.collect_statistics", "true".into());

    let ctx = SessionContext::with_config(session_config);

    Config::setenv("PIPELINE_NAME", schema_name);

    let workspace = Config::get_workspace_name();
    let full_table_name = format!("{}.{}", workspace, schema_name);

    // @todo - check dir exists for provided table name, otherwise we end up creating erroneous dirs


    // itterate over data dir output buffers
    let data_dir = Config::get_data_dir();
    let output_dir = format!("{}/output_buffer", data_dir);

    println!("Querying data dir: {}", output_dir);

    match ctx.register_parquet(&schema_name, &output_dir, ParquetReadOptions::default()).await {
        Ok(_) => {}
        Err(e) => {
            println!("Can't find data for table: {} in dir: {}", schema_name, output_dir);
            process::exit(1);
        }
    }

    let dfn = ctx.table(schema_name).await.unwrap();

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

async fn query(sql: &str) {
    // register the table
    // let mut options = ConfigOptions::default();
    // options.catalog.information_schema = true;

    let mut session_config = SessionConfig::new();
    session_config = session_config.set("datafusion.catalog.information_schema", "true".into());
    session_config = session_config.set("datafusion.catalog.default_catalog", "skippr".into());
    session_config = session_config.set("datafusion.execution.collect_statistics", "true".into());

    let ctx = SessionContext::with_config(session_config);

    let table_name = sql.to_lowercase().split("from").collect::<Vec<&str>>()[1].split(" ").collect::<Vec<&str>>()[1].trim().replace(";", "");

    let workspace = Config::get_workspace_name();
    Config::setenv("PIPELINE_NAME", &table_name);
    let full_table_name = format!("{}.{}", workspace, table_name);

    // @todo - check dir exists for provided table name, otherwise we end up creating erroneous dirs


    // itterate over data dir output buffers
    let data_dir = Config::get_data_dir();
    let output_dir = format!("{}/output_buffer", data_dir);

    println!("Querying data dir: {}", output_dir);

    match ctx.register_parquet(&table_name, &output_dir, ParquetReadOptions::default()).await {
        Ok(_) => {}
        Err(e) => {
            println!("Can't find data for table: {} in dir: {}. Error: {:?}", table_name, output_dir, e);
            process::exit(1);
        }
    }

    let df = match ctx.sql(sql).await {
        Ok(df) => df,
        Err(e) => {
            println!("Error: {}", e);
            process::exit(1);
        }
    };

    match df.show().await {
        Ok(res) => {
            res
        }
        Err(e) => {
            println!("Error: {}", e);
            process::exit(1);
        }
    }



}

async fn discover() {
    println!("Analysing data and generating Skippr metadata");

    // thread::spawn(async move || {
    let options = MatchOptions {
        case_sensitive: false,
        require_literal_separator: false,
        require_literal_leading_dot: false,
    };

    let mut foo: AnalyseSchema = AnalyseSchema { i: 0 };

    let mut has_analysed = false;

    // let mut skippr_metadata: HashMap<String, Metadata> = HashMap::new();

    let data_dir = Config::get_data_dir();
    let metadata_file = format!("{}/metadata.json", data_dir);

    // Get existing metadata
    let mut skippr_metadata: HashMap<String, Metadata> = match File::open(metadata_file) {
        Ok(file) => {
            let reader = BufReader::new(file);
            match serde_json::from_reader(reader) {
                Ok(metadata) => metadata,
                Err(_e) => {
                    // println!("No existing metadata {}", e);
                    HashMap::new()
                }
            }
        }
        Err(_e) => {
            // println!("No existing metadata {}", e);
            HashMap::new()
        }
    };

    // @todo - invoke data input plugin in a mode that doesn't ingest data... just pipe it here
    // @todo - we will probably still want to source_buffer, but won't want check pointing

    let _arrow_schema: Result<Schema, ArrowError> = Ok(Schema::empty());

    let _schema_ref = Arc::new(Schema::empty());

    let mut analyse_count = 0;

    let data_dir = Config::get_data_dir();
    let pattern = &format!("{}/source_buffer/*", data_dir);

    while !has_analysed && analyse_count < 10 {
        analyse_count += 1;

        for entry in glob_with(pattern, options).expect("Failed to read glob pattern") {
            if !has_analysed {
                match entry {
                    Ok(path) => {
                        let input_file = File::open(path.clone()).unwrap();

                        println!("Analysing path: {}", path.to_str().unwrap());

                        // let mut buf_reader = BufReader::new(input_file);

                        // skippr_metadata =
                        //     AnalyseSchema::infer_json_schema(&mut foo, &mut buf_reader, Some(1000))
                        //         .unwrap();
                        AnalyseSchema::infer_json_schema(
                            &mut foo,
                            input_file,
                            Some(1000),
                            &mut skippr_metadata,
                        );

                        // println!("Skippr schema: {:?}", skippr_metadata);

                        // arrowSchema = convert_skippr_to_arrow(
                        //     skippr_metadata.get(&"example_ns".to_string()).unwrap().fields.clone(),
                        // );
                        //
                        // // let json = serde_json::to_string_pretty(&arrowSchema).unwrap();
                        // // eprintln!("Schema:");
                        // // println!("{}", json);
                        //
                        // // println!("Arrow schema: {:?}", arrowSchema);
                        //
                        // // let schema_ref = Arc::new(arrowSchema.unwrap());
                        // schema_ref = Arc::new(arrowSchema.unwrap());
                        // // schema_ref = arrowSchema.unwrap();

                        has_analysed = true;
                    }
                    Err(e) => println!("{:?}", e),
                }
            }
        }
    }

    let flatten = Config::truth_value(&Config::get_transform_config().flatten_events.unwrap());

    AnalyseSchema::determine_field_types(&mut skippr_metadata, None, None, flatten);

    Config::set_config(&skippr_metadata, true).await;

    // let file = OpenOptions::new()
    //     .create(true)
    //     .write(true)
    //     .truncate(true)
    //     .open(&"metadata.json".to_string())
    //     .unwrap();
    //
    // let writer = BufWriter::new(file);
    //
    // serde_json::to_writer(writer, &skippr_metadata).unwrap();

    // skippr_metadata
    // }).join().unwrap();
}

async fn sync() {
    {
        LOGGER.write()
            .await
            .log(LogLevel::Info, "Starting Skippr".to_string())
            .await;

        let mut counter_lock = METRICS.write().unwrap();
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

    let _data_dir = Config::get_data_dir();

    let skippr_metadata = match Config::get_config().await {
        Ok(metadata) => {
            println!("Found Skippr metadata");

            Config::sync_schema(&metadata).await;

            metadata
        }
        Err(_e) => {
            println!("No existing Skippr metadata, will discover and evolve schemas as we sync");
            let _empty_meta = Metadata::new().unwrap();

            HashMap::new()
        }
    };

    {

        METADATA.write().unwrap().clone_from(&skippr_metadata);
    }

    let now = Arc::new(Mutex::new(Instant::now()));

    let offsets = Arc::new(Offsets::init().unwrap());
    let offsets_clone = offsets.clone();
    // let logger_clone = Arc::clone(&logger);

    /**
     * Handle PANICS in threads
     */
    // take_hook() returns the default hook in case when a custom one is not set
    let orig_hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        // invoke the default handler and exit the process
        orig_hook(panic_info);
        // println!("{:?}", panic_info);
        let panic_str = format!("{:?}", panic_info);

        let panic_info_clone = panic_str.clone();

        {
            let mut counter_lock = METRICS.write().unwrap();
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

        if !RUNNING.read().unwrap().load(Ordering::SeqCst) {
            println!("Received another panic - already gracefully shutting down");
        } else {

            let pid = process::id() as i32; // or replace with the PID of the target process

            unsafe {
                kill(Pid::from_raw(pid), Signal::SIGTERM).unwrap();
            }
        }

    }));

    // thread::spawn(move || {
    //     panic!("something bad happened");
    // }).join();

    // this line won't ever be invoked because of process::exit()
    // println!("Won't be printed");

    /**
     * Handle SIGNALS
     */
    let mut signals = Signals::new(&[SIGINT, SIGTERM, SIGQUIT, SIGABRT]).unwrap();

    // let logger_clone = Arc::clone(&logger);

    thread::spawn(move || {
        for sig in signals.forever() {

            {
                let mut counter_lock = METRICS.write().unwrap();
                counter_lock.status = MetricsStatus::Stopped;
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
                        .log(LogLevel::Info, format!("Received SIG: {} - Gracefully shutting down", sig.to_string()))
                        .await;
                    LOGGER.write().await.flush().await.unwrap();
                });
            }).join().unwrap();

            if !RUNNING.read().unwrap().load(Ordering::SeqCst) {
                println!("Received another Ctrl+C signal - terminating immediately, this may result in data loss...");
                std::process::exit(0);
            }

            {
                RUNNING.write().unwrap().store(false, Ordering::SeqCst);
            }

            let offsets_clone = offsets_clone.clone();
            // let logger_clone = Arc::clone(&logger_clone);

            thread::spawn(move || {
                println!("Received SIG: {} - Gracefully shutting down", sig.to_string());
                // println!("Flushing ingest buffers");
                // let mut output_files = OUTPUT_FILES_STATIC.lock().unwrap();
                // Ingest::flush_buffers(true, &mut output_files);
                // sleep(Duration::from_secs(30)); // wait for threads to flush

                // RUNNING.write().unwrap().store(false, Ordering::SeqCst);

                while
                    OUTPUT_RUNNING
                        .read()
                        .unwrap()
                        .load(Ordering::SeqCst) &&
                    !OUTPUT_GRACEFUL_SHUTDOWN_COMPLETE
                        .read()
                        .unwrap()
                        .load(Ordering::SeqCst)
                {
                    sleep(Duration::from_secs(1));
                }

                let mut output_files = OUTPUT_FILES_STATIC.write().unwrap();
                Ingest::flush_buffers(true, &mut output_files);

                offsets_clone.flush();
                println!("Flushed offsets");

                let _metrics_lock = match METRICS.read() {
                    Ok(m) => {
                        println!("Messages Total: {}", m.messages_total);
                    },
                    Err(_e) => {
                        println!("Could not lock metrics, skipping flush");
                        return;
                    }
                };

                let total_times: HashMap<String, Duration> = TimedRwLock::<()>::get_total_wait_times();
                for (key, value) in total_times.iter() {
                    println!("{}: {}ms", key, value.as_millis());
                }


                ////////////// Cleanup part written parquet files START ////////

                let options = MatchOptions {
                    case_sensitive: false,
                    require_literal_separator: false,
                    require_literal_leading_dot: false,
                };

                let data_dir = Config::get_data_dir();

                // println!("Looking for temp files in {}", &format!("{}/output_buffer/*parquet.temp", data_dir));

                for entry in glob_with(&format!("{}/output_buffer/*parquet.temp", data_dir), options)
                    .expect("Failed to read glob 'finalised' pattern")
                {
                    match entry {
                        Ok(path) => {
                            // println!("Removing file {}", path.display().to_string());

                            match std::fs::remove_file(path) {
                                Ok(_t) => {}
                                Err(err) => println!("{:?}", err),
                            }
                        }
                        Err(e) => println!("{:?}", e),
                    }
                }
                ////////////// Cleanup part written parquet files END ////////

                // tokio::runtime::Builder::new_multi_thread()
                //     .enable_all()
                //     .build()
                //     .unwrap()
                //     .block_on(async {
                //         match LOGGER.write().await.flush().await {
                //             Ok(_t) => {}
                //             Err(_err) => {
                //                 // println!("Graceful shutdown complete... bye");
                //             }
                //         }
                //     });


                // sleep(Duration::from_secs(15)); // wait for threads to flush

                println!("Graceful shutdown complete... bye");
                std::process::exit(0);
            });
        }
    });

    use std::time::Duration;

    let mut planner = periodic::Planner::new();

    let now_clone = now.clone();

    let last_messages_total = Arc::new(Mutex::new(0));

    planner.add(
        move || {
            if RUNNING.read().unwrap().load(Ordering::SeqCst) {

                let mut metrics_lock = match METRICS.read() {
                    Ok(lock) => lock,
                    Err(poisoned) => poisoned.into_inner(),
                };

                let now_lock = now_clone.lock().unwrap();

                // metrics_lock.bytes_total += metrics_lock.bytes_current;

                // metrics_lock.run_time_seconds = now_lock.elapsed().as_secs();

                println!("Runtime: {} seconds", now_lock.elapsed().as_secs());

                let last_messages_total_val = *last_messages_total.lock().unwrap();
                let ingested_current = metrics_lock.messages_total - last_messages_total_val;
                *last_messages_total.lock().unwrap() = metrics_lock.messages_total;


                // if DISPLAY_METRICS.read().unwrap().load(Ordering::SeqCst) {
                    println!("Messages per Min: {}", ingested_current);
                    println!("Messages Fixed: {}", metrics_lock.ingeted_slow_total);
                    println!("Messages Total: {}", metrics_lock.messages_total);
                    println!("Deadletter Messages: {}", metrics_lock.deadletters_total);
                    // println!("Bytes per Min: {}", metrics_lock.bytes_current);
                    println!("Bytes: {}", metrics_lock.bytes_total);
                // }
                // metrics_lock.bytes_current = 0;

                let total_times: HashMap<String, Duration> = TimedRwLock::<()>::get_total_wait_times();
                for (key, value) in total_times.iter() {
                    println!("{}: {}ms", key, value.as_millis());
                }


                drop(metrics_lock);

                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {

                        match Metrics::send_metrics(Some(0)).await {
                            Ok(_g) => {}
                            Err(_err) => {}
                        }

                        LOGGER.write().await.flush().await.unwrap();

                    });

                // let mut metrics_lock = METRICS.read().unwrap();



            }
        },
        periodic::Every::new(Duration::from_secs(60)),
    );
    planner.start();

    let mut out_pnanner = periodic::Planner::new();

    use rand::Rng; // 0.8.5

    // let metrics_clone = metrics.clone();

    let chaos = Config::get_pipeline_config().chaos_mode.or(Some("no".to_string())).unwrap();
    if Config::truth_value(&chaos) {
        out_pnanner.add(
            move || {
                if RUNNING.read().unwrap().load(Ordering::SeqCst) {

                    println!("Chaos mode throwing a random exit. You can disable this test mode buy removing CHAOS_MODE flag or setting to 'no'");

                    let pid = process::id() as i32; // or replace with the PID of the target process

                    unsafe {
                        kill(Pid::from_raw(pid), Signal::SIGTERM).unwrap();
                    }
                }

                // exit(0);
            }, periodic::Every::new(Duration::from_secs(rand::thread_rng().gen_range(15..60))),
        );
    }
    out_pnanner.add(
        move || {
            if RUNNING.read().unwrap().load(Ordering::SeqCst) {
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {

                        output_sync();

                        while OUTPUT_RUNNING.read().unwrap().load(Ordering::SeqCst) {
                            // sleep(Duration::from_secs(1));
                            return;
                        }

                        if Config::get_pipeline_config().output.is_some() {
                            {
                                OUTPUT_RUNNING.write().unwrap().store(true, Ordering::SeqCst);
                            }
                            {
                                OUTPUT_RUNNING.write().unwrap().store(true, Ordering::SeqCst);
                            }

                            sync_output_plugin(Config::get_pipeline_output_plugin_name().as_str(), "output".to_string()).await;

                            OUTPUT_RUNNING
                                .write()
                                .unwrap()
                                .store(false, Ordering::SeqCst);
                        }
                    });
            }
        },
        periodic::Every::new(Duration::from_secs(60)),
    );
    out_pnanner.start();

    // @todo - share across s3 ingests
    let _parse_namespace_cache: HashMap<String, String> = HashMap::new();
    let _output_files: HashMap<String, File> = HashMap::new();
    let _options = MatchOptions {
        case_sensitive: false,
        require_literal_separator: false,
        require_literal_leading_dot: false,
    };
    let data_dir = Config::get_data_dir();
    let ingest_dir = &format!("{}/ingest_buffer", data_dir);
    let deadletter_dir = &format!("{}/deadletter_buffer", data_dir);
    let output_dir = &format!("{}/output_buffer", data_dir);
    match fs::create_dir(deadletter_dir) {
        Ok(_g) => {}
        Err(_err) => {}
    }
    match fs::create_dir(format!("{}/done", deadletter_dir)) {
        Ok(_g) => {}
        Err(_err) => {}
    }
    match fs::create_dir(ingest_dir) {
        Ok(_g) => {}
        Err(_err) => {}
    }
    match fs::create_dir(format!("{}/done", ingest_dir)) {
        Ok(_g) => {}
        Err(_err) => {}
    }
    match fs::create_dir(output_dir) {
        Ok(_g) => {}
        Err(_err) => {}
    }

    let offsets_clone = offsets.clone();

    sync_input_plugin(offsets_clone).await;

    println!("Ingest completed, flushing remaining buffers to output plugin {}", Config::get_pipeline_config().output.or(Some("".to_string())).unwrap());

    {
        LOGGER.write()
            .await
            .log(LogLevel::Info, format!("Ingest completed, flushing remaining buffers to output plugin {}", Config::get_pipeline_config().output.or(Some("".to_string())).unwrap()))
            .await;

        let mut counter_lock = METRICS.write().unwrap();
        counter_lock.status = MetricsStatus::Finishing;

    }


    // RUNNING.write().unwrap().store(false, Ordering::SeqCst); // the prevents metrics from printing while shutting down, BUT also prevents output serialisatin

    let mut output_files = OUTPUT_FILES_STATIC.write().unwrap();
    Ingest::flush_buffers(true, &mut output_files);

    while OUTPUT_RUNNING.read().unwrap().load(Ordering::SeqCst) {
        sleep(Duration::from_secs(1));
    }

    output_sync();

    if Config::get_pipeline_config().output.is_some() {
        sync_output_plugin(Config::get_pipeline_output_plugin_name().as_str(), "output".to_string()).await;
    }

    if Config::get_pipeline_config().deadletter.is_some() {
        sync_output_plugin(&Config::get_pipeline_deadletter_plugin_name(), "deadletter".to_string()).await;
    }

    let mut metrics_lock = METRICS.read().unwrap();

    let now_lock = now.lock().unwrap();

    // metrics_lock.bytes_total += metrics_lock.bytes_current;

    // metrics_lock.run_time_seconds = now_lock.elapsed().as_secs();

    println!("Runtime: {} seconds", now_lock.elapsed().as_secs());
    println!("Messages Total: {}", metrics_lock.messages_total);
    println!("Deadletter Messages: {}", metrics_lock.deadletters_total);
    println!("Bytes: {}", metrics_lock.bytes_total);

    drop(metrics_lock);

    let total_times: HashMap<String, Duration> = TimedRwLock::<()>::get_total_wait_times();
    for (key, value) in total_times.iter() {
        println!("{}: {}ms", key, value.as_millis());
    }


    {
        LOGGER.write()
            .await
            .log(LogLevel::Info, "Complete, shutting down".to_string())
            .await;

        let mut counter_lock = METRICS.write().unwrap();
        counter_lock.status = MetricsStatus::Completed;

    }

    match Metrics::send_metrics(Some(0)).await {
        Ok(_g) => {}
        Err(_err) => {}
    }

    LOGGER.write().await.flush().await.unwrap();

    println!("Complete. Shutting Down... bye");
}

fn output_sync() {
    // thread::spawn(move || {
    // println!("Arrow Schema: {:?}", arrowSchema);

    if OUTPUT_RUNNING.read().unwrap().load(Ordering::SeqCst) {
        return;
    } else {
        OUTPUT_RUNNING.write().unwrap().store(true, Ordering::SeqCst);
    }

    let flatten = Config::get_transform_flatten_events();

    let data_dir = Config::get_data_dir();
    let output_dir = &format!("{}/ingest_buffer", data_dir);
    let finalised_dir = &format!("{}/output_buffer", data_dir);

    let options = MatchOptions {
        case_sensitive: false,
        require_literal_separator: false,
        require_literal_leading_dot: false,
    };

    // println!("Finalising output files");
    // Config::list_dir_contents(output_dir).expect(&format!("Could not list output dir {}", output_dir));

    // loop {
    for entry in
        glob_with(&format!("{}/done/*", output_dir), options).expect("Failed to read glob pattern")
    {
        if RUNNING.read().unwrap().load(Ordering::SeqCst) {
            match entry {
                Ok(path) => {
                    // println!("Finalising output file {}", path.display());

                    // Always regenerate arrow schema incase updated skippr metadata, e.g. discovered a new field
                    let mut arrow_schema: Result<Schema, ArrowError> = Ok(Schema::empty());
                    let mut schema_ref = Arc::new(Schema::empty());

                    let skpr_namespace =
                        BufferChunker::decode_file_namespace(path.to_str().unwrap());
                    // let skpr_partition = BufferChunker::decode_file_partition(path.to_str().unwrap());

                    let metadata = METADATA.read().unwrap();

                    if metadata.get(&skpr_namespace).is_some() {
                        let mut output_metadata: HashMap<String, Metadata> = HashMap::new();
                        if flatten {
                            let mut meta: HashMap<String, Metadata> = HashMap::new();

                            flatten_metadata(metadata.get(&skpr_namespace).unwrap(), &mut meta);

                            let mut flat: Metadata = Metadata::new().unwrap();
                            flat.fields = Box::new(meta);
                            output_metadata.insert(skpr_namespace.clone(), flat);
                        } else {
                            output_metadata = metadata.clone();
                        }

                        let skpr_partition =
                            BufferChunker::decode_file_partition(path.to_str().unwrap());
                        let source_time = BufferChunker::decode_file_time(path.to_str().unwrap());

                        arrow_schema = convert_skippr_to_arrow(
                            output_metadata.get(&skpr_namespace).unwrap().fields.clone(),
                        );

                        schema_ref = Arc::new(arrow_schema.unwrap());

                        let tmp_file_path = SerdeParquet::serialize(path.clone(), schema_ref);

                        let finalised_file_name = BufferChunker::encode_chunk_name(
                            "output",
                            Some(&skpr_namespace),
                            Some(&skpr_partition),
                            Some(source_time),
                        );

                        let finalised_file_path = &format!(
                            "{}/{}&part={}.parquet",
                            finalised_dir,
                            finalised_file_name,
                            Helpers::random_str(32).as_str()
                        );

                        match fs::rename(tmp_file_path, finalised_file_path) {
                            Ok(_) => {}
                            Err(_) => {}
                        };

                        match std::fs::remove_file(path) {
                            Ok(_t) => {}
                            Err(err) => println!("{:?}", err),
                        }
                    }
                }
                Err(e) => println!("{:?}", e),
            }
        }
    }

    OUTPUT_RUNNING
        .write()
        .unwrap()
        .store(false, Ordering::SeqCst);

    if !RUNNING.read().unwrap().load(Ordering::SeqCst) {
        OUTPUT_GRACEFUL_SHUTDOWN_COMPLETE
            .write()
            .unwrap()
            .store(true, Ordering::SeqCst);
    }


    // sleep(Duration::from_secs(1));
    // }
    // });
}

pub fn flatten_metadata(metadata: &Metadata, flattened: &mut HashMap<String, Metadata>) {
    for (_key, val) in metadata.fields.iter() {
        if val.determined_type == "record" || val.determined_type == "map" {
            flatten_metadata(val, flattened);
        } else {
            flattened.insert(val.out_field_name.clone(), val.clone());
        }
    }
}

pub async fn sync_output_plugin(plugin_name: &str, buffer_name: String) {
    match plugin_name {
        "stdout" => {
            let mut output = DataOutputStdoutPlugin::new(buffer_name).await;
            output
                .sync()
                .await;
        }
        "file" => {
            let mut output = DataOutputFilePlugin::new(buffer_name).await;
            output
                .sync()
                .await;
        }
        "s3" => {
            if *HAS_LICENSE.read().unwrap() {
                let mut output = DataOutputS3Plugin::new(buffer_name).await;
                output
                    .sync()
                    .await;
            } else {
                println!("No license found for S3 output plugin. Visit https://skippr.io to get a license.");
            }

        }
        "athena" => {
            if *HAS_LICENSE.read().unwrap() {
                let mut output = DataOutputAwsAthenaPlugin::new(buffer_name).await;
                output
                    .sync()
                    .await;
            } else {
                println!("No license found for Athena output plugin. Visit https://skippr.io to get a license.");
            }
        }
        "" => {
            println!("No Data Output plugin specified");
        }
        _ => {
            println!("Unknown Data Output plugin specified");
        }
    }
}

pub async fn sync_input_plugin(offsets_clone: Arc<Offsets>) {
    match Config::get_pipeline_input_plugin_name().as_str() {
        "pcap" => {
            panic!("PCAP input plugin not installed, please contact support")
            // let mut input = DataSourcePcapPlugin::new().await;
            // input
            //     .sync(
            //         offsets_clone,
            //     )
            //     .await;
        }
        "stdin" => {
            let mut input = DataSourceStdinPlugin::new().await;
            input
                .sync(
                    offsets_clone,
                )
                .await;
        }
        "file" => {
            let mut input = DataSourceLocalFilePlugin::new().await;
            input.sync(
                offsets_clone
            )
                .await;
        }
        "s3" => {
            // if *HAS_LICENSE.read().unwrap() {
                let mut input = DataSourceS3Plugin::new().await;
                input.sync(
                    offsets_clone,
                )
                    .await;
            // } else {
            //     println!("No license found for S3 input plugin. Visit https://skippr.io to get a license.");
            // }

        }
        "s3_inventory" => {
            if *HAS_LICENSE.read().unwrap() {
                let mut input = DataSourceS3InventoryPlugin::new().await;
                input.sync(
                    offsets_clone,
                )
                    .await;
            } else {
                println!("No license found for S3 Inventory input plugin. Visit https://skippr.io to get a license.");
            }
        }
        "" => {
            println!("No Data Source plugin specified. You must specify a data source plugin, see documentation for the DATA_SOURCE_PLUGIN_NAME environment variable.");
        }
        unknown => {
            println!("Data Source Plugin {} not supported", unknown);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_flatten_metadata() {
        let mut fields: Box<HashMap<String, Metadata>> = Box::new(HashMap::new());

        let metadata_child = Metadata {
            count: 1,
            types: HashMap::new(),
            parent_type: "record".to_string(),
            fields: Box::new(HashMap::new()),
            date_candidate: None,
            evolution: Box::new(HashMap::new()),
            enabled: true,
            out_field_name: "parent_child".to_string(),
            determined_type: "string".to_string(),
            determined_type_values: "".to_string(),
        };

        fields.insert("child".to_string(), metadata_child.clone());

        let metadata = Metadata {
            count: 1,
            types: HashMap::new(),
            parent_type: "".to_string(),
            fields: fields,
            date_candidate: None,
            evolution: Box::new(HashMap::new()),
            enabled: true,
            out_field_name: "parent".to_string(),
            determined_type: "record".to_string(),
            determined_type_values: "".to_string(),
        };

        let mut flattened: HashMap<String, Metadata> = HashMap::new();

        flatten_metadata(&metadata, &mut flattened);

        println!("{:?}", flattened);

        assert_eq!(flattened.len(), 1);
        assert!(flattened.contains_key("parent_child"));
        assert_eq!(
            flattened.get("parent_child").unwrap().count,
            metadata_child.count
        );
    }
}
