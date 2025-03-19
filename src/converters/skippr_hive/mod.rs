use aws_sdk_glue::types::Column;

use crate::discover::OutputMetadata;
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
    "timestamp" => "timestamp",
    "timestamp_milli" => "timestamp"
};

pub struct SkipprHive {}

impl SkipprHive {
    pub fn convert_skippr_to_hive(metadata: &OutputMetadata) -> Result<Vec<Column>, bool> {
        let field_types: Result<Vec<Column>, bool> =
            SkipprHive::convert_skippr_to_hive_field_types(metadata);
        field_types
    }

    fn convert_skippr_to_hive_field_types(metadata: &OutputMetadata) -> Result<Vec<Column>, bool> {
        let mut field_types: Vec<Column> = vec![];

        for (k, v) in metadata.fields.iter() {
            match &*v.determined_type {
                // Value::Array(array) => {
                "record" => {
                    if v.fields.is_empty() {
                        continue;
                    }

                    let stuct_cols = SkipprHive::convert_skippr_to_hive_field_types(v).unwrap();

                    let mut type_str =
                        format!("{}<", MAPPINGS.get(&v.determined_type).unwrap().clone());

                    let mut types: Vec<String> = vec![];
                    for col in stuct_cols.into_iter() {
                        types.push(format!("{}:{}", col.name, col.r#type().unwrap()));
                        // types.push(format!("{}", col.r#type().unwrap()));
                    }
                    type_str = format!("{}{}>", type_str, types.join(","));

                    field_types.push(
                        Column::builder()
                            .name(v.out_field_name.to_string())
                            .r#type(type_str)
                            .build()
                            .unwrap(),
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
                                .build()
                                .unwrap(),
                        )
                    }
                }
                "array" => {
                    if !v.determined_type_values.is_empty() {
                        let field_type: String = match MAPPINGS.get(&v.determined_type) {
                            Some(mapped_type) => mapped_type.to_string(),
                            None => v.determined_type.to_string(),
                        };

                        if v.determined_type_values == "record" {
                            let object_fields =
                                SkipprHive::convert_skippr_to_hive_field_types(v).unwrap();
                            let mut type_str = format!("{}<", field_type);

                            let mut types: Vec<String> = vec![];
                            for col in object_fields.into_iter() {
                                // types.push(format!("{}:{}", col.name().unwrap(), col.r#type().unwrap()));
                                types.push(format!("{}", col.r#type().unwrap()));
                            }
                            type_str = format!("{}{}>", type_str, types.join(","));

                            field_types.push(
                                Column::builder()
                                    .name(v.out_field_name.to_string())
                                    .r#type(type_str)
                                    .build()
                                    .unwrap(),
                            )
                        } else if v.determined_type_values == "array" {
                            // Handle array of arrays by recursively processing the inner array
                            if let Some(inner_array) = v.fields.get("0") {
                                let field_type: String = match MAPPINGS.get(&v.determined_type) {
                                    Some(mapped_type) => mapped_type.to_string(),
                                    None => v.determined_type.to_string(),
                                };
                                
                                // Get the inner array's value type
                                let inner_value_type: String = match MAPPINGS.get(&inner_array.determined_type_values) {
                                    Some(mapped_value) => mapped_value.to_string(),
                                    None => inner_array.determined_type_values.to_string(),
                                };
                                
                                // Create array<array<type>> format
                                let type_str = format!("{}<{}<{}>>", field_type, field_type, inner_value_type);

                                field_types.push(
                                    Column::builder()
                                        .name(v.out_field_name.to_string())
                                        .r#type(type_str)
                                        .build()
                                        .unwrap(),
                                )
                            }
                        } else {
                            let value_type: String = match MAPPINGS.get(&v.determined_type_values) {
                                Some(mapped_value) => mapped_value.to_string(),
                                None => v.determined_type_values.to_string(),
                            };

                            let type_str = format!("{}<{}>", field_type, value_type);

                            field_types.push(
                                Column::builder()
                                    .name(v.out_field_name.to_string())
                                    .r#type(type_str)
                                    .build()
                                    .unwrap(),
                            )
                        }
                    }
                }
                _ => {
                    match MAPPINGS.get(&v.determined_type) {
                        Some(mapped_type) => field_types.push(
                            Column::builder()
                                .name(&v.out_field_name.to_string())
                                .r#type(mapped_type.to_string())
                                .build()
                                .unwrap(),
                        ),
                        None => {
                            println!(
                                "No Hive mapped type for field '{}' with type of '{}'",
                                k, &v.determined_type
                            );
                        }
                    };
                }
            }
        }

        Self::sort_fields(&mut field_types);

        Ok(field_types)
    }

    fn sort_fields(vec: &mut Vec<Column>) {
        // Sort the map by the count of each OutputMetadata in descending order
        vec.sort_by(|col1, col2| col1.name.cmp(&col2.name));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover::OutputMetadata;
    use std::collections::HashMap;

    #[test]
    fn test_convert_skippr_to_hive_simple_array() {
        // Create a simple array of integers
        let mut metadata = OutputMetadata::new();
        metadata.out_field_name = "root".to_string();
        metadata.determined_type = "record".to_string();
        
        let mut array_field = OutputMetadata::new();
        array_field.out_field_name = "numbers".to_string();
        array_field.determined_type = "array".to_string();
        array_field.determined_type_values = "integer".to_string();
        
        metadata.fields.insert("numbers".to_string(), array_field);
        
        let columns = SkipprHive::convert_skippr_to_hive(&metadata).unwrap();
        
        // Check that we have one column
        assert_eq!(columns.len(), 1);
        
        // Check the column's type
        let column = &columns[0];
        assert_eq!(column.name, "numbers");
        assert_eq!(column.r#type().unwrap(), "array<int>");
    }

    #[test]
    fn test_convert_skippr_to_hive_array_of_records() {
        // Create an array of records
        let mut metadata = OutputMetadata::new();
        metadata.out_field_name = "root".to_string();
        metadata.determined_type = "record".to_string();
        
        let mut array_field = OutputMetadata::new();
        array_field.out_field_name = "contacts".to_string();
        array_field.determined_type = "array".to_string();
        array_field.determined_type_values = "record".to_string();
        
        // Create a record field for the array elements
        let mut record_field = OutputMetadata::new();
        record_field.out_field_name = "0".to_string();
        record_field.determined_type = "record".to_string();
        
        // Add fields to the record
        let mut name_field = OutputMetadata::new();
        name_field.out_field_name = "name".to_string();
        name_field.determined_type = "string".to_string();
        
        let mut tel_field = OutputMetadata::new();
        tel_field.out_field_name = "tel".to_string();
        tel_field.determined_type = "integer".to_string();
        
        record_field.fields.insert("name".to_string(), name_field);
        record_field.fields.insert("tel".to_string(), tel_field);
        
        // Add the record field to the array field
        array_field.fields.insert("0".to_string(), record_field);
        
        // Add the array field to the root metadata
        metadata.fields.insert("contacts".to_string(), array_field);
        
        let columns = SkipprHive::convert_skippr_to_hive(&metadata).unwrap();
        
        // Check that we have one column
        assert_eq!(columns.len(), 1);
        
        // Check the column's type
        let column = &columns[0];
        assert_eq!(column.name, "contacts");
        
        // Check the structure of the array<struct> type
        let type_str = column.r#type().unwrap();
        assert!(type_str.starts_with("array<"));
        assert!(type_str.contains("name:string"));
        assert!(type_str.contains("tel:int"));
    }

    #[test]
    fn test_convert_skippr_to_hive_array_of_arrays() {
        // Create an array of arrays
        let mut metadata = OutputMetadata::new();
        metadata.out_field_name = "root".to_string();
        metadata.determined_type = "record".to_string();
        
        let mut outer_array_field = OutputMetadata::new();
        outer_array_field.out_field_name = "matrix".to_string();
        outer_array_field.determined_type = "array".to_string();
        outer_array_field.determined_type_values = "array".to_string();
        
        // Create an inner array field
        let mut inner_array_field = OutputMetadata::new();
        inner_array_field.out_field_name = "0".to_string();
        inner_array_field.determined_type = "array".to_string();
        inner_array_field.determined_type_values = "integer".to_string();
        
        // Add the inner array to the outer array
        outer_array_field.fields.insert("0".to_string(), inner_array_field);
        
        // Add the outer array to the root metadata
        metadata.fields.insert("matrix".to_string(), outer_array_field);
        
        let columns = SkipprHive::convert_skippr_to_hive(&metadata).unwrap();
        
        // Check that we have one column
        assert_eq!(columns.len(), 1);
        
        // Check the column's type
        let column = &columns[0];
        assert_eq!(column.name, "matrix");
        
        // The type should be an array of arrays
        let type_str = column.r#type().unwrap();
        assert!(type_str.starts_with("array<array<int>"));
    }

    #[test]
    fn test_convert_skippr_to_hive_complex_nested_structure() {
        // Create a complex nested structure with arrays, records, and primitive types
        let mut metadata = OutputMetadata::new();
        metadata.out_field_name = "root".to_string();
        metadata.determined_type = "record".to_string();
        
        // A simple field
        let mut simple_field = OutputMetadata::new();
        simple_field.out_field_name = "name".to_string();
        simple_field.determined_type = "string".to_string();
        metadata.fields.insert("name".to_string(), simple_field);
        
        // An array of records
        let mut array_of_records = OutputMetadata::new();
        array_of_records.out_field_name = "contacts".to_string();
        array_of_records.determined_type = "array".to_string();
        array_of_records.determined_type_values = "record".to_string();
        
        // Create a record field for the array elements
        let mut record_field = OutputMetadata::new();
        record_field.out_field_name = "0".to_string();
        record_field.determined_type = "record".to_string();
        
        // Add fields to the record
        let mut contact_name_field = OutputMetadata::new();
        contact_name_field.out_field_name = "name".to_string();
        contact_name_field.determined_type = "string".to_string();
        
        let mut contact_tel_field = OutputMetadata::new();
        contact_tel_field.out_field_name = "tel".to_string();
        contact_tel_field.determined_type = "integer".to_string();
        
        // A nested array of strings for each contact's emails
        let mut emails_field = OutputMetadata::new();
        emails_field.out_field_name = "emails".to_string();
        emails_field.determined_type = "array".to_string();
        emails_field.determined_type_values = "string".to_string();
        
        record_field.fields.insert("name".to_string(), contact_name_field);
        record_field.fields.insert("tel".to_string(), contact_tel_field);
        record_field.fields.insert("emails".to_string(), emails_field);
        
        // Add the record field to the array field
        array_of_records.fields.insert("0".to_string(), record_field);
        
        // Add the array field to the root metadata
        metadata.fields.insert("contacts".to_string(), array_of_records);
        
        let columns = SkipprHive::convert_skippr_to_hive(&metadata).unwrap();
        
        // Check that we have the correct columns
        assert_eq!(columns.len(), 2); // name and contacts
        
        // Find the contacts column
        let contacts_column = columns.iter().find(|col| col.name == "contacts").unwrap();
        
        // Check the structure of the array<struct> type with the nested array
        let type_str = contacts_column.r#type().unwrap();
        assert!(type_str.starts_with("array<"));
        assert!(type_str.contains("name:string"));
        assert!(type_str.contains("tel:int"));
        assert!(type_str.contains("emails:array<string>"));
    }
}
