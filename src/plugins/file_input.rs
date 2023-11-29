
use std::fs::File;
use std::io::prelude::*;

use std::{fs};
use std::io::BufReader;
use std::path::Path;


use std::sync::{Arc};




use flate2::read::GzDecoder;
use tar::Archive;
use zip::ZipArchive;

use crate::helpers::configuration::{Config, PluginConfig};

use crate::helpers::offsets::{OffsetKey, OffsetTypes, Offsets};
use crate::ingest_work::{Ingest, IngestBatch};

use glob::{glob_with};

use futures::stream::StreamExt;

use serde_derive::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct DataSourceLocalFilePluginConfig {
    pub format: Option<String>,
    pub batch_size_seconds: Option<i64>,
    pub batch_size_bytes: Option<i64>,

    path: String,
}

impl From<PluginConfig> for DataSourceLocalFilePluginConfig {
    fn from(plugin_config: PluginConfig) -> Self {
        match plugin_config {
            PluginConfig::file(file_config) => file_config,
            _ => panic!("Invalid plugin type"),
        }
    }
}


pub struct DataSourceLocalFilePlugin {
    ingest: Ingest,
    temp_dir: String,
    config: DataSourceLocalFilePluginConfig,
}

impl DataSourceLocalFilePlugin {
    pub async fn new() -> DataSourceLocalFilePlugin {
        let data_dir = Config::get_data_dir();
        let temp_dir = &format!("{}/source_buffer", data_dir);

        match fs::create_dir(temp_dir) {
            Ok(_g) => {}
            Err(_err) => {}
        }

        let config: DataSourceLocalFilePluginConfig = match Config::get_pipline_plugin_config("input") {
            Ok(config) => config.into(),
            Err(_) => DataSourceLocalFilePluginConfig {
                format: None,
                batch_size_seconds: Some(Config::getenv("DATA_SOURCE_BATCH_SIZE_SECONDS", "600").parse::<i64>().unwrap()),
                batch_size_bytes: Some(Config::getenv("DATA_SOURCE_BATCH_SIZE_BYTES", "1024000").parse::<i64>().unwrap()),
                path: Config::getenv("DATA_SOURCE_PATH", ""),
            }
        };

        DataSourceLocalFilePlugin {
            ingest: Ingest::new(),
            temp_dir: temp_dir.to_string(),
            config
        }
    }

    pub async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
    ) {
        let offsets_clone = offsets.clone();

        let _file_path_pattern = format!("{}/**/*", self.config.path);

        let mut data_batches_stream = Box::pin(self.prepare_data_for_processing(
            &offsets_clone,
            self.config.path.clone(),
            self.config.batch_size_bytes.unwrap_or(1000000) as i64
        ));

        while let Some(batch) = data_batches_stream.next().await {

            let offsets_clone = offsets_clone.clone();

            // let ingest_handle = task::spawn_blocking(move || {

                // println!("Ingesting batch: {:?}", batch);
                self.ingest.ingest_file(batch, &offsets_clone);
            // });
            // ingest_handle.await.unwrap();

        }

    }

    pub fn prepare_data_for_processing(
        &self,
        offsets_clone: &Arc<Offsets>,
        source_dir: String,
        chunk_size: i64,
    ) -> impl futures::Stream<Item = Vec<IngestBatch>> {

        // let mut datas: Vec<IngestBatch> = Vec::new();

        let file_path_pattern = if Path::new(&source_dir).is_file() {
            source_dir.to_string()
        } else {
            format!("{}/**/*", &source_dir)
        };

        let (tx, rx) = futures::channel::mpsc::unbounded();

        let offsets_clone = offsets_clone.clone();

        let current_batch: Vec<IngestBatch> = Vec::new();
        let mut batch_bytes: i64 = 0;

        tokio::spawn(async move {

            let current_batch = &mut current_batch.clone();

            let options = glob::MatchOptions {
                case_sensitive: false,
                require_literal_separator: false,
                require_literal_leading_dot: false,
            };

            for entry in glob_with(&file_path_pattern, options).expect("Failed to read glob pattern") {
                match entry {
                    Ok(path) => {

                        let offset_key = OffsetKey {
                            namespace: source_dir.clone(),
                            partition: path.to_str().unwrap().to_string(),
                        };

                        let has_offsets = offsets_clone.validate(&offset_key, OffsetTypes::Closed, 1);

                        if Some(true) != has_offsets {
                            let file = match File::open(&path) {
                                Err(why) => {
                                    println!("couldn't open {}: {}", path.display(), why);
                                    continue;
                                },
                                Ok(file) => file,
                            };
                            let _file_content = String::new();
                            let file_ext = match path.extension() {
                                Some(ext) => match ext.to_str() {
                                    Some(ext) => ext,
                                    None => "",
                                },
                                None => "",
                            };

                            match file_ext {
                                "gz" => {
                                    let decoder = GzDecoder::new(file);
                                    let reader = BufReader::new(decoder);

                                    for line in reader.lines() {
                                        let line = line.unwrap();
                                        let line_len = line.len() as i64;

                                        if batch_bytes + line_len > chunk_size && !current_batch.is_empty() {
                                            tx.unbounded_send(current_batch.clone()).unwrap();
                                            current_batch.clear();
                                            batch_bytes = 0;
                                        }

                                        let ingest_data = format!("{}{}", if batch_bytes == 0 { "" } else { "\n" }, line);
                                        batch_bytes += ingest_data.len() as i64;

                                        current_batch.push(IngestBatch {
                                            offset_key: offset_key.clone(),
                                            data: ingest_data,
                                        });
                                    }

                                    if !current_batch.is_empty() {
                                        tx.unbounded_send(current_batch.clone()).unwrap();
                                        current_batch.clear();
                                        batch_bytes = 0;
                                    }
                                },
                                "tar" => {
                                    let mut archive = Archive::new(file);
                                    for entry in archive.entries().unwrap() {
                                        let file_entry = entry.unwrap();
                                        let reader = BufReader::new(file_entry);

                                        for line in reader.lines() {
                                            let line = line.unwrap();
                                            let line_len = line.len() as i64;

                                            if batch_bytes + line_len > chunk_size && !current_batch.is_empty() {
                                                tx.unbounded_send(current_batch.clone()).unwrap();
                                                current_batch.clear();
                                                batch_bytes = 0;
                                            }

                                            let ingest_data = format!("{}{}", if batch_bytes == 0 { "" } else { "\n" }, line);
                                            batch_bytes += ingest_data.len() as i64;

                                            current_batch.push(IngestBatch {
                                                offset_key: offset_key.clone(),
                                                data: ingest_data,
                                            });
                                        }

                                        if !current_batch.is_empty() {
                                            tx.unbounded_send(current_batch.clone()).unwrap();
                                            current_batch.clear();
                                            batch_bytes = 0;
                                        }
                                    }
                                },
                                "tar.gz" => {
                                    let decoder = GzDecoder::new(file);
                                    let mut archive = Archive::new(decoder);
                                    for entry in archive.entries().unwrap() {
                                        let file_entry = entry.unwrap();
                                        let reader = BufReader::new(file_entry);

                                        for line in reader.lines() {
                                            let line = line.unwrap();
                                            let line_len = line.len() as i64;

                                            if batch_bytes + line_len > chunk_size && !current_batch.is_empty() {
                                                tx.unbounded_send(current_batch.clone()).unwrap();
                                                current_batch.clear();
                                                batch_bytes = 0;
                                            }

                                            let ingest_data = format!("{}{}", if batch_bytes == 0 { "" } else { "\n" }, line);
                                            batch_bytes += ingest_data.len() as i64;

                                            current_batch.push(IngestBatch {
                                                offset_key: offset_key.clone(),
                                                data: ingest_data,
                                            });
                                        }

                                        if !current_batch.is_empty() {
                                            tx.unbounded_send(current_batch.clone()).unwrap();
                                            current_batch.clear();
                                            batch_bytes = 0;
                                        }
                                    }
                                },
                                "zip" => {
                                    let mut archive = ZipArchive::new(file).unwrap();

                                    let mut ingest_data = format!("");

                                    for i in 0..archive.len() {
                                        let file = archive.by_index(i).unwrap();
                                        let reader = BufReader::new(file);

                                        for line in reader.lines() {
                                            let line = line.unwrap();
                                            // println!("{}", line);
                                            // current_batch.push(IngestBatch {
                                            //     offset_key: offset_key.clone(),
                                            //     data: line,
                                            // });
                                            // tx.unbounded_send(current_batch.clone()).unwrap();
                                            // sleep(Duration::from_millis(10000));
                                            // exit(0);
                                            let _line_len = line.len() as i64;

                                            ingest_data = format!("{}{}{}", ingest_data, line, "\n");
                                            // ingest_data = format!("{}{}", ingest_data, line);

                                            // println!("ingest_data {}", ingest_data);
                                            // exit(0);

                                            batch_bytes += ingest_data.len() as i64;

                                            if batch_bytes > chunk_size && !ingest_data.is_empty() {

                                                // println!("ingest_data {}", ingest_data);
                                                // exit(0);
                                                // println!("line: {}, events: {}", line_len, ingest_data);

                                                current_batch.push(IngestBatch {
                                                    offset_key: offset_key.clone(),
                                                    data: ingest_data,
                                                });

                                                tx.unbounded_send(current_batch.clone()).unwrap();
                                                current_batch.clear();
                                                batch_bytes = 0;
                                                ingest_data = format!("");
                                            }


                                        }

                                        if !current_batch.is_empty() {
                                            tx.unbounded_send(current_batch.clone()).unwrap();
                                            current_batch.clear();
                                            batch_bytes = 0;
                                        }
                                    }
                                },
                                _ => {
                                    let reader = BufReader::new(file);

                                    for line in reader.lines() {
                                        let line = match line {
                                            Ok(line) => line,
                                            Err(_) => continue,
                                        };
                                        let line_len = line.len() as i64;
                                        if batch_bytes + line_len > chunk_size && !current_batch.is_empty() {
                                            tx.unbounded_send(current_batch.clone()).unwrap();
                                            current_batch.clear();
                                            batch_bytes = 0;
                                        }

                                        let ingest_data = format!("{}{}", if batch_bytes == 0 { "" } else { "\n" }, line);
                                        batch_bytes += ingest_data.len() as i64;

                                        current_batch.push(IngestBatch {
                                            offset_key: offset_key.clone(),
                                            data: ingest_data,
                                        });
                                    }

                                    if !current_batch.is_empty() {
                                        tx.unbounded_send(current_batch.clone()).unwrap();
                                        current_batch.clear();
                                        batch_bytes = 0;
                                    }
                                },
                            }

                            if batch_bytes >= chunk_size {
                                tx.unbounded_send(current_batch.clone()).unwrap();
                                current_batch.clear();
                                batch_bytes = 0;
                            }
                        }
                    },
                    Err(e) => println!("{:?}", e),
                }
            }

            if !current_batch.is_empty() {
                tx.unbounded_send(current_batch.clone()).unwrap();
                current_batch.clear();
                batch_bytes = 0;
            }
        });

        rx
    }
}