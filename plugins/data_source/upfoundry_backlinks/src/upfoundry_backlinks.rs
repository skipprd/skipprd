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
    load_pagerank_for_snapshot, load_spam_scores_for_snapshot, manifest_stale_reason, s3_client,
    EdgeByTargetRow, PageRankRow, SpamScoreRow,
};
use crate::streams::{
    ALL_NAMESPACES, NAMESPACE_ANCHOR_DAILY, NAMESPACE_BACKLINK_DAILY, NAMESPACE_HISTORY_DAILY,
    NAMESPACE_REFERRING_DOMAIN_DAILY, NAMESPACE_SITE_RUN_DAILY, NAMESPACE_SUMMARY_DAILY,
};

pub struct UpfoundryBacklinksPlugin {
    config: UpfoundryBacklinksConfig,
}

/// Match corpus edges whether the crawl normalized the host as apex or `www.`.
fn entity_target_domain_ids(entity_domain: &str) -> Vec<u64> {
    let lower = entity_domain.trim().to_ascii_lowercase();
    let mut ids = vec![domain_id(&lower)];
    if let Some(apex) = lower.strip_prefix("www.") {
        ids.push(domain_id(apex));
    } else {
        ids.push(domain_id(&format!("www.{lower}")));
    }
    ids.sort_unstable();
    ids.dedup();
    ids
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
        submit_payload_batches(
            ctx,
            batches,
        )?;
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
            pagerank
                .get(&domain_id(&self.config.entity_domain.to_ascii_lowercase()))
                .map(|rank| json!(rank.rank_percentile))
                .unwrap_or(Value::Null),
        );
        summary.insert(
            "backlinks_spam_score".into(),
            spam_scores
                .get(&domain_id(&self.config.entity_domain.to_ascii_lowercase()))
                .and_then(|score| score.spam_score.map(|value| json!(value)))
                .unwrap_or(Value::Null),
        );
        summary.insert(
            "spam_score_status".into(),
            spam_scores
                .get(&domain_id(&self.config.entity_domain.to_ascii_lowercase()))
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
        let target_ids = entity_target_domain_ids(&self.config.entity_domain);
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
            projection_index_stale,
            projection_index_stale_reason.as_deref(),
        );

        info!(
            snapshot_id,
            target = %self.config.entity_domain,
            edges = edges.len(),
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
            NAMESPACE_SITE_RUN_DAILY,
            &run_date,
            vec![json!({
                "site": self.config.entity_domain,
                "run_date": run_date,
                "target": self.config.entity_domain,
                "entity_kind": self.config.entity_kind.as_str(),
                "corpus_snapshot_id": snapshot_id,
                "projected_backlinks": edges.len(),
                "tasks_ok": edges.len(),
                "tasks_error": if projection_index_stale { 1 } else { 0 },
                "projection_index_stale": projection_index_stale,
                "projection_index_stale_reason": projection_index_stale_reason,
                "pagerank_status": if pagerank.is_empty() { "missing" } else { "available" },
                "spam_score_status": if spam_scores.is_empty() { "missing" } else { "available" },
                "status": if projection_index_stale { "partial" } else { "complete" },
            })],
        )?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::entity::EntityKind;

    #[test]
    fn entity_kind_serializes() {
        assert_eq!(EntityKind::Target.as_str(), "target");
    }
}
