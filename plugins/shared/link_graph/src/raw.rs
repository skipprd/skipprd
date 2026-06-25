use serde_json::Value;

use crate::canonical::canonicalize_url;
use crate::ids::{anchor_id, domain_id, edge_id, edge_observation_id, url_id, warc_file_id};
use crate::types::{PageFetchRef, ParsedOutboundLink, RawEdgeObservation, RawPageFact};

#[derive(Debug, Clone)]
pub struct RawBuildResult {
    pub page: RawPageFact,
    pub edges: Vec<RawEdgeObservation>,
}

fn hex128(bytes: &[u8; 16]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn rel_flags(rel: &[String]) -> u32 {
    let mut flags = 0u32;
    for r in rel {
        let r = r.to_ascii_lowercase();
        if r == "nofollow" {
            flags |= 1;
        } else if r == "sponsored" {
            flags |= 2;
        } else if r == "ugc" {
            flags |= 4;
        }
    }
    flags
}

fn status_indicates_broken(status: Option<u32>) -> bool {
    status
        .map(|status| !(200..400).contains(&status))
        .unwrap_or(false)
}

pub fn build_raw_page_observations(
    page_ref: &PageFetchRef,
    links: &[ParsedOutboundLink],
    raw_link_count: u32,
    links_truncated: bool,
    page_quality_flags: Value,
    parse_status: &str,
    discovered_by: &str,
) -> RawBuildResult {
    let wf_id = warc_file_id(&page_ref.warc.filename);
    let page = RawPageFact {
        url_id: page_ref.source_url_id,
        domain_id: page_ref.source_domain_id,
        cc_crawl_id: page_ref.cc_crawl_id.clone(),
        warc_file_id: wf_id,
        warc_record_offset: page_ref.warc.record_offset,
        warc_record_length: page_ref.warc.record_length,
        fetch_status: page_ref.fetch_status,
        content_mime_type: page_ref.content_mime_type.clone(),
        fetch_time: page_ref.fetch_time.clone(),
        outbound_link_count: links.len() as u32,
        stored_link_count: links.len() as u32,
        links_truncated,
        raw_link_count,
        page_quality_flags,
        canonicalization_version: "v1".to_string(),
        parser_version: "link_graph_html_v1".into(),
        parse_status: parse_status.to_string(),
    };

    let source_is_broken = status_indicates_broken(page_ref.fetch_status);
    let warc_record_id = format!(
        "{}:{}:{}",
        wf_id, page_ref.warc.record_offset, page_ref.warc.record_length
    );
    let mut edges = Vec::new();
    for (ordinal, link) in links.iter().enumerate() {
        let Some(target) = canonicalize_url(&link.target_url) else {
            continue;
        };
        let rel_sem = if link.rel.is_empty() {
            "none".to_string()
        } else {
            link.rel.join(",")
        };
        let ctx_bucket = link.context.as_str();
        let eid = edge_id(
            &page_ref.source_url,
            &target.canonical,
            &rel_sem,
            ctx_bucket,
        );
        let obs = edge_observation_id(&eid, &page_ref.cc_crawl_id, &warc_record_id, ordinal as u32);
        let normalized_anchor = link
            .anchor_text
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        edges.push(RawEdgeObservation {
            edge_observation_id: hex128(&obs),
            edge_id: hex128(&eid),
            url_from: page_ref.source_url.clone(),
            url_to: target.canonical.clone(),
            domain_from: page_ref.source_host.clone(),
            domain_to: target.host.clone(),
            url_from_id: page_ref.source_url_id,
            url_to_id: url_id(&target.canonical),
            domain_from_id: page_ref.source_domain_id,
            domain_to_id: domain_id(&target.host),
            anchor_text: normalized_anchor.clone(),
            anchor_id: anchor_id(&normalized_anchor),
            link_context: ctx_bucket.to_string(),
            rel_flags: rel_flags(&link.rel),
            is_image_link: link.is_image_link,
            link_ordinal: ordinal as u32,
            cc_crawl_id: page_ref.cc_crawl_id.clone(),
            warc_file_id: wf_id,
            warc_record_offset: page_ref.warc.record_offset,
            warc_record_length: page_ref.warc.record_length,
            http_status_from: page_ref.fetch_status,
            is_broken: source_is_broken,
            fetch_time: page_ref.fetch_time.clone(),
            canonicalization_version: "v1".to_string(),
            discovered_by: discovered_by.to_string(),
        });
    }
    RawBuildResult { page, edges }
}
