use std::collections::HashMap;
use aws_sdk_glue::types::Column;
use clap::builder;
use icu::datetime::fields;
use crate::discover::arrow_schema::convert_skippr_to_arrow;
use crate::discover::Metadata;
use phf::phf_map;

// const MAPPINGS: [(&str, &str); 5] = [
// ("map", "map"),
// ("array", "array"),
// ("record", "struct"),
// ("long", "bigint"),
// ("string", "string") // parquet is binary but athena fails with binary and works with string?
// ];

static MAPPINGS: phf::Map<&'static str, &'static str> = phf_map! {
    "map" => "map",
    "array" => "array",
    "record" => "struct",
    "long" => "bigint",
    "string" => "string",
    "boolean" => "boolean",
    "integer" => "int",
    "double" => "double",
    "NULL" => "null",
    "date" => "timestamp",
    // "timestamp" => "timestamp",
    // 'timestamp_milli' => "BIGINT"
};


pub struct SkipprHive {
}


impl SkipprHive {


    pub fn convert_skippr_to_hive(metadata: &Metadata) -> Result<Vec<Column>, bool> {
        let mut field_types: Result<Vec<Column>, bool> = SkipprHive::convert_skippr_to_hive_field_types(metadata);
        return field_types;
    }

    fn convert_skippr_to_hive_field_types(metadata: &Metadata) -> Result<Vec<Column>, bool>{
        let mut field_types: Vec<Column> = vec![];

        for (k, v) in metadata.fields.iter() {

            match &*v.determined_type {
                // Value::Array(array) => {
                "record" => {

                    let stuct_cols = SkipprHive::convert_skippr_to_hive_field_types(&v).unwrap();

                    let mut type_str = format!("{}<", MAPPINGS.get(&v.determined_type).unwrap().clone());

                    let mut types: Vec<String> = vec![];
                    for col in stuct_cols.into_iter()  {
                        types.push(format!("{}:{}", col.name().unwrap(), col.r#type().unwrap()));
                    }
                    type_str = format!("{}{}>", type_str, types.join(","));

                    field_types.push(
                        Column::builder()
                        .name(k.to_string())
                        .r#type(type_str)
                        .build()
                    )
                }
                "map" => {
                    if &v.determined_type_values != "" {
                        let field_type: String = match MAPPINGS.get(&v.determined_type) {
                            Some(mapped_type) => mapped_type.to_string(),
                            None => v.determined_type.to_string()
                        };

                        let value_type: String = match MAPPINGS.get(&v.determined_type_values) {
                            Some(mapped_value) => mapped_value.to_string(),
                            None => v.determined_type_values.to_string()
                        };

                        let mut type_str = format!("{}<string,{}>", field_type, value_type);

                        field_types.push(
                            Column::builder()
                                .name(k.to_string())
                                .r#type(type_str)
                                .build()
                        )
                    }
                }
                "array" => {
                    if &v.determined_type_values != "" {
                        let field_type: String = match MAPPINGS.get(&v.determined_type) {
                            Some(mapped_type) => mapped_type.to_string(),
                            None => v.determined_type.to_string()
                        };

                        let value_type: String = match MAPPINGS.get(&v.determined_type_values) {
                            Some(mapped_items) => mapped_items.to_string(),
                            None => v.determined_type_values.to_string()
                        };

                        let mut type_str = format!("{}<{}>", field_type, value_type);

                        field_types.push(
                            Column::builder()
                                .name(k.to_string())
                                .r#type(type_str)
                                .build()
                        )
                    }

                }
                _ => {
                    let mapped_type = match MAPPINGS.get(&v.determined_type) {
                        Some(mapped_type) => mapped_type,
                        None => {
                            println!("No Hive mapped type for skippr field '{}' with type of '{}'", k, &v.determined_type);
                            ""
                        }
                    };

                    field_types.push(
                    Column::builder()
                        .name(k.to_string())
                        .r#type(mapped_type)
                        .build()
                    )
                }
            }
        }

        Ok(field_types)
    }


    // fn convert_skippr_type_to_arrow_data_type(skippr_type: &str) -> Result<DataType, ArrowError> {
    //     return match skippr_type {
    //         "boolean" => Ok(DataType::Boolean),
    //         "NULL" => Ok(DataType::Null),
    //         "integer" => Ok(DataType::Int32),
    //         "long" => Ok(DataType::Int64),
    //         "double" => Ok(DataType::Float64),
    //         "string" => Ok(DataType::Utf8),
    //         &_ => {
    //             Ok(DataType::Utf8)
    //             // return Err(ArrowError::JsonError(format!(
    //             //     "Only Scala possible found &_ instead of Scalar: {}", skippr_type
    //             // )));
    //         }
    //     };
    // }

}