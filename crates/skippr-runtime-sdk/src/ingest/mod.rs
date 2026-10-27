use crate::helpers::offsets::OffsetKey;
use crate::plugins::cdc::WalRowMeta;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct IngestBatch {
    pub offset_key: OffsetKey,
    pub data: String,
    pub bytes: usize,
    pub offset_pos: Option<u64>,
    pub source_uri: String,
    pub namespace: Option<String>,
    pub cdc_rows: Option<Vec<WalRowMeta>>,
}

impl IngestBatch {
    pub fn new(
        offset_key: OffsetKey,
        data: String,
        bytes: usize,
        source_uri: String,
        namespace: Option<String>,
        cdc_rows: Option<Vec<WalRowMeta>>,
    ) -> Self {
        Self {
            offset_key,
            data,
            bytes,
            offset_pos: None,
            source_uri,
            namespace,
            cdc_rows: cdc_rows.filter(|rows| !rows.is_empty()),
        }
    }

    pub fn new_with_offset_pos(
        offset_key: OffsetKey,
        data: String,
        bytes: usize,
        offset_pos: u64,
        source_uri: String,
        namespace: Option<String>,
        cdc_rows: Option<Vec<WalRowMeta>>,
    ) -> Self {
        let mut batch = Self::new(offset_key, data, bytes, source_uri, namespace, cdc_rows);
        batch.offset_pos = Some(offset_pos);
        batch
    }

    pub fn normalized_offset_key(
        namespace: impl Into<String>,
        partition: impl Into<String>,
    ) -> OffsetKey {
        OffsetKey {
            namespace: crate::helpers::names::storage_namespace(&namespace.into()),
            partition: crate::helpers::names::storage_partition(&partition.into()),
        }
    }

    /// Offset key as observed by host ingest after the plugin → host round trip.
    /// Child configs do not load skippr.yml (`flatten_events = false`).
    pub fn runtime_roundtrip_offset_key(
        namespace: impl Into<String>,
        partition: impl Into<String>,
    ) -> OffsetKey {
        let once = Self::normalized_offset_key(namespace, partition);
        Self::normalized_offset_key(once.namespace, once.partition)
    }

    pub fn offset_key(&self) -> &OffsetKey {
        &self.offset_key
    }

    pub fn data(&self) -> &str {
        &self.data
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn offset_pos_for_line(&self, line: u64) -> u64 {
        self.offset_pos
            .map(|base| base.saturating_add(line.saturating_sub(1)))
            .unwrap_or(line)
    }

    pub fn source_uri(&self) -> &str {
        &self.source_uri
    }

    pub fn namespace_override(&self) -> Option<&str> {
        self.namespace.as_deref()
    }

    pub fn cdc_rows(&self) -> Option<&[WalRowMeta]> {
        self.cdc_rows.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::IngestBatch;

    #[test]
    fn runtime_roundtrip_does_not_need_host_config() {
        let key = IngestBatch::runtime_roundtrip_offset_key("Bike Hire", "dt=2024-01-01/region=eu");
        assert_eq!(key.namespace, "bike_hire");
        assert!(key.partition.contains('='));
    }
}

#[derive(Clone, Debug)]
pub struct ThroughputMetrics {
    pub bytes_per_second: u64,
    pub active_cores: usize,
    pub queue_length: usize,
    pub optimal_chunk_size: usize,
}

pub mod tuner;
