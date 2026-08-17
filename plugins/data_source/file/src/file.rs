use std::fs;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use flate2::read::GzDecoder;
use futures::channel::mpsc::UnboundedSender;
use futures::stream::StreamExt;
use serde_derive::Deserialize;
use tar::Archive;
use tracing::error;
use zip::ZipArchive;

use crate::helpers::configuration::Config;
use crate::helpers::plugin_config::PluginConfigEntry;
use crate::serdes::input_format::InputFormat;
use skippr_runtime_sdk::plugins::DataSource;
use skippr_runtime_sdk::progress::OffsetKey;
use skippr_runtime_sdk::source_compat::{
    partition_already_closed, submit_payload_batch_groups, IngestBatch, SourceSyncContext,
};

type BatchSender = UnboundedSender<Vec<Vec<IngestBatch>>>;

fn file_source_namespace(source_dir: &str) -> String {
    Path::new(source_dir)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(source_dir)
        .to_string()
}

fn collect_source_files(source: &Path) -> Vec<PathBuf> {
    if source.is_file() {
        return vec![source.to_path_buf()];
    }
    if !source.is_dir() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut stack = vec![source.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

#[derive(Debug, Deserialize, Clone)]
pub struct DataSourceLocalFilePluginConfig {
    pub format: Option<String>,
    pub batch_size_seconds: Option<i64>,
    pub batch_size_bytes: Option<i64>,
    path: String,
}

impl TryFrom<PluginConfigEntry> for DataSourceLocalFilePluginConfig {
    type Error = String;

    fn try_from(plugin_config: PluginConfigEntry) -> Result<Self, Self::Error> {
        plugin_config.decode_for_plugin("File")
    }
}

pub struct DataSourceLocalFilePlugin {
    #[allow(dead_code)]
    temp_dir: String,
    config: DataSourceLocalFilePluginConfig,
}

fn emit_batch(tx: &BatchSender, batch: IngestBatch) {
    tx.unbounded_send(vec![vec![batch]]).unwrap();
}

fn read_whole_payload<R: Read>(mut reader: R, offset_key: &OffsetKey, tx: &BatchSender) {
    let mut ingest_data = String::new();
    if reader.read_to_string(&mut ingest_data).is_err() || ingest_data.is_empty() {
        return;
    }

    emit_batch(
        tx,
        IngestBatch {
            offset_key: offset_key.clone(),
            bytes: ingest_data.len(),
            offset_pos: None,
            data: ingest_data,
            source_uri: String::new(),
            namespace: None,
            cdc_rows: None,
        },
    );
}

fn read_line_chunks<R: Read>(reader: R, offset_key: &OffsetKey, tx: &BatchSender, chunk_size: i64) {
    let reader = BufReader::new(reader);
    let mut ingest_data = String::new();
    let mut batch_bytes = 0i64;

    for line in reader.lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => continue,
        };

        if batch_bytes > 0 {
            ingest_data.push('\n');
            batch_bytes += 1;
        }

        ingest_data.push_str(&line);
        batch_bytes += line.len() as i64;

        if batch_bytes >= chunk_size {
            emit_batch(
                tx,
                IngestBatch {
                    offset_key: offset_key.clone(),
                    data: std::mem::take(&mut ingest_data),
                    bytes: batch_bytes as usize,
                    offset_pos: None,
                    source_uri: String::new(),
                    namespace: None,
                    cdc_rows: None,
                },
            );
            batch_bytes = 0;
        }
    }

    if !ingest_data.is_empty() {
        emit_batch(
            tx,
            IngestBatch {
                offset_key: offset_key.clone(),
                data: ingest_data,
                bytes: batch_bytes as usize,
                offset_pos: None,
                source_uri: String::new(),
                namespace: None,
                cdc_rows: None,
            },
        );
    }
}

fn process_reader<R: Read>(
    reader: R,
    offset_key: &OffsetKey,
    tx: &BatchSender,
    input_format: InputFormat,
    chunk_size: i64,
) {
    if input_format.requires_whole_payload() {
        read_whole_payload(reader, offset_key, tx);
    } else {
        read_line_chunks(reader, offset_key, tx, chunk_size);
    }
}

impl DataSourceLocalFilePlugin {
    pub async fn new() -> DataSourceLocalFilePlugin {
        let data_dir = Config::get_data_dir();
        let temp_dir = &format!("{}/source", data_dir);

        match fs::create_dir(temp_dir) {
            Ok(_g) => {}
            Err(_err) => {}
        }

        let config: DataSourceLocalFilePluginConfig =
            match Config::get_pipeline_input_plugin_config() {
                Ok(config) => config.try_into().unwrap_or_else(|e| panic!("{}", e)),
                Err(_) => DataSourceLocalFilePluginConfig {
                    format: None,
                    batch_size_seconds: Some(
                        Config::getenv("DATA_SOURCE_BATCH_SIZE_SECONDS", "600")
                            .parse::<i64>()
                            .unwrap(),
                    ),
                    batch_size_bytes: Some(
                        Config::getenv("DATA_SOURCE_BATCH_SIZE_BYTES", "1024000")
                            .parse::<i64>()
                            .unwrap(),
                    ),
                    path: Config::getenv("DATA_SOURCE_PATH", ""),
                },
            };

        DataSourceLocalFilePlugin {
            temp_dir: temp_dir.to_string(),
            config,
        }
    }

    pub fn with_runtime_config(config: DataSourceLocalFilePluginConfig) -> Self {
        let data_dir = Config::get_data_dir();
        let temp_dir = format!("{}/source", data_dir);
        let _ = fs::create_dir(&temp_dir);
        DataSourceLocalFilePlugin { temp_dir, config }
    }

    pub async fn run_sync(
        &mut self,
        ctx: Arc<dyn SourceSyncContext>,
    ) -> Result<(), std::io::Error> {
        let mut data_batches_stream = Box::pin(self.prepare_data_for_processing(
            ctx.clone(),
            self.config.path.clone(),
            self.config.batch_size_bytes.unwrap_or(1_000_000),
        ));

        while let Some(batch_groups) = data_batches_stream.next().await {
            submit_payload_batch_groups(ctx.as_ref(), batch_groups)?;
        }
        Ok(())
    }

    pub fn prepare_data_for_processing(
        &self,
        ctx: Arc<dyn SourceSyncContext>,
        source_dir: String,
        chunk_size: i64,
    ) -> impl futures::Stream<Item = Vec<Vec<IngestBatch>>> {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        let input_format = InputFormat::from_option(self.config.format.as_deref());
        for path in collect_source_files(Path::new(&source_dir)) {
            let offset_key = OffsetKey {
                namespace: file_source_namespace(&source_dir),
                partition: path.to_str().unwrap().to_string(),
            };
            // List-time Closed must use the same key host ingest stores after the
            // runtime wire round-trip (normalize twice). See S3 source.
            let closed_key = IngestBatch::runtime_roundtrip_offset_key(
                offset_key.namespace.clone(),
                offset_key.partition.clone(),
            );

            if partition_already_closed(ctx.as_ref(), &closed_key) {
                continue;
            }

            let file = match File::open(&path) {
                Ok(file) => file,
                Err(why) => {
                    error!("couldn't open {}: {}", path.display(), why);
                    continue;
                }
            };

            let path_string = path.to_string_lossy().to_string();
            if path_string.ends_with(".tar.gz") {
                let decoder = GzDecoder::new(file);
                let mut archive = Archive::new(decoder);
                let entries = match archive.entries() {
                    Ok(entries) => entries,
                    Err(err) => {
                        error!("couldn't read tar.gz archive {}: {}", path.display(), err);
                        continue;
                    }
                };

                for entry in entries {
                    let file_entry = match entry {
                        Ok(file_entry) => file_entry,
                        Err(err) => {
                            error!("couldn't read tar.gz entry {}: {}", path.display(), err);
                            continue;
                        }
                    };
                    process_reader(file_entry, &offset_key, &tx, input_format, chunk_size);
                }
                continue;
            }

            let file_ext = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");

            match file_ext {
                "gz" => {
                    let decoder = GzDecoder::new(file);
                    process_reader(decoder, &offset_key, &tx, input_format, chunk_size);
                }
                "tar" => {
                    let mut archive = Archive::new(file);
                    let entries = match archive.entries() {
                        Ok(entries) => entries,
                        Err(err) => {
                            error!("couldn't read tar archive {}: {}", path.display(), err);
                            continue;
                        }
                    };

                    for entry in entries {
                        let file_entry = match entry {
                            Ok(file_entry) => file_entry,
                            Err(err) => {
                                error!("couldn't read tar entry {}: {}", path.display(), err);
                                continue;
                            }
                        };
                        process_reader(file_entry, &offset_key, &tx, input_format, chunk_size);
                    }
                }
                "zip" => {
                    let mut archive = match ZipArchive::new(file) {
                        Ok(archive) => archive,
                        Err(err) => {
                            error!("couldn't read zip archive {}: {}", path.display(), err);
                            continue;
                        }
                    };

                    for idx in 0..archive.len() {
                        let file = match archive.by_index(idx) {
                            Ok(file) => file,
                            Err(err) => {
                                error!("couldn't read zip entry {}: {}", path.display(), err);
                                continue;
                            }
                        };
                        process_reader(file, &offset_key, &tx, input_format, chunk_size);
                    }
                }
                _ => {
                    process_reader(file, &offset_key, &tx, input_format, chunk_size);
                }
            }
        }

        rx
    }
}

#[async_trait]
impl DataSource for DataSourceLocalFilePlugin {
    async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        self.run_sync(ctx).await
    }

    fn execution_contract(&self) -> skippr_runtime_sdk::plugins::SourceExecutionContract {
        skippr_runtime_sdk::plugins::SourceExecutionContract::finite()
    }
}

#[cfg(test)]
mod tests {
    use super::{collect_source_files, file_source_namespace};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn namespace_is_the_directory_name_not_the_absolute_path() {
        assert_eq!(file_source_namespace("/tmp/hla-e2e/events"), "events");
        assert_eq!(file_source_namespace("/tmp/hla-e2e/events/"), "events");
        assert_eq!(file_source_namespace("events"), "events");
    }

    #[test]
    fn collect_source_files_includes_jsonl_in_the_directory() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("skippr-file-source-{unique}"));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("batch1.jsonl");
        fs::write(&file, "{\"id\":\"evt-1\"}\n").unwrap();
        let nested = dir.join("nested");
        fs::create_dir_all(&nested).unwrap();
        let nested_file = nested.join("batch2.jsonl");
        fs::write(&nested_file, "{\"id\":\"evt-2\"}\n").unwrap();
        let found = collect_source_files(&dir);
        let _ = fs::remove_dir_all(&dir);
        assert!(found.iter().any(|path| path.ends_with("batch1.jsonl")));
        assert!(found.iter().any(|path| path.ends_with("batch2.jsonl")));
    }
}
