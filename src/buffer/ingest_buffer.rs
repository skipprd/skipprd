use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::{fs, io,};
use std::io::{BufRead, Cursor, Read, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use arrow::array::RecordBatch;
use arrow::json::ReaderBuilder;
use arrow_schema::{ArrowError, SchemaRef};
use dashmap::DashMap;
use glob::{glob_with, MatchOptions};
use std::sync::atomic::Ordering;
use once_cell::sync::Lazy;
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;
use crate::{ARROW_SCHEMA, BUFFER_FINALISE_RUNNING};
use crate::buffer::BufferChunker;
use crate::helpers::configuration::Config;
use crate::helpers::Helpers;
use crate::helpers::timed_rwlock::TimedRwLock;

pub static BUFFER_INDEX: Lazy<Arc<TimedRwLock<HashMap<String, Buffers>>>> = Lazy::new(|| {
    Arc::new(TimedRwLock::new("buffer_index".to_string(), HashMap::new()))
});

pub struct Buffer {
    name: &'static str,
    // data: Cursor<Vec<u8>>,
    // record_batch: Vec<Arc<RecordBatch>>,
    bytes: u64,
    updated_at: SystemTime,
    wal_file: Option<TimedRwLock<WalFile>>,
}

impl Buffer {
    pub fn new(name: &str) -> Self {
        Buffer {
            name: Box::leak(name.to_string().into_boxed_str()),
            // data: Cursor::new(Vec::new()),
            // record_batch: Vec::new(),
            bytes: 0,
            updated_at: SystemTime::now(),
            wal_file: Some(TimedRwLock::new(name.to_string(), WalFile::new(name).unwrap()))
        }
    }

    pub fn write(&mut self, json_value: &[u8]) {

        // self.data.write_all(json_value).unwrap();

        if self.wal_file.is_none() {
            self.wal_file = Some(TimedRwLock::new(self.name.to_string(), WalFile::new(self.name).unwrap()));
        }

        let wal_file = match &self.wal_file {
            Some(file) => file,
            None => {
                // It was flushed by another thead between this thread write() and flush()
                println!("No WAL file found for buffer: {}", self.name);
                return;
            }
        };

        wal_file.write().write_to_wal(json_value);

        self.bytes += json_value.len() as u64;

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


    fn json_to_arrow(json_value: &[u8], namespace: &str) -> Result<RecordBatch, arrow::error::ArrowError> {

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

    // fn calculate_bytes(&mut self) {
    //     self.bytes = self.record_batch.iter().map(|batch| batch.get_array_memory_size() as u64).sum();
    // }

    pub fn clear(&mut self) {
        self.wal_file = None;
        self.bytes = 0;
        self.updated_at = SystemTime::now();
    }

    pub fn flush(&mut self) {

        /*
         * Flush WAL file
         */
        let wal_file = match &self.wal_file {
            Some(file) => file,
            None => {
                // It was flushed by another thead between this thread write() and flush()
                println!("No WAL file found for buffer: {}", self.name);
                return;
            }
        };

        wal_file.write().flush().unwrap();

        /*
         * Write data buffer to arrow record batches
         */
        // self.data.set_position(0);
        // let reader = io::BufReader::new(self.data.get_ref().as_slice());
        //
        // let name = self.name.clone();
        // let namespace = BufferChunker::decode_file_namespace(name);
        //
        // let arrow_schema_guard = ARROW_SCHEMA.read();
        // let arrow_schema = arrow_schema_guard.get(&namespace).unwrap();
        //
        // let record_batches = Self::read_from_json(reader, Arc::clone(arrow_schema)).unwrap();
        // for batch in record_batches {
        //     self.record_batch.push(Arc::from(batch.unwrap()));
        // }
        //
        // self.updated_at = SystemTime::now();
        // self.calculate_bytes();
        // // println!("Record batch bytes: {}, Rows {}", self.bytes, self.record_batch.iter().map(|batch| batch.num_rows()).sum::<usize>());
        //
        // self.data.get_mut().clear();
        // self.data.set_position(0);

        self.updated_at = SystemTime::now();

        self.check_wal_rotate();

    }

    pub fn recover_from_wal(&mut self) -> io::Result<()> {
        if let Some(wal_file) = &self.wal_file {
            self.bytes = wal_file.read().bytes;
            self.updated_at = wal_file.read().path.metadata()?.modified()?;
            // let data = wal_file.recover_data()?;
            // self.data.get_mut().extend(data);
            // self.data.set_position(0);
        }
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
            self.wal_rotate();
        }
    }

    pub fn wal_rotate(&mut self) {

        let wal_file = match &self.wal_file {
            Some(file) => file,
            None => {
                // It was flushed by another thead between this thread write() and flush()
                println!("No WAL file found for buffer: {}", self.name);
                return;
            }
        };

        let current_path = wal_file.read().path.to_str().unwrap().to_string();
        let closed_path = current_path.replace(".wal", "");
        let closed_path = format!("{}-{}.merged", closed_path, Helpers::random_str(32));

        fs::rename(current_path, closed_path).unwrap();

        self.clear();

        // let wal_file = WalFile::new(self.name).unwrap();
        let wal_file = TimedRwLock::new(self.name.to_string(), WalFile::new(self.name).unwrap());
        self.wal_file = Some(wal_file);

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

    pub fn force_rotate() {

        BUFFER_FINALISE_RUNNING.write().store(true, Ordering::SeqCst);

        let data_dir = Config::get_data_dir();

        let options = MatchOptions {
            case_sensitive: false,
            require_literal_separator: false,
            require_literal_leading_dot: false,
        };

        let patterns = vec![
            format!("{}/ingest_buffer/*.wal", data_dir),
            format!("{}/ingest_buffer/*.merged", data_dir),
        ];

        let paths = patterns.iter().flat_map(|pattern| {
            glob_with(pattern, options)
                .expect("Failed to read glob pattern")
                .filter_map(Result::ok)
                .collect::<Vec<_>>()
        }).collect::<Vec<_>>();

        for path in paths {
            let filename = path.to_str().unwrap();
            Self::output_finalise(filename);
        }

        BUFFER_FINALISE_RUNNING
            .write()
            .store(false, Ordering::SeqCst);

    }

    pub fn finalise() {
        if BUFFER_FINALISE_RUNNING.read().load(Ordering::SeqCst) {
            return;
        } else {
            BUFFER_FINALISE_RUNNING.write().store(true, Ordering::SeqCst);
        }

        let data_dir = Config::get_data_dir();

        let options = MatchOptions {
            case_sensitive: false,
            require_literal_separator: false,
            require_literal_leading_dot: false,
        };

        let paths = glob_with(&format!("{}/ingest_buffer/*.merged", data_dir), options)
            .expect("Failed to read glob pattern")
            .filter_map(Result::ok)
            .collect::<Vec<_>>();

        for path in paths
        {
            let filename = path.to_str().unwrap();
            Self::output_finalise(filename);
        }

        BUFFER_FINALISE_RUNNING
            .write()
            .store(false, Ordering::SeqCst);
    }

    pub fn output_finalise(filename: &str) {

        println!("Finalising output file: {}", filename);

        let data_dir = Config::get_data_dir();

        let namespace = BufferChunker::decode_file_namespace(filename);

        let arrow_schema_guard = ARROW_SCHEMA.read();
        let arrow_schema = arrow_schema_guard.get(&namespace).unwrap().clone();

        let skpr_namespace = BufferChunker::decode_file_namespace(filename);
        let skpr_partition = BufferChunker::decode_file_partition(filename);
        let source_time = BufferChunker::decode_file_time(filename);
        let shard = BufferChunker::decode_file_shard(filename);

        let output_file_name = BufferChunker::encode_chunk_name(
            "output",
            Some(&skpr_namespace),
            Some(&skpr_partition),
            Some(source_time),
            None,
        );

        let temp_file_path = format!("{}/{}/{}-{}.temp", data_dir, "output_buffer", output_file_name, Helpers::random_str(32));
        let write_file = OpenOptions::new().write(true).create(true).open(&temp_file_path).unwrap();

        let props = WriterProperties::builder()
            .set_dictionary_enabled(false)
            .set_encoding(parquet::basic::Encoding::PLAIN)
            .set_compression(Compression::SNAPPY)
            .build();


        let reader = io::BufReader::new(File::open(&filename).unwrap());

        let record_batches = Buffer::read_from_json(reader, Arc::clone(&arrow_schema)).unwrap();

        let mut writer = ArrowWriter::try_new(write_file, arrow_schema, Some(props)).unwrap();

        for batch in record_batches {
            let batch = batch.unwrap();
            match writer.write(&batch) {
                Ok(_g) => {}
                Err(_err) => {
                    panic!("Error writing to parquet file: {}", _err.to_string());
                }
            }
        }

        writer.close().unwrap();

        let parquet_path = temp_file_path.replace(".temp", ".parquet");
        fs::rename(&temp_file_path, parquet_path).unwrap();

        // tombstone by renaming, replacing .wal to .tombstone and moving to ./done dir
        let tombstone_path = format!("{}/ingest_buffer/done/{}.tombstone", data_dir, Helpers::random_str(32));
        fs::rename(filename, tombstone_path).unwrap();

    }

}

pub struct WalFile {
    pub(crate) path: PathBuf,
    pub(crate) bytes: u64,
    // non buffered writer
    pub(crate) file: TimedRwLock<Option<File>>,
    pub(crate) rotated: Option<bool>,
}

impl WalFile {
    pub fn new(name: &str) -> io::Result<Self> {
        let path_str = Self::generate_wal_file_path(name);
        // println!("Creating WAL file: {}", path_str);
        let path= PathBuf::from(&path_str);
        let file = OpenOptions::new().append(true).create(true).open(&path)?;
        Ok(WalFile {
            path,
            bytes: 0,
            file: TimedRwLock::new(path_str, Some(file)),
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
            // sync_file.flush()?;
        }
        Ok(())
    }

    pub fn recover_data(&self) -> io::Result<Vec<u8>> {
        let mut data = Vec::new();
        if let Some(sync_file) = &mut self.file.write().as_mut() {
            sync_file.read_to_end(&mut data)?;
        }

        Ok(data)
    }

    pub fn flush(&mut self) -> io::Result<()> {
        if let Some(sync_file) = &mut self.file.write().as_mut() {
            // sync_file.flush()?;

            unsafe {
                libc::fsync(sync_file.as_raw_fd());
            }

        }
        Ok(())
    }

    // @todo - Additional methods for handling file rotation, error handling, etc.
}
