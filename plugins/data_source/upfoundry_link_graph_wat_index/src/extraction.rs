use serde_json::Value;
use skippr_plugin_shared_link_graph::{
    id64_string, parse_wat_target_index_record, WatRecordLocation,
};

use crate::arrow_batch::TargetIndexArrowRow;
use crate::config::UpfoundryLinkGraphWatIndexConfig;

pub fn parse_wat_member_payload(payload: &[u8]) -> Option<Value> {
    let text = String::from_utf8_lossy(payload);
    let json_start = text.find('{')?;
    let candidate = text[json_start..].trim();
    let json_end = candidate.rfind('}')?;
    serde_json::from_str::<Value>(&candidate[..=json_end]).ok()
}

pub fn target_bucket(config: &UpfoundryLinkGraphWatIndexConfig, target_domain_id: u64) -> u32 {
    (target_domain_id % u64::from(config.target_domain_bucket_count)) as u32
}

pub fn rows_for_extraction_arrow(
    config: &UpfoundryLinkGraphWatIndexConfig,
    crawl_id: &str,
    wat_path: &str,
    location: WatRecordLocation,
    json: &Value,
) -> Vec<TargetIndexArrowRow> {
    let Some(extraction) =
        parse_wat_target_index_record(crawl_id, location, json, config.max_links_per_page)
    else {
        return Vec::new();
    };
    extraction
        .targets
        .into_iter()
        .map(|target| TargetIndexArrowRow {
            crawl_id: crawl_id.to_string(),
            target_domain_hash_bucket: target_bucket(config, target.target_domain_id).to_string(),
            target_domain_id: id64_string(target.target_domain_id),
            target_domain: target.target_domain,
            source_url_id: id64_string(extraction.page_ref.source_url_id),
            source_url: extraction.page_ref.source_url.clone(),
            source_domain_id: id64_string(extraction.page_ref.source_domain_id),
            source_domain: extraction.page_ref.source_host.clone(),
            source_host: extraction.page_ref.source_host.clone(),
            warc_filename: extraction.page_ref.warc.filename.clone(),
            warc_record_offset: extraction.page_ref.warc.record_offset,
            warc_record_length: extraction.page_ref.warc.record_length,
            wat_filename: extraction.page_ref.wat.filename.clone(),
            wat_record_offset: extraction.page_ref.wat.record_offset,
            wat_record_length: extraction.page_ref.wat.record_length,
            fetch_status: extraction.page_ref.fetch_status,
            content_mime_type: extraction.page_ref.content_mime_type.clone(),
            fetch_time: extraction.page_ref.fetch_time.clone(),
            link_count_to_target: target.link_count_to_target,
            wat_path: wat_path.to_string(),
        })
        .collect()
}
