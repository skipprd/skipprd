use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::{fs, io, thread};
use std::io::{BufRead, Cursor, Read, Write};
use std::ops::Deref;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use arrow::array::RecordBatch;
use arrow::json::ReaderBuilder;
use arrow_schema::{ArrowError, SchemaRef};
use dashmap::DashMap;
use once_cell::sync::Lazy;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::data_type::AsBytes;
use parquet::file::properties::WriterProperties;
use crate::ARROW_SCHEMA;
use crate::buffer::BufferChunker;
use crate::helpers::configuration::Config;
use crate::helpers::Helpers;
use crate::helpers::timed_rwlock::TimedRwLock;

pub static BUFFER_INDEX: Lazy<Arc<TimedRwLock<HashMap<String, Buffers>>>> = Lazy::new(|| {
    Arc::new(TimedRwLock::new("buffer_index".to_string(), HashMap::new()))
});

pub struct Buffer {
    name: &'static str,
    data: Cursor<Vec<u8>>,
    record_batch: Vec<Arc<RecordBatch>>,
    bytes: u64,
    wal_file: Option<WalFile>,
}

impl Buffer {
    pub fn new(name: &str) -> Self {
        Buffer {
            name: Box::leak(name.to_string().into_boxed_str()),
            data: Cursor::new(Vec::new()),
            record_batch: Vec::new(),
            bytes: 0,
            wal_file: WalFile::new(name).ok(),
        }
    }

    pub fn write(&mut self, json_value: &[u8]) {

        self.data.write_all(json_value).unwrap();

        // write to wal, creating or appending file
        if let Some(wal_file) = &mut self.wal_file {
            wal_file.write_to_wal(json_value);
        } else {
            let mut wal_file = WalFile::new(self.name).unwrap();
            wal_file.write_to_wal(json_value);
            self.wal_file = Some(wal_file);
        }

    }

    fn read_from_json<R: BufRead>(
        mut reader: R,
        schema: SchemaRef,
    ) -> Result<impl Iterator<Item = Result<RecordBatch, ArrowError>>, ArrowError> {
        let mut decoder = ReaderBuilder::new(schema).build_decoder()?;
        let mut next = move || {
            loop {
                // Decoder is agnostic that buf doesn't contain whole records
                let buf = reader.fill_buf()?;
                if buf.is_empty() {
                    break; // Input exhausted
                }
                let read = buf.len();
                let decoded = decoder.decode(buf)?;

                // Consume the number of bytes read
                reader.consume(decoded);
                if decoded != read {
                    break; // Read batch size
                }
            }
            decoder.flush()
        };
        Ok(std::iter::from_fn(move || next().transpose()))
    }


    fn json_to_arrow(json_value: &[u8],
                     namespace: &str
                     // , arrow_schema: &Arc<arrow::datatypes::Schema>
    ) -> Result<RecordBatch, arrow::error::ArrowError> {

        let arrow_schema_guard = ARROW_SCHEMA.read();
        let arrow_schema = arrow_schema_guard.get(namespace).unwrap();

        let mut decoder = ReaderBuilder::new(Arc::clone(arrow_schema)).build_decoder().unwrap();
        // decoder.serialize(json_value).unwrap();
        let decoded = decoder.decode(json_value)?;
        let batch = decoder.flush().unwrap().unwrap();
        Ok(batch)
    }

    fn append_record_batches(&self, batch1: &RecordBatch, batch2: &RecordBatch) -> RecordBatch {
        let mut columns = Vec::new();

        if batch1.schema() != batch2.schema() {
            panic!("Schemas of record batches do not match"); // Handle this error appropriately
        }

        for i in 0..batch1.num_columns() {
            let column1 = batch1.column(i);
            let column2 = batch2.column(i);

            // Concatenate the arrays
            let combined_column = match arrow::compute::concat(&[column1.as_ref(), column2.as_ref()]) {
                Ok(array) => array,
                Err(error) => panic!("Error concatenating arrays: {}", error),
            };

            columns.push(combined_column);
        }

        RecordBatch::try_new(batch1.schema(), columns).unwrap() // Handle this error appropriately
    }

    fn calculate_bytes(&self, batch: &Vec<Arc<RecordBatch>>) -> u64 {
        // Calculate the bytes used by the RecordBatch
        batch.iter().map(|batch| batch.get_array_memory_size() as u64).sum()

        // batch.columns().iter().map(|column| column.get_array_memory_size() as u64).sum()
    }

    pub fn clear(&mut self) {
        // self.data = Vec::new();
        self.bytes = 0;
    }

    pub async fn flush(&mut self) {

        /*
         * Flush WAL file
         */
        let mut wal_file = match self.wal_file.take() {
            Some(file) => file,
            None => {
                // It was flushed by another thead between this thread write() and flush()
                // println!("No WAL file found for buffer: {}", self.name);
                return;
            }
        };

        wal_file.flush().unwrap();
        self.wal_file = Some(wal_file);

        /*
         * Write data buffer to arrow record batches
         */
        self.data.set_position(0);
        let reader = io::BufReader::new(self.data.get_ref().as_slice());

        let name = self.name.clone();
        let namespace = BufferChunker::decode_file_namespace(name);

        let arrow_schema_guard = ARROW_SCHEMA.read();
        let arrow_schema = arrow_schema_guard.get(&namespace).unwrap();

        let record_batches = Self::read_from_json(reader, Arc::clone(arrow_schema)).unwrap();
        for batch in record_batches {
            self.record_batch.push(Arc::from(batch.unwrap()));
        }

        // self.bytes = self.calculate_bytes(&self.record_batch);
        // println!("Record batch bytes: {}, Rows {}, Columns {}", self.bytes, self.record_batch.as_ref().unwrap().num_rows(), self.record_batch.as_ref().unwrap().num_columns());

        self.data.get_mut().clear();
        self.data.set_position(0);

    }

    fn is_file_size_exceeded(file: &WalFile) -> bool {
        let buffer_size = Config::get_pipeline_buffer_threshold_bytes();
        file.bytes > buffer_size as u64
    }

    fn is_file_time_exceeded(file: &WalFile) -> bool {
        let ttl = Config::get_pipeline_buffer_threshold_seconds();
        SystemTime::now()
            .duration_since(file.updated_at)
            .unwrap()
            .as_secs()
            > ttl as u64
    }

    pub fn check_file_rotation(&mut self) {
        if let Some(wal_file) = &mut self.wal_file {
            if Self::is_file_size_exceeded(wal_file) || Self::is_file_time_exceeded(wal_file) {
                // println!("Rotating WAL file: {}", wal_file.path.to_str().unwrap());
                self.rotate();
            }
        }
    }

    pub fn rotate(&mut self) {

        let namespace = BufferChunker::decode_file_namespace(self.name);

        let arrow_schema_guard = ARROW_SCHEMA.read();
        let arrow_schema = arrow_schema_guard.get(&namespace).unwrap().clone();

        let file_path = format!("{}/{}/{}-{}.parquet", Config::get_data_dir(), "output_buffer", self.name, Helpers::random_str(32));
        let file = File::create(file_path).unwrap();

        let props = WriterProperties::builder()
            .set_dictionary_enabled(false)
            .set_encoding(parquet::basic::Encoding::PLAIN)
            .set_compression(Compression::SNAPPY)
            .build();

        let mut writer = ArrowWriter::try_new(file, arrow_schema, Some(props)).unwrap();

        self.record_batch.iter().for_each(|batch| {
            match writer.write(batch) {
                Ok(_g) => {}
                Err(_err) => {
                    panic!("Error writing to parquet file: {}", _err.to_string());
                }
            }
        });

        writer.close().unwrap();

        // clear record batch
        self.record_batch.clear();

        // tombstone by renaming, replacing .wal to .tombstone and moving to ./done dir
        let wal_file = self.wal_file.take().unwrap();
        let tombstone_path = format!("{}/done/{}.tombstone", wal_file.path.parent().unwrap().to_str().unwrap(), wal_file.path.file_stem().unwrap().to_str().unwrap());
        fs::rename(wal_file.path, tombstone_path).unwrap();

    }


}

pub struct Buffers {
    pub(crate) buffers: DashMap<String, Buffer>,
}

impl Buffers {
    pub fn new() -> Self {
        Buffers {
            buffers: DashMap::new(),
        }
    }

    pub fn clear(self, key: &str) {
        if let Some(mut buffer) = self.buffers.get_mut(key) {
            buffer.clear();
        }
    }

    pub fn clear_all(self) {
        unimplemented!("clear_all() is not yet implemented");
    }

}

pub struct SyncWriteFile {
    file: File,
}

impl SyncWriteFile {
    pub fn new(file: File) -> std::io::Result<Self> {
        Ok(SyncWriteFile { file })
    }
}

impl Write for SyncWriteFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.file.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // println!("fsync during flush()");

        match self.file.sync_all() {
            Ok(_g) => {}
            Err(_err) => {
                println!("Error syncing file: {}", _err.to_string());
            }
        }

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

pub struct WalFile {
    pub(crate) path: PathBuf,
    pub(crate) bytes: u64,
    pub(crate) updated_at: SystemTime,
    // non buffered writer
    pub(crate) file: TimedRwLock<Option<SyncWriteFile>>,
    pub(crate) rotated: Option<bool>,
}

impl WalFile {
    pub fn new(name: &str) -> io::Result<Self> {
        let path_str = Self::generate_wal_file_path(name);
        println!("Creating WAL file: {}", path_str);
        let path= PathBuf::from(&path_str);
        let file = OpenOptions::new().append(true).create(true).open(&path)?;
        let sync_file = SyncWriteFile::new(file)?;
        Ok(WalFile {
            path,
            bytes: 0,
            updated_at: SystemTime::now(),
            file: TimedRwLock::new(path_str, Some(sync_file)),
            rotated: None,
        })
    }

    fn generate_wal_file_path(name: &str) -> String {
        let data_dir = Config::get_data_dir();
        let output_dir = &format!("{}/ingest_buffer", data_dir);

        format!("{}/{}.wal", output_dir, name)
    }

    fn write_to_wal(&mut self, data: &[u8]) {
        match self.append(data) {
            Ok(_g) => {}
            Err(_err) => {
                panic!("Error writing to WAL file: {}", _err.to_string());
            }
        }
    }

    pub fn append(&mut self, data: &[u8]) -> io::Result<()> {
        if let Some(sync_file) = &mut self.file.write().as_mut() {
            sync_file.write_all(data)?;
            self.bytes += data.len() as u64;
            self.updated_at = SystemTime::now(); // bit slow, perhaps use rust-coarsetime
            // sync_file.flush()?;
        }
        Ok(())
    }

    pub fn flush(&mut self) -> io::Result<()> {
        if let Some(sync_file) = &mut self.file.write().as_mut() {
            sync_file.flush()?;
        }
        Ok(())
    }

    // @todo - Additional methods for handling file rotation, error handling, etc.
}
