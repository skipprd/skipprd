use std::fs::{File, OpenOptions};
use std::{fs, io,};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Cursor, Read, Seek, Write};
use std::ops::{Deref, Index};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use arrow::array::{Array, ArrayRef, RecordBatch};
use arrow::json::ReaderBuilder;
use arrow_schema::{ArrowError, DataType, Field, SchemaRef, Schema};
use dashmap::DashMap;
use glob::{glob_with, MatchOptions};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use arrow::compute::concat;
use arrow::ipc::{CompressionType};
use arrow::ipc::reader::StreamReader;
use arrow::ipc::writer::{IpcWriteOptions, StreamWriter};
use arrow::record_batch::RecordBatchOptions;
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
use crate::{ARROW_SCHEMA, METRICS, RUNNING};
use crate::buffer::BufferChunker;
use crate::helpers::configuration::Config;
use crate::helpers::Helpers;
use crate::helpers::offsets::{Offset, OffsetKey, Offsets, OffsetTypes, OffsetValue};
use crate::helpers::timed_rwlock::TimedRwLock;

pub static TOTAL_ROWS: Lazy<TimedRwLock<AtomicU64>> =
    Lazy::new(|| TimedRwLock::new("record_batch_total".to_string(), AtomicU64::new(0)));

pub static WAL_INDEX: Lazy<TimedRwLock<WalIndex>> = Lazy::new(|| TimedRwLock::new("wal_index".to_string(), WalIndex::new()));

// lazy_static! {
//     pub static ref WAL_INDEX: TimedRwLock<WalIndex> = TimedRwLock::new("wal_index".to_string(), WalIndex::new());
// }

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
    pub(crate) shard: String,
    pub(crate) records: Vec<IngestRecord>,
}

pub struct Buffers {
    buf: HashMap<(String, String, Option<i64>, String), IngestBufferBatch>,
}

impl Buffers {

    pub fn new() -> Self {
        Buffers {
            buf: HashMap::new(),
        }
    }


    pub fn write(&mut self, ingest_buffer_batch: HashMap<(String, String, Option<i64>, String), IngestBufferBatch>) {
        for batch in ingest_buffer_batch {
            self.buf.insert(batch.0, batch.1);
        }

    }

    pub fn flush(&mut self) -> Result<(), ArrowError> {

        let arrow_schema_guard = ARROW_SCHEMA.read();

        // let mut index = WAL_INDEX.write();

        let mut rows = 0;

        // println!("Flushing {} WAL files", self.buf.len());

        for ((namespace, partition, time, shard), ingest_buffer_batch) in self.buf.iter_mut() {

            // println!("Writing {} rows to WAL {} {} {} {}", ingest_buffer_batch.records.len(), namespace, partition, time.unwrap_or(0), shard);

            let mut wal_file = WalFile::new(
                namespace,
                partition,
                *time,
                shard,
                ingest_buffer_batch.offset.clone(),
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

            let arrow_schema = arrow_schema_guard.get(&ingest_buffer_batch.namespace).unwrap().clone();

            let mut decoder = ReaderBuilder::new(arrow_schema).build_decoder().unwrap();

            let mut record_batches = Vec::new();

            // @todo - faster to build a vec and pass to decoder?

            rows += ingest_buffer_batch.records.len();

            let json_values = ingest_buffer_batch.records.iter().map(|record| &record.record).collect::<Vec<&Value>>();
            decoder.serialize(&json_values).unwrap();

            record_batches.push(decoder.flush().unwrap().unwrap());


            // let mut ingest_batch = WalRecordBatches::new(offset, record_batches);

            wal_file.write_to_stream(&record_batches)?;
            // wal_file_partition.bytes += wal_file.write_to_stream(&record_batches)?;

            // wal_file_partition.bytes += wal_file.bytes;
            // println!("Wrote records to WAL file: {}", wal_file.path.to_str().unwrap());

            wal_file.flush()?;

            wal_file.close()?;

            // wal_file_partition.updated_at = SystemTime::now();
            // wal_file_partition.files.push(wal_file);

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

    pub async fn compact_all_partitions(force: bool, offsets_db: Arc<Offsets>) {
        let mut wal_index = WAL_INDEX.write();

        wal_index.recover(offsets_db).expect("Failed to recover WAL index");

        let mut compacted_index_partitions = Vec::new();

        for (_key, wal_partition) in wal_index.index.iter_mut() {

            if force {
                wal_partition.compact_batches_to_parquet().await;
                compacted_index_partitions.push((wal_partition.namespace.clone(), wal_partition.partition.clone(), wal_partition.time.clone(), wal_partition.shard.clone()));

            } else {
                let rotated = wal_partition.check_wal_rotate().await;

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

        wal_index.index.clear();
    }

}

// #[derive(Default, Debug)]
pub struct WalIndex {
    // Maps namespace, partition, and time to WAL file information
    index: HashMap<(String, String, Option<i64>, String), WalFilePartition>,
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

        println!("Indexing WAL");

        let wal_files_count = wal_files.len();

        for file_path in wal_files {

            // remove file if zero bytes
            if fs::metadata(&file_path)?.len() == 0 {
                // fs::remove_file(&file_path)?; // not now we're always indexing
                continue;
            }

            let wal_file = WalFile::from_path(&file_path)?;

            let partition_key = (wal_file.namespace.clone(), wal_file.partition.clone(), wal_file.time.clone(), wal_file.shard.clone());

            let wal_file_partition = self.index.entry(partition_key).or_insert_with(|| WalFilePartition {
                files: Vec::new(),
                namespace: wal_file.namespace.clone(),
                partition: wal_file.partition.clone(),
                time: wal_file.time.clone(),
                shard: wal_file.shard.clone(),
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

        println!("Indexed {} of {} WAL files", count, wal_files_count);

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
    pub(crate) shard: String,
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
            self.compact_batches_to_parquet().await;

            return true
        }

        false
    }

    async fn compact_batches_to_parquet(&mut self) {
        let data_dir = Config::get_data_dir();
        let output_file_name = BufferChunker::encode_chunk_name(
            "output",
            Some(&self.namespace),
            Some(&self.partition),
            self.time,
            Some(&self.shard)
        );


        // let schema = ARROW_SCHEMA.read().get(&self.namespace).unwrap().clone();

        let temp_file_path = format!("{}/{}-{}.temp", data_dir, output_file_name, Helpers::random_str(32));

        let mut record_batches = Vec::new();

        let mut compacted_files = Vec::new();

        for wal_file in self.files.iter_mut() {
            if wal_file.bytes == 0 {
                println!("Ignoring empty bytes WAL file: {}", wal_file.path.to_str().unwrap());
                continue;
            }

            // check for empty file
            if wal_file.file.metadata().unwrap().len() == 0 {
                println!("Ignoring empty WAL file: {}", wal_file.path.to_str().unwrap());
                continue;
            } else {
                // println!("Reading WAL file: {}, of bytes: {}", wal_file.path.to_str().unwrap(), wal_file.file.metadata().unwrap().len());

                let mut total_rows = TOTAL_ROWS.read().load(Ordering::Relaxed);
                total_rows += 1;
                TOTAL_ROWS.read().store(total_rows, Ordering::Relaxed);

            }

            compacted_files.push(wal_file.path.clone());

            record_batches.extend(wal_file.read_from_stream()
                .expect(format!("Failed to read from WAL file: {} of bytes: {}", wal_file.path.to_str().unwrap(), wal_file.file.metadata().unwrap().len()).as_str()));

            wal_file.flush().expect("Failed to flush WAL file");
        }

        // println!("Schema: {:?}", schema);

        // order record batches arrays by schema order
        // Self::recusive_sort(schema.clone(), &mut record_batches);

        // let batch = arrow::compute::concat_batches(&schema, &record_batches).unwrap();

        let batch_schema = record_batches[0].schema();
        let batch = Self::concat_batches(&batch_schema, &record_batches).unwrap();

        // let empty_schema = arrow_schema::Schema::empty();
        // let empty_schema = Arc::new(empty_schema);
        // let batch = arrow::compute::concat_batches(&empty_schema, &record_batches).unwrap();

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

        let mut writer = ArrowWriter::try_new(write_file, batch_schema.clone(), Some(props)).unwrap();

        writer.write(&batch).expect("Error writing to parquet file");

        writer.close().unwrap();

        let parquet_output = format!("{}/{}/{}-{}.parquet", data_dir, "output_buffer", output_file_name, Helpers::random_str(32));

        fs::rename(&temp_file_path, parquet_output).unwrap();

        // Rename the processed WAL file to a tombstone file
        for wal_file_path in compacted_files {
            let tombstone_path = format!("{}/ingest_buffer/done/{}.tombstone", data_dir, Helpers::random_str(32));

            // if fs::metadata(&wal_file_path).is_ok() {
                match fs::rename(&wal_file_path, tombstone_path) {
                    Ok(_) => {
                        // println!("Tombstoned WAL file: {}", wal_file_path.to_str().unwrap());
                    },
                    Err(e) => {
                        println!("Failed to tombstone WAL file: {}, Error: {}", wal_file_path.to_str().unwrap(), e);
                    }

                }
                    //.expect(format!("Failed to tombstone WAL file: {}, it doesn't exist", wal_file_path.to_str().unwrap()).as_str());
            // } else {
            //     println!("Failed to tombstone WAL file: {}, it doesn't exist", wal_file_path.to_str().unwrap());
            // }

        }

    }

    pub fn concat_batches<'a>(
        schema: &SchemaRef,
        input_batches: impl IntoIterator<Item = &'a RecordBatch>,
    ) -> Result<RecordBatch, ArrowError> {

        let batches: Vec<&RecordBatch> = input_batches.into_iter().collect();
        if batches.is_empty() {
            return Ok(RecordBatch::new_empty(schema.clone()));
        }
        let field_num = schema.fields().len();
        let mut arrays = Vec::with_capacity(field_num);
        for i in 0..field_num {
            let array = concat(
                &batches
                    .iter()
                    .map(|batch| {
                        // batch.column(i).as_ref()
                        let field = schema.field(i);
                        let field_name = field.name();
                        let index = batch.schema().index_of(field_name).unwrap();

                        // println!("Schema field: {} {}, Batch field {} {}", field_name, i, batch.schema().field(index).name(), index);

                        batch.column(index).as_ref()
                    })
                    .collect::<Vec<_>>(),
            )?;
            arrays.push(array);
        }

        let mut options = RecordBatchOptions::new();
        options.match_field_names = false;

        RecordBatch::try_new_with_options(schema.clone(), arrays, &options)
    }

    fn recusive_sort(schema_ref: SchemaRef, record_batches: &mut Vec<RecordBatch>) -> Vec<RecordBatch> {
        let mut sorted_batches = Vec::new();

        let schema = schema_ref.as_ref();

        for batch in record_batches {
            let mut sorted_batch = Vec::new();

            let field_num = schema.fields().len();
            for i in 0..field_num {

            // for field in schema.fields() {

                let field = schema.field(i);

                println!("Field: {}", field.name());

                match field.data_type() {
                    DataType::Struct(_) => {

                        let index = batch.schema().index_of(field.name()).unwrap();
                        sorted_batch.push(batch.column(index).clone());

                        println!("Schema struct field: {} {}, Batch field {} {}", field.name(), i, batch.schema().field(index).name(), index);

                        // println!("schema: {:?} \n", field);


                        // Find the index of the parent field in the original schema
                        let parent_field_index = schema.index_of(field.name()).ok().unwrap();

                        // Get the parent field from the original schema
                        let parent_field = schema.field(parent_field_index);

                        // Check if the parent field is a struct
                        let struct_data_type = match parent_field.data_type() {
                            DataType::Struct(fields) => fields,
                            _ => panic!("Parent field is not a struct")
                        };

                        // Create a new schema with the subfields of the parent field
                        let field_schema = Arc::new(Schema::new(struct_data_type.clone()));

                        println!("field schema: {:?} \n", field_schema);


                        let sub_batch = batch.column(index).clone();
                        // let sub_batch_record_batches: RecordBatch = sub_batch.as_any().downcast_ref::<RecordBatch>().unwrap().clone();

                        // Create a vector of array references, one for each column
                        let mut columns: Vec<ArrayRef> = vec![];



                        // let mut sub_batch_record_batches: Vec<RecordBatch> = sub_batch.as_any().downcast_ref::<Vec<RecordBatch>>().unwrap().clone();



                        // let mut subfield_batches: Vec<RecordBatch> = Vec::new();
                        //
                        // for subfield in struct_data_type {
                        // // //
                        // // //     println!("sub field: {:?}", subfield);
                        //     let subfield_index = field_schema.index_of(subfield.name()).unwrap();
                        // // //
                        // // //     let batch_schema = batch.schema();
                        // // //     let sub_batch_schema = Arc::new(Schema::new(batch_schema));
                        // // //
                        // // //     let sub_batch: RecordBatch = RecordBatch::try_new(sub_batch_schema, vec![batch.column(subfield_index).clone()]).unwrap();
                        // // //
                        // // //     // println!("field data: {:?} \n", sub_batch);
                        // // //
                        // // //     subfield_batches.push(sub_batch);
                        // //
                        // //     let sub_batch = batch.column(index).clone();
                        // //
                        //
                        //     let sub_batch_field = batch.column(subfield_index).clone();
                        //     columns.push(sub_batch_field);
                        // }


                        let index = batch.schema().index_of(field.name()).unwrap();
                        let foo = batch.column(index).clone();

                        let sub_batch_schema_index = batch.schema().index_of(field.name()).unwrap();
                        let sub_batch_schema = batch.schema().field(sub_batch_schema_index).clone();
                        let sub_batch_schema = match sub_batch_schema.data_type() {
                            DataType::Struct(fields) => fields,
                            _ => panic!("Parent field is not a struct")
                        };

                        let sub_batch_schema = Arc::new(Schema::new(sub_batch_schema.clone()));

                        // create record batch with the data and schema of the sub batch.
                        // We'll later parse this with the expected schema
                        let mut sub_batch_record_batches = vec![RecordBatch::try_new(sub_batch_schema, vec![foo]).unwrap()];

                        // let mut sub_batch_record_batches = vec![RecordBatch::try_new(field_schema.clone(), columns).unwrap()];


                        // let subfield_batches = RecordBatch::try_new(field_schema.clone(), subfield_batches).unwrap();

                        // println!("field data: {:?} \n", subfield_batches);

                        let sorted_array = Self::recusive_sort(field_schema, &mut sub_batch_record_batches);
                        // let sorted_array = Self::recusive_sort_feild(&field_schema, &mut subfield_batches);

                        // println!("sorted array: {:?} \n", sorted_array);

                        // return vec![RecordBatch::try_new(field_schema.clone(), sorted_array).unwrap()];
                        return sorted_array;



                    },
                    DataType::List(_) => {

                        // println!("Schema list field: {} {}, Batch field {} {}", field.name(), i, batch.schema().field(index).name(), index);
                        let index = batch.schema().index_of(field.name()).unwrap();
                        sorted_batch.push(batch.column(index).clone());

                        println!("Schema list field: {} {}, Batch field {} {}", field.name(), i, batch.schema().field(index).name(), index);

                    },
                    _ => {
                        // println!("Data type: {:?}", field.data_type());
                        let index = batch.schema().index_of(field.name()).unwrap();
                        sorted_batch.push(batch.column(index).clone());

                        println!("Schema field: {} {}, Batch field {} {}", field.name(), i, batch.schema().field(index).name(), index);

                    }
                }

                // let field_name = field.name();
                // let index = batch.schema().index_of(field_name).unwrap();
                // sorted_batch.push(batch.column(index).clone());
            }

            let mut options = RecordBatchOptions::new();
            options.match_field_names = false;

            sorted_batches.push(RecordBatch::try_new_with_options(schema_ref.clone(), sorted_batch, &options).unwrap());
        }

        sorted_batches
    }

    fn recusive_sort_feild(schema_ref: &SchemaRef, array: &mut Vec<arrow::array::ArrayRef>) -> Vec<arrow::array::ArrayRef> {

        let mut sorted_batch = Vec::new();

        let field_num = schema_ref.fields().len();

        for i in 0..field_num {

            let field = schema_ref.field(i);

            println!("Sub Field: {}, Data Type: {}", field.name(), field.data_type());

            // let mut sorted_fields = Vec::new();

            match field.data_type() {
                DataType::Struct(_) => {
                    println!("Struct sub field sort: {}", field.name());
                    // println!("{:?}", field);
                    for field in Self::_fields(field.data_type()) {

                        // println!("sub field iter name: {}", field.name());
                        let field_schema = Arc::new(Schema::new(vec![field.clone()]));

                        let mut sub_array = vec![array[i].clone()];

                        let sub_field = Self::recusive_sort_feild(&field_schema, &mut sub_array);

                        println!("sub field struct field: {}", field.name());
                        sorted_batch.extend(sub_field);
                    }
                },
                // Field { name: \"tags\", data_type: List(Field { name: \"item\", data_type: Struct([Field { name: \"value\", data_type: Utf8, nullable: true, dict_id: 0, dict_is_ordered: false, metadata: {} }, Field { name: \"name\", data_type: Utf8, nullable: true, dict_id: 0, dict_is_ordered: false, metadata: {} }]), nullable: true, dict_id: 0, dict_is_ordered: false, metadata: {} }), nullable: true, dict_id: 0, dict_is_ordered: false, metadata: {} }]
                DataType::List(_) => {

                    println!("List sub field sort: {}", field.name());

                    for field in Self::_fields(field.data_type()) {
                        println!("sub field list name: {}", field.name());
                        let field_schema = Arc::new(Schema::new(vec![field.clone()]));

                        let mut sub_array = vec![array[i].clone()];
                        let sub_field = Self::recusive_sort_feild(&field_schema, &mut sub_array);

                        println!("sub field list field: {}", field.name());
                        sorted_batch.extend(sub_field);
                    }

                    // sorted_batch.push(sub_field);
                },
                _ => {
                    // arrow::compute::sort(array, None).unwrap();

                    let field_name = field.name();
                    // let index = batch.schema().index_of(field_name).unwrap();
                    // let index = field.
                    // sorted_batch.push(batch.column(index).clone());

                    let index = schema_ref.index_of(field_name).unwrap();
                    let sub_field = array[index].clone();

                    // println!("Sub field value sort: {}", field.name());

                    println!("Schema field: {} {}, Array field {}", field.name(), i, index);
                    // println!("{:?}", sub_field);

                    sorted_batch.push(sub_field);
                }
            }
        }

        return sorted_batch;
    }

    fn _fields(dt: &DataType) -> Vec<&Field> {
        match dt {
            DataType::Struct(fields) => fields.iter().flat_map(|f| Self::_fields(f.data_type())).collect(),
            DataType::Union(fields, _) => fields.iter().flat_map(|(_, f)| Self::_fields(f.data_type())).collect(),
            DataType::List(field)
            | DataType::LargeList(field)
            | DataType::FixedSizeList(field, _)
            | DataType::Map(field, _) => Self::_fields(field.data_type()),
            DataType::Dictionary(_, value_field) => Self::_fields(value_field.as_ref()),
            _ => vec![],
        }
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
        let sub_dir = format!("tmp/{}-{}-{}", namespace, partition, time);
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

        let wal_partition_dir = WalFile::get_wal_partition_dir(&self.namespace, &self.partition, self.time, &self.shard);
        fs::remove_dir_all(wal_partition_dir).unwrap();

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

#[derive(Debug)]
pub struct WalFile {
    pub(crate) path: PathBuf,
    pub(crate) namespace: String,
    pub(crate) partition: String,
    pub(crate) time: Option<i64>,
    pub(crate) shard: String,
    pub(crate) bytes: u64,
    pub(crate) file: File,
    pub(crate) updated_at: SystemTime,
    pub(crate) offset: OffsetKeySerialize,
}

impl WalFile {
    pub fn new(namespace: &str, partition: &str, time: Option<i64>, shard: &str, offset: OffsetKeySerialize) -> io::Result<Self> {

        let path_str = Self::generate_temp_wal_file_name(namespace, partition, time, shard);
        let path= PathBuf::from(&path_str);
        let file = OpenOptions::new().write(true).read(true).create(true).open(&path)?;

        Ok(WalFile {
            path,
            bytes: 0,
            namespace: namespace.to_string(),
            partition: partition.to_string(),
            time,
            shard: shard.to_string(),
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

        let shard = BufferChunker::decode_file_shard(path.to_str().unwrap());

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
            shard,
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
    pub fn close(&mut self) -> io::Result<()> {
        let wal_file_name = self.path.to_str().unwrap().replace(".tmp", ".wal");
        fs::rename(&self.path, &wal_file_name)?;
        self.path = PathBuf::from(wal_file_name);
        Ok(())
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
