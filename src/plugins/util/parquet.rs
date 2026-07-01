use bytes::Bytes;
use datafusion::physical_plan::SendableRecordBatchStream;
use futures::StreamExt;
use parquet::arrow::ArrowWriter;
use std::io;

pub struct ParquetBytes {
    pub bytes: Bytes,
    pub size_bytes: u64,
    pub meta_data: parquet::format::FileMetaData,
}

pub async fn serialize_to_parquet(
    mut batches: SendableRecordBatchStream,
) -> Result<ParquetBytes, io::Error> {
    let schema = batches.schema();

    let order_fields = crate::converters::parquet_ordering::resolve_effective_order(&schema);
    let props = crate::converters::parquet_ordering::build_writer_properties(
        &schema,
        &order_fields,
        crate::converters::parquet_ordering::default_streaming_row_group_size(),
    );

    let mut bytes = Vec::new();
    let mut writer = ArrowWriter::try_new(&mut bytes, schema, Some(props))?;

    while let Some(batch) = batches.next().await {
        let batch = batch?;
        let batch = crate::converters::parquet_ordering::sort_batch(&batch, &order_fields)?;
        writer.write(&batch)?;
    }

    let writer_meta = writer.close()?;
    if writer_meta.num_rows == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "No rows to write to parquet",
        ));
    }

    let size_bytes = bytes.len() as u64;

    Ok(ParquetBytes {
        meta_data: writer_meta,
        bytes: Bytes::from(bytes),
        size_bytes,
    })
}
