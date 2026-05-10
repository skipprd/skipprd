use crate::discover::{OutputMetadata, SkipprDataType};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::error::ArrowError;
use std::collections::{HashMap, HashSet};
use tracing::warn;

use arrow::datatypes::TimeUnit::{Microsecond, Millisecond};
// use arrow::datatypes::Fields;

const EMPTY_STRUCT_MARKER_FIELD: &str = "__skippr_empty_struct";

#[allow(dead_code)]
#[derive(Debug, Clone)]
enum InferredType {
    Scalar(HashSet<DataType>),
    Array(Box<InferredType>),
    Object(HashMap<String, InferredType>),
    #[allow(dead_code)]
    Any,
}

#[allow(dead_code)]
fn coerce_data_type(dt: Vec<&DataType>) -> DataType {
    let mut dt_iter = dt.into_iter().cloned();
    let dt_init = dt_iter.next().unwrap_or(DataType::Utf8);

    dt_iter.fold(dt_init, |l, r| match (l, r) {
        (DataType::Boolean, DataType::Boolean) => DataType::Boolean,
        (DataType::Int64, DataType::Int64) => DataType::Int64,
        (DataType::Float64, DataType::Float64)
        | (DataType::Float64, DataType::Int64)
        | (DataType::Int64, DataType::Float64) => DataType::Float64,
        (DataType::List(l), DataType::List(r)) => DataType::List(
            Box::new(Field::new(
                "item",
                coerce_data_type(vec![l.data_type(), r.data_type()]),
                true,
            ))
            .into(),
        ),
        // coerce scalar and scalar array into scalar array
        (DataType::List(e), not_list) | (not_list, DataType::List(e)) => DataType::List(
            Box::new(Field::new(
                "item",
                coerce_data_type(vec![e.data_type(), &not_list]),
                true,
            ))
            .into(),
        ),
        _ => DataType::Utf8,
    })
}

#[allow(dead_code)]
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
                InferredType::Object(ref spec) => DataType::List(
                    Box::new(Field::new(
                        "item",
                        DataType::Struct(generate_fields(spec).into()),
                        true,
                    ))
                    .into(),
                ),
                _ => DataType::List(
                    Box::new(Field::new("item", generate_datatype(ele_type)?, true)).into(),
                ),
            }
        }
        InferredType::Any => DataType::Null,
    })
}

// fn generate_fields(spec: &HashMap<String, InferredType>) -> Result<Vec<Field>, ArrowError> {
#[allow(dead_code)]
fn generate_fields(spec: &HashMap<String, InferredType>) -> Vec<Field> {
    // Deterministic field order to avoid schema hash thrash and redundant WAL prefixes
    let mut keys: Vec<&String> = spec.keys().collect();
    keys.sort();
    keys.into_iter()
        .map(|k| {
            let types = spec.get(k).unwrap();
            Field::new(k, generate_datatype(types).unwrap(), true)
        })
        .collect()
}

/// Generate schema from JSON field names and inferred data types
#[allow(dead_code)]
fn generate_schema(spec: HashMap<String, InferredType>) -> Result<Schema, ArrowError> {
    Ok(Schema::new(generate_fields(&spec)))
}

#[allow(dead_code)]
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

fn convert_skippr_type_to_arrow_data_type(
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
        SkipprDataType::Decimal => Ok(DataType::Decimal128(38, 9)),
        SkipprDataType::String => Ok(DataType::Utf8),
        SkipprDataType::Timestamp => Ok(DataType::Timestamp(Millisecond, None)),
        SkipprDataType::TimestampMilli => Ok(DataType::Timestamp(Millisecond, None)),
        SkipprDataType::Date => Ok(DataType::Timestamp(Millisecond, None)),
        SkipprDataType::Time => Ok(DataType::Time64(Microsecond)),
        SkipprDataType::Binary => Ok(DataType::Binary),
        SkipprDataType::Uuid => Ok(DataType::Utf8),
        SkipprDataType::Fixed => Ok(DataType::FixedSizeBinary(16)),
        SkipprDataType::Json => Ok(DataType::Utf8),
        _ => Ok(DataType::Utf8),
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
    Ok(Field::new(
        metadata.out_field_name.clone(),
        output_metadata_data_type(metadata)?,
        metadata.nullable,
    ))
}

fn output_metadata_data_type(metadata: &OutputMetadata) -> Result<DataType, ArrowError> {
    match metadata.determined_type {
        SkipprDataType::Record | SkipprDataType::Map => {
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
        SkipprDataType::Array => {
            let item_type = if metadata.determined_type_values == Some(SkipprDataType::Record) {
                metadata
                    .fields
                    .get("0")
                    .map(output_metadata_data_type)
                    .transpose()?
                    .unwrap_or_else(|| DataType::Struct(Vec::<Field>::new().into()))
            } else if metadata.determined_type_values == Some(SkipprDataType::Array) {
                metadata
                    .fields
                    .get("0")
                    .map(output_metadata_data_type)
                    .transpose()?
                    .unwrap_or_else(|| DataType::Utf8)
            } else {
                convert_skippr_type_to_arrow_data_type(
                    metadata
                        .determined_type_values
                        .as_ref()
                        .unwrap_or(&SkipprDataType::String),
                )?
            };
            Ok(DataType::List(
                Box::new(Field::new("item", item_type, true)).into(),
            ))
        }
        _ => convert_skippr_type_to_arrow_data_type(&metadata.determined_type),
    }
}

#[allow(dead_code)]
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

fn is_datatype_equal_recursive(new_dt: &DataType, old_dt: &DataType) -> bool {
    use DataType::*;
    if new_dt == old_dt {
        return true;
    }
    match (new_dt, old_dt) {
        // Exact timestamp equality must include unit (timezone ignored for our usage since we never set it)
        (Timestamp(u1, _), Timestamp(u2, _)) if u1 == u2 => true,
        // Lists: element types must be exactly equal
        (List(new_field), List(old_field)) => {
            is_datatype_equal_recursive(new_field.data_type(), old_field.data_type())
        }
        // Structs: all old fields must exist and be exactly equal in the new struct
        (Struct(new_fields), Struct(old_fields)) => {
            for old_f in old_fields.iter() {
                if let Some(new_f) = new_fields.iter().find(|f| f.name() == old_f.name()) {
                    if !is_datatype_equal_recursive(new_f.data_type(), old_f.data_type()) {
                        return false;
                    }
                } else {
                    return false;
                }
            }
            true
        }
        _ => false,
    }
}

pub fn is_schema_superset(new_schema: &Schema, old_schema: &Schema) -> bool {
    // Every old field must be present in new with an exactly equal datatype (types are immutable)
    for old_field in old_schema.fields().iter() {
        if let Some(new_field) = new_schema
            .fields()
            .iter()
            .find(|f| f.name() == old_field.name())
        {
            if !is_datatype_equal_recursive(new_field.data_type(), old_field.data_type()) {
                return false;
            }
        } else {
            return false;
        }
    }
    true
}

#[allow(dead_code)]
fn convert_skippr_to_arrow_field_types(
    metadata: &HashMap<String, OutputMetadata>,
) -> Result<HashMap<String, InferredType>, ArrowError> {
    let mut field_types: HashMap<String, InferredType> = HashMap::new();

    for (_k, v) in metadata.iter() {
        match &v.determined_type {
            SkipprDataType::Record => {
                if !v.fields.is_empty() {
                    field_types.insert(
                        v.out_field_name.to_string(),
                        InferredType::Object(
                            convert_skippr_to_arrow_field_types(&v.fields).unwrap(),
                        ),
                    );
                } else {
                    field_types.insert(
                        v.out_field_name.to_string(),
                        InferredType::Scalar(HashSet::from([DataType::Utf8])),
                    );
                }
            }
            SkipprDataType::Array => {
                if v.determined_type_values == Some(SkipprDataType::Record) {
                    let mut fields: HashMap<String, InferredType> = HashMap::new();
                    for (_k, v) in v.fields.iter() {
                        for (sk, sv) in v.fields.iter() {
                            if sv.determined_type == SkipprDataType::Array {
                                let mut inner_field = HashSet::new();
                                let inner_data_type = convert_skippr_type_to_arrow_data_type(
                                    sv.determined_type_values
                                        .as_ref()
                                        .unwrap_or(&SkipprDataType::String),
                                )
                                .unwrap();
                                inner_field.insert(inner_data_type);

                                fields.insert(
                                    sk.to_string(),
                                    InferredType::Array(Box::new(InferredType::Scalar(
                                        inner_field,
                                    ))),
                                );
                            } else {
                                let mut field = HashSet::new();
                                let data_type =
                                    convert_skippr_type_to_arrow_data_type(&sv.determined_type)
                                        .unwrap();
                                field.insert(data_type);

                                fields.insert(sk.to_string(), InferredType::Scalar(field));
                            }
                        }
                    }

                    field_types.insert(
                        v.out_field_name.to_string(),
                        InferredType::Array(Box::new(InferredType::Object(fields))),
                    );
                } else if v.determined_type_values == Some(SkipprDataType::Array) {
                    if let Some(inner_array) = v.fields.get("0") {
                        let mut inner_field = HashSet::new();
                        let inner_data_type = convert_skippr_type_to_arrow_data_type(
                            inner_array
                                .determined_type_values
                                .as_ref()
                                .unwrap_or(&SkipprDataType::String),
                        )
                        .unwrap();
                        inner_field.insert(inner_data_type);

                        let inner_array_type =
                            InferredType::Array(Box::new(InferredType::Scalar(inner_field)));

                        field_types.insert(
                            v.out_field_name.to_string(),
                            InferredType::Array(Box::new(inner_array_type)),
                        );
                    } else {
                        field_types.insert(
                            v.out_field_name.to_string(),
                            InferredType::Array(Box::new(InferredType::Scalar(HashSet::from([
                                DataType::Utf8,
                            ])))),
                        );
                    }
                } else {
                    let mut field = HashSet::new();
                    let data_type = convert_skippr_type_to_arrow_data_type(
                        v.determined_type_values
                            .as_ref()
                            .unwrap_or(&SkipprDataType::String),
                    )
                    .unwrap();
                    field.insert(data_type);

                    field_types.insert(
                        v.out_field_name.to_string(),
                        InferredType::Array(Box::new(InferredType::Scalar(field))),
                    );
                }
            }
            SkipprDataType::Map => {
                field_types.insert(
                    v.out_field_name.to_string(),
                    InferredType::Object(convert_skippr_to_arrow_field_types(&v.fields).unwrap()),
                );
            }
            SkipprDataType::Boolean => {
                set_object_scalar_field_type(
                    &mut field_types,
                    &v.out_field_name,
                    DataType::Boolean,
                )
                .expect("Error setting object scalar field type");
            }
            SkipprDataType::Null => {
                set_object_scalar_field_type(&mut field_types, &v.out_field_name, DataType::Null)
                    .expect("Error setting object scalar NULL type");
            }
            SkipprDataType::Integer => {
                set_object_scalar_field_type(&mut field_types, &v.out_field_name, DataType::Int32)
                    .expect("Error setting object scalar field integer type");
            }
            SkipprDataType::Short => {
                set_object_scalar_field_type(&mut field_types, &v.out_field_name, DataType::Int16)
                    .expect("Error setting object scalar field short type");
            }
            SkipprDataType::Byte => {
                set_object_scalar_field_type(&mut field_types, &v.out_field_name, DataType::Int8)
                    .expect("Error setting object scalar field byte type");
            }
            SkipprDataType::Long => {
                set_object_scalar_field_type(&mut field_types, &v.out_field_name, DataType::Int64)
                    .expect("Error setting object scalar field long type")
            }
            SkipprDataType::Double => {
                set_object_scalar_field_type(&mut field_types, &v.out_field_name, DataType::Float64)
                    .expect("Error setting object scalar field double type")
            }
            SkipprDataType::Float => {
                set_object_scalar_field_type(&mut field_types, &v.out_field_name, DataType::Float32)
                    .expect("Error setting object scalar field float type")
            }
            SkipprDataType::Decimal => set_object_scalar_field_type(
                &mut field_types,
                &v.out_field_name,
                DataType::Decimal128(38, 9),
            )
            .expect("Error setting object scalar field decimal type"),
            SkipprDataType::String => {
                set_object_scalar_field_type(&mut field_types, &v.out_field_name, DataType::Utf8)
                    .expect("Error setting object scalar field string type")
            }
            SkipprDataType::Timestamp => set_object_scalar_field_type(
                &mut field_types,
                &v.out_field_name,
                DataType::Timestamp(Millisecond, None),
            )
            .expect("Error setting object scalar field timestamp type"),
            SkipprDataType::TimestampMilli => set_object_scalar_field_type(
                &mut field_types,
                &v.out_field_name,
                DataType::Timestamp(Millisecond, None),
            )
            .expect("Error setting object scalar field timestamp_milli type"),
            SkipprDataType::Date => set_object_scalar_field_type(
                &mut field_types,
                &v.out_field_name,
                DataType::Timestamp(Millisecond, None),
            )
            .expect("Error setting object scalar field date type"),
            SkipprDataType::Time => set_object_scalar_field_type(
                &mut field_types,
                &v.out_field_name,
                DataType::Time64(Microsecond),
            )
            .expect("Error setting object scalar field time type"),
            SkipprDataType::Binary => {
                set_object_scalar_field_type(&mut field_types, &v.out_field_name, DataType::Binary)
                    .expect("Error setting object scalar field binary type")
            }
            SkipprDataType::Uuid => {
                set_object_scalar_field_type(&mut field_types, &v.out_field_name, DataType::Utf8)
                    .expect("Error setting object scalar field uuid type")
            }
            SkipprDataType::Fixed => set_object_scalar_field_type(
                &mut field_types,
                &v.out_field_name,
                DataType::FixedSizeBinary(16),
            )
            .expect("Error setting object scalar field fixed type"),
            SkipprDataType::Json => {
                set_object_scalar_field_type(&mut field_types, &v.out_field_name, DataType::Utf8)
                    .expect("Error setting object scalar field json type")
            }
            other => {
                warn!(
                    "Unhandled Skippr type {:?} for field '{}', falling back to Utf8",
                    other, v.out_field_name
                );
                set_object_scalar_field_type(&mut field_types, &v.out_field_name, DataType::Utf8)
                    .expect("Error setting fallback Utf8 type");
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
            (SkipprDataType::Decimal, DataType::Decimal128(38, 9)),
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
    fn test_empty_record_and_map_remain_structs() {
        let mut record = OutputMetadata::new();
        record.out_field_name = "consent".to_string();
        record.determined_type = SkipprDataType::Record;

        let mut map = OutputMetadata::new();
        map.out_field_name = "app".to_string();
        map.determined_type = SkipprDataType::Map;

        let mut meta_map = HashMap::new();
        meta_map.insert("consent".to_string(), record);
        meta_map.insert("app".to_string(), map);

        let schema = convert_skippr_to_arrow(Box::new(meta_map)).unwrap();
        let consent = schema.field_with_name("consent").unwrap();
        let app = schema.field_with_name("app").unwrap();

        let expected = DataType::Struct(
            vec![Field::new(
                EMPTY_STRUCT_MARKER_FIELD,
                DataType::Boolean,
                true,
            )]
            .into(),
        );

        assert_eq!(consent.data_type(), &expected);
        assert_eq!(app.data_type(), &expected);
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
