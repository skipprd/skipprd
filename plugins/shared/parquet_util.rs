use std::collections::HashSet;
use std::io;
use std::sync::Arc;

use arrow::array::RecordBatch;
use arrow::compute::cast;
use arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use bytes::Bytes;
use datafusion::physical_plan::SendableRecordBatchStream;
use futures::StreamExt;
use parquet::arrow::ArrowWriter;

pub struct ParquetBytes {
    pub bytes: Bytes,
    pub size_bytes: u64,
    pub meta_data: parquet::format::FileMetaData,
}

pub fn coerce_timestamp_dates_to_date32(
    batch: RecordBatch,
    date_field_names: &HashSet<String>,
) -> Result<RecordBatch, io::Error> {
    if date_field_names.is_empty() {
        return Ok(batch);
    }

    let schema = batch.schema();
    let mut new_columns = batch.columns().to_vec();
    let mut new_fields: Vec<Arc<Field>> = schema.fields().iter().cloned().collect();
    let mut changed = false;

    for (idx, field) in schema.fields().iter().enumerate() {
        if !date_field_names.contains(field.name()) {
            continue;
        }
        if field.data_type() == &DataType::Timestamp(TimeUnit::Millisecond, None) {
            let casted = cast(new_columns[idx].as_ref(), &DataType::Date32)
                .map_err(|err| io::Error::other(err.to_string()))?;
            new_columns[idx] = casted;
            new_fields[idx] = Arc::new(Field::new(
                field.name(),
                DataType::Date32,
                field.is_nullable(),
            ));
            changed = true;
        }
    }

    if !changed {
        return Ok(batch);
    }

    RecordBatch::try_new(Arc::new(Schema::new(new_fields)), new_columns)
        .map_err(|err| io::Error::other(err.to_string()))
}

async fn collect_batches(
    mut batches: SendableRecordBatchStream,
    date_field_names: Option<&HashSet<String>>,
) -> Result<Vec<RecordBatch>, io::Error> {
    let mut raw_batches = Vec::new();
    while let Some(batch) = batches.next().await {
        let batch = batch.map_err(|err| io::Error::other(err.to_string()))?;
        let batch = match date_field_names {
            Some(names) => coerce_timestamp_dates_to_date32(batch, names)?,
            None => batch,
        };
        raw_batches.push(batch);
    }
    Ok(raw_batches)
}

fn write_parquet_batches(
    raw_batches: Vec<RecordBatch>,
    schema: SchemaRef,
) -> Result<ParquetBytes, io::Error> {
    let order_fields =
        skippr_runtime_sdk::converters::parquet_ordering::resolve_effective_order(&schema);
    let sorted_batches = skippr_runtime_sdk::converters::parquet_ordering::materialize_and_sort(
        raw_batches,
        &schema,
        &order_fields,
    )?;

    let row_group_size = skippr_runtime_sdk::converters::parquet_ordering::estimate_row_group_size(
        &sorted_batches,
        &order_fields,
    );
    let props = skippr_runtime_sdk::converters::parquet_ordering::build_writer_properties(
        &schema,
        &order_fields,
        row_group_size,
    );

    let mut bytes = Vec::new();
    let mut writer = ArrowWriter::try_new(&mut bytes, schema, Some(props))?;

    for batch in &sorted_batches {
        writer.write(batch)?;
    }

    let writer_meta = writer.close()?;
    if writer_meta.num_rows == 0 {
        return Err(io::Error::other("No rows to write to parquet"));
    }

    Ok(ParquetBytes {
        size_bytes: bytes.len() as u64,
        bytes: Bytes::from(bytes),
        meta_data: writer_meta,
    })
}

pub async fn serialize_to_parquet(
    batches: SendableRecordBatchStream,
) -> Result<ParquetBytes, io::Error> {
    let raw_batches = collect_batches(batches, None).await?;
    if raw_batches.is_empty() {
        return Err(io::Error::other("No rows to write to parquet"));
    }
    let schema = raw_batches[0].schema();
    write_parquet_batches(raw_batches, schema)
}

pub async fn serialize_to_parquet_for_iceberg(
    batches: SendableRecordBatchStream,
    date_field_names: &HashSet<String>,
) -> Result<ParquetBytes, io::Error> {
    let raw_batches = collect_batches(batches, Some(date_field_names)).await?;
    if raw_batches.is_empty() {
        return Err(io::Error::other("No rows to write to parquet"));
    }
    let schema = raw_batches[0].schema();
    write_parquet_batches(raw_batches, schema)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Date32Array, Int64Array, TimestampMillisecondArray};
    #[test]
    fn coerce_timestamp_date_column_to_date32() {
        let days = 19_872i32; // 2024-06-15
        let millis = i64::from(days) * 86_400_000;
        let schema = Arc::new(Schema::new(vec![Field::new(
            "date",
            DataType::Timestamp(TimeUnit::Millisecond, None),
            false,
        )]));
        let batch = RecordBatch::try_new(
            schema,
            vec![Arc::new(TimestampMillisecondArray::from(vec![millis]))],
        )
        .unwrap();

        let mut names = HashSet::new();
        names.insert("date".to_string());
        let coerced = coerce_timestamp_dates_to_date32(batch, &names).unwrap();
        assert_eq!(
            coerced.schema().field(0).data_type(),
            &DataType::Date32
        );
        let dates = coerced
            .column(0)
            .as_any()
            .downcast_ref::<Date32Array>()
            .unwrap();
        assert_eq!(dates.value(0), days);
    }

    #[test]
    fn coerce_skips_non_date_columns() {
        let schema = Arc::new(Schema::new(vec![Field::new(
            "count",
            DataType::Int64,
            false,
        )]));
        let batch =
            RecordBatch::try_new(schema.clone(), vec![Arc::new(Int64Array::from(vec![1]))])
                .unwrap();
        let names = HashSet::new();
        let coerced = coerce_timestamp_dates_to_date32(batch.clone(), &names).unwrap();
        assert_eq!(coerced.schema(), batch.schema());
    }
}
