mod arr;

use arrow::datatypes::{Schema, SchemaRef};
use arrow::error::ArrowError;
use arrow::json::ReaderBuilder;
use arrow::record_batch::{RecordBatch, RecordBatchOptions};
use std::any::Any;
use std::borrow::BorrowMut;
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::fs::{create_dir, metadata, File, OpenOptions};
use std::io::prelude::*;
use std::io::{BufReader, BufWriter, IoSlice};
use std::ops::{Add, Sub};
use std::path::PathBuf;
use std::process::exit;
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::thread::sleep;
use std::time::{Duration, Instant};
use std::{fs, io};

use flate2::read::GzDecoder;
use futures::executor::block_on;
use glob::glob_with;
use glob::MatchOptions;

mod helpers;

mod metrics;

mod discover;
use crate::discover::AnalyseSchema;
use crate::discover::Metadata;
// mod converters;
// use self::converters::avro_parquet::AvroSchema;
mod cli;
use crate::cli::{Cli, Mode};

extern crate clap;
use clap::{Parser, Subcommand};

use parquet::arrow::ArrowWriter;

mod ingest;
use crate::ingest::ingest_fast::{fast_path_ingest, fast_path_ingest_buf, IngestRecord};

mod serdes;
use crate::serdes::json::SerdeJson;
use crate::serdes::parquet::SerdeParquet;

mod plugins;
use crate::plugins::s3_inventory::DataSourceS3InventoryPlugin;

// use crate::helpers::Config

use crate::discover::arrow_schema::convert_skippr_to_arrow;
use crate::helpers::configuration::{Config, Metrics};
use crate::helpers::Helpers;
use serde_json::Value;
use tokio::fs::remove_file;

fn main() {
    Config::init();

    // let now = Instant::now();


    let cli = Cli::parse();

    match cli.mode {
        Mode::Sync => {
            println!("Command sync.");
            sync();
        }
        Mode::Discover => {
            println!("Command discover.");
            discover();
        }
    }

    // for x in 1..20000 {
    //     untyped_example();
    // }
    // blah();
}


#[tokio::main]
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

    // let mut newMeta: HashMap<String, Metadata> = HashMap::new();

    // Get existing metadata
    let mut newMeta: HashMap<String, Metadata> = match File::open("metadata.json") {
        Ok(file) => {
            let reader = BufReader::new(file);
            match serde_json::from_reader(reader) {
                Ok(metadata) => metadata,
                Err(e) => {
                    println!("No existing metadata {}", e);
                    HashMap::new()
                }
            }
        }
        Err(e) => {
            println!("No existing metadata {}", e);
            HashMap::new()
        }
    };

    let mut arrowSchema: Result<Schema, ArrowError> = Ok(Schema::empty());

    let mut schema_ref = Arc::new(Schema::empty());

    let mut analyseCount = 0;

    while !hasAnalysed && analyseCount < 10 {
        analyseCount += 1;

        for entry in glob_with("/tmp/ddd/s3-*", options).expect("Failed to read glob pattern") {
            if !hasAnalysed {
                match entry {
                    Ok(path) => {
                        let mut input_file = File::open(path.clone()).unwrap();

                        println!("Analysing path: {}", path.to_str().unwrap());

                        let mut buf_reader = BufReader::new(input_file);

                        newMeta =
                            AnalyseSchema::infer_json_schema(&mut foo, &mut buf_reader, Some(1000))
                                .unwrap();

                        // println!("Skippr schema: {:?}", newMeta);

                        arrowSchema = convert_skippr_to_arrow(
                            &mut newMeta.get_mut(&"example_ns".to_string()).unwrap().fields,
                        );

                        // let json = serde_json::to_string_pretty(&arrowSchema).unwrap();
                        // eprintln!("Schema:");
                        // println!("{}", json);

                        println!("Arrow schema: {:?}", arrowSchema);

                        // let schema_ref = Arc::new(arrowSchema.unwrap());
                        schema_ref = Arc::new(arrowSchema.unwrap());
                        // schema_ref = arrowSchema.unwrap();

                        hasAnalysed = true;
                    }
                    Err(e) => println!("{:?}", e),
                }
            }
        }

        sleep(Duration::from_secs(1));
    }

    Config::set_config(&newMeta, false).await;

    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(true)
        .open(&"metadata.json".to_string())
        .unwrap();

    let writer = BufWriter::new(file);

    serde_json::to_writer(writer, &newMeta).unwrap();

    // newMeta
    // }).join().unwrap();
}

#[tokio::main]
async fn sync() {
    // let default_messages = Arc::new(Mutex::new(HashMap::new()));

    let now = Arc::new(Mutex::new(Instant::now()));


    let ingestMsgTotal = Arc::new(Mutex::new(0));
    let ingestMsgCount = Arc::new(Mutex::new(0));
    let ingestMsgCountClone = ingestMsgCount.clone();

    let mut newMeta: HashMap<String, Metadata> = match File::open("metadata.json") {
        Ok(schema_file) => {
            println!("Found Skippr metadata");

            let file = File::open("metadata.json").unwrap();
            let reader = BufReader::new(file);

            let u = serde_json::from_reader(reader).unwrap();

            u
        }
        Err(e) => {
            println!("Could not find Skippr metadata, perhaps run `skippr discover`?");
            exit(1);
        }
    };

    // let mut newMeta: HashMap<String, Metadata> = match File::open("metadata.json") {
    //     Ok(schema_file) => {
    //
    //         println!("Found Skippr metadata");
    //
    //         let file = File::open("metadata.json").unwrap();
    //         let reader = BufReader::new(file);
    //
    //         let u = serde_json::from_reader(reader).unwrap();
    //
    //         u
    //     },
    //     Err(e) => {
    //
    //         println!("Analysing data and generating Skippr metadata");
    //
    //         let analyseThread = thread::spawn(move || {
    //             let options = MatchOptions {
    //                 case_sensitive: false,
    //                 require_literal_separator: false,
    //                 require_literal_leading_dot: false,
    //             };
    //
    //             let mut foo: AnalyseSchema = AnalyseSchema { i: 0 };
    //
    //             let mut hasAnalysed = false;
    //
    //             let mut newMeta: HashMap<String, Metadata> = HashMap::new();
    //
    //             let mut arrowSchema: Result<Schema, ArrowError> = Ok(Schema::empty());
    //
    //             let mut schema_ref = Arc::new(Schema::empty());
    //
    //             let mut analyseCount = 0;
    //
    //             while !hasAnalysed && analyseCount < 10 {
    //
    //                 analyseCount += 1;
    //
    //                 for entry in glob_with("/tmp/ddd/s3-*", options).expect("Failed to read glob pattern") {
    //                     if !hasAnalysed {
    //                         match entry {
    //                             Ok(path) => {
    //                                 let mut input_file = File::open(path.clone()).unwrap();
    //
    //                                 println!("Anakysing path: {}", path.to_str().unwrap());
    //
    //                                 let mut buf_reader = BufReader::new(input_file);
    //
    //                                 newMeta = AnalyseSchema::infer_json_schema(&mut foo, &mut buf_reader, Some(3)).unwrap();
    //
    //                                 // println!("Skippr schema: {:?}", newMeta);
    //
    //                                 arrowSchema = convert_skippr_to_arrow(&mut newMeta.get_mut(&"example_ns".to_string()).unwrap().fields);
    //
    //                                 // let json = serde_json::to_string_pretty(&arrowSchema).unwrap();
    //                                 // eprintln!("Schema:");
    //                                 // println!("{}", json);
    //
    //                                 println!("Arrow schema: {:?}", arrowSchema);
    //
    //                                 // let schema_ref = Arc::new(arrowSchema.unwrap());
    //                                 schema_ref = Arc::new(arrowSchema.unwrap());
    //                                 // schema_ref = arrowSchema.unwrap();
    //
    //                                 hasAnalysed = true;
    //                             },
    //                             Err(e) => println!("{:?}", e),
    //                         }
    //                     }
    //                 }
    //
    //                 sleep(Duration::from_secs(1));
    //             }
    //
    //
    //             let file = OpenOptions::new()
    //                 .create(true)
    //                 .write(true)
    //                 .append(true)
    //                 .open(&"metadata.json".to_string())
    //                 .unwrap();
    //
    //             let writer = BufWriter::new(file);
    //
    //             let u = serde_json::to_writer(writer, &newMeta);
    //
    //             newMeta
    //         });
    //
    //         analyseThread.join().unwrap()
    //     }
    // };

    let mut newMetaThread2 = newMeta.clone();

    use std::time::Duration;

    let mut planner = periodic::Planner::new();

    planner.add(
        move || {
            let mut metrics: Metrics = Metrics::new();


            let mut counter_lock = ingestMsgCount.lock().unwrap();
            let mut total_lock = ingestMsgTotal.lock().unwrap();
            let now_lock = now.lock().unwrap();

            *total_lock += *counter_lock;

            // metrics.msgs_total = total_lock.clone();
            // metrics.msgs_current = counter_lock.clone();
            // metrics.run_time_seconds = now_lock.elapsed().as_secs().clone() as i64;
            // Config::set_status(&metrics, None);

            println!("Runtime: {} seconds", now_lock.elapsed().as_secs());
            println!("Ingested Messages: {}", *counter_lock);
            println!("Total Messages: {}", *total_lock);

            *counter_lock = 0;


        },
        periodic::Every::new(Duration::from_secs(60)),
    );
    planner.start();

    thread::spawn(move || {
        let options = MatchOptions {
            case_sensitive: false,
            require_literal_separator: false,
            require_literal_leading_dot: false,
        };

        let mut output_files: HashMap<String, File> = HashMap::new();
        let mut output_buf: HashMap<String, IoSlice> = HashMap::new();

        let mut write_len: usize = 0;

        while true {
            for entry in glob_with("/tmp/ddd/s3-*", options).expect("Failed to read glob pattern") {
                match entry {
                    Ok(path) => {
                        // println!("{}", path.display());

                        let mut input_file = File::open(path.clone()).unwrap();

                        let mut buf_reader = BufReader::new(input_file);

                        let value_iter = fast_path_ingest_buf(&mut buf_reader);

                        for record in value_iter {
                            // println!("record: {:?}", &record.unwrap());

                            match record {
                                Ok(record) => {
                                    // let mut ingest_record = IngestRecord {
                                    //     source_namespace: "".to_string(),
                                    //     source_partition: "".to_string(),
                                    //     skpr_event_ts: 0,
                                    //     skpr_namespace: "example_ns".to_string(),
                                    //     skpr_partition: "".to_string(),
                                    //     record: Value::Null,
                                    // };

                                    if output_files.get_mut(&"example_ns".to_string()).is_none() {
                                        let f = OpenOptions::new()
                                            .create(true)
                                            .write(true)
                                            .append(true)
                                            .open(&"output/example_ns".to_string())
                                            .unwrap();

                                        output_files.insert("example_ns".to_string(), f);
                                    }

                                    let msg = fast_path_ingest(
                                        &record,
                                        &mut newMeta
                                            .get_mut(&"example_ns".to_string())
                                            .unwrap()
                                            .fields,
                                    );

                                    let mut counter_lock = ingestMsgCountClone.lock().unwrap();

                                    *counter_lock += 1;

                                    // let msg = record;
                                    // println!("{:?}", msg);

                                    let buf_str = msg.to_string() + "\n";

                                    write_len += output_files
                                        .get_mut(&"example_ns".to_string())
                                        .unwrap()
                                        .write(&buf_str.as_bytes())
                                        .unwrap();

                                    if write_len > 1024 * 1024 * 100 {
                                        write_len = 0;

                                        // output_files.get(&"example_ns".to_string()).unwrap().flush();
                                        output_files.remove(&"example_ns".to_string()).unwrap(); // close

                                        fs::rename(
                                            &"output/example_ns".to_string(),
                                            "output/example_ns_done_".to_string()
                                                + &Helpers::random_str(12),
                                        )
                                            .unwrap();
                                    }
                                }
                                Err(error) => println!("Error in record: {:?}", error),
                            }
                        }

                        std::fs::remove_file(path).unwrap();
                    }
                    Err(e) => println!("{:?}", e),
                }

                // for (k, mut file) in &output_files {
                // println!("Flushing file");
                // file.flush().unwrap();
                // }
            }
            sleep(Duration::from_secs(1));
        }
    });

    thread::spawn(move || {
        let file = File::open("metadata.json").unwrap();
        let reader = BufReader::new(file);

        let mut newMeta: HashMap<String, Metadata> = serde_json::from_reader(reader).unwrap();

        // let mut iterCount = 0;
        //
        // while newMetaThread2.get_mut(&"example_ns".to_string()).is_none() && iterCount < 10 {
        //     iterCount += 1;
        //     println!("waiting for metatdata {}", iterCount);
        //     sleep(Duration::from_secs(1));
        // }

        let mut arrowSchema: Result<Schema, ArrowError> = Ok(Schema::empty());

        let mut schema_ref = Arc::new(Schema::empty());

        // arrowSchema = convert_skippr_to_arrow(&mut newMetaThread2.get_mut(&"example_ns".to_string()).unwrap().fields);
        arrowSchema = convert_skippr_to_arrow(
            &mut newMetaThread2
                .get_mut(&"example_ns".to_string())
                .unwrap()
                .fields,
        );

        println!("Arrow Schema: {:?}", arrowSchema);

        schema_ref = Arc::new(arrowSchema.unwrap());

        let options = MatchOptions {
            case_sensitive: false,
            require_literal_separator: false,
            require_literal_leading_dot: false,
        };

        while true {
            for entry in glob_with("output/*done*", options).expect("Failed to read glob pattern") {
                match entry {
                    Ok(path) => {
                        // println!("Finalising output file {}", path.display());

                        schema_ref = SerdeParquet::serialize(path.clone(), schema_ref);

                        std::fs::remove_file(path).unwrap();
                    }
                    Err(e) => println!("{:?}", e),
                }
            }
            sleep(Duration::from_secs(1));
        }
    });
    // .join()
    // .expect("Buffer thread failed");

    let mut ds3 = block_on(DataSourceS3InventoryPlugin::new());

    ds3.sync().await;
    // sleep(Duration::from_secs(60));

    // let now_lock = now.lock().unwrap();
    //
    // println!("Runtime: {} seconds", now_lock.elapsed().as_secs());

}
