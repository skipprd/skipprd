use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use url::Url;

use crate::canonical::canonicalize_url;
use crate::ids::{domain_id, url_id};
use crate::types::{ArchiveRecordRef, LinkContext, PageFetchRef, ParsedOutboundLink};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatLinkExtraction {
    pub page_ref: PageFetchRef,
    pub links: Vec<ParsedOutboundLink>,
    pub raw_link_count: u32,
    pub links_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatTargetDomainCount {
    pub target_domain_id: u64,
    pub target_domain: String,
    pub link_count_to_target: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatTargetIndexExtraction {
    pub page_ref: PageFetchRef,
    pub targets: Vec<WatTargetDomainCount>,
    pub raw_link_count: u32,
    pub links_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatRecordLocation {
    pub filename: String,
    pub record_offset: i64,
    pub record_length: i64,
}

fn value_at<'a>(root: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut value = root;
    for segment in path {
        value = value.get(*segment)?;
    }
    Some(value)
}

fn string_at(root: &Value, paths: &[&[&str]]) -> Option<String> {
    for path in paths {
        if let Some(value) = value_at(root, path) {
            if let Some(s) = value.as_str() {
                if !s.trim().is_empty() {
                    return Some(s.trim().to_string());
                }
            }
        }
    }
    None
}

fn i64_at(root: &Value, paths: &[&[&str]]) -> Option<i64> {
    for path in paths {
        if let Some(value) = value_at(root, path) {
            if let Some(n) = value.as_i64() {
                return Some(n);
            }
            if let Some(s) = value.as_str().and_then(|s| s.parse::<i64>().ok()) {
                return Some(s);
            }
        }
    }
    None
}

fn u32_at(root: &Value, paths: &[&[&str]]) -> Option<u32> {
    i64_at(root, paths).and_then(|n| u32::try_from(n).ok())
}

fn resolve_wat_url_from_base(base: &Url, raw_url: &str) -> Option<crate::canonical::CanonicalUrl> {
    let trimmed = raw_url.trim();
    if trimmed.is_empty()
        || trimmed.starts_with('#')
        || trimmed.starts_with("javascript:")
        || trimmed.starts_with("mailto:")
        || trimmed.starts_with("tel:")
        || trimmed.starts_with("data:")
    {
        return None;
    }
    let resolved = base.join(trimmed).ok()?;
    canonicalize_url(resolved.as_str())
}

fn resolve_wat_url(source_url: &str, raw_url: &str) -> Option<String> {
    let base = Url::parse(source_url).ok()?;
    resolve_wat_url_from_base(&base, raw_url).map(|c| c.canonical)
}

fn rel_tokens(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(s)) => s
            .split_whitespace()
            .filter(|token| !token.trim().is_empty())
            .map(|token| token.trim().to_ascii_lowercase())
            .collect(),
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .flat_map(str::split_whitespace)
            .filter(|token| !token.trim().is_empty())
            .map(|token| token.trim().to_ascii_lowercase())
            .collect(),
        _ => Vec::new(),
    }
}

fn link_context(path: Option<&str>) -> LinkContext {
    let lower = path.unwrap_or_default().to_ascii_lowercase();
    if lower.contains("nav") {
        LinkContext::Nav
    } else if lower.contains("footer") {
        LinkContext::Footer
    } else if lower.contains("aside") || lower.contains("sidebar") {
        LinkContext::Sidebar
    } else {
        LinkContext::Body
    }
}

fn is_anchor_link(value: &Value) -> bool {
    let Some(path) = value.get("path").and_then(Value::as_str) else {
        return true;
    };
    let normalized = path.to_ascii_uppercase();
    normalized.starts_with("A@/HREF") || normalized.contains("/A@/HREF")
}

fn html_links(root: &Value) -> impl Iterator<Item = &Value> {
    value_at(
        root,
        &[
            "Envelope",
            "Payload-Metadata",
            "HTTP-Response-Metadata",
            "HTML-Metadata",
            "Links",
        ],
    )
    .and_then(Value::as_array)
    .into_iter()
    .flatten()
}

fn content_mime_type(root: &Value) -> String {
    string_at(
        root,
        &[
            &[
                "Envelope",
                "Payload-Metadata",
                "HTTP-Response-Metadata",
                "Headers",
                "Content-Type",
            ],
            &["Envelope", "WARC-Header-Metadata", "Content-Type"],
        ],
    )
    .unwrap_or_else(|| "text/html".to_string())
}

fn warc_ref(root: &Value) -> Option<ArchiveRecordRef> {
    let filename = string_at(
        root,
        &[
            &["Container", "Filename"],
            &["Container", "WARC-Filename"],
            &["Container", "Warc-Filename"],
        ],
    )?;
    let record_offset = i64_at(
        root,
        &[
            &["Container", "Offset"],
            &["Container", "WARC-Record-Offset"],
            &["Container", "Warc-Record-Offset"],
        ],
    )?;
    let record_length = i64_at(
        root,
        &[
            &["Container", "Gzip-Metadata", "Deflate-Length"],
            &["Container", "Compressed-Length"],
            &["Container", "WARC-Record-Length"],
            &["Container", "Warc-Record-Length"],
        ],
    )
    .unwrap_or(0);
    Some(ArchiveRecordRef {
        filename,
        record_offset,
        record_length,
    })
}

pub fn parse_wat_metadata_record(
    cc_crawl_id: &str,
    wat_location: WatRecordLocation,
    json: &Value,
    max_links: u32,
) -> Option<WatLinkExtraction> {
    let source_url = string_at(
        json,
        &[&["Envelope", "WARC-Header-Metadata", "WARC-Target-URI"]],
    )?;
    let source = canonicalize_url(&source_url)?;
    let warc = warc_ref(json)?;
    let wat = ArchiveRecordRef {
        filename: wat_location.filename,
        record_offset: wat_location.record_offset,
        record_length: wat_location.record_length,
    };
    let fetch_status = u32_at(
        json,
        &[&[
            "Envelope",
            "Payload-Metadata",
            "HTTP-Response-Metadata",
            "Response-Message",
            "Status",
        ]],
    );
    let fetch_time =
        string_at(json, &[&["Envelope", "WARC-Header-Metadata", "WARC-Date"]]).unwrap_or_default();

    let mut links = Vec::new();
    let mut raw_link_count = 0u32;
    let mut links_truncated = false;
    for link in html_links(json) {
        if !is_anchor_link(link) {
            continue;
        }
        let Some(raw_url) = link.get("url").and_then(Value::as_str) else {
            continue;
        };
        raw_link_count = raw_link_count.saturating_add(1);
        let Some(target_url) = resolve_wat_url(&source.canonical, raw_url) else {
            continue;
        };
        if links.len() as u32 >= max_links {
            links_truncated = true;
            continue;
        }
        let rel = rel_tokens(link.get("rel"));
        let anchor_text = link
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        links.push(ParsedOutboundLink {
            target_url,
            anchor_text,
            is_nofollow: rel.iter().any(|r| r == "nofollow"),
            rel,
            is_image_link: false,
            context: link_context(link.get("path").and_then(Value::as_str)),
        });
    }

    Some(WatLinkExtraction {
        page_ref: PageFetchRef {
            source_url_id: url_id(&source.canonical),
            source_domain_id: domain_id(&source.host),
            source_url: source.canonical,
            source_host: source.host,
            cc_crawl_id: cc_crawl_id.to_string(),
            wat,
            warc,
            fetch_status,
            content_mime_type: content_mime_type(json),
            fetch_time,
            source_role: "wat_index".to_string(),
        },
        links,
        raw_link_count,
        links_truncated,
    })
}

pub fn parse_wat_target_index_record(
    cc_crawl_id: &str,
    wat_location: WatRecordLocation,
    json: &Value,
    max_links: u32,
) -> Option<WatTargetIndexExtraction> {
    let source_url = string_at(
        json,
        &[&["Envelope", "WARC-Header-Metadata", "WARC-Target-URI"]],
    )?;
    let source = canonicalize_url(&source_url)?;
    let source_base = Url::parse(&source.canonical).ok()?;
    let warc = warc_ref(json)?;
    let wat = ArchiveRecordRef {
        filename: wat_location.filename,
        record_offset: wat_location.record_offset,
        record_length: wat_location.record_length,
    };
    let fetch_status = u32_at(
        json,
        &[&[
            "Envelope",
            "Payload-Metadata",
            "HTTP-Response-Metadata",
            "Response-Message",
            "Status",
        ]],
    );
    let fetch_time =
        string_at(json, &[&["Envelope", "WARC-Header-Metadata", "WARC-Date"]]).unwrap_or_default();

    let mut by_target: HashMap<u64, (String, u32)> = HashMap::new();
    let mut raw_link_count = 0u32;
    let mut accepted_links = 0u32;
    let mut links_truncated = false;
    for link in html_links(json) {
        if !is_anchor_link(link) {
            continue;
        }
        let Some(raw_url) = link.get("url").and_then(Value::as_str) else {
            continue;
        };
        raw_link_count = raw_link_count.saturating_add(1);
        if accepted_links >= max_links {
            links_truncated = true;
            continue;
        }
        let Some(target) = resolve_wat_url_from_base(&source_base, raw_url) else {
            continue;
        };
        accepted_links = accepted_links.saturating_add(1);
        let target_domain_id = domain_id(&target.host);
        let entry = by_target
            .entry(target_domain_id)
            .or_insert((target.host, 0));
        entry.1 = entry.1.saturating_add(1);
    }

    let targets = by_target
        .into_iter()
        .map(
            |(target_domain_id, (target_domain, link_count_to_target))| WatTargetDomainCount {
                target_domain_id,
                target_domain,
                link_count_to_target,
            },
        )
        .collect();

    Some(WatTargetIndexExtraction {
        page_ref: PageFetchRef {
            source_url_id: url_id(&source.canonical),
            source_domain_id: domain_id(&source.host),
            source_url: source.canonical,
            source_host: source.host,
            cc_crawl_id: cc_crawl_id.to_string(),
            wat,
            warc,
            fetch_status,
            content_mime_type: content_mime_type(json),
            fetch_time,
            source_role: "wat_index".to_string(),
        },
        targets,
        raw_link_count,
        links_truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_wat_links_and_refs() {
        let record = json!({
            "Container": {
                "Filename": "crawl-data/CC-MAIN-X/segments/1/warc/example.warc.gz",
                "Offset": 123,
                "Gzip-Metadata": { "Deflate-Length": 456 }
            },
            "Envelope": {
                "WARC-Header-Metadata": {
                    "WARC-Target-URI": "https://example.com/page?utm_source=x",
                    "WARC-Date": "2026-01-01T00:00:00Z"
                },
                "Payload-Metadata": {
                    "HTTP-Response-Metadata": {
                        "Response-Message": { "Status": 200 },
                        "Headers": { "Content-Type": "text/html" },
                        "HTML-Metadata": {
                            "Links": [
                                { "path": "A@/href", "url": "/about", "text": " About " },
                                { "path": "IMG@/src", "url": "/logo.png" },
                                { "path": "A@/href", "url": "mailto:test@example.com" }
                            ]
                        }
                    }
                }
            }
        });
        let parsed = parse_wat_metadata_record(
            "CC-MAIN-X",
            WatRecordLocation {
                filename: "crawl-data/CC-MAIN-X/segments/1/wat/example.warc.wat.gz".into(),
                record_offset: 10,
                record_length: 20,
            },
            &record,
            10,
        )
        .unwrap();
        assert_eq!(parsed.raw_link_count, 2);
        assert_eq!(parsed.links.len(), 1);
        assert_eq!(parsed.links[0].target_url, "https://example.com/about");
        assert_eq!(parsed.page_ref.wat.record_offset, 10);
        assert_eq!(parsed.page_ref.warc.record_offset, 123);
    }
}
