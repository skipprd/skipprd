use std::io;

use bytes::Bytes;
use datafusion::physical_plan::SendableRecordBatchStream;
use futures::StreamExt;
use parquet::arrow::ArrowWriter;

pub struct ParquetBytes {
    pub bytes: Bytes,
    pub size_bytes: u64,
    pub meta_data: parquet::format::FileMetaData,
}

pub async fn serialize_to_parquet(
    mut batches: SendableRecordBatchStream,
) -> Result<ParquetBytes, io::Error> {
    let schema = batches.schema();

    let mut raw_batches = Vec::new();
    while let Some(batch) = batches.next().await {
        raw_batches.push(batch?);
    }

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
