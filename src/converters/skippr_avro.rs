use avro_rs::types::Value;
use avro_rs::{SerError, Schema};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use avro_rs::schema::{Name, RecordField, RecordFieldOrder};
use crate::discover::OutputMetadata;

pub fn convert_skippr_to_avro(
    namespace: &str,
    metadata: &HashMap<String, OutputMetadata>,
) -> Result<Schema, SerError> {

    let field_types = convert_skippr_to_avro_field_types(namespace, metadata)?;

    Ok(Schema::Record {
        name: Name::new(namespace),
        doc: None,
        fields: field_types,
        lookup: HashMap::new(),
    })
}

pub fn convert_skippr_to_avro_field_types(
    field_name: &str,
    metadata: &HashMap<String, OutputMetadata>,
) -> Result<Schema, SerError> {
    let mut field_types: Vec<(String, Schema)> = Vec::new();

    for (k, v) in metadata.iter() {
        let skippr_type = &*v.determined_type;
        let avro_type = match skippr_type {
            "array" => {
                let mut items = Vec::new();
                let item_type =
                    convert_skippr_type_to_avro_data_type(&v.determined_type_values).unwrap();
                items.push(item_type.clone());
                Schema::Array(Box::new(item_type))
            }
            "map" => Schema::Map(Box::new(
                convert_skippr_to_avro_field_types(&v.out_field_name, &v.fields).unwrap(),
            )),
            "record" => Schema::Record {
                name: Name::new(&v.out_field_name),
                doc: None,
                fields: convert_skippr_to_avro_record_fields(&v.fields).unwrap(),
                lookup: HashMap::new(),
            },
            "boolean" => Schema::Boolean,
            "NULL" => Schema::Null,
            "integer" => Schema::Int,
            "long" => Schema::Long,
            "double" => Schema::Double,
            "string" => Schema::String,
            "timestamp" => {
                if let Some(date_candidate) = &v.date_candidate {
                    match date_candidate.format.as_str() {
                        "date" => Schema::Date,
                        "timestamp-millis" => Schema::TimestampMillis,
                        _ => {
                            return Err(SerError::SerializeValue(format!(
                                "Unsupported date format: {}",
                                date_candidate.format
                            )));
                        }
                    }
                } else {
                    Schema::Long
                }
            }
            "date" => Schema::Date,
            _ => {
                println!("Unsupported Skippr data type: {}", skippr_type);
                // Schema::Null
                return Err(SerError::SerializeValue(format!(
                    "Unsupported Skippr data type: {}",
                    skippr_type
                )));
            }
        };
        field_types.push((k, avro_type));
    }

    let field_types = convert_skippr_to_avro_record_fields(metadata)?;

    // Ok(field_types)
    Ok(Schema::Record {
        name: Name::new(field_name),
        doc: None,
        fields: field_types,
        lookup: HashMap::new(),
    })
}

fn convert_skippr_to_avro_record_fields(
    metadata: &HashMap<String, OutputMetadata>,
) -> Result<Vec<RecordField>, SerError> {
    let mut field_types: Vec<RecordField> = Vec::new();

    for (index, (k, v)) in metadata.iter().enumerate() {
        let skippr_type = &*v.determined_type;

        let avro_type = convert_skippr_to_avro_field_types(k, &v.fields).unwrap();

        let field = RecordField {
            name: v.out_field_name.clone(),
            doc: None,
            default: None,
            schema: avro_type,
            order: RecordFieldOrder::Ascending,
            position: index, // You may need to properly calculate the position
        };
        field_types.push(field);
    }

    Ok(field_types)
}

fn convert_skippr_type_to_avro_data_type(
    determined_type_values: &String,
) -> Result<Schema, SerError> {
    match determined_type_values.as_str() {
        "boolean" => Ok(Schema::Boolean),
        "NULL" => Ok(Schema::Null),
        "integer" => Ok(Schema::Int),
        "long" => Ok(Schema::Long),
        "double" => Ok(Schema::Double),
        "string" => Ok(Schema::String),
        _ => {
            return Err(SerError::SerializeValue(format!(
                "Unsupported Skippr data type: {}",
                determined_type_values
            )));
        }
    }
}
