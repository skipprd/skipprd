use std::collections::HashMap;
use std::{fs, thread};
use std::fs::{File, OpenOptions};
use std::io::{IoSlice, Write};
use std::ops::Deref;
use std::path::{PathBuf};
use std::sync::{Arc, Mutex};
use glob::MatchOptions;
use lazy_static::lazy_static;
use once_cell::sync::Lazy;
use serde_json::Value;
use crate::buffer::BufferChunker;
use crate::discover::Metadata;
use crate::helpers::configuration::{Config, Metrics};
use crate::helpers::Helpers;
use crate::ingest::ingest_fast::fast_path_ingest;
use crate::serdes::json::SerdeJson;
// use crate::thread_pool::ThreadPool;


static parse_namespace_cache: Lazy<Mutex<HashMap<String, String>>> = Lazy::new(|| Mutex::new(HashMap::new()));
static output_files_static: Lazy<Mutex<HashMap<String, File>>> = Lazy::new(|| Mutex::new(HashMap::new()));



// lazy_static! {
    // static ref ARRAY: Mutex<Vec<u8>> = Mutex::new(vec![]);
    // static ref parse_namespace_cache: HashMap<String, String> = HashMap::new();
// }

pub fn ingest_file(
    datas: &Arc<Mutex<Vec<String>>>,
    // pool: &ThreadPool,
    metadata: &Arc<Mutex<HashMap<String, Metadata>>>,
    metrics: &Arc<Mutex<Metrics>>
    // input_file: &mut File
    // , path: PathBuf
) {

    let data_dir = Config::get_data_dir();
    let output_dir = format!("{}/output", data_dir);

    let mut updatedSchema: Arc<Mutex<String>> = Arc::new(Mutex::new("no".to_string()));

    let mut updatedSchemaClone = updatedSchema.clone();
    let mut metadataClone = metadata.clone();
    let mut metrcisClone = metrics.clone();
    let mut datasClone = datas.clone();

    let mut i = 0;

    // println!("ingest 1");
    thread::spawn(move || {
        println!("Ingesting");
        // println!("ingest 2");
        for str in datasClone.lock().unwrap().clone() {

            let mut output_files = &mut output_files_static.lock().unwrap();

            let mut buf_str: String = String::new();

            let records: Vec<Value> = SerdeJson::deserialize(&str);

            for record in records {
                if record.is_null() {
                    continue;
                }

                let skpr_namespace = Helpers::parse_namespace_field(&record, Config::get_pipeline_name(), &mut parse_namespace_cache.lock().unwrap());
                let skpr_partition = Helpers::parse_partition_field(&record);
                let skpr_time = Helpers::parse_time_field(&record);

                let mut skpr_time_bucket = 0;

                if skpr_time.is_some() {
                    skpr_time_bucket = BufferChunker::event_time_bucket(skpr_time.unwrap());
                }

                let output_file_name = BufferChunker::encode_chunk_name("ingest", Some(&skpr_namespace), Some(&skpr_partition), Some(skpr_time_bucket));
                let output_file = format!("{}/{}", output_dir, &output_file_name);

                if output_files.get_mut(&output_file_name).is_none() {
                    let f = OpenOptions::new()
                        .create(true)
                        // .write(true)
                        .append(true)
                        .open(output_file)
                        .unwrap();

                    output_files.insert(output_file_name.clone(), f);
                }


                let mut meta = metadataClone.lock().unwrap();
                if meta.get(&skpr_namespace).is_none() {
                    meta.insert(skpr_namespace.clone(), Metadata::new().unwrap());
                }

                let msg = fast_path_ingest(
                    &record,
                    &mut meta.get_mut(&skpr_namespace).unwrap().fields,
                    &mut *updatedSchemaClone.lock().unwrap()
                );

                i += 1;

                buf_str = msg.to_string() + "\n";

                output_files.get_mut(&output_file_name).unwrap().write(&buf_str.as_bytes());

                if output_files.get(&output_file_name).unwrap().metadata().unwrap().len() > 1024 * 1024 * 10 {
                    fs::rename(
                        format!("{}/{}", output_dir, &output_file_name),
                        format!("{}/done/{}-{}", output_dir, &Helpers::random_str(12), &output_file_name),
                    ).unwrap();

                    output_files.remove(&output_file_name).unwrap();
                }
            }
        }

        let mut counter_lock = metrcisClone.lock().unwrap();

        counter_lock.msgs_current += i;
        // println!("ingest 3");


    // println!("ingest 4");

    if *updatedSchemaClone.lock().unwrap() == "yes".to_string() {

        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
        .block_on(async {
            Config::set_config(&*metadataClone.lock().unwrap(), *updatedSchemaClone.lock().unwrap() == "yes".to_string()).await;
        });

        // Config::set_config(&metadata.lock().unwrap(), updatedSchema == "yes".to_string()).await;


        *updatedSchemaClone.lock().unwrap() = "no".to_string();
    }
        println!("Ingested");
    });


    // @todo - write buf_str to file buffer and commit offsets
}
