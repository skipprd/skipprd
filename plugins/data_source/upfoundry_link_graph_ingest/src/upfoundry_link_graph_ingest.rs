use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_sdk_s3::Client as S3Client;
use chrono::Utc;
use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};
use skippr_plugin_shared_link_graph::{
    build_raw_page_observations, canonicalize_url, domain_id, parse_html_links,
    parse_wat_metadata_record, url_id, warc_file_id, ArchiveRecordRef, PageFetchRef,
    RawEdgeObservation, RawPageFact, WatRecordLocation,
};
use skippr_runtime_sdk::helpers::offsets::OffsetKey;
use skippr_runtime_sdk::plugins::{
    DataSource, SourceExecutionContract, SourceOnceContract, SourceSyncContext,
};
use skippr_runtime_sdk::source_compat::{submit_payload_batches, IngestBatch};
use tracing::info;

use crate::cc::{domain_hash, domain_matches, load_url_candidates, CcIndexRequest};
use crate::config::UpfoundryLinkGraphIngestConfig;
use crate::live::{fetch_live_html, LiveFetchConfig};
use crate::ops_store::{
    put_cc_urls_index_parquet_object, put_dim_anchor_parquet_object, put_dim_domain_parquet_object,
    put_dim_url_parquet_object, put_dim_warc_file_parquet_object, put_edge_parquet_object,
    put_json_object, put_page_parquet_object, staging_key, DimAnchorRow, DimDomainRow, DimUrlRow,
    DimWarcFileRow,
};
use crate::streams::{all_namespace_contracts, NAMESPACE_AUDIT_SKIP, NAMESPACE_SITE_RUN_DAILY};
use crate::warc::{fetch_warc_html, fetch_wat_json};
use crate::webgraph::import_web_graph_priors;

pub struct UpfoundryLinkGraphIngestPlugin {
    config: UpfoundryLinkGraphIngestConfig,
}

fn de_u64_from_string_or_number<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        Value::Number(n) => n
            .as_u64()
            .ok_or_else(|| serde::de::Error::custom("expected u64")),
        Value::String(s) => s
            .parse::<u64>()
            .map_err(|err| serde::de::Error::custom(err.to_string())),
        _ => Err(serde::de::Error::custom("expected u64 string or number")),
    }
}

fn de_i64_from_string_or_number<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        Value::Number(n) => n
            .as_i64()
            .ok_or_else(|| serde::de::Error::custom("expected i64")),
        Value::String(s) => s
            .parse::<i64>()
            .map_err(|err| serde::de::Error::custom(err.to_string())),
        _ => Err(serde::de::Error::custom("expected i64 string or number")),
    }
}

#[derive(Debug, Clone, Deserialize)]
struct SelectedReferrerPageRef {
    source_url: String,
    #[serde(deserialize_with = "de_u64_from_string_or_number")]
    source_url_id: u64,
    #[serde(deserialize_with = "de_u64_from_string_or_number")]
    source_domain_id: u64,
    #[serde(default)]
    source_domain: String,
    #[serde(default)]
    source_host: String,
    warc_filename: String,
    #[serde(deserialize_with = "de_i64_from_string_or_number")]
    warc_record_offset: i64,
    #[serde(deserialize_with = "de_i64_from_string_or_number")]
    warc_record_length: i64,
    wat_filename: String,
    #[serde(deserialize_with = "de_i64_from_string_or_number")]
    wat_record_offset: i64,
    #[serde(deserialize_with = "de_i64_from_string_or_number")]
    wat_record_length: i64,
    #[serde(default)]
    fetch_status: Option<u32>,
    #[serde(default)]
    content_mime_type: String,
    #[serde(default)]
    fetch_time: String,
}

#[derive(Debug, Clone)]
struct SelectedReferrerLoad {
    rows: Vec<SelectedReferrerPageRef>,
    status: String,
    error: Option<String>,
    invalid_rows: u32,
}

impl UpfoundryLinkGraphIngestPlugin {
    pub fn new(config: UpfoundryLinkGraphIngestConfig) -> Result<Self, std::io::Error> {
        config.validate()?;
        Ok(Self { config })
    }

    fn fixture_dir() -> Option<String> {
        std::env::var("SKIPPR_LINK_GRAPH_FIXTURE_DIR")
            .ok()
            .filter(|s| !s.trim().is_empty())
    }

    fn page_quality_flags(
        url: &str,
        raw_link_count: u32,
        stored_link_count: u32,
        links_truncated: bool,
        max_links: u32,
    ) -> Value {
        let lower_url = url.to_ascii_lowercase();
        let query = lower_url
            .split_once('?')
            .map(|(_, query)| query)
            .unwrap_or("");
        let calendar_pattern = lower_url.contains("/calendar")
            || lower_url.contains("/events/")
            || lower_url.contains("/20")
            || query.contains("month=")
            || query.contains("year=");
        let faceted_url = query.contains("filter")
            || query.contains("facet")
            || query.contains("sort=")
            || query.contains("page=")
            || query.matches('&').count() >= 4;
        json!({
            "links_truncated": links_truncated,
            "link_explosion": raw_link_count >= max_links,
            "raw_link_count": raw_link_count,
            "stored_link_count": stored_link_count,
            "calendar_pattern": calendar_pattern,
            "faceted_url": faceted_url,
        })
    }

    async fn s3_client() -> Result<aws_sdk_s3::Client, std::io::Error> {
        let cfg = aws_config::load_defaults(BehaviorVersion::latest()).await;
        Ok(aws_sdk_s3::Client::new(&cfg))
    }

    async fn read_text_uri(client: &S3Client, uri: &str) -> Result<String, std::io::Error> {
        if let Some(path) = uri.strip_prefix("s3://") {
            let (bucket, key) = path.split_once('/').ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid s3 uri")
            })?;
            let resp = client
                .get_object()
                .bucket(bucket)
                .key(key)
                .send()
                .await
                .map_err(|err| std::io::Error::other(err.to_string()))?;
            return resp
                .body
                .collect()
                .await
                .map_err(|err| std::io::Error::other(err.to_string()))
                .map(|bytes| String::from_utf8_lossy(&bytes.into_bytes()).to_string());
        }
        if uri.starts_with("http://") || uri.starts_with("https://") {
            return reqwest::get(uri)
                .await
                .map_err(|err| std::io::Error::other(err.to_string()))?
                .text()
                .await
                .map_err(|err| std::io::Error::other(err.to_string()));
        }
        std::fs::read_to_string(uri)
    }

    async fn load_selected_referrer_pages(
        &self,
        client: &S3Client,
    ) -> SelectedReferrerLoad {
        let Some(uri) = self.config.selected_referrer_page_refs_uri.as_deref() else {
            return SelectedReferrerLoad {
                rows: Vec::new(),
                status: "not_configured".into(),
                error: None,
                invalid_rows: 0,
            };
        };
        let content = match Self::read_text_uri(client, uri).await {
            Ok(content) => content,
            Err(err) => {
                return SelectedReferrerLoad {
                    rows: Vec::new(),
                    status: "missing".into(),
                    error: Some(err.to_string()),
                    invalid_rows: 0,
                };
            }
        };
        let mut rows = Vec::new();
        let mut seen = HashSet::new();
        let mut invalid_rows = 0u32;
        for (line_no, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let row: SelectedReferrerPageRef = match serde_json::from_str(line) {
                Ok(row) => row,
                Err(_) => {
                    invalid_rows = invalid_rows.saturating_add(1);
                    eprintln!(
                        "invalid selected referrer row line={} uri={}",
                        line_no.saturating_add(1),
                        uri
                    );
                    continue;
                }
            };
            if seen.insert(row.source_url_id) {
                rows.push(row);
            }
            if rows.len() as u32 >= self.config.max_referrer_pages_per_run {
                break;
            }
        }
        SelectedReferrerLoad {
            status: if invalid_rows > 0 {
                "partial".into()
            } else {
                "loaded".into()
            },
            rows,
            error: None,
            invalid_rows,
        }
    }

    fn submit_rows(
        &self,
        ctx: &dyn SourceSyncContext,
        namespace: &str,
        run_date: &str,
        rows: Vec<Value>,
    ) -> Result<(), std::io::Error> {
        if rows.is_empty() {
            return Ok(());
        }
        let payload = rows
            .iter()
            .map(|row| serde_json::to_string(row))
            .collect::<Result<Vec<_>, _>>()?
            .join("\n");
        submit_payload_batches(
            ctx,
            vec![IngestBatch {
                offset_key: OffsetKey::new(namespace, run_date.to_string()),
                data: payload.clone(),
                bytes: payload.len(),
                namespace: Some(namespace.to_string()),
                source_uri: format!("upfoundry-link-graph-ingest://{}", self.config.cc_crawl_id),
                offset_pos: None,
                cdc_rows: None,
            }],
        )?;
        Ok(())
    }
}

#[async_trait]
impl DataSource for UpfoundryLinkGraphIngestPlugin {
    fn execution_contract(&self) -> SourceExecutionContract {
        SourceExecutionContract::stream(SourceOnceContract::Finite)
    }

    fn source_namespace_contracts(
        &self,
    ) -> Vec<skippr_runtime_sdk::plugins::source_contract::SourceNamespaceContract> {
        all_namespace_contracts()
    }

    async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        let run_date = Utc::now().format("%Y-%m-%d").to_string();
        let run_id = self
            .config
            .corpus_run_id
            .clone()
            .unwrap_or_else(|| format!("{}-{}", self.config.cc_crawl_id, Utc::now().timestamp()));
        let fixture_dir = Self::fixture_dir();
        let fixture_ref = fixture_dir.as_deref();
        let client = Self::s3_client().await?;
        let root = self.config.corpus_root();
        let web_graph_stats = import_web_graph_priors(
            &client,
            &self.config.ops_bucket,
            &root,
            self.config.cc_web_graph_uri.as_deref(),
            fixture_ref,
            self.config.cc_web_graph_max_rows,
        )
        .await?;

        let index_result = load_url_candidates(CcIndexRequest {
            fixture_dir: fixture_ref,
            frontier_domains: &self.config.frontier_domains,
            max_urls: self.config.max_urls_per_run,
            include_subdomains: self.config.include_subdomains,
            ops_bucket: &self.config.ops_bucket,
            ops_prefix: &self.config.ops_prefix,
            cc_index_base_uri: &self.config.cc_index_base_uri,
            cc_urls_index_prefix: &self.config.cc_urls_index_prefix,
            cc_index_source: &self.config.cc_index_source,
            cc_direct_index_enabled: self.config.cc_direct_index_enabled,
            crawl_ids: self.config.crawl_ids(),
        })
        .await;
        let index_stats = index_result.stats;
        let candidates = index_result.records;
        let selected_referrer_load = self.load_selected_referrer_pages(&client).await;
        if index_stats.mode == "direct_datafusion_file" && !candidates.is_empty() {
            for domain in &self.config.frontier_domains {
                let domain_rows = candidates
                    .iter()
                    .filter(|record| {
                        domain_matches(
                            &record.url,
                            &[domain.clone()],
                            self.config.include_subdomains,
                        )
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                if domain_rows.is_empty() {
                    continue;
                }
                put_cc_urls_index_parquet_object(
                    &client,
                    &self.config.ops_bucket,
                    &format!(
                        "{root}{}/domain_hash={}/crawl_id={}/part-direct-{}.parquet",
                        self.config.cc_urls_index_prefix.trim_matches('/'),
                        domain_hash(domain),
                        self.config.cc_crawl_id,
                        run_date
                    ),
                    domain,
                    &self.config.cc_crawl_id,
                    &domain_rows,
                )
                .await?;
            }
        }

        let mut edge_rows: Vec<RawEdgeObservation> = Vec::new();
        let mut page_rows: Vec<RawPageFact> = Vec::new();
        let mut skip_rows: Vec<Value> = Vec::new();
        let mut dim_urls: BTreeMap<u64, DimUrlRow> = BTreeMap::new();
        let mut dim_domains: BTreeMap<u64, DimDomainRow> = BTreeMap::new();
        let mut dim_anchors: BTreeMap<u64, DimAnchorRow> = BTreeMap::new();
        let mut dim_warc_files: BTreeMap<u64, DimWarcFileRow> = BTreeMap::new();
        let mut warc_record_state: Vec<Value> = Vec::new();
        let mut page_parse_state: Vec<Value> = Vec::new();
        let mut frontier_domain_state: BTreeMap<String, u32> = BTreeMap::new();
        let mut warc_attempted = 0u32;
        let mut warc_parsed = 0u32;
        let urls_selected = candidates.len() as u32;
        let referrer_selection_status = selected_referrer_load.status.clone();
        let referrer_selection_error = selected_referrer_load.error.clone();
        let referrer_selection_invalid_rows = selected_referrer_load.invalid_rows;
        if let Some(error) = &referrer_selection_error {
            skip_rows.push(json!({
                "cc_crawl_id": self.config.cc_crawl_id,
                "run_date": run_date,
                "reason": "selected_referrer_input_unavailable",
                "selected_referrer_page_refs_uri": self.config.selected_referrer_page_refs_uri,
                "error": error,
            }));
        }
        if referrer_selection_invalid_rows > 0 {
            skip_rows.push(json!({
                "cc_crawl_id": self.config.cc_crawl_id,
                "run_date": run_date,
                "reason": "selected_referrer_invalid_rows",
                "selected_referrer_page_refs_uri": self.config.selected_referrer_page_refs_uri,
                "invalid_rows": referrer_selection_invalid_rows,
            }));
        }
        let selected_referrer_pages = selected_referrer_load.rows;
        let referrer_pages_selected = selected_referrer_pages.len() as u32;
        let mut referrer_pages_parsed = 0u32;

        for record in candidates {
            if !domain_matches(
                &record.url,
                &self.config.frontier_domains,
                self.config.include_subdomains,
            ) {
                continue;
            }
            warc_attempted += 1;
            let source_canon = match canonicalize_url(&record.url) {
                Some(c) => c,
                None => {
                    warc_record_state.push(json!({
                        "cc_crawl_id": self.config.cc_crawl_id,
                        "url": record.url,
                        "run_date": run_date,
                        "status": "invalid_url",
                    }));
                    skip_rows.push(json!({
                        "cc_crawl_id": self.config.cc_crawl_id,
                        "url": record.url,
                        "run_date": run_date,
                        "reason": "invalid_url",
                    }));
                    continue;
                }
            };
            let html_result = fetch_warc_html(
                fixture_ref,
                &source_canon.canonical,
                &record.warc_filename,
                record.warc_record_offset,
                record.warc_record_length,
            )
            .await;
            let (html, discovered_by, parse_status, http_status_from, content_mime_type) =
                match html_result {
                    Ok(h) => {
                        let discovered_by = if h.used_fixture { "fixture" } else { "cc_warc" };
                        let parse_status = if h.used_fixture {
                            "parsed_fixture"
                        } else {
                            "parsed_warc"
                        };
                        (
                            h.html,
                            discovered_by.to_string(),
                            parse_status.to_string(),
                            record.fetch_status,
                            if record.content_mime_type.is_empty() {
                                "text/html".to_string()
                            } else {
                                record.content_mime_type.clone()
                            },
                        )
                    }
                    Err(reason) => {
                        match fetch_live_html(
                            &source_canon.canonical,
                            &LiveFetchConfig {
                                live_crawl_enabled: self.config.live_crawl_enabled,
                                brightdata_proxy_escalation_enabled: self
                                    .config
                                    .brightdata_proxy_escalation_enabled,
                            },
                        )
                        .await
                        {
                            Ok(live) => (
                                live.html,
                                live.discovered_by,
                                "parsed_live_crawl".to_string(),
                                Some(u32::from(live.http_status)),
                                live.content_mime_type,
                            ),
                            Err(live_reason) => {
                                warc_record_state.push(json!({
                                    "cc_crawl_id": self.config.cc_crawl_id,
                                    "url": record.url,
                                    "warc_filename": record.warc_filename,
                                    "warc_record_offset": record.warc_record_offset,
                                    "warc_record_length": record.warc_record_length,
                                    "run_date": run_date,
                                    "status": "fetch_failed",
                                    "reason": reason,
                                    "live_reason": live_reason,
                                }));
                                skip_rows.push(json!({
                                    "cc_crawl_id": self.config.cc_crawl_id,
                                    "url": record.url,
                                    "run_date": run_date,
                                    "reason": reason,
                                    "live_reason": live_reason,
                                }));
                                continue;
                            }
                        }
                    }
                };
            warc_parsed += 1;
            let (links, raw_link_count, links_truncated) = parse_html_links(
                &source_canon.canonical,
                &html,
                self.config.max_links_per_page,
            );
            let src_url_id = url_id(&source_canon.canonical);
            let src_domain_id = domain_id(&source_canon.host);
            let wf_id = warc_file_id(&record.warc_filename);
            *frontier_domain_state
                .entry(source_canon.host.clone())
                .or_insert(0) += 1;
            dim_domains
                .entry(src_domain_id)
                .or_insert_with(|| DimDomainRow {
                    domain_id: src_domain_id,
                    domain: source_canon.host.clone(),
                });
            dim_urls.entry(src_url_id).or_insert_with(|| DimUrlRow {
                url_id: src_url_id,
                url: source_canon.canonical.clone(),
                domain_id: src_domain_id,
                canonicalization_version: source_canon.version.to_string(),
            });
            dim_warc_files
                .entry(wf_id)
                .or_insert_with(|| DimWarcFileRow {
                    warc_file_id: wf_id,
                    warc_filename: record.warc_filename.clone(),
                });
            let page_quality_flags = Self::page_quality_flags(
                &source_canon.canonical,
                raw_link_count,
                links.len() as u32,
                links_truncated,
                self.config.max_links_per_page,
            );
            let page_ref = PageFetchRef {
                source_url: source_canon.canonical.clone(),
                source_host: source_canon.host.clone(),
                source_url_id: src_url_id,
                source_domain_id: src_domain_id,
                cc_crawl_id: self.config.cc_crawl_id.clone(),
                wat: ArchiveRecordRef {
                    filename: String::new(),
                    record_offset: 0,
                    record_length: 0,
                },
                warc: ArchiveRecordRef {
                    filename: record.warc_filename.clone(),
                    record_offset: record.warc_record_offset,
                    record_length: record.warc_record_length,
                },
                fetch_status: http_status_from,
                content_mime_type: content_mime_type.clone(),
                fetch_time: record.fetch_time.clone(),
                source_role: "frontier".into(),
            };
            let built = build_raw_page_observations(
                &page_ref,
                &links,
                raw_link_count,
                links_truncated,
                page_quality_flags.clone(),
                &parse_status,
                &discovered_by,
            );
            page_rows.push(built.page);
            warc_record_state.push(json!({
                "cc_crawl_id": self.config.cc_crawl_id,
                "url": source_canon.canonical.clone(),
                "warc_file_id": wf_id,
                "warc_filename": record.warc_filename.clone(),
                "warc_record_offset": record.warc_record_offset,
                "warc_record_length": record.warc_record_length,
                "http_status": http_status_from,
                "content_mime_type": content_mime_type.clone(),
                "run_date": run_date,
                "status": parse_status.clone(),
            }));
            page_parse_state.push(json!({
                "cc_crawl_id": self.config.cc_crawl_id,
                "url_id": src_url_id,
                "domain_id": src_domain_id,
                "run_date": run_date,
                "raw_link_count": raw_link_count,
                "stored_link_count": links.len(),
                "links_truncated": links_truncated,
                "fetch_status": http_status_from,
                "content_mime_type": content_mime_type.clone(),
                "page_quality_flags": page_quality_flags,
                "parse_status": parse_status.clone(),
            }));

            for edge in built.edges {
                let target_domain = edge.domain_to.clone();
                let target_url = edge.url_to.clone();
                let target_domain_id = edge.domain_to_id;
                let target_url_id = edge.url_to_id;
                dim_domains
                    .entry(target_domain_id)
                    .or_insert_with(|| DimDomainRow {
                        domain_id: target_domain_id,
                        domain: target_domain,
                    });
                dim_urls.entry(target_url_id).or_insert_with(|| DimUrlRow {
                    url_id: target_url_id,
                    url: target_url,
                    domain_id: target_domain_id,
                    canonicalization_version: source_canon.version.to_string(),
                });
                dim_anchors
                    .entry(edge.anchor_id)
                    .or_insert_with(|| DimAnchorRow {
                        anchor_id: edge.anchor_id,
                        anchor_text: edge.anchor_text.clone(),
                    });
                edge_rows.push(edge);
            }
        }

        for selected in selected_referrer_pages {
            let source_canon = match canonicalize_url(&selected.source_url) {
                Some(c) => c,
                None => {
                    skip_rows.push(json!({
                        "cc_crawl_id": self.config.cc_crawl_id,
                        "url": selected.source_url,
                        "run_date": run_date,
                        "reason": "invalid_selected_referrer_url",
                    }));
                    continue;
                }
            };

            let wat_parse = fetch_wat_json(
                fixture_ref,
                &selected.wat_filename,
                selected.wat_record_offset,
                selected.wat_record_length,
            )
            .await
            .ok()
            .and_then(|value| {
                parse_wat_metadata_record(
                    &self.config.cc_crawl_id,
                    WatRecordLocation {
                        filename: selected.wat_filename.clone(),
                        record_offset: selected.wat_record_offset,
                        record_length: selected.wat_record_length,
                    },
                    &value,
                    self.config.max_links_per_page,
                )
            });

            let (page_ref, links, raw_link_count, links_truncated, parse_status, discovered_by) =
                if let Some(extraction) = wat_parse {
                    (
                        extraction.page_ref,
                        extraction.links,
                        extraction.raw_link_count,
                        extraction.links_truncated,
                        "parsed_wat".to_string(),
                        "cc_wat_target_index".to_string(),
                    )
                } else {
                    let html_result = fetch_warc_html(
                        fixture_ref,
                        &source_canon.canonical,
                        &selected.warc_filename,
                        selected.warc_record_offset,
                        selected.warc_record_length,
                    )
                    .await;
                    let html = match html_result {
                        Ok(html) => html,
                        Err(reason) => {
                            skip_rows.push(json!({
                                "cc_crawl_id": self.config.cc_crawl_id,
                                "url": selected.source_url,
                                "run_date": run_date,
                                "reason": "selected_referrer_fetch_failed",
                                "detail": reason,
                            }));
                            continue;
                        }
                    };
                    let (links, raw_link_count, links_truncated) = parse_html_links(
                        &source_canon.canonical,
                        &html.html,
                        self.config.max_links_per_page,
                    );
                    let source_host = if selected.source_host.trim().is_empty() {
                        if selected.source_domain.trim().is_empty() {
                            source_canon.host.clone()
                        } else {
                            selected.source_domain.clone()
                        }
                    } else {
                        selected.source_host.clone()
                    };
                    (
                        PageFetchRef {
                            source_url: source_canon.canonical.clone(),
                            source_host,
                            source_url_id: selected.source_url_id,
                            source_domain_id: selected.source_domain_id,
                            cc_crawl_id: self.config.cc_crawl_id.clone(),
                            wat: ArchiveRecordRef {
                                filename: selected.wat_filename.clone(),
                                record_offset: selected.wat_record_offset,
                                record_length: selected.wat_record_length,
                            },
                            warc: ArchiveRecordRef {
                                filename: selected.warc_filename.clone(),
                                record_offset: selected.warc_record_offset,
                                record_length: selected.warc_record_length,
                            },
                            fetch_status: selected.fetch_status,
                            content_mime_type: if selected.content_mime_type.is_empty() {
                                "text/html".into()
                            } else {
                                selected.content_mime_type.clone()
                            },
                            fetch_time: selected.fetch_time.clone(),
                            source_role: "referrer".into(),
                        },
                        links,
                        raw_link_count,
                        links_truncated,
                        if html.used_fixture {
                            "parsed_referrer_fixture".to_string()
                        } else {
                            "parsed_referrer_warc".to_string()
                        },
                        "cc_wat_target_index_warc_fallback".to_string(),
                    )
                };

            let wf_id = warc_file_id(&page_ref.warc.filename);
            dim_domains
                .entry(page_ref.source_domain_id)
                .or_insert_with(|| DimDomainRow {
                    domain_id: page_ref.source_domain_id,
                    domain: page_ref.source_host.clone(),
                });
            dim_urls
                .entry(page_ref.source_url_id)
                .or_insert_with(|| DimUrlRow {
                    url_id: page_ref.source_url_id,
                    url: page_ref.source_url.clone(),
                    domain_id: page_ref.source_domain_id,
                    canonicalization_version: "v1".into(),
                });
            dim_warc_files
                .entry(wf_id)
                .or_insert_with(|| DimWarcFileRow {
                    warc_file_id: wf_id,
                    warc_filename: page_ref.warc.filename.clone(),
                });
            let page_quality_flags = Self::page_quality_flags(
                &page_ref.source_url,
                raw_link_count,
                links.len() as u32,
                links_truncated,
                self.config.max_links_per_page,
            );
            let built = build_raw_page_observations(
                &page_ref,
                &links,
                raw_link_count,
                links_truncated,
                page_quality_flags.clone(),
                &parse_status,
                &discovered_by,
            );
            page_rows.push(built.page);
            warc_record_state.push(json!({
                "cc_crawl_id": self.config.cc_crawl_id,
                "url": page_ref.source_url,
                "warc_file_id": wf_id,
                "warc_filename": page_ref.warc.filename,
                "warc_record_offset": page_ref.warc.record_offset,
                "warc_record_length": page_ref.warc.record_length,
                "wat_filename": page_ref.wat.filename,
                "wat_record_offset": page_ref.wat.record_offset,
                "wat_record_length": page_ref.wat.record_length,
                "http_status": page_ref.fetch_status,
                "content_mime_type": page_ref.content_mime_type,
                "run_date": run_date,
                "status": parse_status.clone(),
                "source_role": page_ref.source_role,
            }));
            page_parse_state.push(json!({
                "cc_crawl_id": self.config.cc_crawl_id,
                "url_id": page_ref.source_url_id,
                "domain_id": page_ref.source_domain_id,
                "run_date": run_date,
                "raw_link_count": raw_link_count,
                "stored_link_count": links.len(),
                "links_truncated": links_truncated,
                "fetch_status": page_ref.fetch_status,
                "content_mime_type": page_ref.content_mime_type,
                "page_quality_flags": page_quality_flags,
                "parse_status": parse_status.clone(),
                "source_role": "referrer",
            }));

            for edge in built.edges {
                dim_domains
                    .entry(edge.domain_to_id)
                    .or_insert_with(|| DimDomainRow {
                        domain_id: edge.domain_to_id,
                        domain: edge.domain_to.clone(),
                    });
                dim_urls.entry(edge.url_to_id).or_insert_with(|| DimUrlRow {
                    url_id: edge.url_to_id,
                    url: edge.url_to.clone(),
                    domain_id: edge.domain_to_id,
                    canonicalization_version: "v1".into(),
                });
                dim_anchors
                    .entry(edge.anchor_id)
                    .or_insert_with(|| DimAnchorRow {
                        anchor_id: edge.anchor_id,
                        anchor_text: edge.anchor_text.clone(),
                    });
                edge_rows.push(edge);
            }
            referrer_pages_parsed = referrer_pages_parsed.saturating_add(1);
        }

        let edge_key = staging_key(
            &self.config.corpus_root(),
            "edges",
            &self.config.cc_crawl_id,
            &run_date,
            &run_id,
            0,
        );
        let page_key = staging_key(
            &self.config.corpus_root(),
            "pages",
            &self.config.cc_crawl_id,
            &run_date,
            &run_id,
            0,
        );
        put_edge_parquet_object(&client, &self.config.ops_bucket, &edge_key, &edge_rows).await?;
        put_page_parquet_object(&client, &self.config.ops_bucket, &page_key, &page_rows).await?;
        let dim_url_rows = dim_urls.into_values().collect::<Vec<_>>();
        let dim_domain_rows = dim_domains.into_values().collect::<Vec<_>>();
        let dim_anchor_rows = dim_anchors.into_values().collect::<Vec<_>>();
        let dim_warc_file_rows = dim_warc_files.into_values().collect::<Vec<_>>();
        put_dim_url_parquet_object(
            &client,
            &self.config.ops_bucket,
            &format!(
                "{root}dims/dim_url/crawl_id={}/date={}/run_id={}/part-00000.parquet",
                self.config.cc_crawl_id, run_date, run_id
            ),
            &dim_url_rows,
        )
        .await?;
        put_dim_domain_parquet_object(
            &client,
            &self.config.ops_bucket,
            &format!(
                "{root}dims/dim_domain/crawl_id={}/date={}/run_id={}/part-00000.parquet",
                self.config.cc_crawl_id, run_date, run_id
            ),
            &dim_domain_rows,
        )
        .await?;
        put_dim_anchor_parquet_object(
            &client,
            &self.config.ops_bucket,
            &format!(
                "{root}dims/dim_anchor/crawl_id={}/date={}/run_id={}/part-00000.parquet",
                self.config.cc_crawl_id, run_date, run_id
            ),
            &dim_anchor_rows,
        )
        .await?;
        put_dim_warc_file_parquet_object(
            &client,
            &self.config.ops_bucket,
            &format!(
                "{root}dims/dim_warc_file/crawl_id={}/date={}/run_id={}/part-00000.parquet",
                self.config.cc_crawl_id, run_date, run_id
            ),
            &dim_warc_file_rows,
        )
        .await?;
        let materialization_manifest_key = format!(
            "{root}state/materialization/crawl_id={}/date={}/run_id={}.json",
            self.config.cc_crawl_id, run_date, run_id
        );
        put_json_object(
            &client,
            &self.config.ops_bucket,
            &materialization_manifest_key,
            &json!({
                "cc_crawl_id": self.config.cc_crawl_id,
                "run_date": run_date,
                "run_id": run_id,
                "edge_staging_key": edge_key,
                "page_staging_key": page_key,
                "customer_pages_selected": urls_selected,
                "referrer_pages_selected": referrer_pages_selected,
                "referrer_pages_parsed": referrer_pages_parsed,
                "edge_rows": edge_rows.len(),
                "page_rows": page_rows.len(),
            }),
        )
        .await?;

        info!(
            urls_selected,
            warc_attempted,
            warc_parsed,
            referrer_pages_selected,
            referrer_pages_parsed,
            edges = edge_rows.len(),
            pages = page_rows.len(),
            skips = skip_rows.len(),
            "link graph ingest complete"
        );

        let frontier_exhausted = urls_selected < self.config.max_urls_per_run;
        let frontier_state_key = format!(
            "{root}state/frontier/crawl_id={}/date={}.json",
            self.config.cc_crawl_id, run_date
        );
        put_json_object(
            &client,
            &self.config.ops_bucket,
            &frontier_state_key,
            &json!({
                "cc_crawl_id": self.config.cc_crawl_id,
                "run_date": run_date,
                "run_id": run_id,
                "frontier_domains": self.config.frontier_domains,
                "urls_selected": urls_selected,
                "max_urls_per_run": self.config.max_urls_per_run,
                "frontier_exhausted": frontier_exhausted,
                "warc_records_attempted": warc_attempted,
                "warc_records_parsed": warc_parsed,
                "referrer_pages_selected": referrer_pages_selected,
                "referrer_pages_parsed": referrer_pages_parsed,
                "referrer_selection_status": referrer_selection_status,
                "referrer_selection_error": referrer_selection_error,
                "referrer_selection_invalid_rows": referrer_selection_invalid_rows,
                "materialization_manifest_key": materialization_manifest_key,
                "cc_web_graph": web_graph_stats.clone(),
                "cc_index": index_stats.clone(),
            }),
        )
        .await?;
        put_json_object(
            &client,
            &self.config.ops_bucket,
            &format!(
                "{root}state/cc_index_batch_state/crawl_id={}/date={}.json",
                self.config.cc_crawl_id, run_date
            ),
            &json!({
                "cc_crawl_id": self.config.cc_crawl_id,
                "run_date": run_date,
                "mode": index_stats.mode,
                "batches_attempted": index_stats.batches_attempted,
                "batches_failed": index_stats.batches_failed,
                "rows_selected": index_stats.rows_selected,
            }),
        )
        .await?;
        for domain in &self.config.frontier_domains {
            put_json_object(
                &client,
                &self.config.ops_bucket,
                &format!(
                    "{root}state/cc_index_batch_state/domain_hash={}/crawl_id={}.json",
                    domain_hash(domain),
                    self.config.cc_crawl_id
                ),
                &json!({
                    "cc_crawl_id": self.config.cc_crawl_id,
                    "run_date": run_date,
                    "frontier_domain": domain,
                    "domain_hash": domain_hash(domain),
                    "mode": index_stats.mode,
                    "batches_attempted": index_stats.batches_attempted,
                    "batches_failed": index_stats.batches_failed,
                    "rows_selected": index_stats.rows_selected,
                    "files_total": index_stats.files_total,
                    "next_file_index": index_stats.next_file_index,
                    "index_paths": index_stats.index_paths.clone(),
                    "errors": index_stats.errors.clone(),
                    "status": if index_stats.batches_failed == 0 { "complete" } else { "failed" },
                    "updated_at": Utc::now().to_rfc3339(),
                }),
            )
            .await?;
        }
        put_json_object(
            &client,
            &self.config.ops_bucket,
            &format!(
                "{root}state/warc_record_state/crawl_id={}/date={}.json",
                self.config.cc_crawl_id, run_date
            ),
            &json!({
                "cc_crawl_id": self.config.cc_crawl_id,
                "run_date": run_date,
                "records": warc_record_state,
            }),
        )
        .await?;
        put_json_object(
            &client,
            &self.config.ops_bucket,
            &format!(
                "{root}state/frontier_domain_state/crawl_id={}/date={}.json",
                self.config.cc_crawl_id, run_date
            ),
            &json!({
                "cc_crawl_id": self.config.cc_crawl_id,
                "run_date": run_date,
                "domains": frontier_domain_state,
            }),
        )
        .await?;
        put_json_object(
            &client,
            &self.config.ops_bucket,
            &format!(
                "{root}state/page_parse_state/crawl_id={}/date={}.json",
                self.config.cc_crawl_id, run_date
            ),
            &json!({
                "cc_crawl_id": self.config.cc_crawl_id,
                "run_date": run_date,
                "pages": page_parse_state,
            }),
        )
        .await?;

        self.submit_rows(ctx.as_ref(), NAMESPACE_AUDIT_SKIP, &run_date, skip_rows)?;
        self.submit_rows(
            ctx.as_ref(),
            NAMESPACE_SITE_RUN_DAILY,
            &run_date,
            vec![json!({
                "cc_crawl_id": self.config.cc_crawl_id,
                "run_date": run_date,
                "urls_selected": urls_selected,
                "warc_records_attempted": warc_attempted,
                "warc_records_parsed": warc_parsed,
                "referrer_pages_selected": referrer_pages_selected,
                "referrer_pages_parsed": referrer_pages_parsed,
                "referrer_selection_status": referrer_selection_status,
                "referrer_selection_error": referrer_selection_error,
                "referrer_selection_invalid_rows": referrer_selection_invalid_rows,
                "materialization_manifest_key": materialization_manifest_key,
                "cc_index_mode": index_stats.mode,
                "cc_index_batches_attempted": index_stats.batches_attempted,
                "cc_index_batches_failed": index_stats.batches_failed,
                "cc_web_graph_mode": web_graph_stats.mode,
                "cc_web_graph_source_uri": web_graph_stats.source_uri,
                "cc_web_graph_rows_imported": web_graph_stats.rows_imported,
                "raw_edges_written": edge_rows.len(),
                "raw_pages_written": page_rows.len(),
                "live_crawl_enabled": self.config.live_crawl_enabled,
                "brightdata_proxy_escalation_enabled": self.config.brightdata_proxy_escalation_enabled,
                "frontier_exhausted": frontier_exhausted,
            })],
        )?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_validates() {
        let cfg = UpfoundryLinkGraphIngestConfig {
            ops_bucket: "bucket".into(),
            ops_prefix: "link-graph-corpus".into(),
            frontier_domains: vec!["skippr.io".into()],
            cc_crawl_id: "CC-MAIN-2025-01".into(),
            cc_crawl_ids: Vec::new(),
            cc_index_base_uri: "s3://commoncrawl/cc-index/table/cc-main/warc".into(),
            cc_urls_index_prefix: "cc/urls_index".into(),
            cc_index_source: "local_urls_index".into(),
            cc_direct_index_enabled: false,
            max_urls_per_run: 100,
            max_links_per_page: 2000,
            monthly_window: 24,
            cc_web_graph_uri: None,
            cc_web_graph_max_rows: 1_000_000,
            live_crawl_enabled: false,
            brightdata_proxy_escalation_enabled: false,
            include_subdomains: true,
            selected_referrer_page_refs_uri: None,
            corpus_run_id: None,
            max_referrer_pages_per_run: 50_000,
        };
        cfg.validate().expect("valid");
    }
}
