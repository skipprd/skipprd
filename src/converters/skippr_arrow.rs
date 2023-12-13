use crate::discover::Metadata;
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
        _Any => {
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
    metadata: Box<HashMap<String, Metadata>>,
) -> Result<Schema, ArrowError> {
    let field_types: HashMap<String, InferredType> =
        convert_skippr_to_arrow_field_types(&metadata).unwrap();

    generate_schema(field_types)
}

fn convert_skippr_to_arrow_field_types(
    metadata: &HashMap<String, Metadata>,
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
                            let mut field = HashSet::new();
                            let data_type =
                                convert_skippr_type_to_arrow_data_type(&sv.determined_type).unwrap();
                            field.insert(data_type);

                            // fields.insert(
                            //     sk.to_string(),
                            //     InferredType::Array(Box::new(InferredType::Scalar(field))),
                            // );]

                            fields.insert(
                                sk.to_string(),
                                InferredType::Scalar(field),
                            );

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

                } else {
                    let mut field = HashSet::new();
                    let dataType =
                        convert_skippr_type_to_arrow_data_type(&v.determined_type_values).unwrap();
                    field.insert(dataType);

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
            _Any => {
                return Err(ArrowError::JsonError(format!(
                    "Only Scalar possible found Any instead of determined_type string: {}",
                    v.determined_type
                )));
            }
        }
    }

    Ok(field_types)
}
