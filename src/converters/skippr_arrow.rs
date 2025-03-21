use crate::discover::{OutputMetadata};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::error::ArrowError;
use std::collections::{HashMap, HashSet};

use arrow::datatypes::TimeUnit::{Millisecond};
// use arrow::datatypes::Fields;

#[derive(Debug, Clone)]
enum InferredType {
    Scalar(HashSet<DataType>),
    Array(Box<InferredType>),
    Object(HashMap<String, InferredType>),
    #[allow(dead_code)]
    Any,
}

fn coerce_data_type(dt: Vec<&DataType>) -> DataType {
    let mut dt_iter = dt.into_iter().cloned();
    let dt_init = dt_iter.next().unwrap_or(DataType::Utf8);

    dt_iter.fold(dt_init, |l, r| match (l, r) {
        (DataType::Boolean, DataType::Boolean) => DataType::Boolean,
        (DataType::Int64, DataType::Int64) => DataType::Int64,
        (DataType::Float64, DataType::Float64)
        | (DataType::Float64, DataType::Int64)
        | (DataType::Int64, DataType::Float64) => DataType::Float64,
        (DataType::List(l), DataType::List(r)) => DataType::List(Box::new(Field::new(
            "item",
            coerce_data_type(vec![l.data_type(), r.data_type()]),
            true,
        )).into()),
        // coerce scalar and scalar array into scalar array
        (DataType::List(e), not_list) | (not_list, DataType::List(e)) => {
            DataType::List(Box::new(Field::new(
                "item",
                coerce_data_type(vec![e.data_type(), &not_list]),
                true,
            )).into())
        }
        _ => DataType::Utf8,
    })
}

fn generate_datatype(t: &InferredType) -> Result<DataType, ArrowError> {
    Ok(match t {
        InferredType::Scalar(hs) => coerce_data_type(hs.iter().collect()),
        InferredType::Object(spec) => DataType::Struct(generate_fields(spec).into()),
        // InferredType::Array(ele_type) => DataType::List(Box::new(Field::new(
        //     "item",
        //     generate_datatype(ele_type)?,
        //     true,
        // ))),
        InferredType::Array(ele_type) => {
            // Ensure array elements are processed as structs if they are objects
            match **ele_type {
                InferredType::Object(ref spec) => {
                    DataType::List(Box::new(Field::new("item", DataType::Struct(generate_fields(spec).into()), true)).into())
                },
                _ => DataType::List(Box::new(Field::new("item", generate_datatype(ele_type)?, true)).into())
            }
        },
        InferredType::Any => DataType::Null,
    })
}

// fn generate_fields(spec: &HashMap<String, InferredType>) -> Result<Vec<Field>, ArrowError> {
fn generate_fields(spec: &HashMap<String, InferredType>) -> Vec<Field> {
    spec.iter()
        .map(|(k, types)| Field::new(k, generate_datatype(types).unwrap(), true))
        .collect()
}

/// Generate schema from JSON field names and inferred data types
fn generate_schema(spec: HashMap<String, InferredType>) -> Result<Schema, ArrowError> {
    Ok(Schema::new(generate_fields(&spec)))
}

fn set_object_scalar_field_type(
    field_types: &mut HashMap<String, InferredType>,
    key: &str,
    ftype: DataType,
) -> Result<(), arrow::error::ArrowError> {
    field_types.insert(key.to_string(), InferredType::Scalar(HashSet::new()));
    match field_types.get_mut(key).unwrap() {
        InferredType::Scalar(hs) => {
            hs.insert(ftype);
        }
        InferredType::Array(_) => {
            return Err(ArrowError::JsonError(
                "Only Scalar possible found Array instead of Scalar".to_string(),
            ));
        }
        InferredType::Object(_) => {
            return Err(ArrowError::JsonError(
                "Only Scalar possible found Object instead of Scalar".to_string(),
            ));
        }
        _any => {
            return Err(ArrowError::JsonError(
                "Only Scalar possible found Any instead of Scalar".to_string(),
            ));
        }
    }
    Ok(())
}

fn convert_skippr_type_to_arrow_data_type(skippr_type: &str) -> Result<DataType, ArrowError> {
    match skippr_type {
        "boolean" => Ok(DataType::Boolean),
        "NULL" => Ok(DataType::Null),
        "integer" => Ok(DataType::Int32),
        "long" => Ok(DataType::Int64),
        "double" => Ok(DataType::Float64),
        "string" => Ok(DataType::Utf8),
        "timestamp" => Ok(DataType::Timestamp(Millisecond, None)),
        "timestamp_milli" => Ok(DataType::Timestamp(Millisecond, None)),
        "date" => Ok(DataType::Timestamp(Millisecond, None)),
        &_ => {
            Ok(DataType::Utf8)
            // return Err(ArrowError::JsonError(format!(
            //     "Only Scala possible found &_ instead of Scalar: {}", skippr_type
            // )));
        }
    }
}

pub fn convert_skippr_to_arrow(
    metadata: Box<HashMap<String, OutputMetadata>>,
) -> Result<Schema, ArrowError> {
    let field_types: HashMap<String, InferredType> =
        convert_skippr_to_arrow_field_types(&metadata).unwrap();

    generate_schema(field_types)
}

fn convert_skippr_to_arrow_field_types(
    metadata: &HashMap<String, OutputMetadata>,
) -> Result<HashMap<String, InferredType>, ArrowError> {
    let mut field_types: HashMap<String, InferredType> = HashMap::new();

    for (_k, v) in metadata.iter() {

        let _foo = &*v.determined_type;

        match &*v.determined_type {
            "record" => {
                // Skip fields that are empty structs
                if !v.fields.is_empty() {
                    field_types.insert(
                        v.out_field_name.to_string(),
                        InferredType::Object(convert_skippr_to_arrow_field_types(&v.fields).unwrap()),
                    );
                }
            }
            "array" => {
                if v.determined_type_values == "record" {
                    // let object_fields = convert_skippr_to_arrow_field_types(&v.fields).unwrap();
                    // let mut object_fields = InferredType::Object();
                    let mut fields: HashMap<String, InferredType> = HashMap::new();
                    for (_k, v) in v.fields.iter() {
                        for (sk, sv) in v.fields.iter() {
                            // Check if this is an array field within the record
                            if sv.determined_type == "array" {
                                // Handle array field within a record
                                let mut inner_field = HashSet::new();
                                let inner_data_type = 
                                    convert_skippr_type_to_arrow_data_type(&sv.determined_type_values).unwrap();
                                inner_field.insert(inner_data_type);
                                
                                // Create the array field
                                fields.insert(
                                    sk.to_string(),
                                    InferredType::Array(Box::new(InferredType::Scalar(inner_field))),
                                );
                            } else {
                                // Regular field (not an array)
                                let mut field = HashSet::new();
                                let data_type =
                                    convert_skippr_type_to_arrow_data_type(&sv.determined_type).unwrap();
                                field.insert(data_type);

                                fields.insert(
                                    sk.to_string(),
                                    InferredType::Scalar(field),
                                );
                            }
                        }
                        // object_fields.insert(InferredType::Object(convert_skippr_to_arrow_field_types(&v.fields).unwrap()));
                        // fields.insert(v.out_field_name.to_string(), InferredType::Object(convert_skippr_to_arrow_field_types(&v.fields).unwrap()));
                    }

                    field_types.insert(
                        v.out_field_name.to_string(),
                        InferredType::Array(Box::new(InferredType::Object(fields))),
                    );

                    // let fields = convert_skippr_to_arrow_field_types(&v.fields)?;
                    // field_types.insert(
                    //     v.out_field_name.to_string(),
                    //     InferredType::Array(Box::new(InferredType::Object(fields))),
                    // );

                } else if v.determined_type_values == "array" {
                    // Handle array of arrays
                    if let Some(inner_array) = v.fields.get("0") {
                        let mut inner_field = HashSet::new();
                        let inner_data_type = 
                            convert_skippr_type_to_arrow_data_type(&inner_array.determined_type_values).unwrap();
                        inner_field.insert(inner_data_type);
                        
                        // Create the inner array
                        let inner_array_type = InferredType::Array(Box::new(InferredType::Scalar(inner_field)));
                        
                        // Wrap in the outer array
                        field_types.insert(
                            v.out_field_name.to_string(),
                            InferredType::Array(Box::new(inner_array_type)),
                        );
                    }
                } else {
                    let mut field = HashSet::new();
                    let data_type =
                        convert_skippr_type_to_arrow_data_type(&v.determined_type_values).unwrap();
                    field.insert(data_type);

                    field_types.insert(
                        v.out_field_name.to_string(),
                        InferredType::Array(Box::new(InferredType::Scalar(field))),
                    );
                }
            }
            "map" => {
                field_types.insert(
                    v.out_field_name.to_string(),
                    InferredType::Object(convert_skippr_to_arrow_field_types(&v.fields).unwrap()),
                );
            }
            "boolean" => {
                set_object_scalar_field_type(
                    &mut field_types,
                    &v.out_field_name,
                    DataType::Boolean,
                ).expect("Error setting object scalar field type");
            }
            "NULL" => {
                set_object_scalar_field_type(&mut field_types, &v.out_field_name, DataType::Null).expect("Error setting object scalar NULL type");
            }
            "integer" => {
                set_object_scalar_field_type(&mut field_types, &v.out_field_name, DataType::Int32).expect("Error setting object scalar field integer type");
            }
            "long" => {
                set_object_scalar_field_type(&mut field_types, &v.out_field_name, DataType::Int64).expect("Error setting object scalar field long type")
            }
            "double" => {
                set_object_scalar_field_type(
                    &mut field_types,
                    &v.out_field_name,
                    DataType::Float64,
                ).expect("Error setting object scalar field double type")
            }
            "string" => {
                set_object_scalar_field_type(&mut field_types, &v.out_field_name, DataType::Utf8).expect("Error setting object scalar field string type")
            }
            "timestamp" => {
                set_object_scalar_field_type(&mut field_types, &v.out_field_name, DataType::Timestamp(Millisecond, None)).expect("Error setting object scalar field timestamp type")
            }
            "timestamp_milli" => {
                set_object_scalar_field_type(&mut field_types, &v.out_field_name, DataType::Timestamp(Millisecond, None)).expect("Error setting object scalar field timestamp_milli type")
            }
            "date" => {
                set_object_scalar_field_type(&mut field_types, &v.out_field_name, DataType::Timestamp(Millisecond, None)).expect("Error setting object scalar field date type")
            }
            "" => {}
            _any => {
                return Err(ArrowError::JsonError(format!(
                    "Only Scalar possible found Any instead of determined_type string: {}",
                    v.determined_type
                )));
            }
        }
    }

    Ok(field_types)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use arrow::datatypes::{DataType, Field};
    use std::collections::HashMap;

    #[test]
    fn test_convert_skippr_to_arrow_simple_array() {
        // Create a simple array of integers test metadata
        let mut metadata = OutputMetadata::new();
        metadata.out_field_name = "numbers".to_string();
        metadata.determined_type = "array".to_string();
        metadata.determined_type_values = "integer".to_string();
        
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
            },
            _ => panic!("Expected List type, got {:?}", field.data_type()),
        }
    }
    
    #[test]
    fn test_convert_skippr_to_arrow_array_of_records() {
        // Create an array of records test metadata
        let mut root_metadata = OutputMetadata::new();
        root_metadata.out_field_name = "contacts".to_string();
        root_metadata.determined_type = "array".to_string();
        root_metadata.determined_type_values = "record".to_string();
        
        // Create a record field for the array elements
        let mut record_field = OutputMetadata::new();
        record_field.out_field_name = "0".to_string();
        record_field.determined_type = "record".to_string();
        
        // Add fields to the record
        let mut name_field = OutputMetadata::new();
        name_field.out_field_name = "name".to_string();
        name_field.determined_type = "string".to_string();
        
        let mut age_field = OutputMetadata::new();
        age_field.out_field_name = "age".to_string();
        age_field.determined_type = "integer".to_string();
        
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
                        assert_eq!(struct_fields.len(), 2);
                        
                        // Check name field
                        let name_field = struct_fields.iter().find(|f| f.name() == "name").unwrap();
                        assert_eq!(*name_field.data_type(), DataType::Utf8);
                        
                        // Check age field
                        let age_field = struct_fields.iter().find(|f| f.name() == "age").unwrap();
                        assert_eq!(*age_field.data_type(), DataType::Int32);
                    },
                    _ => panic!("Expected Struct type, got {:?}", item_field.data_type()),
                }
            },
            _ => panic!("Expected List type, got {:?}", field.data_type()),
        }
    }

    #[test]
    fn test_convert_skippr_to_arrow_array_of_arrays() {
        // Create an array of arrays test metadata
        let mut root_metadata = OutputMetadata::new();
        root_metadata.out_field_name = "matrix".to_string();
        root_metadata.determined_type = "array".to_string();
        root_metadata.determined_type_values = "array".to_string();
        
        // Create an inner array field
        let mut inner_array_field = OutputMetadata::new();
        inner_array_field.out_field_name = "0".to_string();
        inner_array_field.determined_type = "array".to_string();
        inner_array_field.determined_type_values = "integer".to_string();
        
        // Add the inner array field to the outer array field
        root_metadata.fields.insert("0".to_string(), inner_array_field);
        
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
                    },
                    _ => panic!("Expected List type for inner array, got {:?}", outer_item.data_type()),
                }
            },
            _ => panic!("Expected List type for outer array, got {:?}", field.data_type()),
        }
    }

    #[test]
    fn test_convert_skippr_to_arrow_complex_nested_structure() {
        // Create a complex nested structure with arrays, records, and primitive types
        let mut metadata_map = HashMap::new();
        
        // Add a simple field
        let mut name_field = OutputMetadata::new();
        name_field.out_field_name = "name".to_string();
        name_field.determined_type = "string".to_string();
        metadata_map.insert("name".to_string(), name_field);
        
        // Create an array of records with a nested array
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
        
        let mut contact_age_field = OutputMetadata::new();
        contact_age_field.out_field_name = "age".to_string();
        contact_age_field.determined_type = "integer".to_string();
        
        // A nested array of strings for each contact's emails
        let mut emails_field = OutputMetadata::new();
        emails_field.out_field_name = "emails".to_string();
        emails_field.determined_type = "array".to_string();
        emails_field.determined_type_values = "string".to_string();
        
        record_field.fields.insert("name".to_string(), contact_name_field);
        record_field.fields.insert("age".to_string(), contact_age_field);
        record_field.fields.insert("emails".to_string(), emails_field);
        
        // Add the record field to the array field
        array_of_records.fields.insert("0".to_string(), record_field);
        
        // Add the array field to the metadata map
        metadata_map.insert("contacts".to_string(), array_of_records);
        
        // Convert to Arrow schema
        let schema = convert_skippr_to_arrow(Box::new(metadata_map)).unwrap();
        
        // Check the schema
        assert_eq!(schema.fields().len(), 2); // name and contacts fields
        
        // Find the contacts field
        let contacts_field = schema.fields().iter().find(|f| f.name() == "contacts").unwrap();
        
        // Check that the contacts field is a List type
        match contacts_field.data_type() {
            DataType::List(item_field) => {
                // Check that the list items are structs
                match item_field.data_type() {
                    DataType::Struct(struct_fields) => {
                        assert_eq!(struct_fields.len(), 3); // name, age, emails
                        
                        // Check emails field is an array of strings
                        let emails_field = struct_fields.iter().find(|f| f.name() == "emails").unwrap();
                        match emails_field.data_type() {
                            DataType::List(email_item) => {
                                assert_eq!(*email_item.data_type(), DataType::Utf8);
                            },
                            _ => panic!("Expected List type for emails, got {:?}", emails_field.data_type()),
                        }
                    },
                    _ => panic!("Expected Struct type for contacts, got {:?}", item_field.data_type()),
                }
            },
            _ => panic!("Expected List type for contacts, got {:?}", contacts_field.data_type()),
        }
    }
    
    #[test]
    fn test_convert_skippr_to_arrow_primitive_array_in_record() {
        // Create a record with a primitive array
        let mut metadata_map = HashMap::new();
        
        // Create the parent record
        let mut record_field = OutputMetadata::new();
        record_field.out_field_name = "imu".to_string();
        record_field.determined_type = "record".to_string();
        
        // Create the primitive array field
        let mut array_field = OutputMetadata::new();
        array_field.out_field_name = "x_axis_linear_mean".to_string();
        array_field.determined_type = "array".to_string();
        array_field.determined_type_values = "double".to_string();
        
        // Add the array field to the record field
        record_field.fields.insert("x_axis_linear_mean".to_string(), array_field);
        
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
                assert_eq!(struct_fields.len(), 1);
                
                // Check the array field inside the struct
                let array_field = struct_fields.iter().find(|f| f.name() == "x_axis_linear_mean").unwrap();
                
                // Check that it's a list of doubles
                match array_field.data_type() {
                    DataType::List(item_field) => {
                        assert_eq!(*item_field.data_type(), DataType::Float64);
                    },
                    _ => panic!("Expected List type for x_axis_linear_mean, got {:?}", array_field.data_type()),
                }
            },
            _ => panic!("Expected Struct type for imu, got {:?}", imu_field.data_type()),
        }
    }
}
