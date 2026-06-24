use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}

fn default_max_urls() -> u32 {
    10_000
}

fn default_max_links_per_page() -> u32 {
    2_000
}

fn default_monthly_window() -> u32 {
    24
}

fn default_web_graph_max_rows() -> u32 {
    1_000_000
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UpfoundryLinkGraphIngestConfig {
    /// Ops corpus bucket, e.g. upfoundry-prod-ops
    pub ops_bucket: String,
    /// Prefix under bucket, default link-graph-corpus
    #[serde(default = "default_ops_prefix")]
    pub ops_prefix: String,
    /// Frontier seed domains (registrable), e.g. skippr.io
    #[serde(default)]
    pub frontier_domains: Vec<String>,
    /// Common Crawl collection id, e.g. CC-MAIN-2025-08
    pub cc_crawl_id: String,
    /// Optional explicit crawl IDs. When set, these are scanned before falling back to cc_crawl_id.
    #[serde(default)]
    pub cc_crawl_ids: Vec<String>,
    /// Common Crawl URL Index Parquet root. Supports `{crawl_id}` replacement.
    #[serde(default = "default_cc_index_base_uri")]
    pub cc_index_base_uri: String,
    #[serde(default = "default_max_urls")]
    pub max_urls_per_run: u32,
    #[serde(default = "default_max_links_per_page")]
    pub max_links_per_page: u32,
    #[serde(default = "default_monthly_window")]
    pub monthly_window: u32,
    /// Optional Common Crawl Web Graph rank export URI (s3://, https://, or fixture file).
    #[serde(default)]
    pub cc_web_graph_uri: Option<String>,
    #[serde(default = "default_web_graph_max_rows")]
    pub cc_web_graph_max_rows: u32,
    #[serde(default)]
    pub live_crawl_enabled: bool,
    #[serde(default)]
    pub brightdata_proxy_escalation_enabled: bool,
    #[serde(default = "default_true")]
    pub include_subdomains: bool,
}

fn default_ops_prefix() -> String {
    "link-graph-corpus".into()
}

fn default_cc_index_base_uri() -> String {
    "s3://commoncrawl/cc-index/table/cc-main/warc".into()
}

impl UpfoundryLinkGraphIngestConfig {
    pub fn validate(&self) -> Result<(), std::io::Error> {
        if self.ops_bucket.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "ops_bucket is required",
            ));
        }
        if self.cc_crawl_id.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "cc_crawl_id is required",
            ));
        }
        if self.frontier_domains.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "frontier_domains must not be empty",
            ));
        }
        if self.monthly_window == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "monthly_window must be greater than zero",
            ));
        }
        if self.cc_web_graph_max_rows == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "cc_web_graph_max_rows must be greater than zero",
            ));
        }
        Ok(())
    }

    pub fn corpus_root(&self) -> String {
        format!("{}/", self.ops_prefix.trim_end_matches('/'))
    }

    pub fn crawl_ids(&self) -> Vec<String> {
        if !self.cc_crawl_ids.is_empty() {
            return self
                .cc_crawl_ids
                .iter()
                .take(self.monthly_window as usize)
                .cloned()
                .collect();
        }
        rolling_common_crawl_ids(&self.cc_crawl_id, self.monthly_window)
    }
}

fn parse_common_crawl_id(crawl_id: &str) -> Option<(i32, u32)> {
    let mut parts = crawl_id.split('-');
    if parts.next()? != "CC" || parts.next()? != "MAIN" {
        return None;
    }
    let year = parts.next()?.parse::<i32>().ok()?;
    let week = parts.next()?.parse::<u32>().ok()?;
    if parts.next().is_some() || week == 0 || week > 53 {
        return None;
    }
    Some((year, week))
}

fn rolling_common_crawl_ids(anchor: &str, monthly_window: u32) -> Vec<String> {
    let Some((mut year, mut week)) = parse_common_crawl_id(anchor) else {
        return vec![anchor.to_string()];
    };
    let mut out = Vec::new();
    for _ in 0..monthly_window {
        out.push(format!("CC-MAIN-{year}-{week:02}"));
        if week > 4 {
            week -= 4;
        } else {
            year -= 1;
            week = 52 + week - 4;
        }
    }
    out
}
