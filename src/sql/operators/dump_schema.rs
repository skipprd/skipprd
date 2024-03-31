use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use crate::converters::skippr_hive::SkipprHive;
use crate::discover::{Metadata};
use crate::helpers::configuration::Config;
use crate::sql::parser::{SchemaDumpStatement};

pub fn dump_schema(metadata: &Metadata, stmt: &SchemaDumpStatement) -> Result<(), String> {

    let metadata_file = format!("{}", stmt.target);

    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&metadata_file)
        .expect(&format!("Failed to open target schema file {}", &metadata_file));

    let mut writer = BufWriter::new(file);
    
    let hive_schema = SkipprHive::convert_skippr_to_hive(metadata).unwrap();
    
    for col in hive_schema.iter() {
        let col_str = format!("{} {}\n", col.name().unwrap(), col.r#type().unwrap());
        writer.write(col_str.as_bytes()).expect("Failed to write schema to file");
    };
    
    Ok(())

}


