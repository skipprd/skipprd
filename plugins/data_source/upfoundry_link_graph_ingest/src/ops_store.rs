use crate::cc::CcUrlRecord;
use arrow::array::{BooleanArray, Int64Array, RecordBatch, StringArray, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;
use skippr_plugin_shared_link_graph::{RawEdgeObservation, RawPageFact};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct DimUrlRow {
    pub url_id: u64,
    pub url: String,
    pub domain_id: u64,
    pub canonicalization_version: String,
}

#[derive(Debug, Clone)]
pub struct DimDomainRow {
    pub domain_id: u64,
    pub domain: String,
}

#[derive(Debug, Clone)]
pub struct DimAnchorRow {
    pub anchor_id: u64,
    pub anchor_text: String,
}

#[derive(Debug, Clone)]
pub struct DimWarcFileRow {
    pub warc_file_id: u64,
    pub warc_filename: String,
}

fn parquet_bytes(batch: RecordBatch) -> Result<Vec<u8>, std::io::Error> {
    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::default()))
        .build();
    let schema = batch.schema();
    let mut out = Vec::new();
    {
        let mut writer =
            ArrowWriter::try_new(&mut out, schema, Some(props)).map_err(std::io::Error::other)?;
        writer.write(&batch).map_err(std::io::Error::other)?;
        writer.close().map_err(std::io::Error::other)?;
    }
    Ok(out)
}

async fn put_parquet_object(
    client: &Client,
    bucket: &str,
    key: &str,
    bytes: Vec<u8>,
) -> Result<(), std::io::Error> {
    client
        .put_object()
        .bucket(bucket)
        .key(key)
        .content_type("application/vnd.apache.parquet")
        .body(ByteStream::from(bytes))
        .send()
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(())
}

pub async fn put_edge_parquet_object(
    client: &Client,
    bucket: &str,
    key: &str,
    rows: &[RawEdgeObservation],
) -> Result<(), std::io::Error> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("edge_observation_id", DataType::Utf8, false),
        Field::new("edge_id", DataType::Utf8, false),
        Field::new("url_from", DataType::Utf8, false),
        Field::new("url_to", DataType::Utf8, false),
        Field::new("domain_from", DataType::Utf8, false),
        Field::new("domain_to", DataType::Utf8, false),
        Field::new("url_from_id", DataType::UInt64, false),
        Field::new("url_to_id", DataType::UInt64, false),
        Field::new("domain_from_id", DataType::UInt64, false),
        Field::new("domain_to_id", DataType::UInt64, false),
        Field::new("anchor_text", DataType::Utf8, false),
        Field::new("anchor_id", DataType::UInt64, false),
        Field::new("link_context", DataType::Utf8, false),
        Field::new("rel_flags", DataType::UInt32, false),
        Field::new("is_image_link", DataType::Boolean, false),
        Field::new("link_ordinal", DataType::UInt32, false),
        Field::new("cc_crawl_id", DataType::Utf8, false),
        Field::new("warc_file_id", DataType::UInt64, false),
        Field::new("warc_record_offset", DataType::Int64, false),
        Field::new("warc_record_length", DataType::Int64, false),
        Field::new("http_status_from", DataType::UInt32, true),
        Field::new("is_broken", DataType::Boolean, false),
        Field::new("fetch_time", DataType::Utf8, false),
        Field::new("canonicalization_version", DataType::Utf8, false),
        Field::new("discovered_by", DataType::Utf8, false),
    ]));
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.edge_observation_id.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.edge_id.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.url_from.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.url_to.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.domain_from.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.domain_to.as_str()),
            )),
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|r| r.url_from_id),
            )),
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|r| r.url_to_id),
            )),
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|r| r.domain_from_id),
            )),
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|r| r.domain_to_id),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.anchor_text.as_str()),
            )),
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|r| r.anchor_id),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.link_context.as_str()),
            )),
            Arc::new(UInt32Array::from_iter_values(
                rows.iter().map(|r| r.rel_flags),
            )),
            Arc::new(BooleanArray::from_iter(
                rows.iter().map(|r| Some(r.is_image_link)),
            )),
            Arc::new(UInt32Array::from_iter_values(
                rows.iter().map(|r| r.link_ordinal),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.cc_crawl_id.as_str()),
            )),
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|r| r.warc_file_id),
            )),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|r| r.warc_record_offset),
            )),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|r| r.warc_record_length),
            )),
            Arc::new(UInt32Array::from_iter(
                rows.iter().map(|r| r.http_status_from),
            )),
            Arc::new(BooleanArray::from_iter(
                rows.iter().map(|r| Some(r.is_broken)),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.fetch_time.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.canonicalization_version.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.discovered_by.as_str()),
            )),
        ],
    )
    .map_err(std::io::Error::other)?;
    put_parquet_object(client, bucket, key, parquet_bytes(batch)?).await
}

pub async fn put_page_parquet_object(
    client: &Client,
    bucket: &str,
    key: &str,
    rows: &[RawPageFact],
) -> Result<(), std::io::Error> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("url_id", DataType::UInt64, false),
        Field::new("domain_id", DataType::UInt64, false),
        Field::new("cc_crawl_id", DataType::Utf8, false),
        Field::new("warc_file_id", DataType::UInt64, false),
        Field::new("warc_record_offset", DataType::Int64, false),
        Field::new("warc_record_length", DataType::Int64, false),
        Field::new("fetch_status", DataType::UInt32, true),
        Field::new("content_mime_type", DataType::Utf8, false),
        Field::new("fetch_time", DataType::Utf8, false),
        Field::new("outbound_link_count", DataType::UInt32, false),
        Field::new("stored_link_count", DataType::UInt32, false),
        Field::new("links_truncated", DataType::Boolean, false),
        Field::new("raw_link_count", DataType::UInt32, false),
        Field::new("page_quality_flags", DataType::Utf8, false),
        Field::new("canonicalization_version", DataType::Utf8, false),
        Field::new("parser_version", DataType::Utf8, false),
        Field::new("parse_status", DataType::Utf8, false),
    ]));
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(UInt64Array::from_iter_values(rows.iter().map(|r| r.url_id))),
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|r| r.domain_id),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.cc_crawl_id.as_str()),
            )),
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|r| r.warc_file_id),
            )),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|r| r.warc_record_offset),
            )),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|r| r.warc_record_length),
            )),
            Arc::new(UInt32Array::from_iter(rows.iter().map(|r| r.fetch_status))),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.content_mime_type.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.fetch_time.as_str()),
            )),
            Arc::new(UInt32Array::from_iter_values(
                rows.iter().map(|r| r.outbound_link_count),
            )),
            Arc::new(UInt32Array::from_iter_values(
                rows.iter().map(|r| r.stored_link_count),
            )),
            Arc::new(BooleanArray::from_iter(
                rows.iter().map(|r| Some(r.links_truncated)),
            )),
            Arc::new(UInt32Array::from_iter_values(
                rows.iter().map(|r| r.raw_link_count),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.page_quality_flags.to_string()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.canonicalization_version.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.parser_version.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.parse_status.as_str()),
            )),
        ],
    )
    .map_err(std::io::Error::other)?;
    put_parquet_object(client, bucket, key, parquet_bytes(batch)?).await
}

pub async fn put_dim_url_parquet_object(
    client: &Client,
    bucket: &str,
    key: &str,
    rows: &[DimUrlRow],
) -> Result<(), std::io::Error> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("url_id", DataType::UInt64, false),
        Field::new("url", DataType::Utf8, false),
        Field::new("domain_id", DataType::UInt64, false),
        Field::new("canonicalization_version", DataType::Utf8, false),
    ]));
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(UInt64Array::from_iter_values(rows.iter().map(|r| r.url_id))),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.url.as_str()),
            )),
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|r| r.domain_id),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.canonicalization_version.as_str()),
            )),
        ],
    )
    .map_err(std::io::Error::other)?;
    put_parquet_object(client, bucket, key, parquet_bytes(batch)?).await
}

pub async fn put_dim_domain_parquet_object(
    client: &Client,
    bucket: &str,
    key: &str,
    rows: &[DimDomainRow],
) -> Result<(), std::io::Error> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("domain_id", DataType::UInt64, false),
        Field::new("domain", DataType::Utf8, false),
    ]));
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|r| r.domain_id),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.domain.as_str()),
            )),
        ],
    )
    .map_err(std::io::Error::other)?;
    put_parquet_object(client, bucket, key, parquet_bytes(batch)?).await
}

pub async fn put_dim_anchor_parquet_object(
    client: &Client,
    bucket: &str,
    key: &str,
    rows: &[DimAnchorRow],
) -> Result<(), std::io::Error> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("anchor_id", DataType::UInt64, false),
        Field::new("anchor_text", DataType::Utf8, false),
    ]));
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|r| r.anchor_id),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.anchor_text.as_str()),
            )),
        ],
    )
    .map_err(std::io::Error::other)?;
    put_parquet_object(client, bucket, key, parquet_bytes(batch)?).await
}

pub async fn put_dim_warc_file_parquet_object(
    client: &Client,
    bucket: &str,
    key: &str,
    rows: &[DimWarcFileRow],
) -> Result<(), std::io::Error> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("warc_file_id", DataType::UInt64, false),
        Field::new("warc_filename", DataType::Utf8, false),
    ]));
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|r| r.warc_file_id),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.warc_filename.as_str()),
            )),
        ],
    )
    .map_err(std::io::Error::other)?;
    put_parquet_object(client, bucket, key, parquet_bytes(batch)?).await
}

pub async fn put_cc_urls_index_parquet_object(
    client: &Client,
    bucket: &str,
    key: &str,
    frontier_domain: &str,
    crawl_id: &str,
    rows: &[CcUrlRecord],
) -> Result<(), std::io::Error> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("frontier_domain", DataType::Utf8, false),
        Field::new("url", DataType::Utf8, false),
        Field::new("warc_filename", DataType::Utf8, false),
        Field::new("warc_record_offset", DataType::Int64, false),
        Field::new("warc_record_length", DataType::Int64, false),
        Field::new("fetch_status", DataType::UInt32, true),
        Field::new("content_mime_type", DataType::Utf8, false),
        Field::new("fetch_time", DataType::Utf8, false),
        Field::new("cc_crawl_id", DataType::Utf8, false),
        Field::new("source", DataType::Utf8, false),
        Field::new("query_execution_id", DataType::Utf8, false),
        Field::new("imported_at", DataType::Utf8, false),
    ]));
    let imported_at = chrono::Utc::now().to_rfc3339();
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|_| frontier_domain),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.url.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.warc_filename.as_str()),
            )),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|r| r.warc_record_offset),
            )),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|r| r.warc_record_length),
            )),
            Arc::new(UInt32Array::from_iter(rows.iter().map(|r| r.fetch_status))),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.content_mime_type.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.fetch_time.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(rows.iter().map(|_| crawl_id))),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|_| "direct_datafusion_file"),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|_| "direct_fallback"),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|_| imported_at.as_str()),
            )),
        ],
    )
    .map_err(std::io::Error::other)?;
    put_parquet_object(client, bucket, key, parquet_bytes(batch)?).await
}

pub async fn put_json_object(
    client: &Client,
    bucket: &str,
    key: &str,
    value: &serde_json::Value,
) -> Result<(), std::io::Error> {
    let body = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    client
        .put_object()
        .bucket(bucket)
        .key(key)
        .content_type("application/json")
        .body(ByteStream::from(body))
        .send()
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(())
}

pub fn staging_key(prefix: &str, kind: &str, crawl_id: &str, run_date: &str, shard: u32) -> String {
    let base = prefix.trim_end_matches('/');
    format!(
        "{base}/raw/staging/{kind}/crawl_id={crawl_id}/date={run_date}/shard={shard:04}/part-00000.parquet"
    )
}
