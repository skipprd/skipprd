//! Fast single-line JSON parsing for NDJSON ingest.
//!
//! Primary path uses `serde_json::from_slice` on the line bytes (no extra
//! allocation). Falls back to SIMD JSON when that fails.

use serde_json::Value;
use simd_json::SIMDJSON_PADDING;

/// Reusable padded buffer for SIMD fallback parsing.
pub struct NdjsonLineParser {
    simd_buf: Vec<u8>,
}

impl NdjsonLineParser {
    pub fn new() -> Self {
        Self {
            simd_buf: Vec::with_capacity(4096),
        }
    }

    /// Parse one JSON value from a single NDJSON line.
    #[inline]
    pub fn parse_value(&mut self, line: &str) -> Result<Value, String> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Err("empty line".to_string());
        }

        let bytes = trimmed.as_bytes();
        if let Ok(value) = serde_json::from_slice::<Value>(bytes) {
            return Ok(value);
        }

        self.parse_value_simd(trimmed)
    }

    #[inline]
    fn parse_value_simd(&mut self, trimmed: &str) -> Result<Value, String> {
        self.simd_buf.clear();
        self.simd_buf.extend_from_slice(trimmed.as_bytes());
        let padded_len = self.simd_buf.len() + SIMDJSON_PADDING;
        if self.simd_buf.len() < padded_len {
            self.simd_buf.resize(padded_len, 0);
        }
        simd_json::serde::from_slice::<Value>(&mut self.simd_buf).map_err(|err| err.to_string())
    }
}

impl Default for NdjsonLineParser {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_matches_serde_for_flat_object() {
        let line = r#"{"id":"abc","count":42,"active":true}"#;
        let parsed = NdjsonLineParser::new().parse_value(line).unwrap();
        let expected: Value = serde_json::from_str(line).unwrap();
        assert_eq!(parsed, expected);
    }

    #[test]
    fn simd_parse_handles_wat_shaped_row() {
        let line = r#"{"url":"https://example.com/path","target_domain":"example.com","fetch_time":"2024-01-15T12:00:00Z"}"#;
        let value = NdjsonLineParser::new().parse_value(line).unwrap();
        assert!(value.is_object());
        assert_eq!(value["target_domain"], "example.com");
    }

    #[test]
    fn simd_parse_handles_benchmark_wat_row() {
        let line = r#"{"content_mime_type":"text/html","crawl_id":"CC-MAIN-BENCH","fetch_status":200,"fetch_time":"2026-01-01T00:00:00Z","link_count_to_target":1,"source_domain":"source.example","source_domain_id":"100","source_host":"source.example","source_url":"https://source-0.example/page","source_url_id":"2000000","target_domain":"target-0.example","target_domain_hash_bucket":"0","target_domain_id":"1000000","warc_filename":"warc.warc.gz","warc_record_length":1000,"warc_record_offset":0,"wat_filename":"wat.wat.gz","wat_path":"s3://bucket/wat/0.wat.gz","wat_record_length":500,"wat_record_offset":0}"#;
        let parsed = NdjsonLineParser::new().parse_value(line).unwrap();
        let expected: Value = serde_json::from_str(line).unwrap();
        assert_eq!(parsed, expected);
    }

    #[test]
    fn rejects_empty_line() {
        assert!(NdjsonLineParser::new().parse_value("   ").is_err());
    }
}
