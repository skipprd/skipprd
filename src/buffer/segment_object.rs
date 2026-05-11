use crate::buffer::segment_file::{
    PartitionKey, SegmentFile, SegmentFileMetadata, SegmentPartitionIndexEntry,
};
use crate::metrics::counters as metrics_counters;
use arrow::array::RecordBatch;
use arrow::ipc::writer::{IpcWriteOptions, StreamWriter};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::CompletedPart;
use rand::{thread_rng, Rng};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io;
use std::time::Duration;
use std::time::SystemTime;
use url::Url;

const MPU_PART_SIZE: usize = 8 * 1024 * 1024;

struct S3MultipartWriter {
    bucket: String,
    key: String,
    upload_id: String,
    client: aws_sdk_s3::Client,
    buf: Vec<u8>,
    chunk_size: usize,
    parts: Vec<CompletedPart>,
    hasher: Sha256,
    total_written: u64,
    part_number: i32,
}

impl S3MultipartWriter {
    async fn begin(client: aws_sdk_s3::Client, bucket: String, key: String) -> io::Result<Self> {
        // Retry MPU create to handle transient dispatch/network issues
        let resp = {
            let mut attempts: u32 = 0;
            loop {
                let res = client
                    .create_multipart_upload()
                    .bucket(&bucket)
                    .key(&key)
                    .content_type("application/octet-stream")
                    .send()
                    .await;
                match res {
                    Ok(v) => break v,
                    Err(e) => {
                        attempts = attempts.saturating_add(1);
                        metrics_counters::add_s3_wal_retry(1);
                        if attempts >= 5 {
                            metrics_counters::add_s3_wal_error(1);
                            return Err(io::Error::new(
                                io::ErrorKind::Other,
                                format!("s3 mpu create: {}", e),
                            ));
                        }
                        let base = 200u64.saturating_mul(1u64 << attempts.min(10));
                        let jitter: u64 = thread_rng().gen_range(0..100);
                        tokio::time::sleep(Duration::from_millis((base + jitter).min(5_000))).await;
                        continue;
                    }
                }
            }
        };
        let upload_id = resp.upload_id().unwrap_or_default().to_string();
        Ok(S3MultipartWriter {
            bucket,
            key,
            upload_id,
            client,
            buf: Vec::with_capacity(MPU_PART_SIZE),
            chunk_size: MPU_PART_SIZE,
            parts: Vec::new(),
            hasher: Sha256::new(),
            total_written: 0,
            part_number: 1,
        })
    }

    async fn flush_part(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let body = std::mem::take(&mut self.buf);
        let pn = self.part_number;
        // Retry part upload to mitigate transient dispatch/socket errors
        let out = {
            let mut attempts: u32 = 0;
            loop {
                let res = self
                    .client
                    .upload_part()
                    .bucket(&self.bucket)
                    .key(&self.key)
                    .upload_id(&self.upload_id)
                    .part_number(pn)
                    .body(ByteStream::from(body.clone()))
                    .send()
                    .await;
                match res {
                    Ok(v) => break v,
                    Err(e) => {
                        attempts = attempts.saturating_add(1);
                        metrics_counters::add_s3_wal_retry(1);
                        if attempts >= 5 {
                            metrics_counters::add_s3_wal_error(1);
                            return Err(io::Error::new(
                                io::ErrorKind::Other,
                                format!("s3 upload part {}: {}", pn, e),
                            ));
                        }
                        let base = 200u64.saturating_mul(1u64 << attempts.min(10));
                        let jitter: u64 = thread_rng().gen_range(0..100);
                        tokio::time::sleep(Duration::from_millis((base + jitter).min(5_000))).await;
                        continue;
                    }
                }
            }
        };
        let etag = out.e_tag().unwrap_or_default().to_string();
        self.parts.push(
            CompletedPart::builder()
                .set_e_tag(Some(etag))
                .part_number(pn)
                .build(),
        );
        self.part_number += 1;
        Ok(())
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.hasher.update(bytes);
        self.total_written = self.total_written.saturating_add(bytes.len() as u64);
        self.buf.extend_from_slice(bytes);
        Ok(())
    }

    async fn maybe_flush(&mut self) -> io::Result<()> {
        if self.buf.len() >= self.chunk_size {
            self.flush_part().await?;
        }
        Ok(())
    }

    fn current_sha256(&self) -> [u8; 32] {
        let h = self.hasher.clone();
        let digest = h.finalize();
        let mut sha = [0u8; 32];
        sha.copy_from_slice(&digest[..]);
        sha
    }

    async fn complete(mut self) -> io::Result<()> {
        self.flush_part().await?;
        // Retry MPU complete for transient issues
        let mut attempts: u32 = 0;
        loop {
            let comp = aws_sdk_s3::types::CompletedMultipartUpload::builder()
                .set_parts(Some(self.parts.clone()))
                .build();
            let res = self
                .client
                .complete_multipart_upload()
                .bucket(&self.bucket)
                .key(&self.key)
                .upload_id(&self.upload_id)
                .multipart_upload(comp)
                .send()
                .await;
            match res {
                Ok(_) => break,
                Err(e) => {
                    attempts = attempts.saturating_add(1);
                    metrics_counters::add_s3_wal_retry(1);
                    if attempts >= 5 {
                        metrics_counters::add_s3_wal_error(1);
                        return Err(io::Error::new(
                            io::ErrorKind::Other,
                            format!("s3 mpu complete: {}", e),
                        ));
                    }
                    let base = 200u64.saturating_mul(1u64 << attempts.min(10));
                    let jitter: u64 = thread_rng().gen_range(0..100);
                    tokio::time::sleep(Duration::from_millis((base + jitter).min(5_000))).await;
                    continue;
                }
            }
        }
        Ok(())
    }
}

pub struct SegmentObject;

impl SegmentObject {
    fn compute_keys(prefix_url: &str, snapshot_id: &str) -> io::Result<(String, String, String)> {
        let u = Url::parse(prefix_url)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        if u.scheme() != "s3" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "prefix must be s3://",
            ));
        }
        let bucket = u
            .host_str()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing bucket"))?
            .to_string();
        let base = u
            .path()
            .trim_start_matches('/')
            .trim_end_matches('/')
            .to_string();
        let now = chrono::Utc::now();
        let key_prefix = format!(
            "{}/p_year={}/p_month={}/p_day={}",
            base,
            now.format("%Y"),
            now.format("%m"),
            now.format("%d")
        );
        let seg_key = format!("{}/{}.seg", key_prefix, snapshot_id);
        let commit_key = format!("{}.commit", seg_key);
        Ok((bucket, seg_key, commit_key))
    }

    /// Streams a WAL snapshot to S3 and publishes a commit marker.
    /// Returns (meta, rows, sha256, bucket, key). Segment bytes live on S3 only after commit.
    pub async fn stream_snapshot_to_s3(
        client: &aws_sdk_s3::Client,
        prefix_url: &str,
        snapshot_id: &str,
        offsets: &HashMap<crate::helpers::offsets::OffsetKey, u64>,
        batches: &HashMap<PartitionKey, Vec<RecordBatch>>,
        parts_meta: &HashMap<PartitionKey, (u64, SystemTime)>,
        part_meta_blobs: &HashMap<PartitionKey, Vec<u8>>,
    ) -> io::Result<(
        SegmentFileMetadata,
        u64,      /*rows*/
        [u8; 32], /*sha256*/
        String,   /*bucket*/
        String,   /*key*/
    )> {
        let (bucket, seg_key, commit_key) = Self::compute_keys(prefix_url, snapshot_id)?;
        let mut writer =
            S3MultipartWriter::begin(client.clone(), bucket.clone(), seg_key.clone()).await?;

        writer.write_bytes(b"SEGF")?;
        writer.write_bytes(&3u32.to_le_bytes())?;
        let created_at_secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        writer.write_bytes(&created_at_secs.to_le_bytes())?;

        let offsets_blob = bincode::serialize(offsets).unwrap();
        let offsets_len = offsets_blob.len() as u64;
        writer.write_bytes(&offsets_len.to_le_bytes())?;
        writer.write_bytes(&offsets_blob)?;
        writer.maybe_flush().await?;

        let mut total_rows: u64 = 0;
        let mut total_bytes: u64 = 0;
        let mut parts_count: u32 = 0;
        let mut index: Vec<SegmentPartitionIndexEntry> = Vec::with_capacity(batches.len());
        // Track the byte position within the virtual segment stream.
        for (key, rbatches) in batches.iter() {
            if rbatches.is_empty() {
                continue;
            }
            parts_count = parts_count.saturating_add(1);
            writer.write_bytes(b"PART")?;
            let key_blob = bincode::serialize(key).unwrap();
            let key_len = key_blob.len() as u64;
            writer.write_bytes(&key_len.to_le_bytes())?;
            writer.write_bytes(&key_blob)?;
            let (p_bytes, p_updated) = parts_meta
                .get(key)
                .cloned()
                .unwrap_or((0, SystemTime::now()));
            let updated_secs = p_updated
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            writer.write_bytes(&p_bytes.to_le_bytes())?;
            writer.write_bytes(&updated_secs.to_le_bytes())?;

            let meta_blob = part_meta_blobs.get(key).cloned().unwrap_or_default();
            let meta_len = meta_blob.len() as u64;
            writer.write_bytes(&meta_len.to_le_bytes())?;
            if meta_len > 0 {
                writer.write_bytes(&meta_blob)?;
            }

            let mut data_buf: Vec<u8> = Vec::new();
            {
                let options = IpcWriteOptions::default();
                let mut aw = StreamWriter::try_new_with_options(
                    &mut data_buf,
                    &rbatches[0].schema(),
                    options,
                )
                .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("arrow: {}", e)))?;
                for b in rbatches.iter() {
                    total_rows += b.num_rows() as u64;
                    aw.write(b).map_err(|e| {
                        io::Error::new(io::ErrorKind::Other, format!("arrow: {}", e))
                    })?;
                }
                aw.finish()
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("arrow: {}", e)))?;
            }
            let data_len = data_buf.len() as u64;
            total_bytes = total_bytes.saturating_add(data_len);
            writer.write_bytes(&data_len.to_le_bytes())?;

            // The Arrow data starts right after the data_len u64 we just wrote.
            let start = writer.total_written;
            writer.write_bytes(&data_buf)?;
            writer.maybe_flush().await?;

            index.push(SegmentPartitionIndexEntry {
                key: key.clone(),
                bytes: p_bytes,
                updated_at_secs: updated_secs,
                start,
                len: data_len,
            });
        }

        let sha = writer.current_sha256();
        writer.write_bytes(b"FOOT")?;
        writer.write_bytes(&parts_count.to_le_bytes())?;
        writer.write_bytes(&sha)?;
        writer.maybe_flush().await?;

        writer.complete().await?;

        let commit_bytes = SegmentFile::build_commit_header_bytes(parts_count, total_bytes, &sha);
        {
            let mut attempts: u32 = 0;
            loop {
                let res = client
                    .put_object()
                    .bucket(&bucket)
                    .key(&commit_key)
                    .body(ByteStream::from(commit_bytes.clone().to_vec()))
                    .content_type("application/octet-stream")
                    .send()
                    .await;
                match res {
                    Ok(_) => break,
                    Err(e) => {
                        attempts = attempts.saturating_add(1);
                        metrics_counters::add_s3_wal_retry(1);
                        if attempts >= 5 {
                            metrics_counters::add_s3_wal_error(1);
                            return Err(io::Error::new(
                                io::ErrorKind::Other,
                                format!("s3 put commit: {}", e),
                            ));
                        }
                        let base = 200u64.saturating_mul(1u64 << attempts.min(10));
                        let jitter: u64 = thread_rng().gen_range(0..100);
                        tokio::time::sleep(Duration::from_millis((base + jitter).min(5_000))).await;
                        continue;
                    }
                }
            }
        }

        let meta = SegmentFileMetadata {
            created_at_secs,
            total_bytes,
            num_partitions: parts_count,
            offsets: offsets.clone(),
            index,
        };
        Ok((meta, total_rows, sha, bucket, seg_key))
    }
}
