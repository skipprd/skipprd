//! Shared WAL slice decode for query and STREAM. Catalog `SliceDirEntry` is
//! the identity; this module seeks `start`/`len` and MUST NOT FOOT-admit.

use std::io::{self, Read, Seek};

use arrow::array::RecordBatch;
use arrow::ipc::reader::StreamReader;
use skippr_wal_catalog::{PayloadLocation, SliceDirEntry};

use crate::buffer::segment_file::PART_CODEC_ARROW;
use crate::buffer::wal_store::block_on_async;
use crate::plugins::cdc::WalPartMeta;

pub(crate) fn read_slice_part_meta(
    location: &PayloadLocation,
    slice: &SliceDirEntry,
) -> io::Result<Option<WalPartMeta>> {
    if slice.part_meta_len == 0 {
        return Ok(None);
    }
    let blob = match location {
        PayloadLocation::Disk { path } => read_disk_range(
            std::path::PathBuf::from(path),
            slice.part_meta_start,
            slice.part_meta_len,
        )?,
        PayloadLocation::S3 { bucket, key } => {
            read_s3_range(bucket, key, slice.part_meta_start, slice.part_meta_len)?
        }
    };
    if blob.is_empty() {
        return Ok(None);
    }
    let meta: WalPartMeta = bincode::deserialize(&blob)
        .map_err(|err| io::Error::other(format!("decode WAL PART metadata: {err}")))?;
    meta.validate()
        .map_err(|err| io::Error::other(format!("invalid WAL PART metadata: {err}")))?;
    Ok(Some(meta))
}

fn read_disk_range(path: std::path::PathBuf, start: u64, len: u64) -> io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(&path)?;
    file.seek(std::io::SeekFrom::Start(start))?;
    let mut buf = vec![0u8; len as usize];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

fn map_s3_payload_err(err: io::Error) -> io::Error {
    if err.kind() == io::ErrorKind::NotFound {
        return err;
    }
    let text = err.to_string();
    if text.contains("NoSuchKey") || text.contains("NotFound") {
        io::Error::new(io::ErrorKind::NotFound, err)
    } else {
        err
    }
}

fn read_s3_range(bucket: &str, key: &str, start: u64, len: u64) -> io::Result<Vec<u8>> {
    let bucket = bucket.to_string();
    let key = key.to_string();
    block_on_async(async move {
        let bytes = crate::helpers::s3::get_object_bytes_for_bucket(&bucket, &key)
            .await
            .map_err(map_s3_payload_err)?;
        let end = start.saturating_add(len) as usize;
        let start = start as usize;
        if end > bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "WAL slice range {start}..{end} past object len {}",
                    bytes.len()
                ),
            ));
        }
        Ok(bytes[start..end].to_vec())
    })
}

pub(crate) fn read_slice_batches(
    location: &PayloadLocation,
    slice: &SliceDirEntry,
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
    slice: &SliceDirEntry,
) -> io::Result<Vec<RecordBatch>> {
    let mut file = std::fs::File::open(&path)?;
    file.seek(std::io::SeekFrom::Start(slice.start))?;
    let reader = std::io::BufReader::new(file);
    let mut take = reader.take(slice.len);
    decode_arrow_stream(&mut take)
}

fn read_s3_slice(bucket: &str, key: &str, slice: &SliceDirEntry) -> io::Result<Vec<RecordBatch>> {
    let bucket = bucket.to_string();
    let key = key.to_string();
    let slice = slice.clone();
    block_on_async(async move {
        let bytes = crate::helpers::s3::get_object_bytes_for_bucket(&bucket, &key)
            .await
            .map_err(map_s3_payload_err)?;
        decode_slice_bytes(&bytes, &slice)
    })
}

fn decode_slice_bytes(bytes: &[u8], slice: &SliceDirEntry) -> io::Result<Vec<RecordBatch>> {
    let start = slice.start as usize;
    let end = start.saturating_add(slice.len as usize);
    if end > bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!(
                "WAL slice range {start}..{end} past object len {}",
                bytes.len()
            ),
        ));
    }
    let mut cursor = std::io::Cursor::new(&bytes[start..end]);
    decode_arrow_stream(&mut cursor)
}

fn decode_arrow_stream<R: Read>(reader: &mut R) -> io::Result<Vec<RecordBatch>> {
    let sr = StreamReader::try_new(reader, None)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
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
    use std::collections::HashMap;
    use std::sync::Arc;

    fn dir_entry(
        start: u64,
        len: u64,
        codec: u8,
        part_meta_start: u64,
        part_meta_len: u64,
    ) -> SliceDirEntry {
        SliceDirEntry {
            ordinal: 0,
            namespace: "ns".into(),
            partition: "p0".into(),
            event_time: None,
            start,
            len,
            codec,
            row_count: None,
            sink_ref: "data_sinks.iceberg".into(),
            schema_fingerprint: "fp".into(),
            cdc_meta_hash: None,
            part_meta_start,
            part_meta_len,
        }
    }

    #[test]
    fn decode_does_not_admit_or_hash() {
        let src = include_str!("wal_slice.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        assert!(!prod.contains("admit_payload"));
        assert!(!prod.contains("payload_bytes"));
        assert!(!prod.contains("verify_body_checksum"));
        assert!(!prod.contains("read_index_path"));
        assert!(!prod.contains("return Ok(Vec::new())"));
    }

    #[test]
    fn missing_disk_payload_is_not_found() {
        let loc = PayloadLocation::Disk {
            path: "/no/such/wal-slice.seg".into(),
        };
        let err = read_slice_batches(&loc, &dir_entry(0, 10, PART_CODEC_ARROW, 0, 0)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
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
        let encoded = encode_snapshot(
            &HashMap::new(),
            &batches,
            &HashMap::new(),
            &HashMap::new(),
            std::time::SystemTime::UNIX_EPOCH,
        )
        .unwrap();
        let idx = &encoded.meta.index[0];
        let slice = dir_entry(
            idx.start,
            idx.len,
            idx.codec,
            idx.part_meta_start,
            idx.part_meta_len,
        );
        let out = decode_slice_bytes(&encoded.bytes, &slice).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].num_rows(), 1);
        assert_eq!(slice.part_meta_len, 0);
    }

    #[test]
    fn cdc_part_meta_roundtrips_from_catalog_offsets() {
        use crate::plugins::cdc::{MutationKind, WalPartMeta, WalRowMeta};
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
            part.clone(),
            WalPayload::Arrow(vec![RecordBatch::try_new(
                schema,
                vec![Arc::new(StringArray::from(vec!["evt-a"]))],
            )
            .unwrap()]),
        );
        let meta = WalPartMeta::cdc(
            vec![WalRowMeta {
                mutation: MutationKind::Update,
                event_id: b"1".to_vec(),
                order_token: vec![2],
            }],
            1,
        )
        .unwrap();
        let mut blobs = HashMap::new();
        blobs.insert(part, bincode::serialize(&meta).unwrap());
        let encoded = encode_snapshot(
            &HashMap::new(),
            &batches,
            &HashMap::new(),
            &blobs,
            std::time::SystemTime::UNIX_EPOCH,
        )
        .unwrap();
        let idx = &encoded.meta.index[0];
        assert!(idx.part_meta_len > 0);
        let start = idx.part_meta_start as usize;
        let end = start + idx.part_meta_len as usize;
        let decoded: WalPartMeta = bincode::deserialize(&encoded.bytes[start..end]).unwrap();
        assert_eq!(decoded.kind, crate::plugins::cdc::WalPartKind::Cdc);
        assert_eq!(decoded.rows[0].order_token, vec![2]);
    }
}
