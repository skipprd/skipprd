use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkContext {
    Body,
    Nav,
    Footer,
    Sidebar,
    Unknown,
}

impl LinkContext {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Body => "body",
            Self::Nav => "nav",
            Self::Footer => "footer",
            Self::Sidebar => "sidebar",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedOutboundLink {
    pub target_url: String,
    pub anchor_text: String,
    pub rel: Vec<String>,
    pub is_nofollow: bool,
    pub is_image_link: bool,
    pub context: LinkContext,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawEdgeObservation {
    pub edge_observation_id: String,
    pub edge_id: String,
    #[serde(default)]
    pub url_from: String,
    #[serde(default)]
    pub url_to: String,
    #[serde(default)]
    pub domain_from: String,
    #[serde(default)]
    pub domain_to: String,
    pub url_from_id: u64,
    pub url_to_id: u64,
    pub domain_from_id: u64,
    pub domain_to_id: u64,
    #[serde(default)]
    pub anchor_text: String,
    pub anchor_id: u64,
    pub link_context: String,
    pub rel_flags: u32,
    #[serde(default)]
    pub is_image_link: bool,
    pub link_ordinal: u32,
    pub cc_crawl_id: String,
    pub warc_file_id: u64,
    pub warc_record_offset: i64,
    pub warc_record_length: i64,
    #[serde(default)]
    pub http_status_from: Option<u32>,
    #[serde(default)]
    pub is_broken: bool,
    pub fetch_time: String,
    pub canonicalization_version: String,
    pub discovered_by: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawPageFact {
    pub url_id: u64,
    pub domain_id: u64,
    pub cc_crawl_id: String,
    pub warc_file_id: u64,
    pub warc_record_offset: i64,
    pub warc_record_length: i64,
    #[serde(default)]
    pub fetch_status: Option<u32>,
    #[serde(default)]
    pub content_mime_type: String,
    pub fetch_time: String,
    pub outbound_link_count: u32,
    pub stored_link_count: u32,
    pub links_truncated: bool,
    pub raw_link_count: u32,
    pub page_quality_flags: Value,
    pub canonicalization_version: String,
    #[serde(default = "default_parser_version")]
    pub parser_version: String,
    pub parse_status: String,
}

fn default_parser_version() -> String {
    "link_graph_html_v1".into()
}
