use serde::{Deserialize, Serialize};

use crate::entity::EntityKind;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UpfoundryBacklinksConfig {
    pub site: String,
    #[serde(default)]
    pub entity_kind: EntityKind,
    pub entity_domain: String,
    #[serde(default)]
    pub domain_variants: Vec<String>,
    #[serde(default)]
    pub primary_domain: Option<String>,
    #[serde(default)]
    pub competitor_name: Option<String>,
    pub ops_bucket: String,
    #[serde(default = "default_ops_prefix")]
    pub ops_prefix: String,
    #[serde(default)]
    pub selected_snapshot_id: Option<String>,
    #[serde(default)]
    pub include_subdomains: bool,
    #[serde(default = "default_max_detail_rows")]
    pub max_detail_rows: usize,
}

fn default_ops_prefix() -> String {
    "link-graph-corpus".into()
}

fn default_max_detail_rows() -> usize {
    10_000
}

impl UpfoundryBacklinksConfig {
    pub fn validate(&self) -> Result<(), std::io::Error> {
        if self.site.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "site is required",
            ));
        }
        if self.entity_domain.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "entity_domain is required",
            ));
        }
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

    pub fn primary_site(&self) -> String {
        self.primary_domain
            .clone()
            .unwrap_or_else(|| self.site.clone())
    }
}
