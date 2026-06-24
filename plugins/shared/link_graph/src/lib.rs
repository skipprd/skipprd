//! Shared link-graph types: URL canonicalization, deterministic IDs, HTML link parse.

pub mod canonical;
pub mod html;
pub mod ids;
pub mod types;

pub use canonical::{canonicalize_url, CanonicalUrl, CanonicalizationVersion};
pub use html::parse_html_links;
pub use ids::{
    anchor_id, domain_id, edge_id, edge_observation_id, url_id, warc_file_id, Id128, Id64,
};
pub use types::{LinkContext, ParsedOutboundLink};
pub use types::{RawEdgeObservation, RawPageFact};
