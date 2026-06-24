use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UpfoundryLinkGraphCompactConfig {
    pub ops_bucket: String,
    #[serde(default = "default_ops_prefix")]
    pub ops_prefix: String,
    #[serde(default)]
    pub max_staging_partitions: u32,
    #[serde(default = "default_damping")]
    pub pagerank_damping: f64,
    #[serde(default = "default_max_iterations")]
    pub pagerank_max_iterations: u32,
    #[serde(default = "default_keep_complete_snapshots")]
    pub keep_complete_snapshots: u32,
    #[serde(default = "default_keep_failed_manifest_days")]
    pub keep_failed_manifest_days: u32,
    #[serde(default = "default_keep_staging_days")]
    pub keep_staging_days: u32,
    pub spam_model_version: Option<String>,
}

fn default_ops_prefix() -> String {
    "link-graph-corpus".into()
}

fn default_damping() -> f64 {
    0.85
}

fn default_max_iterations() -> u32 {
    50
}

fn default_keep_complete_snapshots() -> u32 {
    5
}

fn default_keep_failed_manifest_days() -> u32 {
    30
}

fn default_keep_staging_days() -> u32 {
    14
}

impl UpfoundryLinkGraphCompactConfig {
    pub fn validate(&self) -> Result<(), std::io::Error> {
        if self.ops_bucket.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "ops_bucket is required",
            ));
        }
        Ok(())
    }

    pub fn corpus_root(&self) -> String {
        format!("{}/", self.ops_prefix.trim_end_matches('/'))
    }
}
