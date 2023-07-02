use crate::discover::Metadata;
use arrow::datatypes::{DataType, Field, Schema};
use arrow::error::ArrowError;
use std::collections::{HashMap, HashSet};

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
        ))),
        // coerce scalar and scalar array into scalar array
        (DataType::List(e), not_list) | (not_list, DataType::List(e)) => {
            DataType::List(Box::new(Field::new(
                "item",
                coerce_data_type(vec![e.data_type(), &not_list]),
                true,
            )))
        }
        _ => DataType::Utf8,
    })
}

fn generate_datatype(t: &InferredType) -> Result<DataType, ArrowError> {
    Ok(match t {
        InferredType::Scalar(hs) => coerce_data_type(hs.iter().collect()),
        InferredType::Object(spec) => DataType::Struct(generate_fields(spec)?),
        InferredType::Array(ele_type) => DataType::List(Box::new(Field::new(
            "item",
            generate_datatype(ele_type)?,
            true,
        ))),
        InferredType::Any => DataType::Null,
    })
}

fn generate_fields(spec: &HashMap<String, InferredType>) -> Result<Vec<Field>, ArrowError> {
    spec.iter()
        .map(|(k, types)| Ok(Field::new(k, generate_datatype(types)?, true)))
        .collect()
}

/// Generate schema from JSON field names and inferred data types
fn generate_schema(spec: HashMap<String, InferredType>) -> Result<Schema, ArrowError> {
    Ok(Schema::new(generate_fields(&spec)?))
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

    for (k, v) in metadata.iter() {
        let _foo = &*v.determined_type;

        match &*v.determined_type {
            // Value::Array(array) => {
            "array" => {
                let mut field = HashSet::new();
                let dataType =
                    convert_skippr_type_to_arrow_data_type(&v.determined_type_values).unwrap();
                field.insert(dataType);

                field_types.insert(
                    v.out_field_name.to_string(),
                    InferredType::Array(Box::new(InferredType::Scalar(field))),
                );
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
                );
            }
            "NULL" => {
                set_object_scalar_field_type(&mut field_types, &v.out_field_name, DataType::Null);
            }
            "integer" => {
                set_object_scalar_field_type(&mut field_types, &v.out_field_name, DataType::Int32);
            }
            "long" => {
                set_object_scalar_field_type(&mut field_types, &v.out_field_name, DataType::Int64);
            }
            "double" => {
                set_object_scalar_field_type(
                    &mut field_types,
                    &v.out_field_name,
                    DataType::Float64,
                );
            }
            "string" => {
                set_object_scalar_field_type(&mut field_types, &v.out_field_name, DataType::Utf8);
            }
            "timestamp" => {
                set_object_scalar_field_type(&mut field_types, &v.out_field_name, DataType::Int64);
            }
            "date" => {
                set_object_scalar_field_type(&mut field_types, &v.out_field_name, DataType::Int64);
            }
            "record" => {
                field_types.insert(
                    v.out_field_name.to_string(),
                    InferredType::Object(convert_skippr_to_arrow_field_types(&v.fields).unwrap()),
                );
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
