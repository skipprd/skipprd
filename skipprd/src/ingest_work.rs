use std::collections::HashMap;
use std::{fs, thread};
use std::fs::{File, OpenOptions};
use std::io::{Write};
use std::sync::{Arc, Mutex};
use once_cell::sync::Lazy;
use serde_json::Value;
use crate::buffer::BufferChunker;
use crate::discover::Metadata;
use crate::helpers::configuration::{Config, Metrics};
use crate::helpers::Helpers;
use crate::helpers::offsets::{OffsetKey, Offsets, OffsetTypes};
use crate::ingest::ingest_fast::fast_path_ingest;
use crate::serdes::json::SerdeJson;



#[derive(Clone, Debug)]
pub struct IngestBatch {
    pub(crate) offset_key: OffsetKey,
    pub(crate) data: String
}

static parse_namespace_cache: Lazy<Mutex<HashMap<String, String>>> = Lazy::new(|| Mutex::new(HashMap::new()));
static output_files_static: Lazy<Mutex<HashMap<String, File>>> = Lazy::new(|| Mutex::new(HashMap::new()));


pub struct Ingest {
}


impl Ingest {

    pub fn new() -> Ingest{
        Ingest {
        }
    }

    pub fn ingest_file(
        datas: Vec<IngestBatch>,
        metadata: &Arc<Mutex<HashMap<String, Metadata>>>,
        metrics: &Arc<Mutex<Metrics>>,
        offset_db: &Arc<Offsets>,
    ) {
        let data_dir = Config::get_data_dir();
        let output_dir = format!("{}/output", data_dir);

        let mut updated_schema: Arc<Mutex<String>> = Arc::new(Mutex::new("no".to_string()));

        let mut updated_schema_clone = updated_schema.clone();
        let mut metadata_clone = metadata.clone();
        let mut metrcis_clone = metrics.clone();
        let offset_db_clone = offset_db.clone();

        let mut bytes: u64 = 0;

        thread::spawn(move || {
            for ingest_batch in datas {

                // have offsets, don't bother checking each line offset if not.
                // relevant when processing a new file, which is most of the time
                let has_offsets = offset_db_clone.validate(&ingest_batch.offset_key, OffsetTypes::Closed, 0);

                let mut i = 1;

                let mut output_files = &mut output_files_static.lock().unwrap();

                let mut buf_str: String = String::new();

                let records: Vec<Value> = SerdeJson::deserialize(&ingest_batch.data);

                for record in records {

                    if record.is_null() {
                        continue;
                    }

                    if None == has_offsets || Some(false) != offset_db_clone.validate(&ingest_batch.offset_key, OffsetTypes::Line, i) {

                        let usize = serde_json::to_vec(&record).unwrap().len();
                        let _bytes: u64 = usize.try_into().unwrap();
                        bytes += _bytes;

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
                                .write(true)
                                .append(true)
                                .open(output_file)
                                .unwrap();

                            output_files.insert(output_file_name.clone(), f);
                        }


                        let mut meta = metadata_clone.lock().unwrap();
                        if meta.get(&skpr_namespace).is_none() {
                            meta.insert(skpr_namespace.clone(), Metadata::new().unwrap());
                        }

                        let msg = fast_path_ingest(
                            &record,
                            &mut meta.get_mut(&skpr_namespace).unwrap().fields,
                            &mut updated_schema_clone.lock().unwrap()
                        );

                        buf_str = msg.to_string() + "\n";

                        output_files.get_mut(&output_file_name).unwrap().write(&buf_str.as_bytes());

                        offset_db_clone.set(&ingest_batch.offset_key, OffsetTypes::Line, i);

                        if output_files.get(&output_file_name).unwrap().metadata().unwrap().len() > 1024 * 1024 * 10 {

                            fs::rename(
                                format!("{}/{}", output_dir, &output_file_name),
                                format!("{}/done/{}-{}", output_dir, &Helpers::random_str(12), &output_file_name),
                            ).unwrap();

                            output_files.remove(&output_file_name).unwrap();
                        }
                    }

                    i += 1;

                }

                offset_db_clone.set(&ingest_batch.offset_key, OffsetTypes::Closed, 1);

                let mut counter_lock = metrcis_clone.lock().unwrap();

                counter_lock.msgs_current += i;
                counter_lock.bytes_current += bytes;

            }

            if *updated_schema_clone.lock().unwrap() == "yes".to_string() {
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        Config::set_config(&*metadata_clone.lock().unwrap(), *updated_schema_clone.lock().unwrap() == "yes".to_string()).await;
                    });

                *updated_schema_clone.lock().unwrap() = "no".to_string();
            }
        });

    }

}