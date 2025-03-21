use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use crate::converters::skippr_hive::SkipprHive;
use crate::discover::{Metadata, OutputMetadata};
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

    let flatten = Config::truth_value(&Config::get_transform_config().flatten_events.or(Some("no".to_string())).unwrap());

    let output_metadata = if flatten {
        // let mut meta: HashMap<String, OutputMetadata> = HashMap::new();
        // 
        // Metadata::flatten_metadata(metadata, &mut meta);
        // 
        // let mut flat: OutputMetadata = OutputMetadata::new();
        // flat.fields = Box::new(meta);
        // output_metadata = flat;
        OutputMetadata::from_flatterened_metadata(metadata)
        
    } else {
        // output_metadata = HashMap::new();
        // output_metadata.insert("root".to_string(), metadata.clone());
        OutputMetadata::from_metadata(metadata)
    };
    
    let hive_schema = SkipprHive::convert_skippr_to_hive(&output_metadata).unwrap();
    
    for col in hive_schema.iter() {
        let col_str = format!("{} {}\n", col.name(), col.r#type().unwrap());
        writer.write(col_str.as_bytes()).expect("Failed to write schema to file");
    };
    
    Ok(())

}


