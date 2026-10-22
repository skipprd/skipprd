//! Shared WAL slice decode for query and STREAM. Catalog `SliceIndexEntry` is
//! the identity; this module seeks `start`/`len` and MUST NOT FOOT-admit.

use std::io::{self, Read, Seek};

use arrow::array::RecordBatch;
use arrow::ipc::reader::StreamReader;
use skippr_wal_catalog::{PayloadLocation, SliceIndexEntry};

use crate::buffer::segment_file::PART_CODEC_ARROW;
use crate::buffer::wal_store::block_on_async;

pub(crate) fn read_slice_batches(
    location: &PayloadLocation,
    slice: &SliceIndexEntry,
) -> io::Result<Vec<RecordBatch>> {
    if slice.codec != PART_CODEC_ARROW {
        return Err(io::Error::other("sqlrt does not read copy WAL parts"));
    }
    match location {
        PayloadLocation::Disk { path } => read_disk_slice(std::path::PathBuf::from(path), slice),
        PayloadLocation::S3 { bucket, key } => read_s3_slice(bucket, key, slice),
    }
}

fn read_disk_slice(
    path: std::path::PathBuf,
    slice: &SliceIndexEntry,
) -> io::Result<Vec<RecordBatch>> {
    let mut file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(_) => return Ok(Vec::new()),
    };
    if file.seek(std::io::SeekFrom::Start(slice.start)).is_err() {
        return Ok(Vec::new());
    }
    let reader = std::io::BufReader::new(file);
    let mut take = reader.take(slice.len);
    decode_arrow_stream(&mut take)
}

fn read_s3_slice(bucket: &str, key: &str, slice: &SliceIndexEntry) -> io::Result<Vec<RecordBatch>> {
    let bucket = bucket.to_string();
    let key = key.to_string();
    let slice = slice.clone();
    block_on_async(async move {
        let bytes = crate::helpers::s3::get_object_bytes_for_bucket(&bucket, &key)
            .await
            .map_err(|err| io::Error::other(err.to_string()))?;
        decode_slice_bytes(&bytes, &slice)
    })
}

fn decode_slice_bytes(bytes: &[u8], slice: &SliceIndexEntry) -> io::Result<Vec<RecordBatch>> {
    let start = slice.start as usize;
    let end = start.saturating_add(slice.len as usize);
    if end > bytes.len() {
        return Ok(Vec::new());
    }
    let mut cursor = std::io::Cursor::new(&bytes[start..end]);
    decode_arrow_stream(&mut cursor)
}

fn decode_arrow_stream<R: Read>(reader: &mut R) -> io::Result<Vec<RecordBatch>> {
    let sr = match StreamReader::try_new(reader, None) {
        Ok(sr) => sr,
        Err(_) => return Ok(Vec::new()),
    };
    let mut out = Vec::new();
    for batch in sr {
        out.push(batch.map_err(|err| io::Error::other(err.to_string()))?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::segment_file::{encode_snapshot, PartitionKey, WalPayload};
    use arrow::array::StringArray;
    use arrow::datatypes::{DataType, Field, Schema};
    use skippr_lease::SegmentId;
    use std::collections::HashMap;
    use std::sync::Arc;

    #[test]
    fn decode_does_not_admit_or_hash() {
        let src = include_str!("wal_slice.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        assert!(!prod.contains("admit_payload"));
        assert!(!prod.contains("payload_bytes"));
        assert!(!prod.contains("verify_body_checksum"));
        assert!(!prod.contains("read_index_path"));
    }

    #[test]
    fn encoded_part_roundtrips_from_catalog_offsets() {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Utf8, true)]));
        let part = PartitionKey {
            sink_ref: "data_sinks.iceberg".into(),
            namespace: "ns".into(),
            partition: "p0".into(),
            time: None,
            schema_fingerprint: "fp".into(),
        };
        let mut batches = HashMap::new();
        batches.insert(
            part,
            WalPayload::Arrow(vec![RecordBatch::try_new(
                schema,
                vec![Arc::new(StringArray::from(vec!["evt-a"]))],
            )
            .unwrap()]),
        );
        let encoded =
            encode_snapshot(&HashMap::new(), &batches, &HashMap::new(), &HashMap::new()).unwrap();
        let idx = &encoded.meta.index[0];
        let slice = SliceIndexEntry {
            segment_id: SegmentId::new("s").unwrap(),
            ordinal: idx.slice_ordinal,
            namespace: idx.key.namespace.clone(),
            partition: idx.key.partition.clone(),
            start: idx.start,
            len: idx.len,
            codec: idx.codec,
            version: 0,
        };
        let out = decode_slice_bytes(&encoded.bytes, &slice).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].num_rows(), 1);
    }
}
