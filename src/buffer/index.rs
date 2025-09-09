use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::column::reader::ColumnReader;
use parquet::record::{Row, Field};
use std::fs::File;
use std::path::Path;
use std::collections::{HashMap, HashSet};
use serde::{Deserialize, Serialize};
use std::boxed::Box;
use std::io::{Read, Seek, SeekFrom, Write};
use std::ops::Deref;
use parquet::errors::ParquetError;
use parquet::file::metadata::RowGroupMetaData;
use parquet::format::RowGroup;
use crate::discover::Metadata;

use std::sync::{Arc, RwLock};

// Define a struct for Column Index Offset
struct ColIndexOffset {
    // Inverted index of values to idx positions in columnar vector
    offsets: RwLock<HashMap<String, HashSet<usize>>>,
}

// Define a struct for Byte Index Offset
struct ByteIndexOffset {
    // Index of byte offsets for column idx
    offsets: RwLock<HashMap<i64, HashSet<usize>>>
}

// Define a struct for Zone Map
struct ZoneMap {
    // Maps column ID to a tuple of (min, max) values
    min_max: RwLock<HashMap<String, (i64, i64)>>,
}

// Define a struct for Sparse Index
struct SparseIndex {
    // Sparse indexing on potentially large data sets
    index: RwLock<HashMap<String, Vec<usize>>>, // Maps column ID to a list of indexed positions
}

// A struct that encapsulates all the indexes
struct DataIndexes {
    col_index_offset: Arc<ColIndexOffset>,
    byte_index_offset: Arc<ByteIndexOffset>,
    zone_map: Arc<ZoneMap>,
    sparse_index: Arc<SparseIndex>,
}

impl DataIndexes {
    // Method to create a new instance of DataIndexes
    fn new() -> Self {
        Self {
            col_index_offset: Arc::new(ColIndexOffset {
                offsets: RwLock::new(HashMap::new()),
            }),
            byte_index_offset: Arc::new(ByteIndexOffset {
                offsets: RwLock::new(HashMap::new()),
            }),
            zone_map: Arc::new(ZoneMap {
                min_max: RwLock::new(HashMap::new()),
            }),
            sparse_index: Arc::new(SparseIndex {
                index: RwLock::new(HashMap::new()),
            }),
        }
    }
}


struct TrackedRead<R> {
    inner: R,
    bytes_read: usize,
}

impl<R: Read> Read for TrackedRead<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let v = self.inner.read(buf)?;
        self.bytes_read += v;
        Ok(v)
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> std::io::Result<()> {
        let v = self.inner.read_exact(buf)?;
        self.bytes_read += buf.len();
        Ok(v)
    }
}

impl<R: Read + Seek> Seek for TrackedRead<R> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}



pub struct IndexBuilder {
    indexes: DataIndexes, // Holds all the indexes
    metadata: Metadata, // Holds the Metadata for structured indexing
}

impl IndexBuilder {
    pub fn new(metadata: Metadata) -> Self {
        IndexBuilder {
            indexes: DataIndexes::new(),
            metadata,
        }
    }
    pub fn get_byte_index_offset<R: Read + Seek>(file: &mut R, metadata: &RowGroupMetaData, column_index: usize) -> Result<i64, ParquetError> {
        let col_meta = metadata.column(column_index);
        
        let file_offset = col_meta.file_offset();
        file.seek(SeekFrom::Start(file_offset as u64))?;
        
        // Here, you would create your TrackedRead
        let mut tracked_reader = TrackedRead {
            inner: file,
            bytes_read: 0,
        };
        
        // Read the column header, adjust to actual data read requirements
        let mut buffer = vec![0; col_meta.to_column_metadata_thrift().total_uncompressed_size as usize];
        tracked_reader.read_exact(&mut buffer)?;
        
        tracked_reader.seek(SeekFrom::Current(tracked_reader.bytes_read as i64))?;
        
        Ok(tracked_reader.bytes_read as i64)
    }
    
    pub fn get_column_index_offset<R: Read + Seek>(file: &mut R, metadata: &RowGroupMetaData, column_index: usize) -> Result<i64, ParquetError> {
        let col_meta = metadata.column(column_index);
        
        let col_start = match col_meta.dictionary_page_offset() {
            Some(dictionary_page_offset) => dictionary_page_offset,
            None => col_meta.data_page_offset(),
        };
        
        Ok(col_start)
    }

    pub fn read_column_data<R: Read + Seek>(file: &mut R, metadata: &RowGroupMetaData, column_index: usize) -> Result<(), ParquetError> {
        let col_meta = metadata.column(column_index);
        let file_offset = col_meta.file_offset();
        file.seek(SeekFrom::Start(file_offset as u64))?;

        // Here, you would create your TrackedRead
        let mut tracked_reader = TrackedRead {
            inner: file,
            bytes_read: 0,
        };

        // Read the column header, adjust to actual data read requirements
        let mut buffer = vec![0; col_meta.to_column_metadata_thrift().total_uncompressed_size as usize];
        tracked_reader.read_exact(&mut buffer)?;

        let mut msgs: Vec<String> = Vec::new();

        let buffer_string = String::from_utf8_lossy(&buffer);
        msgs.push(format!("read: {}, column index: {}, buffer: {}", tracked_reader.bytes_read, column_index, buffer_string));
        
        tracked_reader.seek(SeekFrom::Current(tracked_reader.bytes_read as i64))?;

        msgs.push(format!("current offset: {}", tracked_reader.bytes_read));
        
        // Read the first column
        let next_column = metadata.column(column_index);
        let mut next_column_offset = next_column.file_offset();

        // let mut buffer: Vec<u8> = vec![0; next_column_offset as usize];
        // let byte_range = col_meta.byte_range();
        // let end = byte_range.1 as usize - tracked_reader.bytes_read;
        let mut buffer: Vec<u8> = vec![0; col_meta.compressed_size() as usize];
        tracked_reader.read_exact(&mut buffer)?;
        let buffer_string = String::from_utf8_lossy(&buffer);
        msgs.push(format!("read: {}, column index: {}, buffer: {}", tracked_reader.bytes_read, column_index, buffer_string));

        // Read the second column

        let previous_column_offset = next_column_offset;
        let next_column = metadata.column(column_index + 1);
        next_column_offset = next_column.file_offset();
        msgs.push(format!("next column offset: {}", next_column_offset));
        if next_column_offset == previous_column_offset {
            // This is the last column
            // next_column_offset = col_meta.index_page_offset().unwrap() as i64;
            // msgs.push(format!("last column, next column offset: {}", next_column_offset));
        }



        // let mut buffer: Vec<u8> = vec![0; tracked_reader.bytes_read + next_column_offset as usize -1];
        // tracked_reader.read_exact(&mut buffer)?;
        // let buffer_string = String::from_utf8_lossy(&buffer);
        // msgs.push(format!("read: {}, column index: {}, buffer: {}", tracked_reader.bytes_read, column_index, buffer_string));

        // panic!("{}", msgs.join("\n"));


                              Ok(())
    }

    pub fn build_from_parquet<P: AsRef<Path>>(&mut self, path: P) -> Result<(), parquet::errors::ParquetError> {
        let mut file = std::fs::File::open(&path)?;
        let reader = SerializedFileReader::new(file)?;

        let mut file = std::fs::File::open(&path)?;
        // let mut tracked = TrackedRead {
        //     inner: std::fs::File::open(&path)?,
        //     bytes_read: 0,
        // };

        // Self::read_column_data(&mut tracked, &reader.metadata().row_group(0), 0)?;
        

        let mut msgs: Vec<String> = Vec::new();

        // Iterate over each row group
        for i in 0..reader.num_row_groups() {
            
            msgs.push(format!("row group index: {}", i));

           let row_group_reader = reader.get_row_group(i)?;

            let mut row_group_iter = row_group_reader.get_row_iter(None)?;

            while let Some(res_row) = row_group_iter.next() {

                msgs.push(format!("row: {:?}", res_row));

                let row: Row = res_row?;

                for (idx, (name, field)) in row.get_column_iter().enumerate() {
                    
                    let col_offset = Self::get_column_index_offset(&mut file, &reader.metadata().row_group(i), idx)?;
                    
                    msgs.push(format!("col offset: {}", col_offset));

                    msgs.push(format!("column index: {}, column name: {}, column value: {}, at bytes: {}", idx, name, field, col_offset));

                    if msgs.len() > 5 {
                        panic!("{}", msgs.join("\n"));
                    }


                    self.col_index_offset(name, field, col_offset)?;
                    
                    self.byte_index_offset(name, i as i64, col_offset)?;
                }

            }
        }

        Ok(())
    }

    fn byte_index_offset(&mut self, name: &str, idx: i64, byte: i64) -> Result<(), parquet::errors::ParquetError> {
       
        let mut index = self.indexes.byte_index_offset.offsets.write().unwrap();
        index.entry(idx).or_insert(HashSet::new()).insert(byte as usize);
        
        Ok(())
    }

    fn col_index_offset(&mut self, name: &str, field: &Field, idx: i64) -> Result<(), parquet::errors::ParquetError> {
        let value_str = match field {
            Field::Int(value) => value.to_string(),
            Field::UInt(value) => value.to_string(),
            Field::Str(value) => value.clone(),
            // Add other necessary field types
            _ => unimplemented!(), // Handle other types as necessary
        };

        let mut index = self.indexes.col_index_offset.offsets.write().unwrap();
        index.entry(value_str.to_string()).or_insert(HashSet::new()).insert(idx as usize);
        

        Ok(())
    }
}


#[cfg(test)]
mod tests {
    use serial_test::serial;
    use std::env;
    use std::fs;
    use std::io::{Read, Write};
    use std::path::PathBuf;
    use arrow_schema::DataType;
    use datafusion::prelude::ParquetReadOptions;
    use parquet::column::writer::{ColumnWriter, GenericColumnWriter};
    use parquet::data_type::{ByteArray, ByteArrayType, Int32Type};
    use parquet::errors::ParquetError;
    use parquet::file::properties::{WriterProperties, WriterPropertiesPtr};
    use parquet::schema::parser::parse_message_type;
    use parquet::file::writer::{SerializedColumnWriter, SerializedFileWriter};
    use parquet::schema::types::TypePtr;
    use crate::discover::Metadata;
    use super::*;

    fn write_test_parquet_file(path: &Path) -> Result<(), parquet::errors::ParquetError> {
        let schema = parse_message_type(
            "message schema {
                REQUIRED INT32 int_field;
                REQUIRED BINARY string_field (UTF8);
            }"
        )?;
        let file = File::create(path)?;
        let props = WriterProperties::builder()
.set_compression(parquet::basic::Compression::UNCOMPRESSED)
            .set_encoding(parquet::basic::Encoding::PLAIN)
            .set_dictionary_enabled(false)
            .set_bloom_filter_enabled(false)
            .build();
        let mut writer = SerializedFileWriter::new(file, TypePtr::from(schema), WriterPropertiesPtr::from(props))?;

        let mut rows: i64 = 0;

        let mut row_group_writer = writer.next_row_group()?;

        // let mut column_writer = row_group_writer.next_column()?.unwrap();

        // write col [1, 2, 3]
        // write col ["hello", "world"]

        let mut col_writer = row_group_writer.next_column()?.unwrap();
        col_writer
            .typed::<Int32Type>()
            .write_batch(&[1, 2, 3], None, None)?;
        col_writer.close()?;

        let mut col_writer = row_group_writer.next_column()?.unwrap();
        col_writer
            .typed::<ByteArrayType>()
            .write_batch(&[
                ByteArray::from("hello"),
                ByteArray::from("world"),
                ByteArray::from("paul"),
            ], None, None)?;
        col_writer.close()?;

        row_group_writer.close()?;

        writer.close()?;

        Ok(())
    }

    #[test]
    fn test_index_building() {
        let dir = PathBuf::from("./");
        let file_path = dir.join("test.parquet");
        write_test_parquet_file(&file_path).expect("Failed to write test Parquet file");

        let metadata = Metadata {
            count: 1,
            parent_type: "root".to_string(),
            types: HashMap::new(),
            date_candidate: None,
            timezone: false,
            fields: Box::new(HashMap::new()),
            enabled: true,
            out_field_name: "int_field".to_string(),
            determined_type: "Int32".to_string(),
            determined_type_values: "".to_string(),
            evolution: Box::new(Default::default()),
            repetition_count: 1,
        };

        let mut index = IndexBuilder::new(metadata);
        index.build_from_parquet(file_path.to_str().unwrap()).expect("Failed to build index");

        

        let mut file = File::open(&file_path).expect("Failed to open test file");
      
        // get "hello" byte offset
        let rows: Vec<usize> = index.indexes.col_index_offset.offsets.read().unwrap().get("hello").unwrap().iter().map(|x| {
            let props = ParquetReadOptions::default();
            let reader = SerializedFileReader::new(file).expect("Failed to create reader");
            let row_group_reader = reader.get_row_group(0).expect("Failed to get row group");
            let col_reader = row_group_reader.get_column_reader(1).expect("Failed to get column reader");
            let mut buffer = vec![0; 5];
        }).collect();
        
        // let bytes: Vec<Vec<u8>> = bytes.iter().map( | x| {
        //     // let mut file = File::open(&file_path).expect("Failed to open test file");
        //     file.seek(SeekFrom::Start(*x as u64)).expect("Failed to seek to byte offset");
        //     let mut buffer = vec![0; 5];
        //     file.read_exact(&mut buffer).expect("Failed to read bytes");
        //     buffer
        // }).collect();
        
        panic!("{:?}, strings: {}", bytes, bytes.iter().map(|x| String::from_utf8_lossy(x)).collect::<Vec<String>>().join(", "));
            // file.bytes().skip(*start_byte_offset as usize).take((*end_byte_offset - *start_byte_offset) as usize).collect::<Result<Vec<u8>, std::io::Error>>().unwrap();

        
        
        fs::remove_file(file_path).expect("Failed to clean up test file"); // Cleanup
    }

}