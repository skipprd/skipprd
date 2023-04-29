mod arr;

use arrow::datatypes::{Schema};
use arrow::error::ArrowError;


// mod thread_pool;
// use thread_pool::ThreadPool;
mod ingest_work;


use std::collections::{HashMap};

use std::fs::{File, OpenOptions};
use std::io::prelude::*;
use std::io::{BufReader, IoSlice};
use std::ops::{Add};


use std::sync::{Arc, Mutex};
use std::thread;
use std::thread::sleep;
use std::time::{Duration, Instant};
use std::{fs};


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
use clap::{Parser};





mod ingest;
use crate::ingest::ingest_fast::{fast_path_ingest};

mod serdes;
use crate::serdes::json::SerdeJson;
use crate::serdes::parquet::SerdeParquet;

mod plugins;
use crate::plugins::athena::{DataOutputAwsAthenaPlugin};


// use crate::helpers::Config

use crate::discover::arrow_schema::convert_skippr_to_arrow;
use crate::helpers::configuration::{Config, Metrics};
use crate::helpers::Helpers;
use serde_json::Value;

use crate::buffer::BufferChunker;
use crate::plugins::s3_inventory::DataSourceS3InventoryPlugin;

fn main() {
    Config::init();

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
            sync();
        }
        Mode::Discover => {
            // println!("Command discover");
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

    // let mut skipprMetadata: HashMap<String, Metadata> = HashMap::new();

    let data_dir= Config::get_data_dir();
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

    let data_dir= Config::get_data_dir();
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
                        skipprMetadata =
                            AnalyseSchema::infer_json_schema(&mut foo, input_file, Some(1000), &mut skipprMetadata)
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

#[tokio::main]
async fn sync() {
    // let default_messages = Arc::new(Mutex::new(HashMap::new()));

    // let emptyMeta = Metadata::new().unwrap();
    // let mut metadata = HashMap::new();
    // metadata.insert("example_ns".to_string(), emptyMeta);
    // Config::set_config(&metadata, true).await;
    // exit(0);

    let now = Arc::new(Mutex::new(Instant::now()));

    // let ingestMsgTotal = Arc::new(Mutex::new(0));
    // let ingestMsgCount = Arc::new(Mutex::new(0));
    // let ingestMsgCountClone = ingestMsgCount.clone();
    let mut metrics: Arc<Mutex<Metrics>> = Arc::new(Mutex::new(Metrics::new()));

    let data_dir= Config::get_data_dir();
    let metadata_file = format!("{}/metadata.json", data_dir);

    // let mut skipprMetadata = Arc::new(Mutex::new(HashMap::new()));

    let mut skipprMetadata = Arc::new(Mutex::new(match File::open(metadata_file.clone()) {
    // let mut skipprMetadata: HashMap<String, Metadata> = match File::open(metadata_file.clone()) {
        Ok(schema_file) => {
            println!("Found Skippr metadata");

            // let file = File::open("metadata.json").unwrap();
            let reader = BufReader::new(schema_file);

            let u = serde_json::from_reader(reader).unwrap();

            u
        }
        Err(_e) => {
            println!("Could not find Skippr metadata, will disover and evolve schemas as we sync.");
            let _emptyMeta = Metadata::new().unwrap();

            let metadata = HashMap::new();
            // metadata.insert("example_ns".to_string(), emptyMeta);
            // let skipprMetadata: HashMap<String, Metadata> = metadata;
            // skipprMetadata
            metadata


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

    // let mut pool = ThreadPool::new(4, skipprMetadata.clone());

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

            // metrics.msgs_total = *total_lock;
            // metrics.msgs_current = *counter_lock;
            metrics_lock.run_time_seconds = now_lock.elapsed().as_secs().clone() as u64;
            Config::set_status(&metrics_lock, None);

            println!("Runtime: {} seconds", now_lock.elapsed().as_secs());
            println!("Deadletters Messages: {}", metrics_lock.deadletters_current);
            println!("Ingested Messages: {}", metrics_lock.msgs_current);
            println!("Total Messages: {}", metrics_lock.msgs_total);

            metrics_lock.msgs_current = 0;

        },
        periodic::Every::new(Duration::from_secs(60)),
    );
    planner.start();


    let inputMetadataClone = skipprMetadata.clone();

    let mut out_pnanner = periodic::Planner::new();
    out_pnanner.add( move || {
        outputSync(inputMetadataClone.lock().unwrap().clone());
        // let dataOutput = block_on(DataOutputAwsAthenaPlugin::new());
        // dataOutput.sync(inputMetadataClone.lock().unwrap().clone()).await;
    },periodic::Every::new(Duration::from_secs(60))
    );
    out_pnanner.start();



    let inputMetadataClone = skipprMetadata.clone();

    // @todo - share across s3 ingests
    let mut parse_namespace_cache: HashMap<String, String> = HashMap::new();
    let mut output_files: HashMap<String, File> = HashMap::new();
    let options = MatchOptions {
        case_sensitive: false,
        require_literal_separator: false,
        require_literal_leading_dot: false,
    };
    let data_dir= Config::get_data_dir();
    let output_dir = &format!("{}/output", data_dir);
    let finalised_dir = &format!("{}/finalised", data_dir);
    match fs::create_dir(output_dir) {
        Ok(_g) => {},
        Err(_err) => {}
    }
    match fs::create_dir(format!("{}/done", output_dir)) {
        Ok(_g) => {},
        Err(_err) => {}
    }
    match fs::create_dir(finalised_dir) {
        Ok(_g) => {},
        Err(_err) => {}
    }

    let metricsClone = metrics.clone();

    let mut ds3 = block_on(DataSourceS3InventoryPlugin::new());
    ds3.sync(
        // &mut pool,
        inputMetadataClone,
        metricsClone
    ).await;

    // let metedata_clone = skipprMetadata.clone();
    //
    // pool.execute(move || {
    //     let mut newMeta = metedata_clone.lock().unwrap();
    //
    // // let _foo = thread::spawn(move || {
    //
    //     let mut parse_namespace_cache: HashMap<String, String> = HashMap::new();
    //
    //     let options = MatchOptions {
    //         case_sensitive: false,
    //         require_literal_separator: false,
    //         require_literal_leading_dot: false,
    //     };
    //
    //     let mut output_files: HashMap<String, File> = HashMap::new();
    //     let _output_buf: HashMap<String, IoSlice> = HashMap::new();
    //
    //     let mut write_len: usize = 0;
    //
    //     let data_dir= Config::get_data_dir();
    //
    //     let output_dir = &format!("{}/output", data_dir);
    //     let finalised_dir = &format!("{}/finalised", data_dir);
    //
    //     match fs::create_dir(output_dir) {
    //         Ok(_g) => {},
    //         Err(_err) => {}
    //     }
    //     match fs::create_dir(format!("{}/done", output_dir)) {
    //         Ok(_g) => {},
    //         Err(_err) => {}
    //     }
    //     match fs::create_dir(finalised_dir) {
    //         Ok(_g) => {},
    //         Err(_err) => {}
    //     }
    //
    //     let pattern = format!("{}/source_buffer/*", data_dir);
    //
    //     let mut updatedSchema: String = "no".to_string();
    //
    //     loop {
    //
    //         for entry in glob_with(&pattern, options).expect("Failed to read glob pattern") {
    //
    //             match entry {
    //                 Ok(path) => {
    //
    //                     let mut input_file = File::open(path.clone()).unwrap();
    //
    //                     let str: &mut String = &mut "".to_string();
    //
    //                     input_file.read_to_string(str).unwrap();
    //
    //                     let records: Vec<Value> = SerdeJson::deserialize(str.clone());
    //
    //                         for record in records {
    //
    //                             if record.is_null() {
    //                                 continue;
    //                             }
    //
    //                             // let source_namespace = Config::getenv("S3_BUCKET", "");
    //                             let source_namespace = BufferChunker::decode_file_namespace(path.to_str().unwrap());
    //                             let source_partition = BufferChunker::decode_file_partition(path.to_str().unwrap());
    //
    //                             let skpr_namespace = Helpers::parse_namespace_field(&record, source_namespace, &mut parse_namespace_cache);
    //                             let skpr_partition = Helpers::parse_partition_field(&record);
    //                             let skpr_time = Helpers::parse_time_field(&record);
    //
    //                             let mut skpr_time_bucket= 0;
    //
    //                             if skpr_time.is_some() {
    //                                 skpr_time_bucket = BufferChunker::event_time_bucket(skpr_time.unwrap());
    //                             }
    //
    //                             if newMeta.get(&skpr_namespace).is_none() {
    //                                 newMeta.insert(skpr_namespace.clone(), Metadata::new().unwrap());
    //                             }
    //
    //                             let output_file_name = BufferChunker::encode_chunk_name("ingest", Some(&skpr_namespace), Some(&skpr_partition), Some(skpr_time_bucket));
    //                             let output_file = format!("{}/{}", output_dir, &output_file_name);
    //
    //                                 if output_files.get_mut(&output_file_name).is_none() {
    //
    //                                     let f = OpenOptions::new()
    //                                         .create(true)
    //                                         .write(true)
    //                                         .append(true)
    //                                         .open(output_file)
    //                                         .unwrap();
    //
    //                                     write_len += f.metadata().unwrap().len() as usize;
    //                                     output_files.insert(output_file_name.clone(), f);
    //                                 }
    //
    //                                 let msg = fast_path_ingest(
    //                                     &record,
    //                                     &mut newMeta
    //                                         .get_mut(&skpr_namespace)
    //                                         .unwrap()
    //                                         .fields,
    //                                         &mut updatedSchema
    //                                 );
    //
    //                                 let mut counter_lock = ingestMsgCountClone.lock().unwrap();
    //
    //                                 *counter_lock += 1;
    //
    //                                 let buf_str = msg.to_string() + "\n";
    //
    //                                 write_len += output_files
    //                                     .get_mut(&output_file_name)
    //                                     .unwrap()
    //                                     .write(&buf_str.as_bytes())
    //                                     .unwrap();
    //
    //                                 if write_len > 1024 * 1024 * 10 {
    //                                     write_len = 0;
    //
    //                                     output_files.remove(&output_file_name).unwrap(); // close
    //
    //                                     fs::rename(
    //                                         format!("{}/{}", output_dir, &output_file_name),
    //                                         format!("{}/done/{}-{}", output_dir, &Helpers::random_str(12), &output_file_name),
    //                                     ).unwrap();
    //                                 }
    //                         }
    //
    //                     match std::fs::remove_file(path.clone()) {
    //                         Ok(_file) => { },
    //                         Err(err) => println!("Failed deleting file: {}", err),
    //                     }
    //
    //                     if updatedSchema == "yes".to_string() {
    //
    //                         tokio::runtime::Builder::new_multi_thread()
    //                             .enable_all()
    //                             .build()
    //                             .unwrap()
    //                             .block_on(async {
    //                                 Config::set_config(&newMeta, updatedSchema == "yes".to_string()).await;
    //                             });
    //
    //                         updatedSchema = "no".to_string();
    //                     }
    //
    //                 }
    //                 Err(e) => println!("{:?}", e),
    //             }
    //         }
    //         sleep(Duration::from_secs(1));
    //     }
    // });




    // let future2 = async move {

        // let mut ds3 = block_on(DataSourceS3InventoryPlugin::new());
        // ds3.sync().await;
    // };

    let newmeta_clone = skipprMetadata.clone();

    // pool.execute(move || {
    //     let mut newMeta = newmeta_clone.lock().unwrap();
    //
    // // let metadata_clone = newMeta.clone();
    // // let _bar = thread::spawn(move || {
    //     // while true {
    //         outputSync(newMeta.clone());
    //
    //     // }
    // });

    // let metedata_out_clone = skipprMetadata.clone();

    // let future2 = async move {
    // loop {
    //     outputSync(newmeta_clone.lock().unwrap().clone());
    //     let dataOutput = block_on(DataOutputAwsAthenaPlugin::new());
    //     dataOutput.sync(metedata_out_clone.lock().unwrap().clone()).await;
    //     sleep(Duration::from_secs(5));
    // }
    // };




    // sleep(Duration::from_secs(125));


    // let now_lock = now.lock().unwrap();
    //
    // println!("Runtime: {} seconds", now_lock.elapsed().as_secs());

}

fn outputSync(
    metadata: HashMap<String, Metadata>
) {
    thread::spawn(move || {

        // println!("Arrow Schema: {:?}", arrowSchema);

        let data_dir= Config::get_data_dir();
        let output_dir = &format!("{}/output", data_dir);

        let options = MatchOptions {
            case_sensitive: false,
            require_literal_separator: false,
            require_literal_leading_dot: false,
        };

        // loop {
            for entry in glob_with(&format!("{}/done/*", output_dir), options).expect("Failed to read glob pattern") {
                match entry {
                    Ok(path) => {
                        // println!("Finalising output file {}", path.display());

                        // alwasy regenerate arrow schema incase updated skippr metadata, e.g. discovered a new field
                        let mut arrowSchema: Result<Schema, ArrowError> = Ok(Schema::empty());
                        let mut schema_ref = Arc::new(Schema::empty());

                        let skpr_namespace = BufferChunker::decode_file_namespace(path.to_str().unwrap());
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
                            metadata
                                .get(&skpr_namespace)
                                .unwrap()
                                .fields.clone(),
                        );

                        schema_ref = Arc::new(arrowSchema.unwrap());


                        schema_ref = SerdeParquet::serialize(path.clone(), schema_ref);

                        match std::fs::remove_file(path) {
                            Ok(_t) => {},
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
