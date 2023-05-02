
use crate::discover::{Metadata};
use crate::helpers::Helpers;
use arrow::datatypes::Schema;
// use clap::{Parser, ValueHint};
use parquet::{
    arrow::ArrowWriter,
    file::properties::{WriterProperties},
};
use arrow::json::{ReaderBuilder};
use serde::{Deserialize, Serialize};



use std::collections::HashMap;
use std::fs;
use std::fs::{File, OpenOptions};
use std::io::{Write};

use std::path::{PathBuf};

use std::sync::{Arc};
use crate::buffer::BufferChunker;
use crate::helpers::configuration::Config;

// #[derive(clap::ValueEnum, Clone)]
// #[allow(non_camel_case_types, clippy::upper_case_acronyms)]
// enum ParquetCompression {
//     UNCOMPRESSED,
//     SNAPPY,
//     GZIP,
//     LZO,
//     BROTLI,
//     LZ4,
//     ZSTD,
// }
//
// #[derive(clap::ValueEnum, Clone)]
// #[allow(non_camel_case_types, clippy::upper_case_acronyms)]
// enum ParquetEncoding {
//     PLAIN,
//     RLE,
//     BIT_PACKED,
//     DELTA_BINARY_PACKED,
//     DELTA_LENGTH_BYTE_ARRAY,
//     DELTA_BYTE_ARRAY,
//     RLE_DICTIONARY,
// }
//
// #[derive(clap::ValueEnum, Clone)]
// #[allow(non_camel_case_types, clippy::upper_case_acronyms)]
// enum ParquetEnabledStatistics {
//     None,
//     Chunk,
//     Page,
// }
//
// #[derive(Parser)]
// #[clap(version = env!("CARGO_PKG_VERSION"), author = "Dominik Moritz <domoritz@cmu.edu>")]
// struct Opts {
//     /// Input JSON file.
//     // #[clap(name = "JSON", value_parser, value_hint = ValueHint::AnyPath)]
//     // input: PathBuf,
//
//     /// Output file.
//     // #[clap(name = "PARQUET", value_parser, value_hint = ValueHint::AnyPath)]
//     // output: PathBuf,
//
//     /// File with Arrow schema in JSON format.
//     #[clap(short = 's', long, value_parser, value_hint = ValueHint::AnyPath)]
//     schema_file: Option<PathBuf>,
//
//     /// The number of records to infer the schema from. All rows if not present. Setting max-read-records to zero will stop schema inference and all columns will be string typed.
//     #[clap(long)]
//     max_read_records: Option<usize>,
//
//     /// Set the compression.
//     #[clap(short, long, value_parser)]
//     compression: Option<ParquetCompression>,
//
//     /// Sets encoding for any column.
//     #[clap(short, long, value_parser)]
//     encoding: Option<ParquetEncoding>,
//
//     /// Sets data page size limit.
//     #[clap(long)]
//     data_pagesize_limit: Option<usize>,
//
//     /// Sets dictionary page size limit.
//     #[clap(long)]
//     dictionary_pagesize_limit: Option<usize>,
//
//     /// Sets write batch size.
//     #[clap(long)]
//     write_batch_size: Option<usize>,
//
//     /// Sets max size for a row group.
//     #[clap(long)]
//     max_row_group_size: Option<usize>,
//
//     /// Sets "created by" property.
//     #[clap(long)]
//     created_by: Option<String>,
//
//     /// Sets flag to enable/disable dictionary encoding for any column.
//     #[clap(long)]
//     dictionary: bool,
//
//     /// Sets flag to enable/disable statistics for any column.
//     #[clap(long, value_parser)]
//     statistics: Option<ParquetEnabledStatistics>,
//
//     /// Sets max statistics size for any column. Applicable only if statistics are enabled.
//     #[clap(long)]
//     max_statistics_size: Option<usize>,
//
//     /// Print the schema to stderr.
//     #[clap(short, long)]
//     print_schema: bool,
//
//     /// Only print the schema
//     #[clap(short = 'n', long)]
//     dry: bool,
// }

#[derive(Serialize, Deserialize, Debug)]
pub struct SerdeParquet {
    pub supported_compression_types: Vec<String>,
    pub compression_type: String,
    pub fh: String,
    pub records: Vec<String>,
}

impl SerdeParquet {
    pub fn new() -> SerdeParquet {
        SerdeParquet {
            supported_compression_types: vec![
                String::from("VALUE_COMPRESSION"),
                String::from("NO_COMPRESSION"),
            ],
            compression_type: String::from("NO_COMPRESSION"),
            fh: String::from(""),
            records: vec![],
        }
    }

    // pub fn deserialize(record: String) -> Vec<Value> {
    //
    // }
    //
    // pub fn open_writer(&mut self, filename: String, schema: Vec<Value>) {
    //
    //     self.fh = filename;
    // }
    //
    // pub fn close_writer(&mut self) {
    //
    // }

    pub fn serialize(path: PathBuf, mut schema_ref: Arc<Schema>) -> Arc<Schema> {
    // pub fn serialize(path: PathBuf, mut schema_ref: Schema) -> Schema {

        // println!("Arrow schema: {:?}", schema_ref);

        // let opts: Opts = Opts::parse();

        // let mut props = WriterProperties::builder().set_dictionary_enabled(opts.dictionary);
        let props = WriterProperties::builder().set_dictionary_enabled(false);

        // if let Some(statistics) = opts.statistics {
        //     let statistics = match statistics {
        //         ParquetEnabledStatistics::Chunk => EnabledStatistics::Chunk,
        //         ParquetEnabledStatistics::Page => EnabledStatistics::Page,
        //         ParquetEnabledStatistics::None => EnabledStatistics::None,
        //     };
        //
        //     props = props.set_statistics_enabled(statistics);
        // }
        //
        // if let Some(compression) = opts.compression {
        //     let compression = match compression {
        //         ParquetCompression::UNCOMPRESSED => Compression::UNCOMPRESSED,
        //         ParquetCompression::SNAPPY => Compression::SNAPPY,
        //         ParquetCompression::GZIP => Compression::GZIP,
        //         ParquetCompression::LZO => Compression::LZO,
        //         ParquetCompression::BROTLI => Compression::BROTLI,
        //         ParquetCompression::LZ4 => Compression::LZ4,
        //         ParquetCompression::ZSTD => Compression::ZSTD,
        //     };
        //
        //     props = props.set_compression(compression);
        // }
        //
        // if let Some(encoding) = opts.encoding {
        //     let encoding = match encoding {
        //         ParquetEncoding::PLAIN => Encoding::PLAIN,
        //         ParquetEncoding::RLE => Encoding::RLE,
        //         ParquetEncoding::BIT_PACKED => Encoding::BIT_PACKED,
        //         ParquetEncoding::DELTA_BINARY_PACKED => Encoding::DELTA_BINARY_PACKED,
        //         ParquetEncoding::DELTA_LENGTH_BYTE_ARRAY => Encoding::DELTA_LENGTH_BYTE_ARRAY,
        //         ParquetEncoding::DELTA_BYTE_ARRAY => Encoding::DELTA_BYTE_ARRAY,
        //         ParquetEncoding::RLE_DICTIONARY => Encoding::RLE_DICTIONARY,
        //     };
        //
        //     props = props.set_encoding(encoding);
        // }
        //
        // if let Some(size) = opts.write_batch_size {
        //     props = props.set_write_batch_size(size);
        // }
        //
        // if let Some(size) = opts.data_pagesize_limit {
        //     props = props.set_data_pagesize_limit(size);
        // }
        //
        // if let Some(size) = opts.dictionary_pagesize_limit {
        //     props = props.set_dictionary_pagesize_limit(size);
        // }
        //
        // if let Some(size) = opts.dictionary_pagesize_limit {
        //     props = props.set_dictionary_pagesize_limit(size);
        // }
        //
        // if let Some(size) = opts.max_row_group_size {
        //     props = props.set_max_row_group_size(size);
        // }
        //
        // if let Some(created_by) = opts.created_by {
        //     props = props.set_created_by(created_by);
        // }
        //
        // if let Some(size) = opts.max_statistics_size {
        //     props = props.set_max_statistics_size(size);
        // }

        let input_file = File::open(path.clone()).unwrap();
        // let mut input_file = File::open("/tmp/ddd/s3-uewnxrmskf").unwrap();

        // let mut output = OpenOptions::new()
        //     .create(true)
        //     .write(true)
        //     .append(true)
        //     .open("bar")
        //     .unwrap();
        //
        // std::io::copy(&mut input_file, &mut output).unwrap();

        let data_dir= Config::get_data_dir();

        // let mut skpr_namespace: String = "".to_string();
        // if let Some((a, b)) = path.display().to_string().split_once("done/") {
        //     if let Some((hash, namespace_part)) = b.to_string().split_once("-") {
        //         skpr_namespace = namespace_part.to_string()
        //     }
        // }
        // let source_namespace = Config::getenv("S3_BUCKET", "");
        /////////////////
        let skpr_namespace = BufferChunker::decode_file_namespace(path.to_str().unwrap());
        let skpr_partition = BufferChunker::decode_file_partition(path.to_str().unwrap());
        let source_time = BufferChunker::decode_file_time(path.to_str().unwrap());

        // let skpr_namespace = Helpers::parse_namespace_field(&record, source_namespace, &mut parse_namespace_cache);

        let output_file_name = BufferChunker::encode_chunk_name("ingest", Some(&skpr_namespace), Some(&skpr_partition), Some(source_time));

        let output_dir = &format!("{}/finalised", data_dir);

        match fs::create_dir(output_dir) {
            Ok(_g) => {},
            Err(_err) => {}
        }

        let output_file_path = &format!("{}/{}&part={}.parquet", output_dir, output_file_name, Helpers::random_str(12).as_str());

        let output = OpenOptions::new()
            .create(true)
            .write(true)
            .append(true)
            .open(output_file_path)
            // .open("parquet")
            .unwrap();

        // let mut output = File::open("finalised").unwrap();

        // println!("\n\n{:?}", schema_ref);

        let builder = ReaderBuilder::new().with_schema(schema_ref);

        let reader = builder.build(input_file).unwrap();

        // println!("\n{:?}\n\n", reader.schema());


        schema_ref = reader.schema();


        // let output = File::create("./foo/".to_string() + &Helpers::random_str(10)).unwrap();

        // println!("Serialising to parquet file: {}", output_file_path);

        let mut writer = ArrowWriter::try_new(output, reader.schema(), Some(props.build())).unwrap();

        for batch in reader {
            // for i in batch.iter() {
            //     println!("Batch part: {:?}", i);
            // }
            match batch {
                Ok(batch) => {
                    // println!("Writing batch");
                    // println!("{:?}", batch);
                    // let mut counter_lock = ingestMsgCountClone.lock().unwrap();
                    // *counter_lock = *counter_lock + batch.num_rows();
                    writer.write(&batch).unwrap()
                }
                // Err(error) => return Err(error.into()),
                Err(_error) => {
                    println!("Failed writing batch");
                    println!("{:?}", _error);
                    // AnalyseSchema::determine_field_types(&mut newMeta.get_mut(&ingest_record.skpr_namespace).unwrap().fields, None);
                    // println!("{:?}", newMeta);
                    // let arrowSchema = convert_skippr_to_arrow(&mut newMeta);
                }
            }
        }

        writer.close().unwrap();

        schema_ref
    }

    pub fn default_message(metadata: &mut HashMap<String, Metadata>) -> Result<Message, String> {
        let mut message: Message = Message::new();

        for (field, value) in metadata {
            if value.fields.len() > 0 {
                message.message.insert(
                    field.clone(),
                    SerdeParquet::default_message(&mut value.fields)
                        .unwrap()
                        .message
                        .get(field)
                        .unwrap()
                        .clone(),
                );
            } else {
                if &mut value.determined_type.len() > &mut 0 {
                    if value.determined_type == "record" {
                        message.message.insert(field.clone(), Message::new());
                        // message.insert(field.clone(), Box::new(Message(
                        //     HashSet::new(),
                        // )));
                    } else if value.determined_type == "array" {
                        message.message.insert(field.clone(), Message::new());
                    } else if value.determined_type == "map" {
                        if value.determined_type_values == "string" {
                            message.message.insert(field.clone(), Message::new());
                        }
                        if value.determined_type_values == "int" {
                            message.message.insert(field.clone(), Message::new());
                        }
                    }
                } else {
                    message.message.insert(field.clone(), Message::new());
                }
            }
        }

        Ok(message)
    }
}

#[derive(Clone)]
pub struct Message {
    pub message: Box<HashMap<String, Message>>, // pub message: Box<Value>
}

impl Message {
    pub fn new() -> Message {
        Message {
            message: None.unwrap(),
        }
        // Message { message: None.unwrap() }
    }
}
