use std::fs::{File, OpenOptions};
use std::{fs, io,};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Cursor, Read, Seek, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use arrow::array::RecordBatch;
use arrow::json::ReaderBuilder;
use arrow_schema::{ArrowError, SchemaRef};
use dashmap::DashMap;
use glob::{glob_with, MatchOptions};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use arrow::ipc::{CompressionType, Schema};
use arrow::ipc::reader::StreamReader;
use arrow::ipc::writer::{IpcWriteOptions, StreamWriter};
use bincode;
use serde_cbor;
use byteorder::LittleEndian;
use datafusion::datasource::MemTable;
use datafusion::execution::options::ArrowReadOptions;
use datafusion::parquet::data_type::AsBytes;
use datafusion::prelude::{ParquetReadOptions, SessionConfig, SessionContext};
use icu::properties::sets::print;
use lazy_static::lazy_static;
use libc::exit;
use once_cell::sync::Lazy;
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;
use serde_derive::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::runtime::Runtime;
use yaml_rust::Yaml::Hash;
use zerocopy::U64;
use crate::{ARROW_SCHEMA, BUFFER_FINALISE_RUNNING, RUNNING};
use crate::buffer::BufferChunker;
use crate::helpers::configuration::Config;
use crate::helpers::Helpers;
use crate::helpers::offsets::{Offset, OffsetKey, Offsets, OffsetTypes, OffsetValue};
use crate::helpers::timed_rwlock::TimedRwLock;

pub static TOTAL_ROWS: Lazy<TimedRwLock<AtomicU64>> =
    Lazy::new(|| TimedRwLock::new("record_batch_total".to_string(), AtomicU64::new(0)));

lazy_static! {
    pub static ref WAL_INDEX: TimedRwLock<WalIndex> = TimedRwLock::new("wal_index".to_string(), WalIndex::new());
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub(crate) offset: OffsetKeySerialize,
    pub(crate) namespace: String,
    pub(crate) partition: String,
    pub(crate) time: Option<i64>,
    pub(crate) records: Vec<IngestRecord>,
}

pub struct Buffers {
    buf: HashMap<(String, String, Option<i64>), IngestBufferBatch>,
    index: WalIndex
}

impl Buffers {

    pub fn new() -> Self {
        Buffers {
            buf: HashMap::new(),
            index: WalIndex::new(),
        }
    }


    pub fn write(&mut self, ingest_buffer_batch: HashMap<(String, String, Option<i64>), IngestBufferBatch>) {
        for batch in ingest_buffer_batch {
            self.buf.insert(batch.0, batch.1);
        }

    }

    pub fn flush(&mut self) -> Result<(), ArrowError> {

        let arrow_schema_guard = ARROW_SCHEMA.read();

        let mut index = WAL_INDEX.write();

        let mut rows = 0;

        for ((namespace, partition, time), ingest_buffer_batch) in self.buf.iter_mut() {

            // println!("Writing {} rows to WAL {} {} {}", ingest_buffer_batch.records.len(), namespace, partition, time.unwrap_or(0));

            let mut wal_file = WalFile::new(
                namespace,
                partition,
                *time,
                ingest_buffer_batch.offset.clone(),
            ).unwrap();

            // println!("WAL File {} offset: {:?}", wal_file.path.to_str().unwrap(), ingest_buffer_batch.offset);

            let wal_file_partition = index.index.entry((
                ingest_buffer_batch.namespace.clone(),
                ingest_buffer_batch.partition.clone(),
                ingest_buffer_batch.time.clone()
           )).or_insert_with(
                || WalFilePartition {
                    files: Vec::new(),
                    namespace: ingest_buffer_batch.namespace.clone(),
                    partition: ingest_buffer_batch.partition.clone(),
                    time: ingest_buffer_batch.time.clone(),
                    updated_at: SystemTime::now(),
                    bytes: 0,
                }
            );

            let arrow_schema = arrow_schema_guard.get(&ingest_buffer_batch.namespace).unwrap().clone();

            let mut decoder = ReaderBuilder::new(arrow_schema).build_decoder().unwrap();

            let mut record_batches = Vec::new();

            // @todo - faster to build a vec and pass to decoder?

            rows += ingest_buffer_batch.records.len();

            let json_values = ingest_buffer_batch.records.iter().map(|record| &record.record).collect::<Vec<&Value>>();
            decoder.serialize(&json_values).unwrap();

            record_batches.push(decoder.flush().unwrap().unwrap());


            // let mut ingest_batch = WalRecordBatches::new(offset, record_batches);

            wal_file_partition.bytes += wal_file.write_to_stream(&record_batches)?;

            // wal_file_partition.bytes += wal_file.bytes;
            // println!("Wrote records to WAL file: {}", wal_file.path.to_str().unwrap());

            wal_file.flush()?;

            wal_file_partition.updated_at = SystemTime::now();
            wal_file_partition.files.push(wal_file);

            // let rotated = wal_file_partition.check_wal_rotate();
            //
            // if rotated {
            //     index.index.remove(&(namespace.clone(), partition.clone(), time.clone()));
            // }

        }

        self.buf.clear();

        // println!("Wrote {} rows to WAL", rows);

        Ok(())
    }

    pub async fn compact_all_partitions(force: bool) {
        let mut wal_index = WAL_INDEX.write();

        let mut compacted_index_partitions = Vec::new();

        for (_key, wal_partition) in wal_index.index.iter_mut() {

            if force {
                wal_partition.compact_to_parquet().await;
                compacted_index_partitions.push((wal_partition.namespace.clone(), wal_partition.partition.clone(), wal_partition.time.clone()));

            } else {
                let rotated = wal_partition.check_wal_rotate().await;

                if rotated {
                   compacted_index_partitions.push((wal_partition.namespace.clone(), wal_partition.partition.clone(), wal_partition.time.clone()));
                }
            }
        }

        for (namespace, partition, time) in compacted_index_partitions {
            wal_index.index.remove(&(namespace.clone(), partition.clone(), time.clone()));

            // remove partition dir
            let wal_partition_dir = WalFile::get_wal_partition_dir(&namespace, &partition, time);
            fs::remove_dir_all(wal_partition_dir).unwrap();
        }
    }

}

// #[derive(Default, Debug)]
pub struct WalIndex {
    // Maps namespace, partition, and time to WAL file information
    index: HashMap<(String, String, Option<i64>), WalFilePartition>,
}

impl WalIndex {
    fn new() -> Self {
        WalIndex {
            index: HashMap::new(),
        }
    }

    pub fn recover(&mut self, offsets_db: Arc<Offsets>) -> io::Result<()> {

        let mut count = 0;

        let wal_files = Self::list_wal_files()?;

        if wal_files.len() == 0 {
            return Ok(());
        }

        println!("Recovering from WAL");

        let wal_files_count = wal_files.len();

        for file_path in wal_files {

            // remove file if zero bytes
            if fs::metadata(&file_path)?.len() == 0 {
                fs::remove_file(&file_path)?;
                continue;
            }

            let wal_file = WalFile::from_path(&file_path)?;

            let partition_key = (wal_file.namespace.clone(), wal_file.partition.clone(), wal_file.time.clone());

            let wal_file_partition = self.index.entry(partition_key).or_insert_with(|| WalFilePartition {
                files: Vec::new(),
                namespace: wal_file.namespace.clone(),
                partition: wal_file.partition.clone(),
                time: wal_file.time.clone(),
                updated_at: SystemTime::now(),
                bytes: 0,
            });

            if wal_file_partition.updated_at < wal_file.updated_at {
                wal_file_partition.updated_at = wal_file.updated_at;
            }
            wal_file_partition.bytes += wal_file.bytes;

            wal_file_partition.files.push(wal_file);

            count += 1;
        }

        println!("Syncing offsets to DB");

        // for wal_partition in self.index.values_mut() {
        //     wal_partition.files.sort_by(|a, b| a.file.metadata().unwrap().created().unwrap().cmp(&b.file.metadata().unwrap().created().unwrap()));
        // }

        for (_key, wal_partition) in self.index.iter_mut() {

            // ensure offsets committed
            for wal_file in wal_partition.files.iter_mut() {
                let offset_key = OffsetKey {
                    namespace: wal_file.offset.source_namespace.clone(),
                    partition: wal_file.offset.source_partition.clone(),
                };

                offsets_db.insert(&offset_key, OffsetTypes::Line, wal_file.offset.position);
                offsets_db.insert(&offset_key, OffsetTypes::Closed, 1);
            }
        }

        offsets_db.flush();

        println!("Rebuilt index from {} of {} WAL files", count, wal_files_count);

        Ok(())
    }

    fn list_wal_files() -> io::Result<Vec<PathBuf>> {
        let data_dir = Config::get_data_dir();
        let wal_dir = PathBuf::from(format!("{}/ingest_buffer", data_dir));
        let mut wal_files = Vec::new();
        // for entry in fs::read_dir(wal_dir)? {
        // recursive glob directory
        let options = MatchOptions {
            case_sensitive: false,
            require_literal_separator: false,
            require_literal_leading_dot: false,
        };

        for entry in glob_with(&format!("{}/**/*.wal", wal_dir.to_str().unwrap()), options).expect("Failed to read WAL files") {

            let path = entry.expect("Failed to read WAL file");
            if path.is_file() && path.extension().and_then(OsStr::to_str) == Some("wal") {
                wal_files.push(path);
            }
        }
        Ok(wal_files)
    }

}

#[derive(Debug)]
struct WalFilePartition {
    files: Vec<WalFile>,
    pub(crate) namespace: String,
    pub(crate) partition: String,
    pub(crate) time: Option<i64>,
    updated_at: SystemTime,
    bytes: u64,
}

impl WalFilePartition {
    pub fn recover_from_wal(&mut self) -> io::Result<()> {

        Ok(())
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

    pub async fn check_wal_rotate(&mut self) -> bool {
        if self.is_file_size_exceeded() || self.is_file_time_exceeded() {
            println!("Rotating WAL: {} Bytes: {}, Segment Files {}", self.namespace, self.bytes, self.files.len());
            self.compact_to_parquet().await;

            return true
        }

        false
    }

    async fn compact_to_parquet(&mut self) {
        let data_dir = Config::get_data_dir();
        let output_file_name = BufferChunker::encode_chunk_name(
            "output",
            Some(&self.namespace),
            Some(&self.partition),
            self.time,
            None,
        );



        // let schema = ARROW_SCHEMA.read().get(&self.namespace).unwrap().clone();
        // let wal_file = self.files.last_mut().unwrap();
        // let schema = wal_file.read_schema_from_stream().expect("Failed to read schema from WAL file");
        // let mut writer = ArrowWriter::try_new(write_file, schema, Some(props)).unwrap();

        // let schema = wal_file.read_schema_from_stream().expect("Failed to read schema from WAL file");
        let schema = ARROW_SCHEMA.read().get(&self.namespace).unwrap().clone();

        let namespace = match self.namespace.as_str() {
           "" => "none",
            _ => self.namespace.as_str()
        };
        let partition = match self.partition.as_str() {
            "" => "none",
            _ => self.partition.as_str()
        };
        let time = self.time.unwrap_or(0);
        let sub_dir = format!("{}-{}-{}", namespace, partition, time);
        let temp_parquet_path = format!("{}/{}", data_dir, sub_dir);
        fs::create_dir_all(&temp_parquet_path).unwrap();


        for wal_file in self.files.iter_mut() {

            if wal_file.bytes == 0 {
                continue;
            }

            // check for empty file
            if wal_file.file.metadata().unwrap().len() == 0 {
                println!("Ignoring empty WAL file: {}", wal_file.path.to_str().unwrap());
                continue;
            }

            let temp_file_path = format!("{}/{}-{}.temp", temp_parquet_path, output_file_name, Helpers::random_str(32));

            let record_batches = wal_file.read_from_stream().expect("Failed to read from WAL file");

            let write_file = OpenOptions::new()
                .create(true)
                .write(true)
                .open(&temp_file_path)
                .unwrap();

            let props = WriterProperties::builder()
                .set_dictionary_enabled(false)
                .set_encoding(parquet::basic::Encoding::PLAIN)
                .set_compression(Compression::SNAPPY)
                .build();


            let schema = wal_file.read_schema_from_stream().expect("Failed to read schema from WAL file");
            let mut writer = ArrowWriter::try_new(write_file, schema, Some(props)).unwrap();

            for batch in record_batches {
                writer.write(&batch).expect("Error writing to parquet file");
            }

            writer.close().unwrap();

            let temp_parquet_file = temp_file_path.replace(".temp", ".parquet");
            fs::rename(&temp_file_path, temp_parquet_file).unwrap();

        }


        let parquet_output = format!("{}/{}/{}-{}.parquet", data_dir, "output_buffer", output_file_name, Helpers::random_str(32));


        /**
         * Support optional SQL query to transform data before writing to parquet
         */
        let sql = Some("SELECT * FROM my_table");

        let record_batches = Self::apply_sql_on_ipc_stream(&temp_parquet_path, sql.unwrap(), schema).await.unwrap();

        let write_file = OpenOptions::new()
            .create(true)
            .write(true)
            .open(&parquet_output)
            .unwrap();

        let props = WriterProperties::builder()
            .set_dictionary_enabled(false)
            .set_encoding(parquet::basic::Encoding::PLAIN)
            .set_compression(Compression::SNAPPY)
            .build();

        let schema = ARROW_SCHEMA.read().get(&self.namespace).unwrap().clone();

        let mut writer = ArrowWriter::try_new(write_file, schema.clone(), Some(props)).unwrap();

        for batch in record_batches {
            writer.write(&batch).expect("Error writing to parquet file");
        }

        writer.close().unwrap();

        fs::remove_dir_all(temp_parquet_path).unwrap();


        // Rename the processed WAL file to a tombstone file
        for wal_file in self.files.iter_mut() {
            let tombstone_path = format!("{}/ingest_buffer/done/{}.tombstone", data_dir, Helpers::random_str(32));

            if fs::metadata(&tombstone_path).is_ok() {
                fs::rename(&wal_file.path, tombstone_path).expect("Failed to tombstone WAL file");
            } else {
                println!("Failed to tombstone WAL file: {}, it doesn't exist", wal_file.path.to_str().unwrap());
            }

        }

        let wal_partition_dir = WalFile::get_wal_partition_dir(&self.namespace, &self.partition, self.time);
        fs::remove_dir_all(wal_partition_dir).unwrap();

    }

    async fn apply_sql_on_ipc_stream(temp_parquet_path: &str, sql: &str, schema_ref: SchemaRef) -> Result<Vec<arrow::array::RecordBatch>, Box<dyn std::error::Error>> {

        let mut session_config = SessionConfig::new();
        session_config = session_config.set("datafusion.catalog.information_schema", "true".into());
        session_config = session_config.set("datafusion.catalog.default_catalog", "skippr".into());
        session_config = session_config.set("datafusion.execution.collect_statistics", "true".into());

        let ctx = SessionContext::with_config(session_config);

        // for dir in dirs {
            ctx.register_parquet("my_table", temp_parquet_path, ParquetReadOptions {
                schema: Some(schema_ref.as_ref()),
                file_extension: "parquet",
                table_partition_cols: vec![],
                parquet_pruning: None,
                skip_metadata: Some(true),
                file_sort_order: vec![],
            }).await?;
        // }

        // let df = ctx
        //     .read_parquet(
        //         temp_parquet_path,
        //         ParquetReadOptions::default(),
        //     )
        //     .await?;
        //
        // let results = df.collect().await?;

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

        // Execute the SQL query
        let df = ctx.sql(sql).await?;
        let results = df.collect().await?;

        Ok(results)

    }
}

#[derive(Debug)]
pub struct WalFile {
    pub(crate) path: PathBuf,
    pub(crate) namespace: String,
    pub(crate) partition: String,
    pub(crate) time: Option<i64>,
    pub(crate) bytes: u64,
    pub(crate) file: File,
    pub(crate) updated_at: SystemTime,
    pub(crate) offset: OffsetKeySerialize,
}

impl WalFile {
    pub fn new(namespace: &str, partition: &str, time: Option<i64>, offset: OffsetKeySerialize) -> io::Result<Self> {

        let path_str = Self::generate_wal_file_path(namespace, partition, time);
        let path= PathBuf::from(&path_str);
        let file = OpenOptions::new().write(true).read(true).create(true).open(&path)?;

        Ok(WalFile {
            path,
            bytes: 0,
            namespace: namespace.to_string(),
            partition: partition.to_string(),
            time,
            file,
            updated_at: SystemTime::now(),
            offset
        })
    }

    fn from_path(path: &PathBuf) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).open(&path)?;

        let offset = Self::offset_from_path(path).unwrap();

        let namespace = BufferChunker::decode_file_namespace(path.to_str().unwrap());
        let partition = BufferChunker::decode_file_partition(path.to_str().unwrap());
        let time = BufferChunker::decode_file_time(path.to_str().unwrap());
        let time = if time > 0 { None } else { Some(time) };

        let updated_at = match file.metadata() {
            Ok(metadata) => metadata.modified().unwrap_or_else(|_err| SystemTime::now()),
            Err(_err) => SystemTime::now(),
        };

        Ok(WalFile {
            path: path.clone(),
            bytes: file.metadata().unwrap().len(),
            namespace,
            partition,
            time,
            file,
            updated_at,
            offset
        })
    }

    pub fn offset_from_path(path: &PathBuf) -> io::Result<OffsetKeySerialize> {
        let mut file = OpenOptions::new().read(true).open(&path)?;

        let mut offset_size = [0u8; 8];

        file.read_exact(&mut offset_size).expect("Failed to read offset size from WAL");

        let mut bin_offset = vec![0u8; u64::from_le_bytes(offset_size) as usize];

        let mut reader = io::BufReader::new(file);

        reader.read_exact(&mut bin_offset).expect("Failed to read offset from WAL");

        Ok(bincode::deserialize(&bin_offset).unwrap())
    }

    pub fn seek_offset(reader: &mut BufReader<&File>) -> Result<usize, ArrowError> {

        reader.seek(io::SeekFrom::Start(0)).unwrap();

        // println!("Reading offset from WAL file: {}", self.path.to_str().unwrap());

        let mut offset_size = [0u8; 8];
        reader.read_exact(&mut offset_size).expect("Failed to read offset size from WAL");

        reader.seek(io::SeekFrom::Current(i64::from_le_bytes(offset_size))).unwrap();

       Ok(usize::from_le_bytes(offset_size))

    }

    pub fn read_schema_from_stream(&mut self) -> Result<SchemaRef, ArrowError> {

        let mut reader = io::BufReader::new(&self.file);

        Self::seek_offset(&mut reader)?;

        let mut stream_reader = StreamReader::try_new(reader, None).expect("Failed to create WAL stream reader");

        let schema = stream_reader.schema();

        Ok(schema)
    }

    pub fn read_from_stream(&mut self) -> Result<Vec<RecordBatch>, ArrowError> {

        let mut reader = io::BufReader::new(&self.file);

        Self::seek_offset(&mut reader)?;

        let stream_reader = StreamReader::try_new(reader, None)
            .map_err(|e| ArrowError::from_external_error(Box::new(e)))?;

        let mut record_batches = Vec::new();

        for batch in stream_reader {
            let batch = batch.expect("Failed to read record batch from WAL stream reader");
            record_batches.push(batch);
        }

        Ok(record_batches)
    }

    pub fn write_to_stream(&mut self, record_batches: &[RecordBatch]) -> Result<u64, ArrowError> {
        // let writer = self.file.as_mut().ok_or(ArrowError::IoError("Can't write to WAL file".to_string(), io::Error::new(io::ErrorKind::NotFound, "File not found")))?;
        let mut writer = io::BufWriter::new(&self.file);

        writer.seek(io::SeekFrom::Start(0))?;

        let bin_offset = bincode::serialize(&self.offset).unwrap();

        let offset_size: u64 = bin_offset.len() as u64;

        writer.write_all(&offset_size.to_le_bytes())?;
        writer.write_all(&bin_offset)?;

        writer.seek(io::SeekFrom::End(0))?;

        // let mut size: usize = 0;
        let codec = Some(CompressionType::LZ4_FRAME);
        let options = IpcWriteOptions::default().try_with_compression(codec)?;

        let mut stream_writer = StreamWriter::try_new_with_options(writer, &record_batches[0].schema(), options)?;
        for batch in record_batches {
            // size += batch.get_array_memory_size() / 10; // @todo - approximate 10x compression ratio
            stream_writer.write(batch).expect("Failed to write record batch to stream writer");
        }

        stream_writer.finish()?;

        self.bytes += self.file.metadata().unwrap().len();

        Ok(self.bytes)
    }

    fn get_wal_partition_dir(namespace: &str, partition: &str, time: Option<i64>) -> String {
        let data_dir = Config::get_data_dir();
        let output_dir = &format!("{}/ingest_buffer", data_dir);

        let namespace = match namespace {
            "" => "none",
            _ => namespace
        };

        let partition = match partition {
            "" => "none",
            _ => partition
        };

        let time = time.unwrap_or_else(|| 0);

        let wal_partition_dir = format!("{}/{}/{}/{}", output_dir, namespace, partition, time);

        fs::create_dir_all(&wal_partition_dir).expect("Failed to create WAL partition directories");

        wal_partition_dir


    }

    fn generate_wal_file_path(namespace: &str, partition: &str, time: Option<i64>) -> String {

        let wal_file_name = BufferChunker::encode_chunk_name(
            "ingest",
            Some(namespace),
            Some(partition),
            time,
            None,
        );

        let wal_partition_dir = WalFile::get_wal_partition_dir(namespace, partition, time);

        let wal_file_name = format!("{}/{}-{}", wal_partition_dir, wal_file_name, Helpers::random_str(32));

        format!("{}.wal", wal_file_name)
    }

}

impl Seek for WalFile {
    fn seek(&mut self, pos: io::SeekFrom) -> std::io::Result<u64> {
        self.file.seek(pos)
    }
}

impl Drop for WalFile {
    fn drop(&mut self) {

        let fd = self.file.as_raw_fd();
        unsafe {
            libc::fsync(fd);
        };
    }
}

impl Read for WalFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.file.read(buf)
    }
}

impl Write for WalFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.file.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {

        let fd = self.file.as_raw_fd();
        unsafe {
            libc::fsync(fd);
        };

        Ok(())
    }

    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.file.write_all(buf)
    }
}
