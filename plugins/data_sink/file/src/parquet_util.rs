use std::io;

use bytes::Bytes;
use datafusion::physical_plan::SendableRecordBatchStream;
use futures::StreamExt;
use parquet::arrow::ArrowWriter;
use parquet::file::metadata::ParquetMetaData;

pub struct ParquetBytes {
    pub bytes: Bytes,
    pub size_bytes: u64,
    pub num_rows: u64,
    #[allow(dead_code)]
    pub meta_data: ParquetMetaData,
}

#[allow(dead_code)]
pub async fn serialize_to_parquet(
    batches: SendableRecordBatchStream,
) -> Result<ParquetBytes, io::Error> {
    serialize_to_parquet_with_order_fields(batches, &[]).await
}

pub async fn serialize_to_parquet_with_order_fields(
    mut batches: SendableRecordBatchStream,
    configured_order_fields: &[String],
) -> Result<ParquetBytes, io::Error> {
    let schema = batches.schema();

    let order_fields =
        skippr_runtime_sdk::converters::parquet_ordering::resolve_effective_order_from_fields(
            &schema,
            configured_order_fields,
        );
    let row_group_size =
        skippr_runtime_sdk::converters::parquet_ordering::default_streaming_row_group_size();
    let props = skippr_runtime_sdk::converters::parquet_ordering::build_writer_properties(
        &schema,
        &order_fields,
        row_group_size,
    );

    let mut bytes = Vec::new();
    let mut writer = ArrowWriter::try_new(&mut bytes, schema, Some(props))?;

    while let Some(batch) = batches.next().await {
        let batch = batch?;
        let batch =
            skippr_runtime_sdk::converters::parquet_ordering::sort_batch(&batch, &order_fields)?;
        writer.write(&batch)?;
    }

    let writer_meta = writer.close()?;
    let num_rows = writer_meta.file_metadata().num_rows() as u64;
    if num_rows == 0 {
        return Err(io::Error::other("No rows to write to parquet"));
    }

    Ok(ParquetBytes {
        size_bytes: bytes.len() as u64,
        bytes: Bytes::from(bytes),
        num_rows,
        meta_data: writer_meta,
    })
}
