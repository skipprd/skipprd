use skippr_runtime_sdk::SkipprConfig;
use serde::{Deserialize, Serialize};

fn default_bucket_count() -> u32 {
    32_768
}

fn default_batch_size_bytes() -> usize {
    32 * 1024 * 1024
}

fn default_max_records_per_batch() -> usize {
    25_000
}

fn default_max_links_per_page() -> u32 {
    2_000
}

fn default_max_wat_object_bytes() -> usize {
    2 * 1024 * 1024 * 1024
}

fn default_sqs_visibility_timeout_seconds() -> i32 {
    14_400
}

#[derive(Debug, Clone, Serialize, Deserialize, SkipprConfig, PartialEq)]
pub struct UpfoundryLinkGraphWatIndexConfig {
    #[serde(default)]
    pub crawl_id: String,
    #[serde(default)]
    pub wat_paths_manifest_uri: Option<String>,
    #[serde(default)]
    pub wat_path_start: Option<usize>,
    #[serde(default)]
    pub wat_path_end: Option<usize>,
    #[serde(default = "default_bucket_count")]
    pub target_domain_bucket_count: u32,
    #[serde(default = "default_batch_size_bytes")]
    pub batch_size_bytes: usize,
    #[serde(default = "default_max_records_per_batch")]
    pub max_records_per_batch: usize,
    #[serde(default = "default_max_links_per_page")]
    pub max_links_per_page: u32,
    /// When `None`, process all remaining manifest paths in one sync (production default).
    #[serde(default)]
    pub max_wat_objects_per_sync: Option<usize>,
    #[serde(default = "default_max_wat_object_bytes")]
    pub max_wat_object_bytes: usize,
    /// When set, stop parsing each WAT object after this many gzip member records.
    #[serde(default)]
    pub max_wat_records_per_object: Option<usize>,
    #[serde(default)]
    pub include_subdomains: bool,
    #[serde(default)]
    pub sqs_queue_url: Option<String>,
    #[serde(default = "default_sqs_visibility_timeout_seconds")]
    pub sqs_visibility_timeout_seconds: i32,
}

impl UpfoundryLinkGraphWatIndexConfig {
    pub fn uses_sqs_jobs(&self) -> bool {
        self.sqs_queue_url
            .as_ref()
            .is_some_and(|url| !url.trim().is_empty())
    }

    pub fn validate(&self) -> Result<(), std::io::Error> {
        let fixture_mode = std::env::var("SKIPPR_WAT_INDEX_FIXTURE_DIR")
            .ok()
            .is_some_and(|value| !value.trim().is_empty());
        if !self.uses_sqs_jobs() && self.crawl_id.trim().is_empty() && !fixture_mode {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "crawl_id is required when sqs_queue_url is not configured",
            ));
        }
        if self.target_domain_bucket_count == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "target_domain_bucket_count must be greater than zero",
            ));
        }
        if self.batch_size_bytes == 0 || self.max_records_per_batch == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "batch_size_bytes and max_records_per_batch must be greater than zero",
            ));
        }
        if self.max_wat_object_bytes == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "max_wat_object_bytes must be greater than zero",
            ));
        }
        if self.sqs_visibility_timeout_seconds <= 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "sqs_visibility_timeout_seconds must be greater than zero",
            ));
        }
        Ok(())
    }
}
