use std::fs::{File, OpenOptions};
use std::{fs, io,};
use std::collections::HashMap;
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
use arrow::ipc::CompressionType;
use arrow::ipc::reader::StreamReader;
use arrow::ipc::writer::{IpcWriteOptions, StreamWriter};
use bincode;
use serde_cbor;
use byteorder::LittleEndian;
use datafusion::datasource::MemTable;
use datafusion::parquet::data_type::AsBytes;
use datafusion::prelude::{SessionConfig, SessionContext};
use lazy_static::lazy_static;
use libc::exit;
use once_cell::sync::Lazy;
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;
use serde_derive::{Deserialize, Serialize};
use serde_json::Value;
use tokio::runtime::Runtime;
use yaml_rust::Yaml::Hash;
use zerocopy::U64;
use crate::{ARROW_SCHEMA, BUFFER_FINALISE_RUNNING, RUNNING};
use crate::buffer::BufferChunker;
use crate::helpers::configuration::Config;
use crate::helpers::Helpers;
use crate::helpers::offsets::{Offset, OffsetKey, OffsetValue};
use crate::helpers::timed_rwlock::TimedRwLock;

pub static TOTAL_ROWS: Lazy<TimedRwLock<AtomicU64>> =
    Lazy::new(|| TimedRwLock::new("record_batch_total".to_string(), AtomicU64::new(0)));

lazy_static! {
    static ref WAL_INDEX: TimedRwLock<WalIndex> = TimedRwLock::new("wal_index".to_string(), WalIndex::new());
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OffsetKeySerialize {
    pub(crate) namespace: String,
    pub(crate) partition: String,
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
    pub(crate) records: Vec<IngestRecord>,
}

pub struct WalRecordBatches {
    pub(crate) offset: OffsetKeySerialize,
    pub(crate) record_batches: Vec<RecordBatch>,
}

impl WalRecordBatches {
    pub fn new(offset: OffsetKeySerialize, record_batches: Vec<RecordBatch>) -> Self {
        WalRecordBatches {
            offset,
            record_batches,
        }
    }

    pub fn read_from_stream<R: Read + Seek>(reader: &mut R) -> Result<Self, ArrowError> {

        reader.seek(io::SeekFrom::Start(0)).unwrap();

        let mut offset_size = [0u8; 8];
        reader.read_exact(&mut offset_size).expect("Failed to read offset size from WAL");

        let mut bin_offset = vec![0u8; u64::from_le_bytes(offset_size) as usize];
        reader.read_exact(&mut bin_offset).expect("Failed to read offset from WAL");

        let offset: OffsetKeySerialize = bincode::deserialize(&bin_offset).unwrap();

        // println!("Offset of size: {} value {:?}", u64::from_le_bytes(offset_size), offset);

        // Deserialize RecordBatch using Arrow's IPC format
        // let mut buf = Vec::new();
        // reader.read_to_end(&mut buf)?;
        // let cursor = Cursor::new(buf);
        let mut stream_reader = StreamReader::try_new(reader, None).unwrap();

        let mut record_batches = Vec::new();
        while let Some(batch) = stream_reader.next() {
            let batch = batch.unwrap();
            record_batches.push(batch);
        }

        Ok(WalRecordBatches {
            offset,
            record_batches,
        })
    }

    pub fn write_to_stream<W: Write + Seek>(&mut self, writer: &mut W) -> Result<usize, ArrowError> {

        // seek to start of file
        writer.seek(io::SeekFrom::Start(0))?;

        let bin_offset = bincode::serialize(&self.offset).unwrap();
        let offset_size = bin_offset.len() as u64;

        writer.write_all(&offset_size.as_bytes())?;
        writer.write_all(&bin_offset)?;

        // seek to end of file
        writer.seek(io::SeekFrom::End(0))?;

        let mut size: usize = 0;

        let codec = Some(CompressionType::LZ4_FRAME);
        let options = IpcWriteOptions::default().try_with_compression(codec)?;

        let mut stream_writer = StreamWriter::try_new_with_options(writer, &self.record_batches[0].schema(), options)?;
        for batch in &self.record_batches {
            stream_writer.write(batch).expect("Failed to write record batch to stream writer");
            size += batch.get_array_memory_size();
        }

        stream_writer.finish()?;

        Ok(size)
    }
}

pub struct Buffers {
    buf: IngestBufferBatch,
    index: WalIndex
}

impl Buffers {
    pub fn new() -> Self {
        Buffers {
            buf: IngestBufferBatch {
                offset: OffsetKeySerialize {
                    namespace: "".to_string(),
                    partition: "".to_string(),
                    position: 0,
                },
                records: Vec::new(),
            },
            index: WalIndex::new(),
        }
    }

    pub fn write(&mut self, ingest_buffer_batch: IngestBufferBatch) {
        self.buf = ingest_buffer_batch;

    }

    pub fn flush(&mut self) -> Result<(), ArrowError> {

        let mut buffers: HashMap<(String, String, i64), Vec<IngestRecord>> = HashMap::new();

        for record in self.buf.records.iter_mut() {
            let key = (record.namespace.clone(), record.partition.clone(), record.time.unwrap_or(0));

            buffers.entry(key).or_insert_with(|| Vec::new()).push(record.clone());
        }

        let arrow_schema_guard = ARROW_SCHEMA.read();

        let mut index = WAL_INDEX.write();

        for ((namespace, partition, time), ingest_records) in buffers.iter_mut() {

            // add a new wal file to index.index
            let wal_file_name = format!("{}-{}-{}-{}", namespace, partition, time, Helpers::random_str(32));
            let wal_file = WalFile::new(namespace).unwrap();

            let mut wal_file_partition = index.index.entry((namespace.clone(), partition.clone(), *time)).or_insert_with(
                || WalFilePartition {
                    files: Vec::new(),
                    namespace: namespace.clone(),
                    partition: partition.clone(),
                    time: *time,
                    updated_at: SystemTime::now(),
                    bytes: 0,
                }
            );

            wal_file_partition.files.push(wal_file);

            let arrow_schema = arrow_schema_guard.get(namespace).unwrap().clone();

            let mut decoder = ReaderBuilder::new(arrow_schema).build_decoder().unwrap();

            let mut record_batches = Vec::new();

            for ingest_record in ingest_records.iter_mut() {
                let json_value = serde_json::to_vec(&ingest_record.record).unwrap();

                println!("JSON value: {}", ingest_record.record.to_string());

                decoder.serialize(&json_value).unwrap();

            }

            record_batches.push(decoder.flush().unwrap().unwrap());

            let offset = self.buf.offset.clone();
            let mut ingest_batch = WalRecordBatches::new(offset, record_batches);

            // get the WAL file we just added to the index
            let mut wal_file = wal_file_partition.files.last_mut().unwrap();

            ingest_batch.write_to_stream(&mut wal_file.file.as_mut().unwrap())?;

            wal_file.file.as_mut().unwrap().flush()?;

            wal_file_partition.updated_at = SystemTime::now();
            wal_file_partition.bytes += ingest_batch.record_batches.iter().map(|batch| batch.get_array_memory_size() as u64).sum::<u64>();

            wal_file_partition.check_wal_rotate();


        }


        Ok(())

    }




}

#[derive(Default, Debug)]
struct WalIndex {
    // Maps namespace, partition, and time to WAL file information
    index: HashMap<(String, String, i64), WalFilePartition>,
}

impl WalIndex {
    fn new() -> Self {
        WalIndex {
            index: HashMap::new(),
        }
    }
}

#[derive(Debug)]
struct WalFilePartition {
    files: Vec<WalFile>,
    pub(crate) namespace: String,
    pub(crate) partition: String,
    pub(crate) time: i64,
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

    pub fn check_wal_rotate(&mut self) {
        if self.is_file_size_exceeded() || self.is_file_time_exceeded() {
            // println!("Rotating WAL file: {}", self.name);
            self.output_finalise();
        }
    }

    async fn output_finalise(&mut self) {

        // println!("Finalising output file: {}", filename);

        let data_dir = Config::get_data_dir();

        let output_file_name = BufferChunker::encode_chunk_name(
            "output",
            Some(&self.namespace),
            Some(&self.partition),
            Some(self.time),
            None,
        );

        let temp_file_path = format!("{}/{}/{}-{}.temp", data_dir, "output_buffer", output_file_name, Helpers::random_str(32));
        let write_file = OpenOptions::new()
            .create(true)
            .write(true)
            .open(&temp_file_path)
            .unwrap();

        let write_file = SyncWriteFile::new(write_file).unwrap();

        let props = WriterProperties::builder()
            .set_dictionary_enabled(false)
            .set_encoding(parquet::basic::Encoding::PLAIN)
            .set_compression(Compression::SNAPPY)
            .build();

        let schema = ARROW_SCHEMA.read().get(&self.namespace).unwrap().clone();

        let mut writer = ArrowWriter::try_new(write_file, schema, Some(props)).unwrap();

        for wal_file in self.files.iter_mut() {

            let filename = format!("{}/ingest_buffer/{}.wal", data_dir, wal_file.path.file_name().unwrap().to_str().unwrap());

            let mut reader = io::BufReader::new(File::open(&filename).unwrap());

            // create record batches from arrow IPC WAL file
            let record_batch_ingest_batch = WalRecordBatches::read_from_stream(&mut reader).unwrap();

            /**
            * Support optional SQL query to transform data before writing to parquet
            */
            let mut batches = Vec::new();

            let sql = None; // "SELECT * FROM my_table";

            if sql.is_some() {
                batches = Self::apply_sql_on_ipc_stream(record_batch_ingest_batch.record_batches, sql.unwrap()).await.unwrap();
            } else {
                batches = record_batch_ingest_batch.record_batches;
            }

            for batch in batches {
                match writer.write(&batch) {
                    Ok(_g) => {}
                    Err(_err) => {
                        panic!("Error writing to parquet file: {}", _err.to_string());
                    }
                }
            }

            // tombstone by naming .tombstone and moving to ./done dir
            let tombstone_path = format!("{}/ingest_buffer/done/{}.tombstone", data_dir, Helpers::random_str(32));
            fs::rename(filename, tombstone_path).unwrap();

        }

        writer.close().unwrap();


        let parquet_path = temp_file_path.replace(".temp", ".parquet");
        fs::rename(&temp_file_path, parquet_path).unwrap();

    }

    async fn apply_sql_on_ipc_stream(record_batches: Vec<arrow::array::RecordBatch>, sql: &str) -> Result<Vec<arrow::array::RecordBatch>, Box<dyn std::error::Error>> {

        let mut session_config = SessionConfig::new();
        session_config = session_config.set("datafusion.catalog.information_schema", "true".into());
        session_config = session_config.set("datafusion.catalog.default_catalog", "skippr".into());
        session_config = session_config.set("datafusion.execution.collect_statistics", "true".into());

        let ctx = SessionContext::with_config(session_config);

        // Create a new DataFusion context
        // let mut ctx = ExecutionContext::new();

        // Read batches and register them as a table in the context
        let schema_ref = record_batches[0].schema();

        ctx.register_table("my_table", Arc::new(MemTable::try_new(schema_ref, vec![record_batches])?))?;

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
    pub(crate) bytes: u64,
    pub(crate) file: Option<SyncWriteFile>,
    pub(crate) rotated: Option<bool>,
}

impl WalFile {
    pub fn new(namespace: &str) -> io::Result<Self> {
        let path_str = Self::generate_wal_file_path(namespace);
        // println!("Creating WAL file: {}", path_str);
        let path= PathBuf::from(&path_str);
        let file = OpenOptions::new().append(true).create(true).open(&path)?;
        let file = SyncWriteFile::new(file)?;
        Ok(WalFile {
            path,
            bytes: 0,
            namespace: namespace.to_string(),
            file: Some(file),
            rotated: None,
        })
    }

    fn generate_wal_file_path(name: &str) -> String {
        let data_dir = Config::get_data_dir();
        let output_dir = &format!("{}/ingest_buffer", data_dir);

        format!("{}/{}.wal", output_dir, name)
    }

    fn write_to_wal(&mut self, data: &[u8]) -> io::Result<()> {
        if let Some(sync_file) = &mut self.file.as_mut() {
            sync_file.write_all(data)?;
            self.bytes += data.len() as u64;
            // sync_file.flush()?;
        }
        Ok(())
    }

    pub fn recover_data(&mut self) -> io::Result<Vec<u8>> {
        let mut data = Vec::new();
        if let Some(sync_file) = &mut self.file.as_mut() {
            sync_file.read_to_end(&mut data)?;
        }

        Ok(data)
    }

    pub fn flush(&mut self) -> io::Result<()> {
        if let Some(sync_file) = &mut self.file.as_mut() {

            sync_file.flush()?;

            // unsafe {
            //     libc::fsync(sync_file.file.as_raw_fd());
            // }

        }
        Ok(())
    }

    // @todo - Additional methods for handling file rotation, error handling, etc.
}

#[derive(Debug)]
pub struct SyncWriteFile {
    file: File,
}

impl SyncWriteFile {
    pub fn new(file: File) -> std::io::Result<Self> {
        Ok(SyncWriteFile { file })
    }
}

impl Seek for SyncWriteFile {
    fn seek(&mut self, pos: io::SeekFrom) -> std::io::Result<u64> {
        self.file.seek(pos)
    }
}

impl Drop for SyncWriteFile {
    fn drop(&mut self) {

        // match self.file.sync_all() {
        //     Ok(_g) => {}
        //     Err(_err) => {
        //         println!("Error syncing file: {}", _err.to_string());
        //     }
        // }

        let fd = self.file.as_raw_fd();
        unsafe {
            libc::fsync(fd);
        };
    }
}

// implement read_to_end for SyncWriteFile
impl Read for SyncWriteFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.file.read(buf)
    }
}

impl Write for SyncWriteFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.file.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {

        // match self.file.sync_all() {
        //     Ok(_g) => {}
        //     Err(_err) => {
        //         println!("Error syncing file: {}", _err.to_string());
        //     }
        // }

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
