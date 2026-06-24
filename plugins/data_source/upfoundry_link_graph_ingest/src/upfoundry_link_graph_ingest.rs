use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use aws_config::BehaviorVersion;
use chrono::Utc;
use serde_json::{json, Value};
use skippr_plugin_shared_link_graph::{
    anchor_id, canonicalize_url, domain_id, edge_id, edge_observation_id, parse_html_links, url_id,
    warc_file_id, RawEdgeObservation, RawPageFact,
};
use skippr_runtime_sdk::helpers::offsets::OffsetKey;
use skippr_runtime_sdk::plugins::{
    DataSource, SourceExecutionContract, SourceOnceContract, SourceSyncContext,
};
use skippr_runtime_sdk::source_compat::{submit_payload_batches, IngestBatch};
use tracing::info;

use crate::cc::{domain_matches, load_url_candidates, CcIndexRequest};
use crate::config::UpfoundryLinkGraphIngestConfig;
use crate::live::{fetch_live_html, LiveFetchConfig};
use crate::ops_store::{
    put_dim_anchor_parquet_object, put_dim_domain_parquet_object, put_dim_url_parquet_object,
    put_dim_warc_file_parquet_object, put_edge_parquet_object, put_json_object,
    put_page_parquet_object, staging_key, DimAnchorRow, DimDomainRow, DimUrlRow, DimWarcFileRow,
};
use crate::streams::{all_namespace_contracts, NAMESPACE_AUDIT_SKIP, NAMESPACE_SITE_RUN_DAILY};
use crate::warc::fetch_warc_html;
use crate::webgraph::import_web_graph_priors;

pub struct UpfoundryLinkGraphIngestPlugin {
    config: UpfoundryLinkGraphIngestConfig,
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

    fn hex128(bytes: &[u8; 16]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn status_indicates_broken(status: Option<u32>) -> bool {
        status
            .map(|status| !(200..400).contains(&status))
            .unwrap_or(false)
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
        let fixture_dir = Self::fixture_dir();
        let fixture_ref = fixture_dir.as_deref();
        let client = Self::s3_client().await?;
        let web_graph_stats = import_web_graph_priors(
            &client,
            &self.config.ops_bucket,
            &self.config.corpus_root(),
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
            cc_index_base_uri: &self.config.cc_index_base_uri,
            crawl_ids: self.config.crawl_ids(),
        })
        .await;
        let index_stats = index_result.stats;
        let candidates = index_result.records;

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
            let warc_record_id = format!(
                "{}:{}:{}",
                wf_id, record.warc_record_offset, record.warc_record_length
            );
            let page_quality_flags = Self::page_quality_flags(
                &source_canon.canonical,
                raw_link_count,
                links.len() as u32,
                links_truncated,
                self.config.max_links_per_page,
            );
            let source_is_broken = Self::status_indicates_broken(http_status_from);

            page_rows.push(RawPageFact {
                url_id: src_url_id,
                domain_id: src_domain_id,
                cc_crawl_id: self.config.cc_crawl_id.clone(),
                warc_file_id: wf_id,
                warc_record_offset: record.warc_record_offset,
                warc_record_length: record.warc_record_length,
                fetch_status: http_status_from,
                content_mime_type: content_mime_type.clone(),
                fetch_time: record.fetch_time.clone(),
                outbound_link_count: links.len() as u32,
                stored_link_count: links.len() as u32,
                links_truncated,
                raw_link_count,
                page_quality_flags: page_quality_flags.clone(),
                canonicalization_version: source_canon.version.to_string(),
                parser_version: "link_graph_html_v1".into(),
                parse_status: parse_status.clone(),
            });
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
                    &source_canon.canonical,
                    &target.canonical,
                    &rel_sem,
                    ctx_bucket,
                );
                let eid_hex = Self::hex128(&eid);
                let obs = edge_observation_id(
                    &eid,
                    &self.config.cc_crawl_id,
                    &warc_record_id,
                    ordinal as u32,
                );
                let normalized_anchor = link
                    .anchor_text
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                let target_url_id = url_id(&target.canonical);
                let target_domain_id = domain_id(&target.host);
                let normalized_anchor_id = anchor_id(&normalized_anchor);
                dim_domains
                    .entry(target_domain_id)
                    .or_insert_with(|| DimDomainRow {
                        domain_id: target_domain_id,
                        domain: target.host.clone(),
                    });
                dim_urls.entry(target_url_id).or_insert_with(|| DimUrlRow {
                    url_id: target_url_id,
                    url: target.canonical.clone(),
                    domain_id: target_domain_id,
                    canonicalization_version: target.version.to_string(),
                });
                dim_anchors
                    .entry(normalized_anchor_id)
                    .or_insert_with(|| DimAnchorRow {
                        anchor_id: normalized_anchor_id,
                        anchor_text: normalized_anchor.clone(),
                    });
                edge_rows.push(RawEdgeObservation {
                    edge_observation_id: Self::hex128(&obs),
                    edge_id: eid_hex,
                    url_from: source_canon.canonical.clone(),
                    url_to: target.canonical.clone(),
                    domain_from: source_canon.host.clone(),
                    domain_to: target.host.clone(),
                    url_from_id: src_url_id,
                    url_to_id: target_url_id,
                    domain_from_id: src_domain_id,
                    domain_to_id: target_domain_id,
                    anchor_text: normalized_anchor.clone(),
                    anchor_id: normalized_anchor_id,
                    link_context: ctx_bucket.to_string(),
                    rel_flags: Self::rel_flags(&link.rel),
                    is_image_link: link.is_image_link,
                    link_ordinal: ordinal as u32,
                    cc_crawl_id: self.config.cc_crawl_id.clone(),
                    warc_file_id: wf_id,
                    warc_record_offset: record.warc_record_offset,
                    warc_record_length: record.warc_record_length,
                    http_status_from,
                    is_broken: source_is_broken,
                    fetch_time: record.fetch_time.clone(),
                    canonicalization_version: source_canon.version.to_string(),
                    discovered_by: discovered_by.clone(),
                });
            }
        }

        let edge_key = staging_key(
            &self.config.corpus_root(),
            "edges",
            &self.config.cc_crawl_id,
            &run_date,
            0,
        );
        let page_key = staging_key(
            &self.config.corpus_root(),
            "pages",
            &self.config.cc_crawl_id,
            &run_date,
            0,
        );
        put_edge_parquet_object(&client, &self.config.ops_bucket, &edge_key, &edge_rows).await?;
        put_page_parquet_object(&client, &self.config.ops_bucket, &page_key, &page_rows).await?;
        let root = self.config.corpus_root();
        let dim_url_rows = dim_urls.into_values().collect::<Vec<_>>();
        let dim_domain_rows = dim_domains.into_values().collect::<Vec<_>>();
        let dim_anchor_rows = dim_anchors.into_values().collect::<Vec<_>>();
        let dim_warc_file_rows = dim_warc_files.into_values().collect::<Vec<_>>();
        put_dim_url_parquet_object(
            &client,
            &self.config.ops_bucket,
            &format!(
                "{root}dims/dim_url/crawl_id={}/date={}/part-00000.parquet",
                self.config.cc_crawl_id, run_date
            ),
            &dim_url_rows,
        )
        .await?;
        put_dim_domain_parquet_object(
            &client,
            &self.config.ops_bucket,
            &format!(
                "{root}dims/dim_domain/crawl_id={}/date={}/part-00000.parquet",
                self.config.cc_crawl_id, run_date
            ),
            &dim_domain_rows,
        )
        .await?;
        put_dim_anchor_parquet_object(
            &client,
            &self.config.ops_bucket,
            &format!(
                "{root}dims/dim_anchor/crawl_id={}/date={}/part-00000.parquet",
                self.config.cc_crawl_id, run_date
            ),
            &dim_anchor_rows,
        )
        .await?;
        put_dim_warc_file_parquet_object(
            &client,
            &self.config.ops_bucket,
            &format!(
                "{root}dims/dim_warc_file/crawl_id={}/date={}/part-00000.parquet",
                self.config.cc_crawl_id, run_date
            ),
            &dim_warc_file_rows,
        )
        .await?;

        info!(
            urls_selected,
            warc_attempted,
            warc_parsed,
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
                "frontier_domains": self.config.frontier_domains,
                "urls_selected": urls_selected,
                "max_urls_per_run": self.config.max_urls_per_run,
                "frontier_exhausted": frontier_exhausted,
                "warc_records_attempted": warc_attempted,
                "warc_records_parsed": warc_parsed,
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
            max_urls_per_run: 100,
            max_links_per_page: 2000,
            monthly_window: 24,
            cc_web_graph_uri: None,
            cc_web_graph_max_rows: 1_000_000,
            live_crawl_enabled: false,
            brightdata_proxy_escalation_enabled: false,
            include_subdomains: true,
        };
        cfg.validate().expect("valid");
    }
}
