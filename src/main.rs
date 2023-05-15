mod arr;

use arrow::datatypes::Schema;
use arrow::error::ArrowError;

// mod thread_pool;
// use thread_pool::ThreadPool;
mod ingest_work;

use std::collections::HashMap;

use std::fs::File;

use std::io::BufReader;
use std::ops::Add;

use std::sync::{Arc, Mutex};
use std::thread;

use std::fs;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use futures::executor::block_on;
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

mod ingest;

mod serdes;

use crate::serdes::parquet::SerdeParquet;

mod plugins;

// use crate::helpers::Config

use crate::discover::arrow_schema::convert_skippr_to_arrow;
use crate::helpers::configuration::{Config, Metrics};

use crate::buffer::BufferChunker;

use crate::plugins::s3_input::DataSourceS3Plugin;
use crate::plugins::s3_inventory::DataSourceS3InventoryPlugin;

#[tokio::main]
async fn main() {
    Config::init().await;

    // let now = Instant::now();

    // lazy_static! {
    //     static ref metadata: Mutex<HashMap<String, Metadata>> = Mutex::new(HashMap::new());
    //     static ref arrowSchema: Mutex<Result<Schema, ArrowError>> = Mutex::new(Ok(Schema::empty()));
    //     // static ref my_mutex: Mutex<i32> = Mutex::new(0i32);
    // }

    let cli = Cli::parse();

    match cli.mode {
        Mode::Sync => {
            // println!("Command sync");
            sync().await;
        }
        Mode::Discover => {
            // println!("Command discover");
            discover().await;
        }
    }

    // for x in 1..20000 {
    //     untyped_example();
    // }
    // blah();
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

    let mut hasAnalysed = false;

    // let mut skipprMetadata: HashMap<String, Metadata> = HashMap::new();

    let data_dir = Config::get_data_dir();
    let metadata_file = format!("{}/metadata.json", data_dir);

    // Get existing metadata
    let mut skipprMetadata: HashMap<String, Metadata> = match File::open(metadata_file) {
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

    let _arrowSchema: Result<Schema, ArrowError> = Ok(Schema::empty());

    let _schema_ref = Arc::new(Schema::empty());

    let mut analyseCount = 0;

    let data_dir = Config::get_data_dir();
    let pattern = &format!("{}/source_buffer/*", data_dir);

    while !hasAnalysed && analyseCount < 10 {
        analyseCount += 1;

        for entry in glob_with(pattern, options).expect("Failed to read glob pattern") {
            if !hasAnalysed {
                match entry {
                    Ok(path) => {
                        let input_file = File::open(path.clone()).unwrap();

                        println!("Analysing path: {}", path.to_str().unwrap());

                        // let mut buf_reader = BufReader::new(input_file);

                        // skipprMetadata =
                        //     AnalyseSchema::infer_json_schema(&mut foo, &mut buf_reader, Some(1000))
                        //         .unwrap();
                        skipprMetadata = AnalyseSchema::infer_json_schema(
                            &mut foo,
                            input_file,
                            Some(1000),
                            &mut skipprMetadata,
                        )
                        .unwrap();

                        // println!("Skippr schema: {:?}", skipprMetadata);

                        // arrowSchema = convert_skippr_to_arrow(
                        //     skipprMetadata.get(&"example_ns".to_string()).unwrap().fields.clone(),
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

                        hasAnalysed = true;
                    }
                    Err(e) => println!("{:?}", e),
                }
            }
        }
    }

    AnalyseSchema::determine_field_types(&mut skipprMetadata, None);

    Config::set_config(&skipprMetadata, false).await;

    // let file = OpenOptions::new()
    //     .create(true)
    //     .write(true)
    //     .truncate(true)
    //     .open(&"metadata.json".to_string())
    //     .unwrap();
    //
    // let writer = BufWriter::new(file);
    //
    // serde_json::to_writer(writer, &skipprMetadata).unwrap();

    // skipprMetadata
    // }).join().unwrap();
}

async fn sync() {
    // let default_messages = Arc::new(Mutex::new(HashMap::new()));

    // let emptyMeta = Metadata::new().unwrap();
    // let mut metadata = HashMap::new();
    // metadata.insert("example_ns".to_string(), emptyMeta);
    // Config::set_config(&metadata, true).await;
    // exit(0);

    // let mut skippr_metadata = Arc::new(Mutex::new(HashMap::new()));

    let data_dir = Config::get_data_dir();

    let _metadata_file = format!("{}/metadata.json", data_dir);

    // let skippr_metadata = Arc::new(Mutex::new(match File::open(metadata_file.clone()) {
    let skippr_metadata = Arc::new(Mutex::new(match Config::get_config().await {
        // let mut skippr_metadata: HashMap<String, Metadata> = match File::open(metadata_file.clone()) {
        Ok(metadata) => {
            println!("Found Skippr metadata");

            // let reader = BufReader::new(schema_file);

            // let u = serde_json::from_reader(reader).unwrap();
            let u: HashMap<String, Metadata> = metadata;

            u
        }
        Err(_e) => {
            println!("Could not find Skippr metadata, will disover and evolve schemas as we sync.");
            let _empty_meta = Metadata::new().unwrap();

            
            // metadata.insert("example_ns".to_string(), empty_meta);
            // let skippr_metadata: HashMap<String, Metadata> = metadata;
            // skippr_metadata
            HashMap::new()

            // exit(1);
            // discover();
            //
            // let file = File::open("metadata.json").unwrap();
            // let reader = BufReader::new(file);
            //
            // let u = serde_json::from_reader(reader).unwrap();
            //
            // u
        }
    }));

    let _newmeta_clone = skippr_metadata.clone();

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    ctrlc::set_handler(move || {
        if r.load(Ordering::SeqCst) {
            println!("Received Ctrl+C: Gracefully shutting down");
            r.store(false, Ordering::SeqCst);

            // Config::set_config(&newmeta_clone.lock().unwrap(),true);

            // println!("Flushing ingest buffers");
            // // @todo - implemnt Ingest{} build glob for existing files
            // Ingest::flush_buffers(true, output_files);
            // println!("Flushing output buffers");
            // outputSync(_newmeta_clone.lock().unwrap().clone());
        } else {
            println!("Received another Ctrl+C signal - no worries, terminating immediately...");
            std::process::exit(0);
        }
    })
    .expect("Error during graceful shutdown");

    while running.load(Ordering::SeqCst) {
        let now = Arc::new(Mutex::new(Instant::now()));

        let metrics: Arc<Mutex<Metrics>> = Arc::new(Mutex::new(Metrics::new()));

        // let mut pool = ThreadPool::new(4, skippr_metadata.clone());

        use std::time::Duration;

        let mut planner = periodic::Planner::new();

        let metricsClone = metrics.clone();

        planner.add(
            move || {
                // let mut metrics: Metrics = Metrics::new();
                let mut metrics_lock = metricsClone.lock().unwrap();

                // let mut counter_lock = ingestMsgCount.lock().unwrap();
                // let mut total_lock = ingestMsgTotal.lock().unwrap();
                let now_lock = now.lock().unwrap();

                metrics_lock.msgs_total += metrics_lock.msgs_current;
                metrics_lock.bytes_total += metrics_lock.bytes_current;

                // metrics.msgs_total = *total_lock;
                // metrics.msgs_current = *counter_lock;
                metrics_lock.run_time_seconds = now_lock.elapsed().as_secs();

                println!("Runtime: {} seconds", now_lock.elapsed().as_secs());
                println!("Deadletters Messages: {}", metrics_lock.deadletters_current);
                println!("Ingested Messages: {}", metrics_lock.msgs_current);
                println!("Total Messages: {}", metrics_lock.msgs_total);
                println!("Bytes: {}", metrics_lock.bytes_total);

                metrics_lock.msgs_current = 0;

                Config::set_status(metrics_lock, None);
            },
            periodic::Every::new(Duration::from_secs(60)),
        );
        planner.start();

        let inputMetadataClone = skippr_metadata.clone();

        let mut out_pnanner = periodic::Planner::new();
        out_pnanner.add(
            move || {
                outputSync(inputMetadataClone.lock().unwrap().clone());
                // let dataOutput = block_on(DataOutputAwsAthenaPlugin::new());
                // dataOutput.sync(inputMetadataClone.lock().unwrap().clone()).await;
            },
            periodic::Every::new(Duration::from_secs(60)),
        );
        out_pnanner.start();

        let inputMetadataClone = skippr_metadata.clone();

        // @todo - share across s3 ingests
        let _parse_namespace_cache: HashMap<String, String> = HashMap::new();
        let _output_files: HashMap<String, File> = HashMap::new();
        let _options = MatchOptions {
            case_sensitive: false,
            require_literal_separator: false,
            require_literal_leading_dot: false,
        };
        let data_dir = Config::get_data_dir();
        let output_dir = &format!("{}/output", data_dir);
        let finalised_dir = &format!("{}/finalised", data_dir);
        match fs::create_dir(output_dir) {
            Ok(_g) => {}
            Err(_err) => {}
        }
        match fs::create_dir(format!("{}/done", output_dir)) {
            Ok(_g) => {}
            Err(_err) => {}
        }
        match fs::create_dir(finalised_dir) {
            Ok(_g) => {}
            Err(_err) => {}
        }

        let metricsClone = metrics.clone();

        match Config::getenv("DATA_SOURCE_PLUGIN_NAME", "").as_str() {
            "s3" => {
                let mut ds3 = block_on(DataSourceS3Plugin::new());
                ds3.sync(
                    // &m1ut pool,
                    inputMetadataClone,
                    metricsClone,
                )
                .await;
            }
            "s3_inventory" => {
                let mut ds3 = block_on(DataSourceS3InventoryPlugin::new());
                ds3.sync(
                    // &mut pool,
                    inputMetadataClone,
                    metricsClone,
                )
                .await;
            }
            unknown => {
                println!("Plugin {} not supported", unknown);
            }
        };
    }
}

fn outputSync(metadata: HashMap<String, Metadata>) {
    thread::spawn(move || {
        // println!("Arrow Schema: {:?}", arrowSchema);

        let data_dir = Config::get_data_dir();
        let output_dir = &format!("{}/output", data_dir);

        let options = MatchOptions {
            case_sensitive: false,
            require_literal_separator: false,
            require_literal_leading_dot: false,
        };

        // loop {
        for entry in glob_with(&format!("{}/done/*", output_dir), options)
            .expect("Failed to read glob pattern")
        {
            match entry {
                Ok(path) => {
                    // println!("Finalising output file {}", path.display());

                    // alwasy regenerate arrow schema incase updated skippr metadata, e.g. discovered a new field
                    let mut arrowSchema: Result<Schema, ArrowError> = Ok(Schema::empty());
                    let mut schema_ref = Arc::new(Schema::empty());

                    let skpr_namespace =
                        BufferChunker::decode_file_namespace(path.to_str().unwrap());
                    // let skpr_partition = BufferChunker::decode_file_partition(path.to_str().unwrap());

                    // let mut skpr_namespace: String = "".to_string();
                    // if let Some((a, b)) = path.display().to_string().split_once("done/") {
                    //     if let Some((hash, namespace_part)) = b.to_string().split_once("-") {
                    //         skpr_namespace = namespace_part.to_string()
                    //     }
                    // }

                    // if metadata.get(&skpr_namespace).is_none() {
                    //     metadata.insert(skpr_namespace.clone(), Metadata::new().unwrap());
                    // }

                    // println!("getting schema: {} from file: {}", skpr_namespace, path.to_str().unwrap());

                    arrowSchema = convert_skippr_to_arrow(
                        metadata.get(&skpr_namespace).unwrap().fields.clone(),
                    );

                    schema_ref = Arc::new(arrowSchema.unwrap());

                    schema_ref = SerdeParquet::serialize(path.clone(), schema_ref);

                    match std::fs::remove_file(path) {
                        Ok(_t) => {}
                        Err(err) => println!("{:?}", err),
                    }
                }
                Err(e) => println!("{:?}", e),
            }
        }

        // sleep(Duration::from_secs(1));
        // }
    });
}
