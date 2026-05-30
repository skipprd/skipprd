use std::collections::HashSet;

use aws_sdk_glue::types::Column;

use crate::discover::{OutputMetadata, SkipprDataType};
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
    "short" => "smallint",
    "byte" => "tinyint",
    "double" => "double",
    "float" => "float",
    "decimal" => "decimal(38,9)",
    "NULL" => "null",
    "date" => "timestamp",
    "timestamp" => "timestamp",
    "timestamp_milli" => "timestamp",
    "time" => "string",
    "binary" => "binary",
    "uuid" => "string",
    "fixed" => "binary",
    "json" => "string"
};

pub struct SkipprHive {}

impl SkipprHive {
    pub fn convert_skippr_to_hive(metadata: &OutputMetadata) -> Result<Vec<Column>, bool> {
        let field_types: Result<Vec<Column>, bool> =
            SkipprHive::convert_skippr_to_hive_field_types(metadata);
        field_types
    }

    /// Glue/Hive storage descriptor columns must not repeat partition keys (Athena
    /// `HIVE_INVALID_METADATA: duplicate columns`). Data files may still contain those
    /// fields; only the table metadata must omit them from `StorageDescriptor.Columns`.
    pub fn storage_columns_excluding_partition_keys(
        metadata: &OutputMetadata,
        partition_keys: &[Column],
    ) -> Result<Vec<Column>, bool> {
        let exclude: HashSet<String> = partition_keys
            .iter()
            .map(|column| column.name.clone())
            .collect();
        let columns = SkipprHive::convert_skippr_to_hive(metadata)?;
        Ok(columns
            .into_iter()
            .filter(|column| !exclude.contains(&column.name))
            .collect())
    }

    fn convert_skippr_to_hive_field_types(metadata: &OutputMetadata) -> Result<Vec<Column>, bool> {
        let mut field_types: Vec<Column> = vec![];

        for (k, v) in metadata.fields.iter() {
            match v.determined_type {
                SkipprDataType::Record => {
                    if v.fields.is_empty() {
                        continue;
                    }

                    let stuct_cols = SkipprHive::convert_skippr_to_hive_field_types(v).unwrap();

                    let mut type_str =
                        format!("{}<", MAPPINGS.get(v.determined_type.as_str()).unwrap());

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
                SkipprDataType::Map => {
                    if v.determined_type_values.is_some() {
                        let field_type: String = match MAPPINGS.get(v.determined_type.as_str()) {
                            Some(mapped_type) => mapped_type.to_string(),
                            None => v.determined_type.to_string(),
                        };

                        let dtv = v.determined_type_values.as_ref().unwrap();
                        let value_type: String = match MAPPINGS.get(dtv.as_str()) {
                            Some(mapped_value) => mapped_value.to_string(),
                            None => dtv.to_string(),
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
                SkipprDataType::Array => {
                    if v.determined_type_values.is_some() {
                        let field_type: String = match MAPPINGS.get(v.determined_type.as_str()) {
                            Some(mapped_type) => mapped_type.to_string(),
                            None => v.determined_type.to_string(),
                        };

                        if v.determined_type_values == Some(SkipprDataType::Record) {
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
                        } else if v.determined_type_values == Some(SkipprDataType::Array) {
                            // Handle array of arrays by recursively processing the inner array
                            if let Some(inner_array) = v.fields.get("0") {
                                let field_type: String =
                                    match MAPPINGS.get(v.determined_type.as_str()) {
                                        Some(mapped_type) => mapped_type.to_string(),
                                        None => v.determined_type.to_string(),
                                    };

                                let inner_dtv =
                                    inner_array.determined_type_values.as_ref().unwrap();
                                let inner_value_type: String =
                                    match MAPPINGS.get(inner_dtv.as_str()) {
                                        Some(mapped_value) => mapped_value.to_string(),
                                        None => inner_dtv.to_string(),
                                    };

                                // Create array<array<type>> format
                                let type_str =
                                    format!("{}<{}<{}>>", field_type, field_type, inner_value_type);

                                field_types.push(
                                    Column::builder()
                                        .name(v.out_field_name.to_string())
                                        .r#type(type_str)
                                        .build()
                                        .unwrap(),
                                )
                            }
                        } else {
                            let dtv = v.determined_type_values.as_ref().unwrap();
                            let value_type: String = match MAPPINGS.get(dtv.as_str()) {
                                Some(mapped_value) => mapped_value.to_string(),
                                None => dtv.to_string(),
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
                    match MAPPINGS.get(v.determined_type.as_str()) {
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
                                k, v.determined_type
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
    #[allow(unused_imports)]
    use std::collections::HashMap;

    #[test]
    fn test_skippr_type_to_hive_matrix() {
        let cases = vec![
            (SkipprDataType::Long, "bigint"),
            (SkipprDataType::String, "string"),
            (SkipprDataType::Boolean, "boolean"),
            (SkipprDataType::Integer, "int"),
            (SkipprDataType::Short, "smallint"),
            (SkipprDataType::Byte, "tinyint"),
            (SkipprDataType::Double, "double"),
            (SkipprDataType::Float, "float"),
            (SkipprDataType::Decimal, "decimal(38,9)"),
            (SkipprDataType::Date, "timestamp"),
            (SkipprDataType::Timestamp, "timestamp"),
            (SkipprDataType::TimestampMilli, "timestamp"),
            (SkipprDataType::Time, "string"),
            (SkipprDataType::Binary, "binary"),
            (SkipprDataType::Uuid, "string"),
            (SkipprDataType::Fixed, "binary"),
            (SkipprDataType::Json, "string"),
        ];

        for (skippr_type, hive_type) in cases {
            let mut root = OutputMetadata::new();
            root.determined_type = SkipprDataType::Record;
            let mut field = OutputMetadata::new();
            field.out_field_name = format!("field_{}", skippr_type.as_str());
            field.determined_type = skippr_type;
            root.fields.insert(field.out_field_name.clone(), field);

            let columns = SkipprHive::convert_skippr_to_hive(&root).unwrap();
            assert_eq!(columns.len(), 1);
            assert_eq!(columns[0].r#type().unwrap(), hive_type);
        }
    }

    #[test]
    fn storage_columns_exclude_partition_keys() {
        let mut metadata = OutputMetadata::new();
        metadata.determined_type = SkipprDataType::Record;
        for name in ["date", "metric"] {
            let mut field = OutputMetadata::new();
            field.out_field_name = name.to_string();
            field.determined_type = SkipprDataType::String;
            metadata.fields.insert(name.to_string(), field);
        }
        let partition = Column::builder()
            .name("date")
            .r#type("string")
            .build()
            .unwrap();
        let columns =
            SkipprHive::storage_columns_excluding_partition_keys(&metadata, &[partition])
                .unwrap();
        assert_eq!(columns.len(), 1);
        assert_eq!(columns[0].name(), "metric");
    }

    #[test]
    fn test_convert_skippr_to_hive_simple_array() {
        // Create a simple array of integers
        let mut metadata = OutputMetadata::new();
        metadata.out_field_name = "root".to_string();
        metadata.determined_type = SkipprDataType::Record;

        let mut array_field = OutputMetadata::new();
        array_field.out_field_name = "numbers".to_string();
        array_field.determined_type = SkipprDataType::Array;
        array_field.determined_type_values = Some(SkipprDataType::Integer);

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
        metadata.determined_type = SkipprDataType::Record;

        let mut array_field = OutputMetadata::new();
        array_field.out_field_name = "contacts".to_string();
        array_field.determined_type = SkipprDataType::Array;
        array_field.determined_type_values = Some(SkipprDataType::Record);

        // Create a record field for the array elements
        let mut record_field = OutputMetadata::new();
        record_field.out_field_name = "0".to_string();
        record_field.determined_type = SkipprDataType::Record;

        // Add fields to the record
        let mut name_field = OutputMetadata::new();
        name_field.out_field_name = "name".to_string();
        name_field.determined_type = SkipprDataType::String;

        let mut tel_field = OutputMetadata::new();
        tel_field.out_field_name = "tel".to_string();
        tel_field.determined_type = SkipprDataType::Integer;

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
        metadata.determined_type = SkipprDataType::Record;

        let mut outer_array_field = OutputMetadata::new();
        outer_array_field.out_field_name = "matrix".to_string();
        outer_array_field.determined_type = SkipprDataType::Array;
        outer_array_field.determined_type_values = Some(SkipprDataType::Array);

        // Create an inner array field
        let mut inner_array_field = OutputMetadata::new();
        inner_array_field.out_field_name = "0".to_string();
        inner_array_field.determined_type = SkipprDataType::Array;
        inner_array_field.determined_type_values = Some(SkipprDataType::Integer);

        // Add the inner array to the outer array
        outer_array_field
            .fields
            .insert("0".to_string(), inner_array_field);

        // Add the outer array to the root metadata
        metadata
            .fields
            .insert("matrix".to_string(), outer_array_field);

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
        metadata.determined_type = SkipprDataType::Record;

        // A simple field
        let mut simple_field = OutputMetadata::new();
        simple_field.out_field_name = "name".to_string();
        simple_field.determined_type = SkipprDataType::String;
        metadata.fields.insert("name".to_string(), simple_field);

        // An array of records
        let mut array_of_records = OutputMetadata::new();
        array_of_records.out_field_name = "contacts".to_string();
        array_of_records.determined_type = SkipprDataType::Array;
        array_of_records.determined_type_values = Some(SkipprDataType::Record);

        // Create a record field for the array elements
        let mut record_field = OutputMetadata::new();
        record_field.out_field_name = "0".to_string();
        record_field.determined_type = SkipprDataType::Record;

        // Add fields to the record
        let mut contact_name_field = OutputMetadata::new();
        contact_name_field.out_field_name = "name".to_string();
        contact_name_field.determined_type = SkipprDataType::String;

        let mut contact_tel_field = OutputMetadata::new();
        contact_tel_field.out_field_name = "tel".to_string();
        contact_tel_field.determined_type = SkipprDataType::Integer;

        // A nested array of strings for each contact's emails
        let mut emails_field = OutputMetadata::new();
        emails_field.out_field_name = "emails".to_string();
        emails_field.determined_type = SkipprDataType::Array;
        emails_field.determined_type_values = Some(SkipprDataType::String);

        record_field
            .fields
            .insert("name".to_string(), contact_name_field);
        record_field
            .fields
            .insert("tel".to_string(), contact_tel_field);
        record_field
            .fields
            .insert("emails".to_string(), emails_field);

        // Add the record field to the array field
        array_of_records
            .fields
            .insert("0".to_string(), record_field);

        // Add the array field to the root metadata
        metadata
            .fields
            .insert("contacts".to_string(), array_of_records);

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

    #[test]
    fn test_convert_skippr_to_hive_primitive_array_in_record() {
        // Create a record with a primitive array
        let mut metadata = OutputMetadata::new();
        metadata.out_field_name = "root".to_string();
        metadata.determined_type = SkipprDataType::Record;

        // Create the primitive array field
        let mut array_field = OutputMetadata::new();
        array_field.out_field_name = "x_axis_linear_mean".to_string();
        array_field.determined_type = SkipprDataType::Array;
        array_field.determined_type_values = Some(SkipprDataType::Double);

        // Create the parent record
        let mut record_field = OutputMetadata::new();
        record_field.out_field_name = "imu".to_string();
        record_field.determined_type = SkipprDataType::Record;
        record_field
            .fields
            .insert("x_axis_linear_mean".to_string(), array_field);

        metadata.fields.insert("imu".to_string(), record_field);

        // Convert to Hive schema
        let columns = SkipprHive::convert_skippr_to_hive(&metadata).unwrap();

        // Check the result
        assert_eq!(columns.len(), 1);

        // Find the imu column
        let column = &columns[0];
        assert_eq!(column.name, "imu");

        // Extract the struct definition
        let type_str = column.r#type().unwrap();

        // Check that the struct contains the array field
        assert!(type_str.starts_with("struct<"));
        assert!(type_str.contains("x_axis_linear_mean:array<double>"));
    }
}
