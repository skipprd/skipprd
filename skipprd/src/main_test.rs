
use std::collections::HashMap;
use arrow::json::{Reader, ReaderBuilder, writer};
use clap::{Parser, ValueHint};
use parquet::{
    arrow::ArrowWriter,
    basic::{Compression, Encoding},
    errors::ParquetError,
    file::properties::{EnabledStatistics, WriterProperties},
};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::process::exit;
use std::sync::Arc;
use arrow::datatypes::Schema;
use flate2::read::GzDecoder;
use serde_json::Value;
mod ingest;
use crate::ingest::ingest_fast::IngestRecord;
mod discover;
use crate::discover::{AnalyseSchema, Metadata};
use crate::discover::arrow_schema::convert_skippr_to_arrow;
mod serdes;
use crate::serdes::json::SerdeJson;
mod helpers;
mod arr;

#[derive(clap::ValueEnum, Clone)]
#[allow(non_camel_case_types, clippy::upper_case_acronyms)]
enum ParquetCompression {
    UNCOMPRESSED,
    SNAPPY,
    GZIP,
    LZO,
    BROTLI,
    LZ4,
    ZSTD,
}

#[derive(clap::ValueEnum, Clone)]
#[allow(non_camel_case_types, clippy::upper_case_acronyms)]
enum ParquetEncoding {
    PLAIN,
    RLE,
    BIT_PACKED,
    DELTA_BINARY_PACKED,
    DELTA_LENGTH_BYTE_ARRAY,
    DELTA_BYTE_ARRAY,
    RLE_DICTIONARY,
}

#[derive(clap::ValueEnum, Clone)]
#[allow(non_camel_case_types, clippy::upper_case_acronyms)]
enum ParquetEnabledStatistics {
    None,
    Chunk,
    Page,
}

#[derive(Parser)]
#[clap(version = env!("CARGO_PKG_VERSION"), author = "Dominik Moritz <domoritz@cmu.edu>")]
struct Opts {
    /// Input JSON file.
    #[clap(name = "JSON", value_parser, value_hint = ValueHint::AnyPath)]
    input: PathBuf,

    /// Output file.
    #[clap(name = "PARQUET", value_parser, value_hint = ValueHint::AnyPath)]
    output: PathBuf,

    /// File with Arrow schema in JSON format.
    #[clap(short = 's', long, value_parser, value_hint = ValueHint::AnyPath)]
    schema_file: Option<PathBuf>,

    /// The number of records to infer the schema from. All rows if not present. Setting max-read-records to zero will stop schema inference and all columns will be string typed.
    #[clap(long)]
    max_read_records: Option<usize>,

    /// Set the compression.
    #[clap(short, long, value_parser)]
    compression: Option<ParquetCompression>,

    /// Sets encoding for any column.
    #[clap(short, long, value_parser)]
    encoding: Option<ParquetEncoding>,

    /// Sets data page size limit.
    #[clap(long)]
    data_pagesize_limit: Option<usize>,

    /// Sets dictionary page size limit.
    #[clap(long)]
    dictionary_pagesize_limit: Option<usize>,

    /// Sets write batch size.
    #[clap(long)]
    write_batch_size: Option<usize>,

    /// Sets max size for a row group.
    #[clap(long)]
    max_row_group_size: Option<usize>,

    /// Sets "created by" property.
    #[clap(long)]
    created_by: Option<String>,

    /// Sets flag to enable/disable dictionary encoding for any column.
    #[clap(long)]
    dictionary: bool,

    /// Sets flag to enable/disable statistics for any column.
    #[clap(long, value_parser)]
    statistics: Option<ParquetEnabledStatistics>,

    /// Sets max statistics size for any column. Applicable only if statistics are enabled.
    #[clap(long)]
    max_statistics_size: Option<usize>,

    /// Print the schema to stderr.
    #[clap(short, long)]
    print_schema: bool,

    /// Only print the schema
    #[clap(short = 'n', long)]
    dry: bool,
}

fn main() -> Result<(), ParquetError> {
    let opts: Opts = Opts::parse();

    let mut schema: Schema = Schema::empty();

    let mut temp_input = File::open(opts.input.clone()).unwrap();

    // let newMeta = Metadata {
    //     count: 0,
    //     types: HashMap::new(),
    //     parent_type: "".to_string(),
    //     fields: Box::new(Default::default()),
    //     date_candidate: None,
    //     evolution: Box::new(Default::default()),
    //     enabled: true,
    //     determined_type: "".to_string(),
    //     determined_type_values: "".to_string(),
    // };
    let newMeta = Metadata::new().unwrap();

    let mut metadata = HashMap::new();
    // metadata.insert("skpr-time".to_string(), newMeta);
    metadata.insert("example_ns".to_string(), newMeta);
    let mut newMeta: &mut HashMap<String, Metadata> = &mut metadata;


    // let d = GzDecoder::new(temp_input);

    use std::io;

    let mut foo: AnalyseSchema = AnalyseSchema { i: 0};

    let mut ingest_msg_count = 0;
    let mut hasAnalysed = false;

    // for line in io::BufReader::new(d).lines() {
    for line in io::BufReader::new(temp_input).lines() {

        let mut vs: Vec<Value> = SerdeJson::deserialize(line.unwrap());

        for mut v in vs {

            // let mut vv = v.clone();

            let mut ingest_record = IngestRecord {
                source_namespace: "".to_string(),
                source_partition: "".to_string(),
                skpr_event_ts: 0,
                skpr_namespace: "example_ns".to_string(),
                skpr_partition: "".to_string(),
                record: v,
            };

            if ingest_msg_count < 3 && !hasAnalysed {
                println!("Analysing {}", ingest_msg_count);
                AnalyseSchema::analyse_payload(&mut foo, &mut ingest_record.record, &mut newMeta.get_mut(&ingest_record.skpr_namespace).unwrap().fields);
            } else {
                if !hasAnalysed {

                    AnalyseSchema::determine_field_types(&mut newMeta.get_mut(&ingest_record.skpr_namespace).unwrap().fields, None);

                    schema = convert_skippr_to_arrow(&mut newMeta.get_mut(&ingest_record.skpr_namespace).unwrap().fields)?;
                }
                hasAnalysed = true;
                // let msg = fast_path_ingest(&mut ingest_record, &mut newMeta);
                // fast_path_ingest(&mut ingest_record, &mut newMeta);
                // println!("{:?}", msg);
                // @todo - convert Message -> Value
                // @todo - convert skippr metadata to arrow schema
                // @todo - test write value to Parquet
            }

            ingest_msg_count = ingest_msg_count + 1;
        }
    }

    if opts.print_schema || opts.dry {
        let json = serde_json::to_string_pretty(&schema).unwrap();
        eprintln!("Schema:");
        println!("{}", json);
        if opts.dry {
            return Ok(());
        }
    }

    let output = File::create(opts.output)?;

    let mut input = File::open(opts.input).unwrap();
    // let gz_reader = GzDecoder::new(input);

    let schema_ref = Arc::new(schema);
    let builder = ReaderBuilder::new().with_schema(schema_ref);
    // let reader = builder.build(gz_reader.into_inner())?;
    let reader = builder.build(input)?;

    let mut props = WriterProperties::builder().set_dictionary_enabled(opts.dictionary);

    if let Some(statistics) = opts.statistics {
        let statistics = match statistics {
            ParquetEnabledStatistics::Chunk => EnabledStatistics::Chunk,
            ParquetEnabledStatistics::Page => EnabledStatistics::Page,
            ParquetEnabledStatistics::None => EnabledStatistics::None,
        };

        props = props.set_statistics_enabled(statistics);
    }

    if let Some(compression) = opts.compression {
        let compression = match compression {
            ParquetCompression::UNCOMPRESSED => Compression::UNCOMPRESSED,
            ParquetCompression::SNAPPY => Compression::SNAPPY,
            ParquetCompression::GZIP => Compression::GZIP,
            ParquetCompression::LZO => Compression::LZO,
            ParquetCompression::BROTLI => Compression::BROTLI,
            ParquetCompression::LZ4 => Compression::LZ4,
            ParquetCompression::ZSTD => Compression::ZSTD,
        };

        props = props.set_compression(compression);
    }

    if let Some(encoding) = opts.encoding {
        let encoding = match encoding {
            ParquetEncoding::PLAIN => Encoding::PLAIN,
            ParquetEncoding::RLE => Encoding::RLE,
            ParquetEncoding::BIT_PACKED => Encoding::BIT_PACKED,
            ParquetEncoding::DELTA_BINARY_PACKED => Encoding::DELTA_BINARY_PACKED,
            ParquetEncoding::DELTA_LENGTH_BYTE_ARRAY => Encoding::DELTA_LENGTH_BYTE_ARRAY,
            ParquetEncoding::DELTA_BYTE_ARRAY => Encoding::DELTA_BYTE_ARRAY,
            ParquetEncoding::RLE_DICTIONARY => Encoding::RLE_DICTIONARY,
        };

        props = props.set_encoding(encoding);
    }

    if let Some(size) = opts.write_batch_size {
        props = props.set_write_batch_size(size);
    }

    if let Some(size) = opts.data_pagesize_limit {
        props = props.set_data_pagesize_limit(size);
    }

    if let Some(size) = opts.dictionary_pagesize_limit {
        props = props.set_dictionary_pagesize_limit(size);
    }

    if let Some(size) = opts.dictionary_pagesize_limit {
        props = props.set_dictionary_pagesize_limit(size);
    }

    if let Some(size) = opts.max_row_group_size {
        props = props.set_max_row_group_size(size);
    }

    if let Some(created_by) = opts.created_by {
        props = props.set_created_by(created_by);
    }

    if let Some(size) = opts.max_statistics_size {
        props = props.set_max_statistics_size(size);
    }

    println!("{:?}", reader.schema());

    println!("Creating writer");

    let mut writer = ArrowWriter::try_new(output, reader.schema(), Some(props.build()))?;

    for batch in reader {
        println!("iter batch");


        match batch {
            Ok(batch) => {
                println!("Writing batch");
                println!("{:?}", batch);
                writer.write(&batch)?;
                exit(0);
            },
            Err(error) => {
                println!("batch error");;
                return Err(error.into());
            },
        }
    }

    writer.flush()?;

    match writer.close() {
        Ok(_) => Ok(()),
        Err(error) => Err(error),
    }
}
