use std::fs::{File, OpenOptions};
use std::{fs, io,};
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Cursor, Read, Seek, Write};
use std::ops::{Deref, Index};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::{Arc};
use std::time::{Instant, SystemTime};
use arrow::array::{Array, ArrayRef, RecordBatch};
use arrow::json::ReaderBuilder;
use arrow_schema::{ArrowError, DataType, Field, SchemaRef, Schema};
use dashmap::{DashMap, Map};
use glob::{glob_with, MatchOptions};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use arrow::compute::concat;
use arrow::ipc::{CompressionType};
use arrow::ipc::reader::StreamReader;
use arrow::ipc::writer::{IpcWriteOptions, StreamWriter};
use arrow::json::reader::Decoder;
use arrow::json::writer::record_batches_to_json_rows;
use arrow::record_batch::RecordBatchOptions;
use bincode;
use serde_cbor;
use byteorder::LittleEndian;
use datafusion::datasource::MemTable;
use datafusion::execution::options::ArrowReadOptions;
use datafusion::parquet::data_type::AsBytes;
use datafusion::physical_plan::memory::MemoryStream;
use datafusion::physical_plan::SendableRecordBatchStream;
use datafusion::prelude::{ParquetReadOptions, SessionConfig, SessionContext};
use icu::properties::sets::print;
use indexmap::IndexMap;
use lazy_static::lazy_static;
use libc::exit;
use once_cell::sync::Lazy;
use parquet::arrow::{arrow_to_parquet_schema, ArrowWriter};
use parquet::arrow::arrow_writer::{ArrowLeafColumn, compute_leaves, get_column_writers};
use parquet::basic::Compression;
use parquet::file::properties::{ReaderProperties, WriterProperties};
use parquet::file::reader::SerializedFileReader;
use parquet::file::writer::SerializedFileWriter;
use serde_derive::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::runtime::Runtime;
use yaml_rust::Yaml::Hash;
use zerocopy::U64;
use crate::{ARROW_SCHEMA, METRICS, RUNNING, sync_output_plugin};
use crate::buffer::BufferChunker;
use crate::helpers::configuration::Config;
use crate::helpers::Helpers;
use crate::helpers::offsets::{Offset, OffsetKey, Offsets, OffsetTypes, OffsetValue};
use crate::helpers::timed_rwlock::TimedRwLock;
use crate::ingest_work::Ingest;
use crate::metrics::Metrics;
use crate::plugins::athena::DataOutputAwsAthenaPlugin;
use crate::plugins::DataOutputPlugin;

pub static TOTAL_ROWS: Lazy<TimedRwLock<AtomicU64>> =
    Lazy::new(|| TimedRwLock::new("record_batch_total".to_string(), AtomicU64::new(0)));

pub static WAL_PARTITION_INDEX: Lazy<TimedRwLock<WalPartitionIndex>> = Lazy::new(|| TimedRwLock::new("wal_partition_index".to_string(), WalPartitionIndex::new()));

// lazy_static! {
//     pub static ref WAL_INDEX: TimedRwLock<WalIndex> = TimedRwLock::new("wal_index".to_string(), WalIndex::new());
// }

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
pub struct OffsetKeySerialize {
    pub(crate) source_namespace: String,
    pub(crate) source_partition: String,
    pub(crate) position: u64,
}

#[derive(Debug, Clone)]
pub struct IngestRecord {
    pub(crate) namespace: String,
    pub(crate) partition: String,
    pub(crate) time: Option<i64>,
    pub(crate) record: Value
}

pub struct IngestBufferBatch {
    pub(crate) offsets: HashMap<OffsetKey, u64>,
    pub(crate) namespace: String,
    pub(crate) partition: String,
    pub(crate) time: Option<i64>,
    pub(crate) shard: String,
    pub(crate) records: Vec<IngestRecord>,
    pub(crate) schema: SchemaRef,
}

pub struct Buffers {
    buf: IndexMap<(String, String, Option<i64>, String), IngestBufferBatch>,
}

impl Buffers {

    pub fn new() -> Self {
        Buffers {
            buf: IndexMap::new(),
        }
    }


    pub fn write(&mut self, ingest_buffer_batch: HashMap<(String, String, Option<i64>, String), IngestBufferBatch>) {
        for batch in ingest_buffer_batch {
            self.buf.insert(batch.0, batch.1);
        }
    }

    pub async fn flush(&mut self, offsets_db: Arc<Offsets>, shared_output: Arc<TimedRwLock<Box<dyn DataOutputPlugin + Send + Sync>>>) -> Result<(), ArrowError> {

        // let arrow_schema_guard = ARROW_SCHEMA.read().clone();

        // let mut index = WAL_INDEX.write();

        let mut bytes: u64 = 0;
        let mut rows: u64 = 0;

        // println!("Flushing {} WAL files", self.buf.len());

        let mut partitions: HashMap<(String, String, Option<i64>, String), Vec<WalFile>> = HashMap::new();

        for ((namespace, partition, time, shard), ingest_buffer_batch) in self.buf.iter_mut() {

            // println!("Writing {} rows to WAL {} {} {} {}", ingest_buffer_batch.records.len(), namespace, partition, time.unwrap_or(0), shard);

            let partition_entry = partitions.entry((namespace.clone(), partition.clone(), time.clone(), shard.clone())).or_insert_with(|| Vec::new());

            let mut wal_file = WalFile::new(
                namespace,
                partition,
                *time,
                shard,
                ingest_buffer_batch.offsets.clone(),
            ).unwrap();


            // println!("WAL File {} offset: {:?}", wal_file.path.to_str().unwrap(), ingest_buffer_batch.offset);

            //  let wal_file_partition = index.index.entry((
            //      ingest_buffer_batch.namespace.clone(),
            //      ingest_buffer_batch.partition.clone(),
            //      ingest_buffer_batch.time.clone(),
            //      ingest_buffer_batch.shard.clone(),
            // )).or_insert_with(
            //      || WalFilePartition {
            //          files: Vec::new(),
            //          namespace: ingest_buffer_batch.namespace.clone(),
            //          partition: ingest_buffer_batch.partition.clone(),
            //          time: ingest_buffer_batch.time.clone(),
            //          shard: ingest_buffer_batch.shard.clone(),
            //          updated_at: SystemTime::now(),
            //          bytes: 0,
            //      }
            //  );

            // let arrow_schema = arrow_schema_guard.get(&ingest_buffer_batch.namespace).unwrap().clone();

            let arrow_schema = ingest_buffer_batch.schema.clone();
            // let arrow_schema = ARROW_SCHEMA.read().get(&ingest_buffer_batch.namespace).unwrap().clone();

            let mut decoder = ReaderBuilder::new(arrow_schema).build_decoder().unwrap();

            let mut record_batches = Vec::new();

            // @todo - faster to build a vec and pass to decoder?

            ingest_buffer_batch.records.len();

            let json_values = ingest_buffer_batch.records.iter().map(|record| &record.record).collect::<Vec<&Value>>();
            decoder.serialize(&json_values).unwrap();

            match decoder.flush() {
                Ok(Some(batch)) => {
                    record_batches.push(batch);
                },
                Ok(None) => {
                    // println!("No record batch");
                },
                Err(e) => {
                    println!("Error decoding record batch: {}. Deadlettering", e);
                    
                    let deadletters: String = ingest_buffer_batch.records.iter().map(|record| record.record.to_string()).collect::<Vec<String>>().join("\n");
                    Ingest::deadletter(&deadletters);
                    
                    continue;
                }
            }
            
            // let mut ingest_batch = WalRecordBatches::new(offset, record_batches);

            let stat = wal_file.write_to_stream(&record_batches)?;

            bytes += stat.0;
            rows += stat.1;

            // wal_file_partition.bytes += wal_file.write_to_stream(&record_batches)?;

            // wal_file_partition.bytes += wal_file.bytes;
            // println!("Wrote records to WAL file: {}", wal_file.path.to_str().unwrap());

            wal_file.flush()?;

            wal_file.finish()?;

            // let offset_key = OffsetKey {
            //     namespace: ingest_buffer_batch.offset.source_namespace.clone(),
            //     partition: ingest_buffer_batch.offset.source_partition.clone(),
            // };
            // offsets_db.insert(&offset_key, OffsetTypes::Line, ingest_buffer_batch.offset.position.clone());
            // offsets_db.insert(&offset_key, OffsetTypes::Closed, 1);

            // println!("Committing {} offsets", ingest_buffer_batch.offsets.len());

            ingest_buffer_batch.offsets.iter().for_each(|(offset, position)| {
                let offset_key = OffsetKey {
                    namespace: offset.namespace.clone(),
                    partition: offset.partition.clone(),
                };

                offsets_db.insert(&offset_key, OffsetTypes::Position, *position);
                offsets_db.insert(&offset_key, OffsetTypes::Closed, 1);
            });

            offsets_db.flush();

            partition_entry.push(wal_file);

        }

        // println!("Ingested {} rows of {} bytes to WAL", stats.1, stats.0);

        self.buf.clear();

        {
            let mut counter_lock = METRICS.write();
            counter_lock.wal_write_bytes_total += bytes;
            counter_lock.wal_write_rows_total += rows;
        }

        let mut compact_index_partitions: HashMap<(String, String, Option<i64>, String), WalPartition> = HashMap::new();

        {
            let mut index = WAL_PARTITION_INDEX.write();

            for ((namespace, partition, time, shard), wal_files) in partitions.iter_mut() {
                let mut wal_partition = index.index.entry((namespace.clone(), partition.clone(), time.clone(), shard.clone()))
                    .or_insert_with(|| WalPartition {
                        files: Vec::new(),
                        namespace: namespace.clone(),
                        partition: partition.clone(),
                        time: time.clone(),
                        shard: shard.clone(),
                        updated_at: SystemTime::now(),
                        bytes: 0,
                    });


                wal_files.iter().for_each(|wal_file| wal_partition.bytes += wal_file.bytes);
                wal_files.iter().for_each(|wal_file| {
                    if wal_file.updated_at > wal_partition.updated_at {
                        wal_partition.updated_at = wal_file.updated_at
                    }
                });

                wal_partition.files.append(wal_files);
            }

            // Evaluate candidates for compaction
            for (key, wal_partition) in index.index.iter() {

                if wal_partition.check_wal_rotate() {
                    compact_index_partitions.insert(key.clone(), wal_partition.clone());
                }
            }

            // pop partitions ready for compaction from the index.
            // in failure scenario, index is recovered on startup.
            for key in compact_index_partitions.keys() {
                index.index.remove(&key);
            }

        }


        let shared_output_clone = shared_output.clone();
        // Compact the partitions (now the index is unlocked, WAL flushed and offset committed)
        for partition in compact_index_partitions.values_mut() {
            let shared_output_clone2 = shared_output_clone.clone();
            partition.compact_batches_to_parquet(shared_output_clone2).await;
        }

        // println!("Wrote {} rows to WAL", rows);

        Ok(())
    }

    pub async fn compact_all_partitions(force: bool, offsets_db: Arc<Offsets>, shared_output: Arc<TimedRwLock<Box<dyn DataOutputPlugin + Send + Sync>>>) {
        let mut wal_index = WAL_PARTITION_INDEX.write();

        wal_index.index.clear(); // avoid duplicates

        wal_index.recover(offsets_db).expect("Failed to recover WAL index");

        let mut compacted_index_partitions = Vec::new();

        let cloned_shared_output = shared_output.clone();
        for (_key, wal_partition) in wal_index.index.iter_mut() {

            let shared_output2 = cloned_shared_output.clone();
            if force {
                wal_partition.compact_batches_to_parquet(shared_output2).await;
                compacted_index_partitions.push((wal_partition.namespace.clone(), wal_partition.partition.clone(), wal_partition.time.clone(), wal_partition.shard.clone()));

            } else {
                let rotated = wal_partition.check_wal_rotate();

                if rotated {
                   compacted_index_partitions.push((wal_partition.namespace.clone(), wal_partition.partition.clone(), wal_partition.time.clone(), wal_partition.shard.clone()));
                }
            }
        }

        // for (namespace, partition, time, shard) in compacted_index_partitions {
        //     wal_index.index.remove(&(namespace.clone(), partition.clone(), time.clone(), shard));

            // remove partition dir
            // let wal_partition_dir = WalFile::get_wal_partition_dir(&namespace, &partition, time);
            // fs::remove_dir_all(wal_partition_dir).unwrap();
        // }

        // wal_index.index.clear();
    }

}

#[derive(Default, Clone)]
pub struct WalPartitionIndex {
    // Maps namespace, partition, and time to WAL file information
    index: HashMap<(String, String, Option<i64>, String), WalPartition>,
}

impl WalPartitionIndex {
    fn new() -> Self {
        WalPartitionIndex {
            index: HashMap::new(),
        }
    }

    pub fn recover(&mut self, offsets_db: Arc<Offsets>) -> io::Result<()> {

        let mut count = 0;

        println!("Indexing WAL");

        let wal_files = Self::list_wal_files()?;

        let wal_files_count = wal_files.len();

        if wal_files_count == 0 {
            println!("Indexed {} of {} WAL files", count, wal_files_count);
            return Ok(());
        }

        for file_path in wal_files {

            let wal_file = match WalFile::from_path(&file_path) {
                Ok(wal_file) => wal_file,
                Err(e) => {
                    // Probably a zero byte file being written to
                    // println!("Failed to read WAL file: {}, Error: {}", file_path.to_str().unwrap(), e);
                    println!("Failed to index WAL file: {} of bytes: {}, Error {}.", file_path.to_str().unwrap(), file_path.metadata().unwrap().len(), e);

                    continue;
                }
            };

            let partition_key = (wal_file.namespace.clone(), wal_file.partition.clone(), wal_file.time.clone(), wal_file.shard.clone());

            let wal_file_partition = self.index.entry(partition_key).or_insert_with(|| WalPartition {
                files: Vec::new(),
                namespace: wal_file.namespace.clone(),
                partition: wal_file.partition.clone(),
                time: wal_file.time.clone(),
                shard: wal_file.shard.clone(),
                updated_at: SystemTime::UNIX_EPOCH,
                bytes: 0,
            });

            if wal_file_partition.updated_at < wal_file.updated_at {
                wal_file_partition.updated_at = wal_file.updated_at;
            }
            wal_file_partition.bytes += wal_file.bytes;

            wal_file_partition.files.push(wal_file);

            count += 1;

            if count % 1000 == 0 {
                println!("Indexed {} of {} WAL files in {} partitions", count, wal_files_count, self.index.len());
            }
        }

        println!("Indexed {} of {} WAL files in {} partitions", count, wal_files_count, self.index.len());

        println!("Syncing offsets to DB");

        // for wal_partition in self.index.values_mut() {
        //     wal_partition.files.sort_by(|a, b| a.file.metadata().unwrap().created().unwrap().cmp(&b.file.metadata().unwrap().created().unwrap()));
        // }

        for (_key, wal_partition) in self.index.iter_mut() {

            // ensure offsets committed
            for wal_file in wal_partition.files.iter_mut() {
                // wal_file.offsets.iter().for_each(|offset| {
                //     let offset_key = OffsetKey {
                //         namespace: offset.source_namespace.clone(),
                //         partition: offset.source_partition.clone(),
                //     };
                //
                //     offsets_db.insert(&offset_key, OffsetTypes::Line, offset.position);
                //     offsets_db.insert(&offset_key, OffsetTypes::Closed, 1);
                // });

                wal_file.offsets.iter().for_each(|(offset, position)| {
                    let offset_key = OffsetKey {
                        namespace: offset.namespace.clone(),
                        partition: offset.partition.clone(),
                    };

                    offsets_db.insert(&offset_key, OffsetTypes::Position, *position);
                    offsets_db.insert(&offset_key, OffsetTypes::Closed, 1);
                });
            }
        }

        offsets_db.flush();


        Ok(())
    }

    fn list_wal_files() -> io::Result<Vec<PathBuf>> {
        let data_dir = Config::get_data_dir();
        let wal_dir = PathBuf::from(format!("{}/ingest_buffer", data_dir));
        let mut wal_files = Vec::new();

        // remove empty dirs
        for entry in glob_with(&format!("{}/**/*", wal_dir.to_str().unwrap()), MatchOptions {
            case_sensitive: false,
            require_literal_separator: false,
            require_literal_leading_dot: false,
        }).expect("Failed to clean WAL dir") {

            let path = entry.expect("Failed to read WAL file");

            // remove empty files
            if path.is_file() && path.metadata().unwrap().len() == 0 {
                println!("Removing empty WAL file: {}", path.to_str().unwrap());
                fs::remove_file(&path).unwrap();
            }

            // remove .tmp files
            if path.is_file() && path.extension().and_then(OsStr::to_str) == Some("tmp") {
                println!("Removing temp WAL file: {}", path.to_str().unwrap());
                fs::remove_file(&path).unwrap();
            }

            // remove empty dirs
            // if path.is_dir() {
            //     if fs::read_dir(&path).unwrap().count() == 0 {
            //         println!("Removing empty WAL dir: {}", path.to_str().unwrap());
            //         fs::remove_dir_all(&path).unwrap();
            //     }
            // }
        }

        let options = MatchOptions {
            case_sensitive: false,
            require_literal_separator: false,
            require_literal_leading_dot: false,
        };

        for entry in glob_with(&format!("{}/**/*.wal", wal_dir.to_str().unwrap()), options).expect("Failed to read WAL files") {

            let path = entry.expect("Failed to read WAL file");
            wal_files.push(path);
        }
        Ok(wal_files)
    }

}

#[derive(Clone)]
struct WalPartition {
    files: Vec<WalFile>,
    pub(crate) namespace: String,
    pub(crate) partition: String,
    pub(crate) time: Option<i64>,
    pub(crate) shard: String,
    updated_at: SystemTime,
    bytes: u64,
}

impl WalPartition {
    fn prune_tombstone_wals(&mut self) {
        // println!("Purging tombstone WAL files");
        let data_dir = Config::get_data_dir();
        let wal_dir = PathBuf::from(format!("{}/ingest_buffer/done", data_dir));
        fs::remove_dir_all(&wal_dir).unwrap_or_default();
        fs::create_dir_all(&wal_dir).unwrap();
    }

    fn is_file_size_exceeded(&self) -> bool {
        let buffer_size = Config::get_pipeline_buffer_threshold_bytes();
        self.bytes > buffer_size
    }

    fn is_file_time_exceeded(&self) -> bool {
        let ttl = Config::get_pipeline_buffer_threshold_seconds();
        SystemTime::now()
            .duration_since(self.updated_at)
            .unwrap()
            .as_secs()
            > ttl as u64
    }

    pub fn check_wal_rotate(&self) -> bool {
        if self.is_file_size_exceeded() || self.is_file_time_exceeded() {
            // println!("Compacting WAL: {} Bytes: {}, Segment Files: {}", self.namespace, self.bytes, self.files.len());
            let elapsed = SystemTime::now().duration_since(self.updated_at).unwrap().as_secs();
            println!("Compacting WAL partition Namespace: {}, Partition: {}, Time: {}, of Bytes: {}, Elapsed Secs: {}, Segment Files: {}", self.namespace, self.partition, self.time.unwrap_or(0), self.bytes, elapsed, self.files.len());
            return true
        }

        false
    }

    async fn compact_batches_to_parquet(&mut self, shared_output: Arc<TimedRwLock<Box<dyn DataOutputPlugin + Send + Sync>>>) {
        let data_dir = Config::get_data_dir();
        let mut output_file_name = BufferChunker::encode_chunk_name(
            "output",
            Some(&self.namespace),
            Some(&self.partition),
            self.time,
            Some(&self.shard)
        );

        // @todo - replace random_str with a sequence/segment number for imdepotent object uploads.
        // Suspect that will be required to handle retries and failures, while still avoiding overwriting existing data.
        output_file_name = format!("{}-{}", output_file_name, Helpers::random_str(32));

        // println!("Compacting WAL partition to Parquet, Namespace: {} Partition: {} {}", self.namespace, self.partition, self.time.unwrap_or(0));

        let mut wal_compacted_bytes_total = 0;
        let mut wal_compacted_rows_total = 0;
        let mut wal_compacted_files_total = 0;
       
        let schema = match self.files.first_mut().unwrap().read_schema_from_stream() {
            Ok(schema) => schema,
            Err(e) => {
                let file = self.files.first().unwrap();
                println!("Failed to read schema from WAL file: {} of bytes: {}, Error {}. Skipping to next WAL partition.", file.path.to_str().unwrap(), file.bytes, e);
                return;
            }
        };


        let batches = self.files.iter_mut().map(|wal_file| {
            wal_compacted_bytes_total += wal_file.bytes;
            wal_compacted_files_total += 1;

            let batches = wal_file.read_from_stream().unwrap();

            wal_compacted_rows_total += batches.iter().map(|batch| batch.num_rows()).sum::<usize>() as u64;

            batches

        }).flatten().collect::<Vec<RecordBatch>>();

        let batch_stream: SendableRecordBatchStream = Box::pin(MemoryStream::try_new(batches, schema.clone(), None).unwrap());


        match shared_output.write().sync(batch_stream, output_file_name).await {
            Ok(()) => {
                // println!("Synced WAL partition to output: {} {}", self.namespace, self.partition);
        
                {
                    let mut counter_lock = METRICS.write();
                    counter_lock.wal_compacted_bytes_total += wal_compacted_bytes_total;
                    counter_lock.wal_compacted_rows_total += wal_compacted_rows_total;
                    counter_lock.wal_compacted_files_total += wal_compacted_files_total;
        
                }
        
                // Rename the processed WAL file to a tombstone file
                for wal_file in self.files.iter() {
                    let tombstone_path = format!("{}/ingest_buffer/done/{}.tombstone", data_dir, Helpers::random_str(32));
        
                    match fs::rename(&wal_file.path, tombstone_path) {
                        Ok(_) => {
                            // println!("Tombstoned WAL file: {}", wal_file.path.to_str().unwrap());
                        },
                        Err(e) => {
                            println!("Failed to tombstone WAL file: {}, Error: {}", wal_file.path.to_str().unwrap(), e);
                        }
        
                    }
                }
        
                self.prune_tombstone_wals();
            },
            Err(e) => {
                let output_plugin_name = Config::get_pipeline_output_plugin_name();
                println!("Failed to sync WAL partition to output plugin: {}, Namespace {}, Partition {}, Error {}", output_plugin_name, self.namespace, self.partition, e);
            }
        }

    }

    async fn apply_sql_on_ipc_stream(temp_parquet_path: &str, sql: &str, schema_ref: SchemaRef) -> Result<Vec<arrow::array::RecordBatch>, Box<dyn std::error::Error>> {

        let mut session_config = SessionConfig::new();
        session_config = session_config.set("datafusion.catalog.information_schema", "true".into());
        session_config = session_config.set("datafusion.catalog.default_catalog", "skippr".into());
        session_config = session_config.set("datafusion.execution.collect_statistics", "true".into());

        let ctx = SessionContext::with_config(session_config);

        // ctx.register_parquet("my_table", temp_parquet_path, ParquetReadOptions {
        //     schema: Some(schema_ref.as_ref()),
        //     file_extension: "parquet",
        //     table_partition_cols: vec![],
        //     parquet_pruning: None,
        //     skip_metadata: Some(true),
        //     file_sort_order: vec![],
        // }).await.unwrap();
        // // Execute the SQL query
        //
        // let df = ctx.sql(sql).await?;
        // let results = df.collect().await.unwrap();


        let df = ctx
            .read_parquet(
                temp_parquet_path,
                ParquetReadOptions {
                    // schema: Some(schema_ref.as_ref()),
                    schema: None,
                    file_extension: "parquet",
                    table_partition_cols: vec![],
                    parquet_pruning: None,
                    skip_metadata: Some(true),
                    file_sort_order: vec![],
                },
            )
            .await.unwrap();
        let results = df.collect().await.unwrap();

        // Create a new DataFusion context
        // let mut ctx = ExecutionContext::new();

        // Read batches and register them as a table in the context
        // let schema_ref = record_batches[0].schema();

        // let schema_ref = ARROW_SCHEMA.read().get("my_table").unwrap().clone();

        // ctx.register_table("my_table", Arc::new(MemTable::try_new(schema_ref, record_batches)?))?;

        // let mut results = Vec::new();
        //
        // let df = ctx.read_a
        //
        //
        // for batch in record_batches {
        //     for b in batch {
        //        let df = ctx.read_batch(b.clone()).unwrap();
        //         results.extend(df.collect().await.unwrap());
        //     }
        // }



        Ok(results)

    }

    async fn apply_sql_on_ipc_record_batches(record_batches: Vec<RecordBatch>, sql: &str, schema_ref: SchemaRef) -> Result<Vec<arrow::array::RecordBatch>, Box<dyn std::error::Error>> {

        let mut session_config = SessionConfig::new();
        // session_config = session_config.set("datafusion.catalog.information_schema", "true".into());
        // session_config = session_config.set("datafusion.catalog.default_catalog", "skippr".into());
        // session_config = session_config.set("datafusion.execution.collect_statistics", "true".into());

        let ctx = SessionContext::with_config(session_config);

        // Read batches and register them as a table in the context
        // let schema_ref = record_batches[0].schema();
        // let schema_ref = ARROW_SCHEMA.read().get("my_table").unwrap().clone();

        // ctx.register_table("my_table", Arc::new(MemTable::try_new(schema_ref, record_batches)?))?;

        // let mut results = Vec::new();
        //
        // let df = ctx.read_a
        //
        //

        let batch = arrow::compute::concat_batches(&schema_ref, &record_batches).unwrap();

        let df = ctx.read_batch(batch).unwrap();
        let results = df.collect().await.unwrap();


        Ok(results)

    }
}

#[derive(Clone)]
pub struct WalFile {
    pub(crate) path: PathBuf,
    pub(crate) namespace: String,
    pub(crate) partition: String,
    pub(crate) time: Option<i64>,
    pub(crate) shard: String,
    pub(crate) bytes: u64,
    // We really only Arc File to support clone of the partition index to avoid future writes waiting on compaction reads
    // Additionally, the file is optional with lazy opening of a handle to avoid "Too many open files error", there may be thousands of WAL file segments
    pub(crate) file: Arc<TimedRwLock<Option<File>>>,
    pub(crate) updated_at: SystemTime,
    pub(crate) offsets: HashMap<OffsetKey, u64>,
}

impl WalFile {
    pub fn new(namespace: &str, partition: &str, time: Option<i64>, shard: &str, offsets: HashMap<OffsetKey, u64>,) -> io::Result<Self> {

        let path_str = Self::generate_temp_wal_file_name(namespace, partition, time, shard);
        let path= PathBuf::from(&path_str);

        // Ensure file exists and then allow the fp to drop out of scope to limit open file handles
        let fd = OpenOptions::new().append(true).read(true).create(true).open(&path)?;
        fd.sync_all()?;
        unsafe { // belt and braces
            libc::fsync(fd.as_raw_fd());
        };

        Ok(WalFile {
            path,
            bytes: 0,
            namespace: namespace.to_string(),
            partition: partition.to_string(),
            time,
            shard: shard.to_string(),
            file: Arc::new(TimedRwLock::new("wal_file".to_string(), None)),
            updated_at: SystemTime::now(),
            offsets
        })
    }

    fn get_or_open_file(&self) -> io::Result<File> {
        // let mut file_lock = self.file.write();
        OpenOptions::new().read(true).create(true).append(true).open(&self.path)
        // file.try_clone()
        // if file.is_none() {
        //     *file_lock = Some(OpenOptions::new().write(true).read(true).create(true).open(&self.path)?);
        // }
        // // Clone the file handle via Arc. File itself does not implement Clone.
        // file_lock.as_ref().unwrap().try_clone()
    }

    fn from_path(path: &PathBuf) -> io::Result<Self> {

        let mut file = OpenOptions::new()
            .read(true)
            .open(&path)?;

        let offsets = Self::offset_from_file(&mut file)?;

        let namespace = BufferChunker::decode_file_namespace(path.to_str().unwrap());
        let partition = BufferChunker::decode_file_partition(path.to_str().unwrap());
        let time = BufferChunker::decode_file_time(path.to_str().unwrap());
        let time = if time > 0 { Some(time) } else { None };

        let shard = BufferChunker::decode_file_shard(path.to_str().unwrap());

        let metadata = file.metadata()?;

        let updated_at = metadata.modified().unwrap_or_else(|_err| SystemTime::now());

        let bytes = metadata.len();

        Ok(WalFile {
            path: path.clone(),
            bytes,
            namespace,
            partition,
            time,
            shard,
            file: Arc::new(TimedRwLock::new("wal_file".to_string(), None)),
            updated_at,
            offsets
        })
    }

    pub fn offset_from_file(file: &mut File) -> io::Result<HashMap<OffsetKey, u64>> {
        // let mut file = OpenOptions::new().read(true).open(&path)?;

        let mut offset_size = [0u8; 8];

        file.read_exact(&mut offset_size)?;

        let mut bin_offset = vec![0u8; u64::from_le_bytes(offset_size) as usize];

        let mut reader = io::BufReader::new(file);

        reader.read_exact(&mut bin_offset)?;

        Ok(bincode::deserialize(&bin_offset).unwrap())
    }

    pub fn seek_offset(reader: &mut BufReader<&File>) -> Result<usize, ArrowError> {

        reader.seek(io::SeekFrom::Start(0)).unwrap();

        // println!("Reading offset from WAL file: {}", self.path.to_str().unwrap());

        let mut offset_size = [0u8; 8];
        reader.read_exact(&mut offset_size)?;

        reader.seek(io::SeekFrom::Current(i64::from_le_bytes(offset_size))).unwrap();

       Ok(usize::from_le_bytes(offset_size))

    }

    pub fn read_schema_from_stream(&mut self) -> Result<SchemaRef, ArrowError> {

        let file= self.get_or_open_file()?;
        let mut reader = io::BufReader::new(&file);

        Self::seek_offset(&mut reader)?;

        let mut stream_reader = StreamReader::try_new(reader, None)?;

        let schema = stream_reader.schema();

        Ok(schema)
    }

    pub fn read_from_stream(&mut self) -> Result<Vec<RecordBatch>, ArrowError> {

        let file= self.get_or_open_file()?;
        let mut reader = io::BufReader::new(&file);

        Self::seek_offset(&mut reader)?;

        let stream_reader = StreamReader::try_new(reader, None)
            .map_err(|e| ArrowError::from_external_error(Box::new(e)))?;

        let mut record_batches = Vec::new();

        for batch in stream_reader {
            let batch = batch?;
            record_batches.push(batch);
        }

        Ok(record_batches)
    }

    /**
     * Write record batches to the WAL file
     * @param record_batches
     * @return tuple of bytes and total rows written (bytes, row_count)
     */
    pub fn write_to_stream(&mut self, record_batches: &[RecordBatch]) -> Result<(u64, u64), ArrowError> {
        // let writer = self.file.as_mut().ok_or(ArrowError::IoError("Can't write to WAL file".to_string(), io::Error::new(io::ErrorKind::NotFound, "File not found")))?;

        let file = self.get_or_open_file().expect("Failed to clone WAL file for write");
        let mut writer = io::BufWriter::new(&file);

        writer.seek(io::SeekFrom::Start(0))?;

        let bin_offset = bincode::serialize(&self.offsets).unwrap();

        let offset_size: u64 = bin_offset.len() as u64;

        writer.write_all(&offset_size.to_le_bytes())?;
        writer.write_all(&bin_offset)?;

        writer.seek(io::SeekFrom::End(0))?;

        let mut size: usize = 0;
        let codec = Some(CompressionType::LZ4_FRAME);
        let options = IpcWriteOptions::default().try_with_compression(codec)?;

        let mut row_count = 0;

        let mut stream_writer = StreamWriter::try_new_with_options(writer, &record_batches[0].schema(), options)?;
        for batch in record_batches {
            size += batch.get_array_memory_size(); // @todo - account for compression ratio, observed ~50% reduction
            row_count += batch.num_rows();
            stream_writer.write(batch).expect("Failed to write record batch to stream writer");
        }

        stream_writer.finish()?;


        // let write_file = OpenOptions::new()
        //     .create(true)
        //     .write(true)
        //     .open(&self.path)
        //     .unwrap();
        //
        // let props = WriterProperties::builder()
        //     .set_dictionary_enabled(false)
        //     .set_encoding(parquet::basic::Encoding::PLAIN)
        //     .set_compression(Compression::SNAPPY)
        //     .build();
        //
        // let schema = ARROW_SCHEMA.read().get(&self.namespace).unwrap().clone();
        //
        // let mut writer = ArrowWriter::try_new(write_file, schema, Some(props)).unwrap();
        //
        // for batch in record_batches {
        //     writer.write(&batch).expect("Error writing to parquet file");
        // }
        //
        // writer.close().unwrap();

        let file = self.get_or_open_file().expect("Failed to get file for metadata read");
        self.bytes += file.metadata().unwrap().len();

        Ok((self.bytes, row_count as u64))
    }

    fn get_wal_partition_dir(namespace: &str, partition: &str, time: Option<i64>, shard: &str) -> String {
        let data_dir = Config::get_data_dir();

        // let metrics_guard = METRICS.read();
        // let run_id = metrics_guard.run_id.clone();
        let output_dir = &format!("{}/ingest_buffer", data_dir);

        let shard = match shard {
            "" => "none",
            _ => shard
        };

        let wal_partition_dir = format!("{}/{}", output_dir, shard);

        fs::create_dir_all(&wal_partition_dir).expect("Failed to create WAL partition directories");

        wal_partition_dir


    }

    fn generate_temp_wal_file_name(namespace: &str, partition: &str, time: Option<i64>, shard: &str) -> String {

        let wal_file_name = BufferChunker::encode_chunk_name(
            "ingest",
            Some(namespace),
            Some(partition),
            time,
            Some(shard),
        );

        let wal_partition_dir = WalFile::get_wal_partition_dir(namespace, partition, time, shard);

        let wal_file_name = format!("{}/{}&id={}", wal_partition_dir, wal_file_name, Helpers::random_str(32));

        format!("{}.tmp", wal_file_name)
    }

    // close file by renaming it .tmp to .wal
    pub fn finish(&mut self) -> io::Result<()> {
        let wal_file_name = self.path.to_str().unwrap().replace(".tmp", ".wal");
        fs::rename(&self.path, &wal_file_name)?;
        self.path = PathBuf::from(wal_file_name);
        Ok(())
    }

    // pub fn close(&mut self) {
    //     let mut file_lock = self.file.write();
    //         // .expect("Failed to lock file for closing");
    //
    //     if file_lock.is_some() {
    //         file_lock.as_mut().unwrap().sync_all().expect("Failed to sync WAL file");
    //         // Drop the file by replacing it with None, which closes the file
    //         *file_lock = None;
    //     }
    // }

}

impl Seek for WalFile {
    fn seek(&mut self, pos: io::SeekFrom) -> std::io::Result<u64> {
        self.get_or_open_file().unwrap().seek(pos)
    }
}

// impl Drop for WalFile {
//     fn drop(&mut self) {
//         // let fd = self.get_or_open_file().unwrap().as_raw_fd();
//         // unsafe {
//         //     libc::fsync(fd);
//         // };
//     }
// }

impl Read for WalFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.get_or_open_file().unwrap().read(buf)
    }
}

impl Write for WalFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.get_or_open_file().unwrap().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let fd = self.get_or_open_file().unwrap().as_raw_fd();
        unsafe {
            libc::fsync(fd);
        };

        Ok(())
    }

    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.get_or_open_file().unwrap().write_all(buf)
    }
}
