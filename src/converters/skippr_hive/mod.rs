use aws_sdk_glue::types::Column;

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

pub struct SkipprHive {}

impl SkipprHive {
    pub fn convert_skippr_to_hive(metadata: &Metadata) -> Result<Vec<Column>, bool> {
        let field_types: Result<Vec<Column>, bool> =
            SkipprHive::convert_skippr_to_hive_field_types(metadata);
        field_types
    }

    fn convert_skippr_to_hive_field_types(metadata: &Metadata) -> Result<Vec<Column>, bool> {
        let mut field_types: Vec<Column> = vec![];

        for (k, v) in metadata.fields.iter() {
            match &*v.determined_type {
                // Value::Array(array) => {
                "record" => {
                    let stuct_cols = SkipprHive::convert_skippr_to_hive_field_types(v).unwrap();

                    let mut type_str =
                        format!("{}<", MAPPINGS.get(&v.determined_type).unwrap().clone());

                    let mut types: Vec<String> = vec![];
                    for col in stuct_cols.into_iter() {
                        types.push(format!("{}:{}", col.name().unwrap(), col.r#type().unwrap()));
                    }
                    type_str = format!("{}{}>", type_str, types.join(","));

                    field_types.push(
                        Column::builder()
                            .name(v.out_field_name.to_string())
                            .r#type(type_str)
                            .build(),
                    )
                }
                "map" => {
                    if !v.determined_type_values.is_empty() {
                        let field_type: String = match MAPPINGS.get(&v.determined_type) {
                            Some(mapped_type) => mapped_type.to_string(),
                            None => v.determined_type.to_string(),
                        };

                        let value_type: String = match MAPPINGS.get(&v.determined_type_values) {
                            Some(mapped_value) => mapped_value.to_string(),
                            None => v.determined_type_values.to_string(),
                        };

                        let type_str = format!("{}<string,{}>", field_type, value_type);

                        field_types.push(
                            Column::builder()
                                .name(v.out_field_name.to_string())
                                .r#type(type_str)
                                .build(),
                        )
                    }
                }
                "array" => {
                    if !v.determined_type_values.is_empty() {
                        let field_type: String = match MAPPINGS.get(&v.determined_type) {
                            Some(mapped_type) => mapped_type.to_string(),
                            None => v.determined_type.to_string(),
                        };

                        let value_type: String = match MAPPINGS.get(&v.determined_type_values) {
                            Some(mapped_items) => mapped_items.to_string(),
                            None => v.determined_type_values.to_string(),
                        };

                        let type_str = format!("{}<{}>", field_type, value_type);

                        field_types.push(
                            Column::builder()
                                .name(&v.out_field_name.to_string())
                                .r#type(type_str)
                                .build(),
                        )
                    }
                }
                _ => {
                    let mapped_type = match MAPPINGS.get(&v.determined_type) {
                        Some(mapped_type) => mapped_type,
                        None => {
                            println!(
                                "No Hive mapped type for skippr field '{}' with type of '{}'",
                                &v.out_field_name, &v.determined_type
                            );
                            ""
                        }
                    };

                    field_types.push(
                        Column::builder()
                            .name(&v.out_field_name.to_string())
                            .r#type(mapped_type)
                            .build(),
                    )
                }
            }
        }

        Self::sort_fields(&mut field_types);

        Ok(field_types)
    }

    fn sort_fields(vec: &mut Vec<Column>) {
        // Sort the map by the count of each Metadata in descending order
        vec.sort_by(|col1, col2| col1.name.as_ref().unwrap().cmp(col2.name.as_ref().unwrap()));
    }
}
