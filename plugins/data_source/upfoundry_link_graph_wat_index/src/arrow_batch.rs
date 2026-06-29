use std::sync::Arc;

use arrow::array::{ArrayRef, Int64Builder, RecordBatch, StringBuilder, UInt32Builder};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::{IpcWriteOptions, StreamWriter};
use arrow_schema::SchemaRef;

use crate::streams::NAMESPACE_TARGET_INDEX;

pub fn target_index_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("crawl_id", DataType::Utf8, true),
        Field::new("target_domain_hash_bucket", DataType::Utf8, true),
        Field::new("target_domain_id", DataType::Utf8, true),
        Field::new("target_domain", DataType::Utf8, true),
        Field::new("source_url_id", DataType::Utf8, true),
        Field::new("source_url", DataType::Utf8, true),
        Field::new("source_domain_id", DataType::Utf8, true),
        Field::new("source_domain", DataType::Utf8, true),
        Field::new("source_host", DataType::Utf8, true),
        Field::new("warc_filename", DataType::Utf8, true),
        Field::new("warc_record_offset", DataType::Int64, true),
        Field::new("warc_record_length", DataType::Int64, true),
        Field::new("wat_filename", DataType::Utf8, true),
        Field::new("wat_record_offset", DataType::Int64, true),
        Field::new("wat_record_length", DataType::Int64, true),
        Field::new("fetch_status", DataType::UInt32, true),
        Field::new("content_mime_type", DataType::Utf8, true),
        Field::new("fetch_time", DataType::Utf8, true),
        Field::new("link_count_to_target", DataType::UInt32, true),
        Field::new("wat_path", DataType::Utf8, true),
    ]))
}

#[derive(Debug, Clone)]
pub struct TargetIndexArrowRow {
    pub crawl_id: String,
    pub target_domain_hash_bucket: String,
    pub target_domain_id: String,
    pub target_domain: String,
    pub source_url_id: String,
    pub source_url: String,
    pub source_domain_id: String,
    pub source_domain: String,
    pub source_host: String,
    pub warc_filename: String,
    pub warc_record_offset: i64,
    pub warc_record_length: i64,
    pub wat_filename: String,
    pub wat_record_offset: i64,
    pub wat_record_length: i64,
    pub fetch_status: Option<u32>,
    pub content_mime_type: String,
    pub fetch_time: String,
    pub link_count_to_target: u32,
    pub wat_path: String,
}

pub struct TargetIndexBatchBuilder {
    schema: SchemaRef,
    crawl_id: StringBuilder,
    target_domain_hash_bucket: StringBuilder,
    target_domain_id: StringBuilder,
    target_domain: StringBuilder,
    source_url_id: StringBuilder,
    source_url: StringBuilder,
    source_domain_id: StringBuilder,
    source_domain: StringBuilder,
    source_host: StringBuilder,
    warc_filename: StringBuilder,
    warc_record_offset: Int64Builder,
    warc_record_length: Int64Builder,
    wat_filename: StringBuilder,
    wat_record_offset: Int64Builder,
    wat_record_length: Int64Builder,
    fetch_status: UInt32Builder,
    content_mime_type: StringBuilder,
    fetch_time: StringBuilder,
    link_count_to_target: UInt32Builder,
    wat_path: StringBuilder,
    rows: usize,
    approx_bytes: usize,
}

impl TargetIndexBatchBuilder {
    pub fn new() -> Self {
        Self::with_capacity(1024)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            schema: target_index_schema(),
            crawl_id: StringBuilder::with_capacity(capacity, capacity * 16),
            target_domain_hash_bucket: StringBuilder::with_capacity(capacity, capacity * 4),
            target_domain_id: StringBuilder::with_capacity(capacity, capacity * 16),
            target_domain: StringBuilder::with_capacity(capacity, capacity * 24),
            source_url_id: StringBuilder::with_capacity(capacity, capacity * 16),
            source_url: StringBuilder::with_capacity(capacity, capacity * 48),
            source_domain_id: StringBuilder::with_capacity(capacity, capacity * 16),
            source_domain: StringBuilder::with_capacity(capacity, capacity * 24),
            source_host: StringBuilder::with_capacity(capacity, capacity * 24),
            warc_filename: StringBuilder::with_capacity(capacity, capacity * 64),
            warc_record_offset: Int64Builder::with_capacity(capacity),
            warc_record_length: Int64Builder::with_capacity(capacity),
            wat_filename: StringBuilder::with_capacity(capacity, capacity * 64),
            wat_record_offset: Int64Builder::with_capacity(capacity),
            wat_record_length: Int64Builder::with_capacity(capacity),
            fetch_status: UInt32Builder::with_capacity(capacity),
            content_mime_type: StringBuilder::with_capacity(capacity, capacity * 24),
            fetch_time: StringBuilder::with_capacity(capacity, capacity * 24),
            link_count_to_target: UInt32Builder::with_capacity(capacity),
            wat_path: StringBuilder::with_capacity(capacity, capacity * 64),
            rows: 0,
            approx_bytes: 0,
        }
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn approx_bytes(&self) -> usize {
        self.approx_bytes
    }

    pub fn append_row(&mut self, row: &TargetIndexArrowRow) {
        self.crawl_id.append_value(&row.crawl_id);
        self.target_domain_hash_bucket
            .append_value(&row.target_domain_hash_bucket);
        self.target_domain_id.append_value(&row.target_domain_id);
        self.target_domain.append_value(&row.target_domain);
        self.source_url_id.append_value(&row.source_url_id);
        self.source_url.append_value(&row.source_url);
        self.source_domain_id.append_value(&row.source_domain_id);
        self.source_domain.append_value(&row.source_domain);
        self.source_host.append_value(&row.source_host);
        self.warc_filename.append_value(&row.warc_filename);
        self.warc_record_offset.append_value(row.warc_record_offset);
        self.warc_record_length.append_value(row.warc_record_length);
        self.wat_filename.append_value(&row.wat_filename);
        self.wat_record_offset.append_value(row.wat_record_offset);
        self.wat_record_length.append_value(row.wat_record_length);
        if let Some(status) = row.fetch_status {
            self.fetch_status.append_value(status);
        } else {
            self.fetch_status.append_null();
        }
        self.content_mime_type.append_value(&row.content_mime_type);
        self.fetch_time.append_value(&row.fetch_time);
        self.link_count_to_target
            .append_value(row.link_count_to_target);
        self.wat_path.append_value(&row.wat_path);
        self.rows = self.rows.saturating_add(1);
        self.approx_bytes = self
            .approx_bytes
            .saturating_add(row.source_url.len())
            .saturating_add(row.target_domain.len())
            .saturating_add(row.wat_path.len())
            .saturating_add(128);
    }

    pub fn finish_record_batch(&mut self) -> Result<Option<RecordBatch>, arrow::error::ArrowError> {
        if self.rows == 0 {
            return Ok(None);
        }
        let batch = RecordBatch::try_new(
            Arc::clone(&self.schema),
            vec![
                Arc::new(self.crawl_id.finish()) as ArrayRef,
                Arc::new(self.target_domain_hash_bucket.finish()),
                Arc::new(self.target_domain_id.finish()),
                Arc::new(self.target_domain.finish()),
                Arc::new(self.source_url_id.finish()),
                Arc::new(self.source_url.finish()),
                Arc::new(self.source_domain_id.finish()),
                Arc::new(self.source_domain.finish()),
                Arc::new(self.source_host.finish()),
                Arc::new(self.warc_filename.finish()),
                Arc::new(self.warc_record_offset.finish()),
                Arc::new(self.warc_record_length.finish()),
                Arc::new(self.wat_filename.finish()),
                Arc::new(self.wat_record_offset.finish()),
                Arc::new(self.wat_record_length.finish()),
                Arc::new(self.fetch_status.finish()),
                Arc::new(self.content_mime_type.finish()),
                Arc::new(self.fetch_time.finish()),
                Arc::new(self.link_count_to_target.finish()),
                Arc::new(self.wat_path.finish()),
            ],
        )?;
        *self = Self::new();
        Ok(Some(batch))
    }
}

impl Default for TargetIndexBatchBuilder {
    fn default() -> Self {
        Self::new()
    }
}

pub fn encode_record_batch_ipc(batch: &RecordBatch) -> Result<Vec<u8>, std::io::Error> {
    encode_record_batches_ipc(std::slice::from_ref(batch))
}

pub fn encode_record_batches_ipc(batches: &[RecordBatch]) -> Result<Vec<u8>, std::io::Error> {
    let schema = batches
        .first()
        .map(|batch| batch.schema())
        .unwrap_or_else(target_index_schema);
    let mut bytes = Vec::new();
    let options = IpcWriteOptions::default();
    let mut writer = StreamWriter::try_new_with_options(&mut bytes, &schema, options)
        .map_err(|err| std::io::Error::other(err.to_string()))?;
    for batch in batches {
        writer
            .write(batch)
            .map_err(|err| std::io::Error::other(err.to_string()))?;
    }
    writer
        .finish()
        .map_err(|err| std::io::Error::other(err.to_string()))?;
    Ok(bytes)
}

pub fn namespace_label() -> &'static str {
    NAMESPACE_TARGET_INDEX
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_non_empty_batch() {
        let mut builder = TargetIndexBatchBuilder::new();
        builder.append_row(&TargetIndexArrowRow {
            crawl_id: "CC-MAIN-X".into(),
            target_domain_hash_bucket: "42".into(),
            target_domain_id: "1".into(),
            target_domain: "example.com".into(),
            source_url_id: "2".into(),
            source_url: "https://source.example/page".into(),
            source_domain_id: "3".into(),
            source_domain: "source.example".into(),
            source_host: "source.example".into(),
            warc_filename: "warc.gz".into(),
            warc_record_offset: 1,
            warc_record_length: 2,
            wat_filename: "wat.gz".into(),
            wat_record_offset: 3,
            wat_record_length: 4,
            fetch_status: Some(200),
            content_mime_type: "text/html".into(),
            fetch_time: "2026-01-01".into(),
            link_count_to_target: 1,
            wat_path: "path.wat.gz".into(),
        });
        let batch = builder.finish_record_batch().unwrap().unwrap();
        let ipc = encode_record_batch_ipc(&batch).unwrap();
        assert!(!ipc.is_empty());
        assert_eq!(batch.num_rows(), 1);
    }
}
