use crate::discover::{OutputMetadata, SkipprDataType};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::error::ArrowError;
use std::collections::HashMap;

use arrow::datatypes::TimeUnit::{Microsecond, Millisecond};

pub const EMPTY_STRUCT_MARKER_FIELD: &str = "__skippr_empty_struct";

pub const SKIPPR_DECIMAL_PRECISION: u8 = 38;
pub const SKIPPR_DECIMAL_SCALE: i8 = 9;

pub(crate) fn convert_skippr_type_to_arrow_data_type(
    skippr_type: &SkipprDataType,
) -> Result<DataType, ArrowError> {
    match skippr_type {
        SkipprDataType::Boolean => Ok(DataType::Boolean),
        SkipprDataType::Null => Ok(DataType::Null),
        SkipprDataType::Integer => Ok(DataType::Int32),
        SkipprDataType::Short => Ok(DataType::Int16),
        SkipprDataType::Byte => Ok(DataType::Int8),
        SkipprDataType::Long => Ok(DataType::Int64),
        SkipprDataType::Double => Ok(DataType::Float64),
        SkipprDataType::Float => Ok(DataType::Float32),
        SkipprDataType::Decimal => Ok(DataType::Decimal128(
            SKIPPR_DECIMAL_PRECISION,
            SKIPPR_DECIMAL_SCALE,
        )),
        SkipprDataType::String => Ok(DataType::Utf8),
        SkipprDataType::Timestamp => Ok(DataType::Timestamp(Millisecond, None)),
        SkipprDataType::TimestampMilli => Ok(DataType::Timestamp(Millisecond, None)),
        SkipprDataType::Date => Ok(DataType::Timestamp(Millisecond, None)),
        SkipprDataType::Time => Ok(DataType::Time64(Microsecond)),
        SkipprDataType::Binary => Ok(DataType::Binary),
        SkipprDataType::Uuid => Ok(DataType::Utf8),
        SkipprDataType::Fixed => Ok(DataType::FixedSizeBinary(16)),
        SkipprDataType::Json => Ok(DataType::Utf8),
        SkipprDataType::Record
        | SkipprDataType::Map
        | SkipprDataType::Array
        | SkipprDataType::Unknown => Err(ArrowError::SchemaError(format!(
            "cannot map dest Skippr type '{}' to an Arrow primitive",
            skippr_type.as_str()
        ))),
    }
}

pub fn convert_skippr_to_arrow(
    metadata: Box<HashMap<String, OutputMetadata>>,
) -> Result<Schema, ArrowError> {
    Ok(Schema::new(output_metadata_fields(&metadata)?))
}

fn output_metadata_fields(
    metadata: &HashMap<String, OutputMetadata>,
) -> Result<Vec<Field>, ArrowError> {
    let mut values: Vec<&OutputMetadata> = metadata.values().collect();
    values.sort_by(|a, b| a.out_field_name.cmp(&b.out_field_name));
    values.into_iter().map(output_metadata_field).collect()
}

fn output_metadata_field(metadata: &OutputMetadata) -> Result<Field, ArrowError> {
    let data_type = output_metadata_data_type(metadata)?;
    if metadata.field_id() != 0 {
        Ok(crate::compute::field_with_id(
            &metadata.out_field_name,
            data_type,
            metadata.nullable,
            metadata.field_id(),
        ))
    } else {
        Ok(Field::new(
            metadata.out_field_name.clone(),
            data_type,
            metadata.nullable,
        ))
    }
}

fn output_metadata_struct(metadata: &OutputMetadata) -> Result<DataType, ArrowError> {
    let mut fields = output_metadata_fields(&metadata.fields)?;
    if !fields
        .iter()
        .any(|field| field.name() == EMPTY_STRUCT_MARKER_FIELD)
    {
        fields.insert(
            0,
            Field::new(EMPTY_STRUCT_MARKER_FIELD, DataType::Boolean, true),
        );
    }
    Ok(DataType::Struct(fields.into()))
}

fn output_metadata_data_type(metadata: &OutputMetadata) -> Result<DataType, ArrowError> {
    match metadata.determined_type {
        SkipprDataType::Map
            if metadata.determined_type_values.is_none() && metadata.fields.is_empty() =>
        {
            Err(ArrowError::SchemaError(
                "map dest metadata is missing determined_type_values".to_string(),
            ))
        }
        SkipprDataType::Record | SkipprDataType::Map => output_metadata_struct(metadata),
        SkipprDataType::Array => {
            let item_type = if metadata.determined_type_values == Some(SkipprDataType::Record) {
                metadata
                    .fields
                    .get("0")
                    .map(output_metadata_data_type)
                    .transpose()?
                    .ok_or_else(|| {
                        ArrowError::SchemaError(
                            "array-of-record dest metadata is missing field 0".to_string(),
                        )
                    })?
            } else if metadata.determined_type_values == Some(SkipprDataType::Array) {
                metadata
                    .fields
                    .get("0")
                    .map(output_metadata_data_type)
                    .transpose()?
                    .ok_or_else(|| {
                        ArrowError::SchemaError(
                            "array-of-array dest metadata is missing field 0".to_string(),
                        )
                    })?
            } else {
                let item = metadata.determined_type_values.as_ref().ok_or_else(|| {
                    ArrowError::SchemaError(
                        "array dest metadata is missing determined_type_values".to_string(),
                    )
                })?;
                convert_skippr_type_to_arrow_data_type(item)?
            };
            Ok(DataType::List(
                Box::new(Field::new("item", item_type, true)).into(),
            ))
        }
        _ => convert_skippr_type_to_arrow_data_type(&metadata.determined_type),
    }
}

/// WAL / Iceberg compaction identity for a schema: first 8 bytes of SHA-256, hex.
///
/// Segment descriptors store this value, not [`stable_schema_fingerprint`].
pub fn wal_schema_fingerprint(schema: &Schema) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for field in schema.fields().iter() {
        hasher.update(field.name().as_bytes());
        hasher.update(format!("{:?}", field.data_type()).as_bytes());
        hasher.update(&[if field.is_nullable() { 1 } else { 0 }]);
    }
    hex::encode(&hasher.finalize()[..8])
}

pub fn stable_schema_fingerprint(schema: &Schema) -> String {
    // Create a deterministic, minimal representation: sorted fields by name with canonicalized datatypes
    let mut pairs: Vec<(String, String)> = schema
        .fields()
        .iter()
        .map(|f| (f.name().to_string(), format!("{:?}", f.data_type())))
        .collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    let mut s = String::new();
    for (name, dtype) in pairs.into_iter() {
        s.push_str(&name);
        s.push(':');
        s.push_str(&dtype);
        s.push('|');
    }
    format!("{:x}", md5::compute(s))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use arrow::datatypes::{DataType, Field};
    use std::collections::HashMap;

    #[test]
    fn wal_schema_fingerprint_is_16_hex_chars() {
        let schema = Schema::new(vec![Field::new("id", DataType::Utf8, true)]);
        let fp = wal_schema_fingerprint(&schema);
        assert_eq!(fp.len(), 16);
        assert!(fp.chars().all(|ch| ch.is_ascii_hexdigit()));
    }

    #[test]
    fn test_skippr_type_to_arrow_matrix() {
        let cases = vec![
            (SkipprDataType::Boolean, DataType::Boolean),
            (SkipprDataType::Null, DataType::Null),
            (SkipprDataType::Byte, DataType::Int8),
            (SkipprDataType::Short, DataType::Int16),
            (SkipprDataType::Integer, DataType::Int32),
            (SkipprDataType::Long, DataType::Int64),
            (SkipprDataType::Float, DataType::Float32),
            (SkipprDataType::Double, DataType::Float64),
            (
                SkipprDataType::Decimal,
                DataType::Decimal128(SKIPPR_DECIMAL_PRECISION, SKIPPR_DECIMAL_SCALE),
            ),
            (SkipprDataType::String, DataType::Utf8),
            (
                SkipprDataType::Timestamp,
                DataType::Timestamp(Millisecond, None),
            ),
            (
                SkipprDataType::TimestampMilli,
                DataType::Timestamp(Millisecond, None),
            ),
            (SkipprDataType::Date, DataType::Timestamp(Millisecond, None)),
            (SkipprDataType::Time, DataType::Time64(Microsecond)),
            (SkipprDataType::Binary, DataType::Binary),
            (SkipprDataType::Uuid, DataType::Utf8),
            (SkipprDataType::Fixed, DataType::FixedSizeBinary(16)),
            (SkipprDataType::Json, DataType::Utf8),
        ];

        for (skippr_type, arrow_type) in cases {
            assert_eq!(
                convert_skippr_type_to_arrow_data_type(&skippr_type).unwrap(),
                arrow_type,
                "unexpected Arrow mapping for {:?}",
                skippr_type
            );
        }

        for nested in [
            SkipprDataType::Unknown,
            SkipprDataType::Record,
            SkipprDataType::Map,
            SkipprDataType::Array,
        ] {
            let err = convert_skippr_type_to_arrow_data_type(&nested).unwrap_err();
            assert!(
                err.to_string().contains("cannot map dest Skippr type"),
                "dest Arrow primitive map must fail closed on {:?}",
                nested
            );
        }
    }

    #[test]
    fn convert_skippr_to_arrow_fails_closed_on_unknown() {
        let mut metadata = OutputMetadata::new();
        metadata.out_field_name = "mystery".to_string();
        metadata.determined_type = SkipprDataType::Unknown;
        let mut meta_map = HashMap::new();
        meta_map.insert("mystery".to_string(), metadata);
        let err = convert_skippr_to_arrow(Box::new(meta_map)).unwrap_err();
        assert!(
            err.to_string().contains("cannot map dest Skippr type"),
            "{err}"
        );
    }

    #[test]
    fn convert_skippr_to_arrow_fails_closed_on_array_missing_item_type() {
        let mut metadata = OutputMetadata::new();
        metadata.out_field_name = "numbers".to_string();
        metadata.determined_type = SkipprDataType::Array;
        let mut meta_map = HashMap::new();
        meta_map.insert("numbers".to_string(), metadata);
        let err = convert_skippr_to_arrow(Box::new(meta_map)).unwrap_err();
        assert!(
            err.to_string()
                .contains("array dest metadata is missing determined_type_values"),
            "{err}"
        );
    }

    #[test]
    fn convert_skippr_to_arrow_stamps_parquet_field_id() {
        let mut metadata = OutputMetadata::new();
        metadata.out_field_name = "name".to_string();
        metadata.determined_type = SkipprDataType::String;
        metadata.field_id = 540703409;
        let mut meta_map = HashMap::new();
        meta_map.insert("name".to_string(), metadata);
        let schema = convert_skippr_to_arrow(Box::new(meta_map)).unwrap();
        assert_eq!(
            crate::compute::field_id_of(schema.field(0)).map(|id| id.get()),
            Some(540703409),
            "CREATE TABLE measures look up keys by PARQUET:field_id"
        );
    }

    #[test]
    fn test_output_metadata_nullable_controls_arrow_field() {
        let mut metadata = OutputMetadata::new();
        metadata.out_field_name = "required_id".to_string();
        metadata.determined_type = SkipprDataType::Long;
        metadata.nullable = false;

        let mut meta_map = HashMap::new();
        meta_map.insert("required_id".to_string(), metadata);
        let schema = convert_skippr_to_arrow(Box::new(meta_map)).unwrap();

        assert_eq!(schema.fields().len(), 1);
        assert!(!schema.field(0).is_nullable());
    }

    #[test]
    fn test_empty_record_remains_struct() {
        let mut record = OutputMetadata::new();
        record.out_field_name = "consent".to_string();
        record.determined_type = SkipprDataType::Record;

        let mut meta_map = HashMap::new();
        meta_map.insert("consent".to_string(), record);

        let schema = convert_skippr_to_arrow(Box::new(meta_map)).unwrap();
        let consent = schema.field_with_name("consent").unwrap();

        let expected = DataType::Struct(
            vec![Field::new(
                EMPTY_STRUCT_MARKER_FIELD,
                DataType::Boolean,
                true,
            )]
            .into(),
        );

        assert_eq!(consent.data_type(), &expected);
    }

    #[test]
    fn convert_skippr_to_arrow_fails_closed_on_map_missing_value_type() {
        let mut map = OutputMetadata::new();
        map.out_field_name = "app".to_string();
        map.determined_type = SkipprDataType::Map;
        let mut meta_map = HashMap::new();
        meta_map.insert("app".to_string(), map);
        let err = convert_skippr_to_arrow(Box::new(meta_map)).unwrap_err();
        assert!(
            err.to_string()
                .contains("map dest metadata is missing determined_type_values"),
            "{err}"
        );
    }

    #[test]
    fn test_convert_skippr_to_arrow_simple_array() {
        // Create a simple array of integers test metadata
        let mut metadata = OutputMetadata::new();
        metadata.out_field_name = "numbers".to_string();
        metadata.determined_type = SkipprDataType::Array;
        metadata.determined_type_values = Some(SkipprDataType::Integer);

        let mut meta_map = HashMap::new();
        meta_map.insert("numbers".to_string(), metadata);

        // Convert to Arrow schema
        let schema = convert_skippr_to_arrow(Box::new(meta_map)).unwrap();

        // Check the schema
        assert_eq!(schema.fields().len(), 1);
        let field = &schema.fields()[0];
        assert_eq!(field.name(), "numbers");

        // Check that the field is a List type
        match field.data_type() {
            DataType::List(item_field) => {
                // Check that item type is Int32
                assert_eq!(*item_field.data_type(), DataType::Int32);
            }
            _ => panic!("Expected List type, got {:?}", field.data_type()),
        }
    }

    #[test]
    fn test_convert_skippr_to_arrow_array_of_records() {
        // Create an array of records test metadata
        let mut root_metadata = OutputMetadata::new();
        root_metadata.out_field_name = "contacts".to_string();
        root_metadata.determined_type = SkipprDataType::Array;
        root_metadata.determined_type_values = Some(SkipprDataType::Record);

        // Create a record field for the array elements
        let mut record_field = OutputMetadata::new();
        record_field.out_field_name = "0".to_string();
        record_field.determined_type = SkipprDataType::Record;

        // Add fields to the record
        let mut name_field = OutputMetadata::new();
        name_field.out_field_name = "name".to_string();
        name_field.determined_type = SkipprDataType::String;

        let mut age_field = OutputMetadata::new();
        age_field.out_field_name = "age".to_string();
        age_field.determined_type = SkipprDataType::Integer;

        record_field.fields.insert("name".to_string(), name_field);
        record_field.fields.insert("age".to_string(), age_field);

        // Add the record field to the array field
        root_metadata.fields.insert("0".to_string(), record_field);

        let mut meta_map = HashMap::new();
        meta_map.insert("contacts".to_string(), root_metadata);

        // Convert to Arrow schema
        let schema = convert_skippr_to_arrow(Box::new(meta_map)).unwrap();

        // Check the schema
        assert_eq!(schema.fields().len(), 1);
        let field = &schema.fields()[0];
        assert_eq!(field.name(), "contacts");

        // Check that the field is a List type
        match field.data_type() {
            DataType::List(item_field) => {
                // Check that item type is a Struct
                match item_field.data_type() {
                    DataType::Struct(struct_fields) => {
                        assert_eq!(struct_fields.len(), 3);
                        assert!(struct_fields
                            .iter()
                            .any(|f| f.name() == EMPTY_STRUCT_MARKER_FIELD));

                        // Check name field
                        let name_field = struct_fields.iter().find(|f| f.name() == "name").unwrap();
                        assert_eq!(*name_field.data_type(), DataType::Utf8);

                        // Check age field
                        let age_field = struct_fields.iter().find(|f| f.name() == "age").unwrap();
                        assert_eq!(*age_field.data_type(), DataType::Int32);
                    }
                    _ => panic!("Expected Struct type, got {:?}", item_field.data_type()),
                }
            }
            _ => panic!("Expected List type, got {:?}", field.data_type()),
        }
    }

    #[test]
    fn test_convert_skippr_to_arrow_array_of_arrays() {
        // Create an array of arrays test metadata
        let mut root_metadata = OutputMetadata::new();
        root_metadata.out_field_name = "matrix".to_string();
        root_metadata.determined_type = SkipprDataType::Array;
        root_metadata.determined_type_values = Some(SkipprDataType::Array);

        // Create an inner array field
        let mut inner_array_field = OutputMetadata::new();
        inner_array_field.out_field_name = "0".to_string();
        inner_array_field.determined_type = SkipprDataType::Array;
        inner_array_field.determined_type_values = Some(SkipprDataType::Integer);

        // Add the inner array field to the outer array field
        root_metadata
            .fields
            .insert("0".to_string(), inner_array_field);

        let mut meta_map = HashMap::new();
        meta_map.insert("matrix".to_string(), root_metadata);

        // Convert to Arrow schema
        let schema = convert_skippr_to_arrow(Box::new(meta_map)).unwrap();

        // Check the schema
        assert_eq!(schema.fields().len(), 1);
        let field = &schema.fields()[0];
        assert_eq!(field.name(), "matrix");

        // Check that the field is a List type for outer array
        match field.data_type() {
            DataType::List(outer_item) => {
                // Check that the item type is also a List for inner array
                match outer_item.data_type() {
                    DataType::List(inner_item) => {
                        // Check that inner array elements are Int32
                        assert_eq!(*inner_item.data_type(), DataType::Int32);
                    }
                    _ => panic!(
                        "Expected List type for inner array, got {:?}",
                        outer_item.data_type()
                    ),
                }
            }
            _ => panic!(
                "Expected List type for outer array, got {:?}",
                field.data_type()
            ),
        }
    }

    #[test]
    fn test_convert_skippr_to_arrow_complex_nested_structure() {
        // Create a complex nested structure with arrays, records, and primitive types
        let mut metadata_map = HashMap::new();

        // Add a simple field
        let mut name_field = OutputMetadata::new();
        name_field.out_field_name = "name".to_string();
        name_field.determined_type = SkipprDataType::String;
        metadata_map.insert("name".to_string(), name_field);

        // Create an array of records with a nested array
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

        let mut contact_age_field = OutputMetadata::new();
        contact_age_field.out_field_name = "age".to_string();
        contact_age_field.determined_type = SkipprDataType::Integer;

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
            .insert("age".to_string(), contact_age_field);
        record_field
            .fields
            .insert("emails".to_string(), emails_field);

        // Add the record field to the array field
        array_of_records
            .fields
            .insert("0".to_string(), record_field);

        // Add the array field to the metadata map
        metadata_map.insert("contacts".to_string(), array_of_records);

        // Convert to Arrow schema
        let schema = convert_skippr_to_arrow(Box::new(metadata_map)).unwrap();

        // Check the schema
        assert_eq!(schema.fields().len(), 2); // name and contacts fields

        // Find the contacts field
        let contacts_field = schema
            .fields()
            .iter()
            .find(|f| f.name() == "contacts")
            .unwrap();

        // Check that the contacts field is a List type
        match contacts_field.data_type() {
            DataType::List(item_field) => {
                // Check that the list items are structs
                match item_field.data_type() {
                    DataType::Struct(struct_fields) => {
                        assert_eq!(struct_fields.len(), 4); // marker, name, age, emails
                        assert!(struct_fields
                            .iter()
                            .any(|f| f.name() == EMPTY_STRUCT_MARKER_FIELD));

                        // Check emails field is an array of strings
                        let emails_field =
                            struct_fields.iter().find(|f| f.name() == "emails").unwrap();
                        match emails_field.data_type() {
                            DataType::List(email_item) => {
                                assert_eq!(*email_item.data_type(), DataType::Utf8);
                            }
                            _ => panic!(
                                "Expected List type for emails, got {:?}",
                                emails_field.data_type()
                            ),
                        }
                    }
                    _ => panic!(
                        "Expected Struct type for contacts, got {:?}",
                        item_field.data_type()
                    ),
                }
            }
            _ => panic!(
                "Expected List type for contacts, got {:?}",
                contacts_field.data_type()
            ),
        }
    }

    #[test]
    fn test_convert_skippr_to_arrow_primitive_array_in_record() {
        // Create a record with a primitive array
        let mut metadata_map = HashMap::new();

        // Create the parent record
        let mut record_field = OutputMetadata::new();
        record_field.out_field_name = "imu".to_string();
        record_field.determined_type = SkipprDataType::Record;

        // Create the primitive array field
        let mut array_field = OutputMetadata::new();
        array_field.out_field_name = "x_axis_linear_mean".to_string();
        array_field.determined_type = SkipprDataType::Array;
        array_field.determined_type_values = Some(SkipprDataType::Double);

        // Add the array field to the record field
        record_field
            .fields
            .insert("x_axis_linear_mean".to_string(), array_field);

        // Add the record field to the metadata map
        metadata_map.insert("imu".to_string(), record_field);

        // Convert to Arrow schema
        let schema = convert_skippr_to_arrow(Box::new(metadata_map)).unwrap();

        // Check the schema
        assert_eq!(schema.fields().len(), 1);
        let imu_field = &schema.fields()[0];
        assert_eq!(imu_field.name(), "imu");

        // Check that imu field is a Struct
        match imu_field.data_type() {
            DataType::Struct(struct_fields) => {
                assert_eq!(struct_fields.len(), 2);
                assert!(struct_fields
                    .iter()
                    .any(|f| f.name() == EMPTY_STRUCT_MARKER_FIELD));

                // Check the array field inside the struct
                let array_field = struct_fields
                    .iter()
                    .find(|f| f.name() == "x_axis_linear_mean")
                    .unwrap();

                // Check that it's a list of doubles
                match array_field.data_type() {
                    DataType::List(item_field) => {
                        assert_eq!(*item_field.data_type(), DataType::Float64);
                    }
                    _ => panic!(
                        "Expected List type for x_axis_linear_mean, got {:?}",
                        array_field.data_type()
                    ),
                }
            }
            _ => panic!(
                "Expected Struct type for imu, got {:?}",
                imu_field.data_type()
            ),
        }
    }
}
