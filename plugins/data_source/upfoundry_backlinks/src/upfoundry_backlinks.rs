use std::collections::HashMap;

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{json, Map, Value};
use skippr_plugin_shared_link_graph::domain_id;
use skippr_runtime_sdk::helpers::offsets::OffsetKey;
use skippr_runtime_sdk::plugins::source_contract::{
    FieldPath, SourceNamespaceContract, SourceSemantics, WritePolicy,
};
use skippr_runtime_sdk::plugins::{
    DataSource, SourceExecutionContract, SourceOnceContract, SourceSyncContext,
};
use skippr_runtime_sdk::source_compat::{submit_payload_batches, IngestBatch};
use tracing::info;

use crate::config::UpfoundryBacklinksConfig;
use crate::ops_reader::{
    latest_complete_manifest, load_edges_for_snapshot, load_manifest_for_snapshot,
    load_materialized_edges_from_manifest, load_pagerank_for_snapshot,
    load_spam_scores_for_snapshot, manifest_stale_reason, s3_client, EdgeByTargetRow,
    MaterializedEdgeRow, PageRankRow, SpamScoreRow,
};
use crate::streams::{
    ALL_NAMESPACES, NAMESPACE_ANCHOR_DAILY, NAMESPACE_BACKLINK_DAILY, NAMESPACE_HISTORY_DAILY,
    NAMESPACE_OUTBOUND_CONTEXT_DAILY, NAMESPACE_REFERRING_DOMAIN_DAILY, NAMESPACE_SITE_RUN_DAILY,
    NAMESPACE_SUMMARY_DAILY,
};

pub struct UpfoundryBacklinksPlugin {
    config: UpfoundryBacklinksConfig,
}

fn entity_target_domains(entity_domain: &str, domain_variants: &[String]) -> Vec<String> {
    let canonical = entity_domain.trim().to_ascii_lowercase();
    let mut domains = vec![canonical];
    for domain in domain_variants
        .iter()
        .map(|domain| domain.trim().to_ascii_lowercase())
        .filter(|domain| !domain.is_empty())
    {
        if !domains.contains(&domain) {
            domains.push(domain);
        }
    }
    domains
}

fn entity_target_domain_ids(entity_domain: &str, domain_variants: &[String]) -> Vec<u64> {
    let mut ids = Vec::new();
    for id in entity_target_domains(entity_domain, domain_variants)
        .iter()
        .map(|domain| domain_id(domain))
    {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids
}

fn first_pagerank<'a>(
    pagerank: &'a HashMap<u64, PageRankRow>,
    target_domain_ids: &[u64],
) -> Option<&'a PageRankRow> {
    target_domain_ids.iter().find_map(|id| pagerank.get(id))
}

fn first_spam_score<'a>(
    spam_scores: &'a HashMap<u64, SpamScoreRow>,
    target_domain_ids: &[u64],
) -> Option<&'a SpamScoreRow> {
    target_domain_ids.iter().find_map(|id| spam_scores.get(id))
}

impl UpfoundryBacklinksPlugin {
    pub fn new(config: UpfoundryBacklinksConfig) -> Result<Self, std::io::Error> {
        config.validate()?;
        Ok(Self { config })
    }

    fn namespace_contract(namespace: &str) -> SourceNamespaceContract {
        let run_date = FieldPath::single("run_date");
        let site = FieldPath::single("site");
        let target = FieldPath::single("target");
        let entity_kind = FieldPath::single("entity_kind");
        let competitor = FieldPath::single("competitor_name");
        let entity_pk = vec![site, target, entity_kind, competitor, run_date.clone()];
        let (primary_key, description) = match namespace {
            NAMESPACE_SITE_RUN_DAILY => (
                vec![FieldPath::single("site"), run_date.clone()],
                "Upfoundry backlinks projection run rollup",
            ),
            NAMESPACE_BACKLINK_DAILY => {
                let mut pk = entity_pk.clone();
                pk.insert(4, FieldPath::single("url_from_id"));
                pk.push(FieldPath::single("url_to_id"));
                (pk, "Projected backlink rows")
            }
            NAMESPACE_SUMMARY_DAILY => (entity_pk.clone(), "Projected summary"),
            NAMESPACE_REFERRING_DOMAIN_DAILY => {
                let mut pk = entity_pk.clone();
                pk.push(FieldPath::single("source_domain_id"));
                (pk, "Referring domain summary")
            }
            NAMESPACE_ANCHOR_DAILY => {
                let mut pk = entity_pk.clone();
                pk.push(FieldPath::single("anchor_id"));
                (pk, "Anchor summary")
            }
            NAMESPACE_HISTORY_DAILY => {
                let mut pk = entity_pk.clone();
                pk.push(FieldPath::single("edge_id"));
                (pk, "Link history")
            }
            NAMESPACE_OUTBOUND_CONTEXT_DAILY => {
                let mut pk = entity_pk.clone();
                pk.push(FieldPath::single("edge_id"));
                pk.push(FieldPath::single("url_from_id"));
                pk.push(FieldPath::single("url_to_id"));
                (pk, "Outbound links found on customer and referring pages")
            }
            _ => panic!("unknown namespace: {namespace}"),
        };
        SourceNamespaceContract {
            namespace: namespace.to_string(),
            primary_key,
            cursor: Some(run_date.clone()),
            partition_key: vec![run_date],
            write_policy: WritePolicy::ReplacePartition,
            refresh_window: None,
            description: description.into(),
            semantics: Some(SourceSemantics::MutableReport),
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
        let mut batches = Vec::new();
        for chunk in rows.chunks(1_000) {
            let payload = chunk
                .iter()
                .map(serde_json::to_string)
                .collect::<Result<Vec<_>, _>>()?
                .join("\n");
            batches.push(IngestBatch {
                offset_key: OffsetKey::new(namespace, run_date.to_string()),
                bytes: payload.len(),
                data: payload,
                namespace: Some(namespace.to_string()),
                source_uri: format!("upfoundry-backlinks://{}", self.config.entity_domain),
                offset_pos: None,
                cdc_rows: None,
            });
        }
        submit_payload_batches(ctx, batches)?;
        Ok(())
    }

    fn envelope(&self, run_date: &str) -> Map<String, Value> {
        let mut row = Map::new();
        row.insert("site".into(), json!(self.config.entity_domain));
        row.insert("target".into(), json!(self.config.entity_domain));
        row.insert(
            "entity_kind".into(),
            json!(self.config.entity_kind.as_str()),
        );
        row.insert(
            "competitor_name".into(),
            self.config
                .competitor_name
                .as_ref()
                .map(|n| json!(n))
                .unwrap_or(Value::Null),
        );
        row.insert("run_date".into(), json!(run_date));
        row
    }

    fn project_rows(
        &self,
        run_date: &str,
        snapshot_id: &str,
        edges: &[EdgeByTargetRow],
        pagerank: &HashMap<u64, PageRankRow>,
        spam_scores: &HashMap<u64, SpamScoreRow>,
        target_domain_ids: &[u64],
        projection_index_stale: bool,
        projection_index_stale_reason: Option<&str>,
    ) -> (Vec<Value>, Vec<Value>, Vec<Value>, Vec<Value>, Vec<Value>) {
        let mut backlink_rows = Vec::new();
        let mut ref_domains: HashMap<u64, u32> = HashMap::new();
        let mut anchors: HashMap<u64, u32> = HashMap::new();
        for edge in edges {
            let source_rank = pagerank.get(&edge.source_domain_id);
            let target_spam = spam_scores.get(&edge.target_domain_id);
            if backlink_rows.len() < self.config.max_detail_rows {
                let mut row = self.envelope(run_date);
                row.insert("corpus_snapshot_id".into(), json!(snapshot_id));
                row.insert("edge_id".into(), json!(edge.edge_id));
                row.insert("url_from".into(), json!(edge.url_from));
                row.insert("url_to".into(), json!(edge.url_to));
                row.insert("domain_from".into(), json!(edge.domain_from));
                row.insert("target_domain".into(), json!(edge.domain_to));
                row.insert("anchor".into(), json!(edge.anchor_text));
                row.insert("url_from_id".into(), json!(edge.url_from_id));
                row.insert("url_to_id".into(), json!(edge.url_to_id));
                row.insert("source_domain_id".into(), json!(edge.source_domain_id));
                row.insert("target_domain_id".into(), json!(edge.target_domain_id));
                row.insert(
                    "target_domain_hash_bucket".into(),
                    json!(edge.target_domain_hash_bucket),
                );
                row.insert(
                    "source_domain_hash_bucket".into(),
                    json!(edge.source_domain_hash_bucket),
                );
                row.insert("anchor_id".into(), json!(edge.anchor_id));
                row.insert("anchor_hash".into(), json!(edge.anchor_hash));
                row.insert("link_context".into(), json!(edge.link_context));
                row.insert("rel_semantics".into(), json!(edge.rel_semantics));
                row.insert("first_seen".into(), json!(edge.first_seen));
                row.insert("last_seen".into(), json!(edge.last_seen));
                row.insert(
                    "lost_seen_date".into(),
                    edge.lost_seen_date
                        .as_ref()
                        .map(|v| json!(v))
                        .unwrap_or(Value::Null),
                );
                row.insert("state".into(), json!(edge.state));
                row.insert("rel_flags".into(), json!(edge.rel_flags));
                row.insert("dofollow".into(), json!(edge.rel_flags & 1 == 0));
                row.insert("is_image_link".into(), json!(edge.is_image_link));
                row.insert(
                    "latest_edge_observation_id".into(),
                    json!(edge.latest_edge_observation_id),
                );
                row.insert("cc_crawl_id".into(), json!(edge.cc_crawl_id));
                row.insert("warc_record_id".into(), json!(edge.warc_record_id));
                row.insert(
                    "backlink_spam_score".into(),
                    target_spam
                        .and_then(|score| score.spam_score.map(|value| json!(value)))
                        .unwrap_or(Value::Null),
                );
                row.insert(
                    "page_from_rank".into(),
                    edge.page_from_rank
                        .map(|rank| json!(rank))
                        .or_else(|| source_rank.map(|rank| json!(rank.rank_percentile)))
                        .unwrap_or(Value::Null),
                );
                row.insert(
                    "http_status_from".into(),
                    edge.http_status_from
                        .map(|status| json!(status))
                        .unwrap_or(Value::Null),
                );
                row.insert("is_broken".into(), json!(edge.is_broken));
                row.insert(
                    "discovered_by".into(),
                    json!(if edge.discovered_by.is_empty() {
                        "cc_warc"
                    } else {
                        edge.discovered_by.as_str()
                    }),
                );
                row.insert(
                    "source_rank_percentile".into(),
                    source_rank
                        .map(|rank| json!(rank.rank_percentile))
                        .unwrap_or(Value::Null),
                );
                backlink_rows.push(Value::Object(row));
            }
            *ref_domains.entry(edge.source_domain_id).or_insert(0) += 1;
            *anchors.entry(edge.anchor_id).or_insert(0) += 1;
        }
        let referring_domain_rows: Vec<Value> = ref_domains
            .into_iter()
            .map(|(source_domain_id, count)| {
                let mut row = self.envelope(run_date);
                row.insert("corpus_snapshot_id".into(), json!(snapshot_id));
                row.insert("source_domain_id".into(), json!(source_domain_id));
                row.insert("backlink_count".into(), json!(count));
                Value::Object(row)
            })
            .collect();
        let anchor_rows: Vec<Value> = anchors
            .into_iter()
            .map(|(anchor_id, count)| {
                let mut row = self.envelope(run_date);
                row.insert("corpus_snapshot_id".into(), json!(snapshot_id));
                row.insert("anchor_id".into(), json!(anchor_id));
                row.insert("backlink_count".into(), json!(count));
                Value::Object(row)
            })
            .collect();
        let history_rows = edges
            .iter()
            .take(self.config.max_detail_rows)
            .map(|edge| {
                let mut row = self.envelope(run_date);
                row.insert("corpus_snapshot_id".into(), json!(snapshot_id));
                row.insert("edge_id".into(), json!(edge.edge_id));
                row.insert("first_seen".into(), json!(edge.first_seen));
                row.insert("last_seen".into(), json!(edge.last_seen));
                row.insert(
                    "lost_seen_date".into(),
                    edge.lost_seen_date
                        .as_ref()
                        .map(|v| json!(v))
                        .unwrap_or(Value::Null),
                );
                row.insert("state".into(), json!(edge.state));
                row.insert(
                    "latest_edge_observation_id".into(),
                    json!(edge.latest_edge_observation_id),
                );
                row.insert("cc_crawl_id".into(), json!(edge.cc_crawl_id));
                row.insert("warc_record_id".into(), json!(edge.warc_record_id));
                Value::Object(row)
            })
            .collect();
        let mut summary = self.envelope(run_date);
        summary.insert("corpus_snapshot_id".into(), json!(snapshot_id));
        summary.insert("total_backlinks".into(), json!(edges.len()));
        summary.insert("backlinks".into(), json!(edges.len()));
        summary.insert(
            "referring_domains".into(),
            json!(referring_domain_rows.len()),
        );
        summary.insert(
            "rank".into(),
            first_pagerank(pagerank, target_domain_ids)
                .map(|rank| json!(rank.rank_percentile))
                .unwrap_or(Value::Null),
        );
        summary.insert(
            "backlinks_spam_score".into(),
            first_spam_score(spam_scores, target_domain_ids)
                .and_then(|score| score.spam_score.map(|value| json!(value)))
                .unwrap_or(Value::Null),
        );
        summary.insert(
            "spam_score_status".into(),
            first_spam_score(spam_scores, target_domain_ids)
                .map(|score| json!(score.spam_score_status))
                .unwrap_or_else(|| json!("missing")),
        );
        summary.insert(
            "projection_index_stale".into(),
            json!(projection_index_stale),
        );
        if let Some(reason) = projection_index_stale_reason {
            summary.insert("projection_index_stale_reason".into(), json!(reason));
        }
        (
            backlink_rows,
            vec![Value::Object(summary)],
            referring_domain_rows,
            anchor_rows,
            history_rows,
        )
    }

    fn project_outbound_context_rows(
        &self,
        run_date: &str,
        snapshot_id: &str,
        edges: &[MaterializedEdgeRow],
        target_domain_ids: &[u64],
    ) -> Vec<Value> {
        let mut rows = Vec::new();
        for edge in edges.iter().take(self.config.max_outbound_rows) {
            let source_is_target = target_domain_ids.contains(&edge.domain_from_id);
            let target_is_target = target_domain_ids.contains(&edge.domain_to_id);
            let mut row = self.envelope(run_date);
            row.insert("corpus_snapshot_id".into(), json!(snapshot_id));
            row.insert("edge_id".into(), json!(edge.edge_id));
            row.insert("url_from".into(), json!(edge.url_from));
            row.insert("url_to".into(), json!(edge.url_to));
            row.insert("domain_from".into(), json!(edge.domain_from));
            row.insert("domain_to".into(), json!(edge.domain_to));
            row.insert("url_from_id".into(), json!(edge.url_from_id));
            row.insert("url_to_id".into(), json!(edge.url_to_id));
            row.insert("source_domain_id".into(), json!(edge.domain_from_id));
            row.insert("target_domain_id".into(), json!(edge.domain_to_id));
            row.insert("anchor_id".into(), json!(edge.anchor_id));
            row.insert("anchor".into(), json!(edge.anchor_text));
            row.insert("link_context".into(), json!(edge.link_context));
            row.insert("rel_flags".into(), json!(edge.rel_flags));
            row.insert("dofollow".into(), json!(edge.rel_flags & 1 == 0));
            row.insert("is_image_link".into(), json!(edge.is_image_link));
            row.insert("cc_crawl_id".into(), json!(edge.cc_crawl_id));
            row.insert("warc_record_offset".into(), json!(edge.warc_record_offset));
            row.insert("warc_record_length".into(), json!(edge.warc_record_length));
            row.insert(
                "http_status_from".into(),
                edge.http_status_from
                    .map(|status| json!(status))
                    .unwrap_or(Value::Null),
            );
            row.insert("discovered_by".into(), json!(edge.discovered_by));
            row.insert("source_is_customer_domain".into(), json!(source_is_target));
            row.insert("target_is_customer_domain".into(), json!(target_is_target));
            row.insert(
                "context_role".into(),
                json!(if source_is_target {
                    "customer_page_outbound"
                } else if target_is_target {
                    "referrer_link_to_customer"
                } else {
                    "referrer_page_outbound"
                }),
            );
            rows.push(Value::Object(row));
        }
        rows
    }
}

#[async_trait]
impl DataSource for UpfoundryBacklinksPlugin {
    fn execution_contract(&self) -> SourceExecutionContract {
        SourceExecutionContract::stream(SourceOnceContract::Finite)
    }

    fn source_namespace_contracts(&self) -> Vec<SourceNamespaceContract> {
        ALL_NAMESPACES
            .iter()
            .map(|ns| Self::namespace_contract(ns))
            .collect()
    }

    async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        let run_date = Utc::now().format("%Y-%m-%d").to_string();
        let client = s3_client().await?;
        let (snapshot_id, projection_index_stale_reason) = match &self.config.selected_snapshot_id {
            Some(id) => match load_manifest_for_snapshot(
                &client,
                &self.config.ops_bucket,
                &self.config.corpus_root(),
                id,
            )
            .await
            {
                Ok(manifest) => (id.clone(), manifest_stale_reason(&manifest)),
                Err(err) => (id.clone(), Some(format!("manifest_unavailable: {err}"))),
            },
            None => match latest_complete_manifest(
                &client,
                &self.config.ops_bucket,
                &self.config.corpus_root(),
            )
            .await
            {
                Ok(manifest) => (manifest.corpus_snapshot_id, None),
                Err(err) => (
                    "unavailable".into(),
                    Some(format!("manifest_unavailable: {err}")),
                ),
            },
        };
        let projection_index_stale = projection_index_stale_reason.is_some();
        let target_ids =
            entity_target_domain_ids(&self.config.entity_domain, &self.config.domain_variants);
        let mut all_edges = Vec::new();
        if !projection_index_stale {
            for target_id in &target_ids {
                all_edges.extend(
                    load_edges_for_snapshot(
                        &client,
                        &self.config.ops_bucket,
                        &self.config.corpus_root(),
                        &snapshot_id,
                        *target_id,
                    )
                    .await?,
                );
            }
        }
        let pagerank_rows = load_pagerank_for_snapshot(
            &client,
            &self.config.ops_bucket,
            &self.config.corpus_root(),
            &snapshot_id,
        )
        .await
        .unwrap_or_default();
        let spam_rows = load_spam_scores_for_snapshot(
            &client,
            &self.config.ops_bucket,
            &self.config.corpus_root(),
            &snapshot_id,
        )
        .await
        .unwrap_or_default();
        let pagerank: HashMap<u64, PageRankRow> = pagerank_rows
            .into_iter()
            .map(|row| (row.node_id, row))
            .collect();
        let spam_scores: HashMap<u64, SpamScoreRow> = spam_rows
            .into_iter()
            .map(|row| (row.domain_id, row))
            .collect();
        let edges: Vec<EdgeByTargetRow> = all_edges;

        let (backlinks, summaries, referring, anchors, history) = self.project_rows(
            &run_date,
            &snapshot_id,
            &edges,
            &pagerank,
            &spam_scores,
            &target_ids,
            projection_index_stale,
            projection_index_stale_reason.as_deref(),
        );
        let (materialized_edges, materialization_status, materialization_error) =
            if let Some(manifest_key) = &self.config.materialization_manifest_key {
                match load_materialized_edges_from_manifest(
                    &client,
                    &self.config.ops_bucket,
                    manifest_key,
                )
                .await
                {
                    Ok(rows) => (rows, "loaded", None),
                    Err(err) => (Vec::new(), "degraded", Some(err.to_string())),
                }
            } else {
                (Vec::new(), "not_configured", None)
            };
        let outbound_context = self.project_outbound_context_rows(
            &run_date,
            &snapshot_id,
            &materialized_edges,
            &target_ids,
        );

        info!(
            snapshot_id,
            target = %self.config.entity_domain,
            edges = edges.len(),
            outbound_context_rows = outbound_context.len(),
            "upfoundry backlinks projection complete"
        );

        self.submit_rows(ctx.as_ref(), NAMESPACE_BACKLINK_DAILY, &run_date, backlinks)?;
        self.submit_rows(ctx.as_ref(), NAMESPACE_SUMMARY_DAILY, &run_date, summaries)?;
        self.submit_rows(
            ctx.as_ref(),
            NAMESPACE_REFERRING_DOMAIN_DAILY,
            &run_date,
            referring,
        )?;
        self.submit_rows(ctx.as_ref(), NAMESPACE_ANCHOR_DAILY, &run_date, anchors)?;
        self.submit_rows(ctx.as_ref(), NAMESPACE_HISTORY_DAILY, &run_date, history)?;
        self.submit_rows(
            ctx.as_ref(),
            NAMESPACE_OUTBOUND_CONTEXT_DAILY,
            &run_date,
            outbound_context,
        )?;
        let materialization_degraded = materialization_error.is_some();
        let run_status_partial = projection_index_stale || materialization_degraded;
        let run_stale_reason = materialization_error
            .as_deref()
            .or(projection_index_stale_reason.as_deref());
        self.submit_rows(
            ctx.as_ref(),
            NAMESPACE_SITE_RUN_DAILY,
            &run_date,
            vec![json!({
                "site": self.config.entity_domain,
                "run_date": run_date,
                "target": self.config.entity_domain,
                "entity_kind": self.config.entity_kind.as_str(),
                "corpus_snapshot_id": snapshot_id,
                "projected_backlinks": edges.len(),
                "projected_outbound_context_rows": materialized_edges.len().min(self.config.max_outbound_rows),
                "materialization_manifest_key": self.config.materialization_manifest_key,
                "materialization_status": materialization_status,
                "materialization_error": materialization_error.clone(),
                "tasks_ok": edges.len(),
                "tasks_error": if run_status_partial { 1 } else { 0 },
                "projection_index_stale": run_status_partial,
                "projection_index_stale_reason": run_stale_reason,
                "pagerank_status": if pagerank.is_empty() { "missing" } else { "available" },
                "spam_score_status": if spam_scores.is_empty() { "missing" } else { "available" },
                "status": if run_status_partial { "partial" } else { "complete" },
            })],
        )?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::config::UpfoundryBacklinksConfig;
    use crate::entity::EntityKind;
    use crate::ops_reader::{MaterializedEdgeRow, PageRankRow, SpamScoreRow};
    use skippr_plugin_shared_link_graph::domain_id;

    use super::{
        entity_target_domain_ids, first_pagerank, first_spam_score, UpfoundryBacklinksPlugin,
    };

    #[test]
    fn entity_kind_serializes() {
        assert_eq!(EntityKind::Target.as_str(), "target");
    }

    #[test]
    fn target_ids_use_explicit_variants_only() {
        let canonical = domain_id("semrush.com");
        let www = domain_id("www.semrush.com");

        assert_eq!(
            entity_target_domain_ids("semrush.com", &[]),
            vec![canonical]
        );
        assert_eq!(
            entity_target_domain_ids("semrush.com", &["www.semrush.com".into()]),
            vec![canonical, www]
        );
    }

    #[test]
    fn authority_fallback_prefers_canonical_then_variants() {
        let canonical = domain_id("semrush.com");
        let www = domain_id("www.semrush.com");
        let ids = vec![canonical, www];
        let mut pagerank = HashMap::new();
        pagerank.insert(
            www,
            PageRankRow {
                node_id: www,
                pagerank: 0.2,
                rank_percentile: 22.0,
                converged: true,
            },
        );
        pagerank.insert(
            canonical,
            PageRankRow {
                node_id: canonical,
                pagerank: 0.9,
                rank_percentile: 99.0,
                converged: true,
            },
        );

        assert_eq!(
            first_pagerank(&pagerank, &ids).unwrap().rank_percentile,
            99.0
        );

        let mut spam_scores = HashMap::new();
        spam_scores.insert(
            www,
            SpamScoreRow {
                domain_id: www,
                spam_score: Some(12),
                spam_score_status: "available".into(),
            },
        );
        assert_eq!(
            first_spam_score(&spam_scores, &ids).unwrap().spam_score,
            Some(12)
        );
    }

    #[test]
    fn outbound_context_uses_raw_edge_schema() {
        let plugin = UpfoundryBacklinksPlugin {
            config: UpfoundryBacklinksConfig {
                site: "example.com".into(),
                entity_kind: EntityKind::Target,
                entity_domain: "example.com".into(),
                domain_variants: vec![],
                primary_domain: None,
                competitor_name: None,
                ops_bucket: "bucket".into(),
                ops_prefix: "link-graph-corpus".into(),
                selected_snapshot_id: None,
                include_subdomains: true,
                max_detail_rows: 100,
                materialization_manifest_key: Some("manifest.json".into()),
                max_outbound_rows: 10,
            },
        };
        let edge = MaterializedEdgeRow {
            edge_id: "edge-1".into(),
            url_from: "https://source.example/page".into(),
            url_to: "https://example.com/target".into(),
            domain_from: "source.example".into(),
            domain_to: "example.com".into(),
            url_from_id: 1,
            url_to_id: 2,
            domain_from_id: domain_id("source.example"),
            domain_to_id: domain_id("example.com"),
            anchor_text: "Anchor".into(),
            anchor_id: 3,
            link_context: "body".into(),
            rel_flags: 0,
            is_image_link: false,
            cc_crawl_id: "CC-MAIN-X".into(),
            warc_record_offset: 10,
            warc_record_length: 20,
            http_status_from: Some(200),
            discovered_by: "cc_wat_target_index".into(),
        };
        let rows = plugin.project_outbound_context_rows(
            "2026-01-01",
            "snapshot-1",
            &[edge],
            &[domain_id("example.com")],
        );
        let row = rows[0].as_object().unwrap();
        assert_eq!(
            row.get("url_from").and_then(|v| v.as_str()),
            Some("https://source.example/page")
        );
        assert_eq!(
            row.get("url_to").and_then(|v| v.as_str()),
            Some("https://example.com/target")
        );
        assert_eq!(
            row.get("domain_from").and_then(|v| v.as_str()),
            Some("source.example")
        );
        assert_eq!(
            row.get("domain_to").and_then(|v| v.as_str()),
            Some("example.com")
        );
        assert_eq!(row.get("anchor").and_then(|v| v.as_str()), Some("Anchor"));
        assert!(row.get("source_url").is_none());
        assert!(row.get("target_url").is_none());
        assert!(row.get("anchor_text").is_none());
    }
}
