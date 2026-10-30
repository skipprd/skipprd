//! Coerce source Arrow (parquet/avro/ipc) onto Skippr physical Arrow.
//!
//! Decimal is always `Decimal128(38, 9)`. Unknown Arrow types fail closed.

use std::sync::Arc;

use arrow::array::{
    new_null_array, Array, ArrayRef, Decimal128Array, Decimal256Array, FixedSizeListArray,
    LargeListArray, ListArray, StructArray,
};
use arrow::buffer::OffsetBuffer;
use arrow::compute::cast;
use arrow::datatypes::{DataType, SchemaRef};
use arrow::error::ArrowError;
use arrow::record_batch::RecordBatch;
use arrow_schema::Field as ArrowField;

use serde_json::Value;

use crate::converters::skippr_arrow::convert_skippr_to_arrow;
use crate::discover::{Metadata, OutputMetadata, SkipprDataType};

pub use crate::converters::skippr_arrow::{SKIPPR_DECIMAL_PRECISION, SKIPPR_DECIMAL_SCALE};

pub fn skippr_type_for_arrow(data_type: &DataType) -> Result<SkipprDataType, ArrowError> {
    match data_type {
        DataType::Boolean => Ok(SkipprDataType::Boolean),
        DataType::Int8 | DataType::UInt8 => Ok(SkipprDataType::Byte),
        DataType::Int16 | DataType::UInt16 => Ok(SkipprDataType::Short),
        DataType::Int32 | DataType::UInt32 => Ok(SkipprDataType::Integer),
        DataType::Int64 | DataType::UInt64 => Ok(SkipprDataType::Long),
        DataType::Float16 | DataType::Float32 => Ok(SkipprDataType::Float),
        DataType::Float64 => Ok(SkipprDataType::Double),
        DataType::Decimal128(_, _) | DataType::Decimal256(_, _) => Ok(SkipprDataType::Decimal),
        DataType::Date32 | DataType::Date64 => Ok(SkipprDataType::Date),
        DataType::Timestamp(_, _) => Ok(SkipprDataType::Timestamp),
        DataType::Time32(_) | DataType::Time64(_) => Ok(SkipprDataType::Time),
        DataType::Binary
        | DataType::LargeBinary
        | DataType::FixedSizeBinary(_)
        | DataType::BinaryView => Ok(SkipprDataType::Binary),
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => Ok(SkipprDataType::String),
        DataType::List(_) | DataType::LargeList(_) | DataType::FixedSizeList(_, _) => {
            Ok(SkipprDataType::Array)
        }
        DataType::Map(_, _) => Err(ArrowError::CastError(
            "unsupported Arrow Map for Skippr ingest; use struct fields or JSON".to_string(),
        )),
        DataType::Struct(_) => Ok(SkipprDataType::Record),
        DataType::Null => Ok(SkipprDataType::Null),
        DataType::Dictionary(_, value) => skippr_type_for_arrow(value),
        other => Err(ArrowError::CastError(format!(
            "unsupported Arrow type for Skippr ingest: {other}"
        ))),
    }
}

pub fn output_metadata_for_arrow_field(field: &ArrowField) -> Result<OutputMetadata, ArrowError> {
    let mut metadata = OutputMetadata::new();
    metadata.out_field_name = field.name().clone();
    metadata.source_field_name = field.name().clone();
    metadata.determined_type = skippr_type_for_arrow(field.data_type())?;
    metadata.nullable = true;
    attach_nested_arrow_fields(&mut metadata, field.data_type())?;
    Ok(metadata)
}

fn attach_nested_arrow_fields(
    metadata: &mut OutputMetadata,
    data_type: &DataType,
) -> Result<(), ArrowError> {
    match data_type {
        DataType::Struct(fields) => {
            let mut children = std::collections::HashMap::new();
            for child in fields {
                children.insert(
                    child.name().clone(),
                    output_metadata_for_arrow_field(child)?,
                );
            }
            metadata.fields = Box::new(children);
        }
        DataType::List(item) | DataType::LargeList(item) | DataType::FixedSizeList(item, _) => {
            let item_meta = output_metadata_for_arrow_field(item)?;
            metadata.determined_type_values = Some(item_meta.determined_type.clone());
            if matches!(
                item_meta.determined_type,
                SkipprDataType::Record | SkipprDataType::Array | SkipprDataType::Map
            ) {
                let mut children = std::collections::HashMap::new();
                children.insert("0".to_string(), item_meta);
                metadata.fields = Box::new(children);
            }
        }
        _ => {}
    }
    Ok(())
}

fn metadata_from_output(om: &OutputMetadata) -> Metadata {
    let mut metadata = Metadata::new_with_type(om.determined_type.clone(), &om.out_field_name);
    metadata.source_field_name = om.source_field_name.clone();
    metadata.determined_type_values = om.determined_type_values.clone();
    metadata.nullable = om.nullable;
    for (name, child) in om.fields.iter() {
        metadata.set_field(name, metadata_from_output(child));
    }
    metadata
}

pub fn output_metadata_for_arrow_schema(schema: &SchemaRef) -> Result<OutputMetadata, ArrowError> {
    let mut metadata = OutputMetadata::new();
    metadata.determined_type = SkipprDataType::Record;
    let mut fields = std::collections::HashMap::new();
    for field in schema.fields() {
        fields.insert(
            field.name().clone(),
            output_metadata_for_arrow_field(field)?,
        );
    }
    metadata.fields = Box::new(fields);
    Ok(metadata)
}

pub fn metadata_for_arrow_field(field: &ArrowField) -> Result<Metadata, ArrowError> {
    Ok(metadata_from_output(&output_metadata_for_arrow_field(
        field,
    )?))
}

pub fn metadata_for_arrow_schema(schema: &SchemaRef) -> Result<Metadata, ArrowError> {
    let mut metadata = Metadata::new_with_type(SkipprDataType::Record, "");
    metadata.nullable = true;
    for field in schema.fields() {
        metadata.set_field(field.name(), metadata_for_arrow_field(field)?);
    }
    Ok(metadata)
}

pub fn skippr_schema_from_source(schema: &SchemaRef) -> Result<SchemaRef, ArrowError> {
    let metadata = output_metadata_for_arrow_schema(schema)?;
    Ok(Arc::new(convert_skippr_to_arrow(metadata.fields)?))
}

pub fn record_batch_to_json_values(batch: &RecordBatch) -> Result<Vec<Value>, ArrowError> {
    let mut buf = Vec::new();
    let mut writer = arrow::json::LineDelimitedWriter::new(&mut buf);
    writer.write_batches(&[batch])?;
    writer.finish()?;
    let text = String::from_utf8(buf).map_err(|err| ArrowError::ParseError(err.to_string()))?;
    let mut values = Vec::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        values.push(
            serde_json::from_str(line).map_err(|err| ArrowError::ParseError(err.to_string()))?,
        );
    }
    Ok(values)
}

pub fn coerced_batches_to_json_values(batches: &[RecordBatch]) -> Result<Vec<Value>, ArrowError> {
    let mut values = Vec::new();
    for batch in batches {
        let coerced =
            coerce_record_batch_to_skippr(batch, skippr_schema_from_source(&batch.schema())?)?;
        values.extend(record_batch_to_json_values(&coerced)?);
    }
    Ok(values)
}

pub fn coerce_record_batch_to_skippr(
    batch: &RecordBatch,
    skippr_schema: SchemaRef,
) -> Result<RecordBatch, ArrowError> {
    let mut columns = Vec::with_capacity(skippr_schema.fields().len());
    for field in skippr_schema.fields() {
        match batch.schema().index_of(field.name()) {
            Ok(idx) => columns.push(coerce_array(batch.column(idx).as_ref(), field.data_type())?),
            Err(_) => columns.push(new_null_array(field.data_type(), batch.num_rows())),
        }
    }
    RecordBatch::try_new(skippr_schema, columns)
}

fn coerce_array(array: &dyn Array, dest: &DataType) -> Result<ArrayRef, ArrowError> {
    match (array.data_type(), dest) {
        (DataType::Decimal128(_, src_scale), DataType::Decimal128(precision, scale))
            if *precision == SKIPPR_DECIMAL_PRECISION && *scale == SKIPPR_DECIMAL_SCALE =>
        {
            rescale_decimal128(array, *src_scale)
        }
        (DataType::Decimal256(_, src_scale), DataType::Decimal128(precision, scale))
            if *precision == SKIPPR_DECIMAL_PRECISION && *scale == SKIPPR_DECIMAL_SCALE =>
        {
            rescale_decimal256(array, *src_scale)
        }
        (
            DataType::List(_) | DataType::LargeList(_) | DataType::FixedSizeList(_, _),
            DataType::List(dest_field),
        ) => coerce_list_like(array, dest_field),
        (DataType::Struct(_), DataType::Struct(dest_fields)) => {
            let st = array
                .as_any()
                .downcast_ref::<StructArray>()
                .ok_or_else(|| {
                    ArrowError::CastError(
                        "expected StructArray for nested decimal coerce".to_string(),
                    )
                })?;
            let mut columns = Vec::with_capacity(dest_fields.len());
            for field in dest_fields {
                match st.column_by_name(field.name()) {
                    Some(col) => columns.push(coerce_array(col.as_ref(), field.data_type())?),
                    None => columns.push(new_null_array(field.data_type(), st.len())),
                }
            }
            Ok(Arc::new(StructArray::try_new(
                dest_fields.clone(),
                columns,
                st.nulls().cloned(),
            )?))
        }
        (src, dest) if src == dest => Ok(arrow::array::make_array(array.to_data())),
        (src, dest) => {
            let owned = arrow::array::make_array(array.to_data());
            cast(&owned, dest)
                .map_err(|err| ArrowError::CastError(format!("cannot cast {src} to {dest}: {err}")))
        }
    }
}

fn coerce_list_like(array: &dyn Array, dest_field: &ArrowField) -> Result<ArrayRef, ArrowError> {
    let (values, offsets, nulls) = match array.data_type() {
        DataType::List(_) => {
            let list = array.as_any().downcast_ref::<ListArray>().ok_or_else(|| {
                ArrowError::CastError("expected ListArray for nested coerce".to_string())
            })?;
            (
                coerce_array(list.values().as_ref(), dest_field.data_type())?,
                list.offsets().clone(),
                list.nulls().cloned(),
            )
        }
        DataType::LargeList(_) => {
            let list = array
                .as_any()
                .downcast_ref::<LargeListArray>()
                .ok_or_else(|| {
                    ArrowError::CastError("expected LargeListArray for nested coerce".to_string())
                })?;
            let mut lengths = Vec::with_capacity(list.len());
            let offsets64 = list.offsets();
            for i in 0..list.len() {
                let len = offsets64[i + 1] - offsets64[i];
                if !(0..=i64::from(i32::MAX)).contains(&len) {
                    return Err(ArrowError::CastError(
                        "LargeList offset does not fit Skippr List".to_string(),
                    ));
                }
                lengths.push(len as usize);
            }
            (
                coerce_array(list.values().as_ref(), dest_field.data_type())?,
                OffsetBuffer::from_lengths(lengths),
                list.nulls().cloned(),
            )
        }
        DataType::FixedSizeList(_, width) => {
            let list = array
                .as_any()
                .downcast_ref::<FixedSizeListArray>()
                .ok_or_else(|| {
                    ArrowError::CastError(
                        "expected FixedSizeListArray for nested coerce".to_string(),
                    )
                })?;
            let width = *width;
            if width < 0 {
                return Err(ArrowError::CastError(
                    "FixedSizeList width must be non-negative".to_string(),
                ));
            }
            (
                coerce_array(list.values().as_ref(), dest_field.data_type())?,
                OffsetBuffer::from_lengths(vec![width as usize; list.len()]),
                list.nulls().cloned(),
            )
        }
        other => {
            return Err(ArrowError::CastError(format!(
                "expected list-like array, got {other}"
            )))
        }
    };
    let field = Arc::new(ArrowField::new(
        dest_field.name(),
        dest_field.data_type().clone(),
        dest_field.is_nullable(),
    ));
    Ok(Arc::new(ListArray::try_new(field, offsets, values, nulls)?))
}

fn rescale_decimal256(array: &dyn Array, src_scale: i8) -> Result<ArrayRef, ArrowError> {
    let decimals = array
        .as_any()
        .downcast_ref::<Decimal256Array>()
        .ok_or_else(|| ArrowError::CastError("expected Decimal256Array".to_string()))?;
    let mut values = Vec::with_capacity(decimals.len());
    for i in 0..decimals.len() {
        if decimals.is_null(i) {
            values.push(None);
            continue;
        }
        let as_i128 = decimals.value(i).to_i128().ok_or_else(|| {
            ArrowError::CastError("Decimal256 does not fit Decimal128(38,9)".to_string())
        })?;
        values.push(Some(rescale_unscaled(
            as_i128,
            src_scale,
            SKIPPR_DECIMAL_SCALE,
        )?));
    }
    let out = Decimal128Array::from(values)
        .with_precision_and_scale(SKIPPR_DECIMAL_PRECISION, SKIPPR_DECIMAL_SCALE)?;
    Ok(Arc::new(out))
}

fn rescale_decimal128(array: &dyn Array, src_scale: i8) -> Result<ArrayRef, ArrowError> {
    let decimals = array
        .as_any()
        .downcast_ref::<Decimal128Array>()
        .ok_or_else(|| ArrowError::CastError("expected Decimal128Array".to_string()))?;
    let mut values = Vec::with_capacity(decimals.len());
    for i in 0..decimals.len() {
        if decimals.is_null(i) {
            values.push(None);
            continue;
        }
        values.push(Some(rescale_unscaled(
            decimals.value(i),
            src_scale,
            SKIPPR_DECIMAL_SCALE,
        )?));
    }
    let out = Decimal128Array::from(values)
        .with_precision_and_scale(SKIPPR_DECIMAL_PRECISION, SKIPPR_DECIMAL_SCALE)?;
    Ok(Arc::new(out))
}

fn rescale_unscaled(value: i128, from_scale: i8, to_scale: i8) -> Result<i128, ArrowError> {
    if from_scale == to_scale {
        return Ok(value);
    }
    if to_scale > from_scale {
        let exp = (to_scale - from_scale) as u32;
        let factor = 10i128
            .checked_pow(exp)
            .ok_or_else(|| ArrowError::CastError("decimal rescale factor overflow".to_string()))?;
        value.checked_mul(factor).ok_or_else(|| {
            ArrowError::CastError("decimal overflow rescaling to (38,9)".to_string())
        })
    } else {
        let exp = (from_scale - to_scale) as u32;
        let factor = 10i128
            .checked_pow(exp)
            .ok_or_else(|| ArrowError::CastError("decimal rescale factor overflow".to_string()))?;
        if value % factor != 0 {
            return Err(ArrowError::CastError(
                "decimal rescale would lose precision".to_string(),
            ));
        }
        Ok(value / factor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Int64Array, StringArray};
    use arrow::datatypes::{Field, Schema};

    fn decimal_batch(values: Vec<Option<i128>>, precision: u8, scale: i8) -> RecordBatch {
        let arr = Decimal128Array::from(values)
            .with_precision_and_scale(precision, scale)
            .unwrap();
        let schema = Arc::new(Schema::new(vec![Field::new(
            "price",
            DataType::Decimal128(precision, scale),
            true,
        )]));
        RecordBatch::try_new(schema, vec![Arc::new(arr)]).unwrap()
    }

    #[test]
    fn decimal_15_2_rescales_to_38_9() {
        // 123.45 at scale 2 is unscaled 12345.
        let src = decimal_batch(vec![Some(12345), None], 15, 2);
        let dest = skippr_schema_from_source(&src.schema()).unwrap();
        let out = coerce_record_batch_to_skippr(&src, dest).unwrap();
        assert_eq!(
            out.schema().field(0).data_type(),
            &DataType::Decimal128(SKIPPR_DECIMAL_PRECISION, SKIPPR_DECIMAL_SCALE)
        );
        let col = out
            .column(0)
            .as_any()
            .downcast_ref::<Decimal128Array>()
            .unwrap();
        assert_eq!(col.value(0), 12345 * 10i128.pow(7));
        assert!(col.is_null(1));
    }

    #[test]
    fn decimal_38_9_is_identity() {
        let src = decimal_batch(vec![Some(123_450_000_000)], 38, 9);
        let dest = skippr_schema_from_source(&src.schema()).unwrap();
        let out = coerce_record_batch_to_skippr(&src, dest).unwrap();
        let col = out
            .column(0)
            .as_any()
            .downcast_ref::<Decimal128Array>()
            .unwrap();
        assert_eq!(col.value(0), 123_450_000_000);
    }

    #[test]
    fn decimal_overflow_fails_closed() {
        let src = decimal_batch(vec![Some(i128::MAX)], 38, 0);
        let dest = skippr_schema_from_source(&src.schema()).unwrap();
        let err = coerce_record_batch_to_skippr(&src, dest).unwrap_err();
        assert!(err.to_string().contains("overflow"));
    }

    #[test]
    fn missing_skippr_column_is_null() {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, true)]));
        let batch =
            RecordBatch::try_new(schema, vec![Arc::new(Int64Array::from(vec![1i64, 2]))]).unwrap();
        let dest = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, true),
            Field::new("name", DataType::Utf8, true),
        ]));
        let out = coerce_record_batch_to_skippr(&batch, dest).unwrap();
        assert_eq!(out.num_columns(), 2);
        assert_eq!(out.column(1).null_count(), 2);
    }

    #[test]
    fn required_source_fields_are_nullable_for_iceberg() {
        let field = ArrowField::new("l_quantity", DataType::Float64, false);
        let meta = output_metadata_for_arrow_field(&field).unwrap();
        assert!(
            meta.nullable(),
            "TPC-H parquet marks required columns; Iceberg scan must still accept nulls after compact"
        );
    }

    #[test]
    fn unknown_arrow_type_fails_closed() {
        let err = skippr_type_for_arrow(&DataType::Duration(arrow::datatypes::TimeUnit::Second))
            .unwrap_err();
        assert!(err.to_string().contains("unsupported Arrow type"));
    }

    #[test]
    fn utf8_passthrough() {
        let schema = Arc::new(Schema::new(vec![Field::new("name", DataType::Utf8, true)]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(StringArray::from(vec!["a", "b"]))],
        )
        .unwrap();
        let dest = skippr_schema_from_source(&schema).unwrap();
        let out = coerce_record_batch_to_skippr(&batch, dest).unwrap();
        let col = out
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(col.value(0), "a");
    }

    #[test]
    fn decimal256_15_2_rescales_to_38_9() {
        use arrow::datatypes::i256;
        let arr = Decimal256Array::from(vec![Some(i256::from_i128(12345))])
            .with_precision_and_scale(15, 2)
            .unwrap();
        let schema = Arc::new(Schema::new(vec![Field::new(
            "price",
            DataType::Decimal256(15, 2),
            true,
        )]));
        let batch = RecordBatch::try_new(schema, vec![Arc::new(arr)]).unwrap();
        let dest = skippr_schema_from_source(&batch.schema()).unwrap();
        let out = coerce_record_batch_to_skippr(&batch, dest).unwrap();
        let col = out
            .column(0)
            .as_any()
            .downcast_ref::<Decimal128Array>()
            .unwrap();
        assert_eq!(col.precision(), SKIPPR_DECIMAL_PRECISION);
        assert_eq!(col.scale(), SKIPPR_DECIMAL_SCALE);
        assert_eq!(col.value(0), 12345 * 10i128.pow(7));
    }

    #[test]
    fn large_list_decimal_15_2_rescales_to_38_9() {
        let values = Decimal128Array::from(vec![Some(12345i128)])
            .with_precision_and_scale(15, 2)
            .unwrap();
        let item = Arc::new(Field::new("item", DataType::Decimal128(15, 2), true));
        let list = LargeListArray::try_new(
            item,
            OffsetBuffer::<i64>::from_lengths([1usize]),
            Arc::new(values),
            None,
        )
        .unwrap();
        let schema = Arc::new(Schema::new(vec![Field::new(
            "prices",
            list.data_type().clone(),
            true,
        )]));
        let batch = RecordBatch::try_new(schema, vec![Arc::new(list)]).unwrap();
        let dest = skippr_schema_from_source(&batch.schema()).unwrap();
        let out = coerce_record_batch_to_skippr(&batch, dest).unwrap();
        let list_out = out.column(0).as_any().downcast_ref::<ListArray>().unwrap();
        let inner = list_out
            .values()
            .as_any()
            .downcast_ref::<Decimal128Array>()
            .unwrap();
        assert_eq!(inner.value(0), 12345 * 10i128.pow(7));
    }

    #[test]
    fn list_decimal_15_2_rescales_to_38_9() {
        use arrow::buffer::OffsetBuffer;
        let values = Decimal128Array::from(vec![Some(12345i128)])
            .with_precision_and_scale(15, 2)
            .unwrap();
        let item = Arc::new(Field::new("item", DataType::Decimal128(15, 2), true));
        let list = ListArray::try_new(
            item,
            OffsetBuffer::from_lengths([1usize]),
            Arc::new(values),
            None,
        )
        .unwrap();
        let schema = Arc::new(Schema::new(vec![Field::new(
            "prices",
            list.data_type().clone(),
            true,
        )]));
        let batch = RecordBatch::try_new(schema, vec![Arc::new(list)]).unwrap();
        let dest = skippr_schema_from_source(&batch.schema()).unwrap();
        let out = coerce_record_batch_to_skippr(&batch, dest).unwrap();
        let list_out = out.column(0).as_any().downcast_ref::<ListArray>().unwrap();
        let inner = list_out
            .values()
            .as_any()
            .downcast_ref::<Decimal128Array>()
            .unwrap();
        assert_eq!(inner.precision(), SKIPPR_DECIMAL_PRECISION);
        assert_eq!(inner.scale(), SKIPPR_DECIMAL_SCALE);
        assert_eq!(inner.value(0), 12345 * 10i128.pow(7));
    }

    #[test]
    fn struct_decimal_15_2_rescales_to_38_9() {
        let values = Decimal128Array::from(vec![Some(12345i128)])
            .with_precision_and_scale(15, 2)
            .unwrap();
        let struct_fields = vec![Field::new("price", DataType::Decimal128(15, 2), true)];
        let st = StructArray::try_new(struct_fields.clone().into(), vec![Arc::new(values)], None)
            .unwrap();
        let schema = Arc::new(Schema::new(vec![Field::new(
            "detail",
            DataType::Struct(struct_fields.into()),
            true,
        )]));
        let batch = RecordBatch::try_new(schema, vec![Arc::new(st)]).unwrap();
        let dest = skippr_schema_from_source(&batch.schema()).unwrap();
        let out = coerce_record_batch_to_skippr(&batch, dest).unwrap();
        let st_out = out
            .column(0)
            .as_any()
            .downcast_ref::<StructArray>()
            .unwrap();
        let price = st_out
            .column_by_name("price")
            .unwrap()
            .as_any()
            .downcast_ref::<Decimal128Array>()
            .unwrap();
        assert_eq!(price.precision(), SKIPPR_DECIMAL_PRECISION);
        assert_eq!(price.scale(), SKIPPR_DECIMAL_SCALE);
        assert_eq!(price.value(0), 12345 * 10i128.pow(7));
    }

    #[test]
    fn list_of_int64_keeps_int64_items() {
        let item = Arc::new(Field::new("item", DataType::Int64, true));
        let schema = Arc::new(Schema::new(vec![Field::new(
            "ids",
            DataType::List(item),
            true,
        )]));
        let dest = skippr_schema_from_source(&schema).unwrap();
        match dest.field(0).data_type() {
            DataType::List(inner) => assert_eq!(inner.data_type(), &DataType::Int64),
            other => panic!("expected List<Int64>, got {other:?}"),
        }
    }

    #[test]
    fn arrow_map_fails_closed() {
        let entries = Arc::new(ArrowField::new(
            "entries",
            DataType::Struct(
                vec![
                    ArrowField::new("key", DataType::Utf8, false),
                    ArrowField::new("value", DataType::Int64, true),
                ]
                .into(),
            ),
            false,
        ));
        let err = skippr_type_for_arrow(&DataType::Map(entries, false)).unwrap_err();
        assert!(err.to_string().contains("unsupported Arrow Map"), "{err}");
    }

    #[test]
    fn coerced_json_rescales_decimal_15_2() {
        let src = decimal_batch(vec![Some(12345)], 15, 2);
        let values = coerced_batches_to_json_values(&[src]).unwrap();
        assert_eq!(values.len(), 1);
        let price = &values[0]["price"];
        let text = price.to_string();
        assert!(
            text.contains("123.45"),
            "expected coerced 123.45, got {price}"
        );
    }
}
