use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{json, Map, Value};
use skippr_runtime_sdk::helpers::offsets::OffsetKey;
use skippr_runtime_sdk::plugins::cdc::{CheckpointAuthority, CheckpointEnvelope, CheckpointKind};
use skippr_runtime_sdk::plugins::source_contract::{
    FieldPath, SourceNamespaceContract, SourceSemantics, WritePolicy,
};
use skippr_runtime_sdk::plugins::{
    DataSource, SourceExecutionContract, SourceOnceContract, SourceSyncContext,
};
use skippr_runtime_sdk::protocol::SKIPPR_RUNTIME_EXECUTION_MODE_ENV;
use skippr_runtime_sdk::source_compat::{
    load_checkpoint_payload, submit_payload_batches, IngestBatch,
};
use tracing::{info, warn};

use crate::checkpoint::{checkpoint_key, JobPaginationCheckpoint};
use crate::client::DataForSeoClient;
use crate::config::{
    BacklinkJob, DataForSeoBacklinksPluginConfig, IntersectionJob, RunMode, StreamKind,
};
use crate::entity::{EntityKind, SyncEntity};
use crate::parse_anchors::parse_anchor_items;
use crate::parse_backlinks::{
    build_backlink_task, next_pagination_state, parse_backlinks_items, prepare_backlink_target,
    BacklinkParseContext, PaginationAdvance,
};
use crate::parse_history::parse_history_items;
use crate::parse_intersection::{
    build_intersection_task, normalize_intersection_job, parse_intersection_items,
    IntersectionParseContext,
};
use crate::parse_referring_domains::parse_referring_domain_items;
use crate::parse_summary::parse_summary_row;
use crate::parse_target::{build_target_task, TargetQueryOptions};
use crate::streams::{
    NAMESPACE_ANCHOR_DAILY, NAMESPACE_BACKLINK_DAILY, NAMESPACE_HISTORY_DAILY,
    NAMESPACE_PAGE_INTERSECTION_DAILY, NAMESPACE_REFERRING_DOMAIN_DAILY, NAMESPACE_SITE_RUN_DAILY,
    NAMESPACE_SUMMARY_DAILY,
};

const CHECKPOINT_PAYLOAD_VERSION: u32 = 1;
const PAGINATED_DEFAULT_LIMIT: u32 = 100;
const PAGINATED_DEFAULT_MAX_PAGES: u32 = 5;

fn runtime_is_discover_mode() -> bool {
    std::env::var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV)
        .map(|mode| mode.eq_ignore_ascii_case("discover"))
        .unwrap_or(false)
}

#[derive(Debug, Default)]
struct RunStats {
    total_api_cost_usd: f64,
    tasks_ok: u32,
    tasks_error: u32,
    rows_by_stream: HashMap<String, u32>,
}

impl RunStats {
    fn add_rows(&mut self, stream: &str, count: u32) {
        *self.rows_by_stream.entry(stream.to_string()).or_insert(0) += count;
    }
}

pub struct DataForSeoBacklinksPlugin {
    config: DataForSeoBacklinksPluginConfig,
    client: DataForSeoClient,
    site: String,
}

impl DataForSeoBacklinksPlugin {
    pub fn new(config: DataForSeoBacklinksPluginConfig) -> Result<Self, std::io::Error> {
        config.validate()?;
        let (login, password) = if std::env::var("SKIPPR_DATAFORSEO_BACKLINKS_FIXTURE_DIR")
            .map(|d| !d.trim().is_empty())
            .unwrap_or(false)
        {
            ("fixture".into(), "fixture".into())
        } else {
            config.resolve_credentials()?
        };
        let site = config.site_label();
        let client = DataForSeoClient::new(
            login,
            password,
            config.max_api_retries,
            config.request_interval_ms,
        );
        Ok(Self {
            config,
            client,
            site,
        })
    }

    fn run_date() -> String {
        Utc::now().format("%Y-%m-%d").to_string()
    }

    fn namespace_contract(namespace: &str) -> SourceNamespaceContract {
        let run_date = FieldPath::single("run_date");
        let site = FieldPath::single("site");
        let target = FieldPath::single("target");
        let entity_kind = FieldPath::single("entity_kind");
        let competitor = FieldPath::single("competitor_name");
        let entity_pk = vec![site, target, entity_kind, competitor, run_date.clone()];
        let partition_key = vec![run_date.clone()];

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
            NAMESPACE_PAGE_INTERSECTION_DAILY => (
                vec![
                    FieldPath::single("job_name"),
                    FieldPath::single("url_from"),
                    run_date.clone(),
                ],
                "Page intersection daily snapshot",
            ),
            NAMESPACE_SUMMARY_DAILY => (entity_pk.clone(), "Projected backlink summary"),
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
            _ => (vec![run_date.clone()], "Upfoundry backlinks namespace"),
        };

        SourceNamespaceContract {
            namespace: namespace.to_string(),
            primary_key,
            cursor: Some(run_date),
            partition_key,
            write_policy: WritePolicy::ReplacePartition,
            refresh_window: None,
            description: description.into(),
            semantics: Some(SourceSemantics::MutableReport),
        }
    }

    fn load_job_checkpoint(ctx: &dyn SourceSyncContext, key: &str) -> JobPaginationCheckpoint {
        load_checkpoint_payload::<JobPaginationCheckpoint>(ctx, key).unwrap_or(
            JobPaginationCheckpoint {
                offset: 0,
                search_after_token: None,
                pages_completed: 0,
            },
        )
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
            .into_iter()
            .map(|row| serde_json::to_string(&row))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .join("\n");
        let bytes = payload.len();
        submit_payload_batches(
            ctx,
            vec![IngestBatch {
                offset_key: OffsetKey::new(namespace, run_date.to_string()),
                data: payload,
                bytes,
                offset_pos: None,
                source_uri: format!("dataforseo-backlinks://{}", self.site),
                namespace: Some(namespace.to_string()),
                cdc_rows: None,
            }],
        )?;
        Ok(())
    }

    fn store_job_checkpoint(
        ctx: &dyn SourceSyncContext,
        key: &str,
        checkpoint: &JobPaginationCheckpoint,
    ) -> Result<(), std::io::Error> {
        let envelope = CheckpointEnvelope::from_payload(
            CheckpointAuthority::AdvisoryHint,
            CheckpointKind::SourceResume,
            CHECKPOINT_PAYLOAD_VERSION,
            checkpoint,
        )
        .map_err(|e| std::io::Error::other(e.to_string()))?;
        ctx.store_checkpoint(key, &envelope)
            .map_err(std::io::Error::other)
    }

    fn target_query_for_entity(&self, entity: &SyncEntity) -> TargetQueryOptions {
        TargetQueryOptions::from_jobs(&entity.backlink_jobs)
    }

    fn paginated_limit_max_pages(&self, discover: bool) -> (u32, u32) {
        if discover {
            return (
                crate::config::DISCOVER_LIMIT,
                crate::config::DISCOVER_MAX_PAGES,
            );
        }
        (PAGINATED_DEFAULT_LIMIT, PAGINATED_DEFAULT_MAX_PAGES)
    }

    async fn sync_backlink_job(
        &self,
        ctx: &Arc<dyn SourceSyncContext>,
        entity: &SyncEntity,
        job: &BacklinkJob,
        run_date: &str,
        discover: bool,
        stats: &mut RunStats,
    ) -> Result<(), std::io::Error> {
        let target = prepare_backlink_target(job)?;
        let job_id = job.job_id();
        let cp_key = checkpoint_key(
            StreamKind::Backlinks.as_str(),
            &entity.checkpoint_key(),
            &job_id,
            run_date,
        );
        let mut checkpoint = if discover {
            JobPaginationCheckpoint::default()
        } else {
            Self::load_job_checkpoint(ctx.as_ref(), &cp_key)
        };

        let limit = job.effective_limit(discover);
        let max_pages = job.effective_max_pages(discover);
        let rank_scale = self.config.rank_scale.as_deref();
        let mut all_rows = Vec::new();
        let fixture = format!("backlinks_live_{}", checkpoint.pages_completed);

        loop {
            let task = build_backlink_task(
                job,
                &target,
                limit,
                checkpoint.offset,
                checkpoint.search_after_token.as_deref(),
                rank_scale,
            );
            let response = self
                .client
                .post_backlinks_live(vec![task], &fixture)
                .await?;
            stats.total_api_cost_usd += response.top_level_cost;

            let Some(task_result) = response.tasks.into_iter().next() else {
                break;
            };
            stats.total_api_cost_usd += task_result.task_cost;

            if !task_result.task_ok {
                stats.tasks_error += 1;
                warn!(
                    target = %target,
                    status_code = task_result.task_status_code,
                    message = %task_result.task_status_message,
                    "DataForSEO backlinks task error"
                );
                break;
            }
            stats.tasks_ok += 1;

            let ctx_parse = BacklinkParseContext {
                entity: entity.parse_context(run_date),
                job_tag: job.job_tag.as_deref().unwrap_or(&job_id),
                backlinks_status_type: job.backlinks_status_type.as_deref(),
                mode: job.mode.as_deref(),
                api_cost_usd: task_result.task_cost,
            };
            all_rows.extend(parse_backlinks_items(&task_result.items, &ctx_parse));

            let advance = next_pagination_state(
                limit,
                checkpoint.offset,
                task_result.items_count,
                task_result.search_after_token,
                checkpoint.pages_completed,
                max_pages,
            );
            match advance {
                PaginationAdvance::Done => break,
                PaginationAdvance::ContinueWithOffset {
                    offset,
                    pages_completed,
                } => {
                    checkpoint = JobPaginationCheckpoint {
                        offset,
                        search_after_token: None,
                        pages_completed,
                    };
                }
                PaginationAdvance::ContinueWithToken {
                    search_after_token,
                    pages_completed,
                } => {
                    checkpoint = JobPaginationCheckpoint {
                        offset: 0,
                        search_after_token: Some(search_after_token),
                        pages_completed,
                    };
                }
            }

            if !discover {
                Self::store_job_checkpoint(ctx.as_ref(), &cp_key, &checkpoint)?;
            }
        }

        if !all_rows.is_empty() {
            let count = all_rows.len() as u32;
            stats.add_rows(StreamKind::Backlinks.as_str(), count);
            self.submit_rows(ctx.as_ref(), NAMESPACE_BACKLINK_DAILY, run_date, all_rows)?;
        }

        if !discover {
            Self::store_job_checkpoint(ctx.as_ref(), &cp_key, &JobPaginationCheckpoint::default())?;
        }

        Ok(())
    }

    async fn sync_summary(
        &self,
        ctx: &Arc<dyn SourceSyncContext>,
        entity: &SyncEntity,
        run_date: &str,
        stats: &mut RunStats,
    ) -> Result<(), std::io::Error> {
        let query = self.target_query_for_entity(entity);
        let task = build_target_task(
            &entity.target,
            0,
            0,
            None,
            self.config.rank_scale.as_deref(),
            &query,
            Map::new(),
        );
        let response = self
            .client
            .post_summary_live(vec![task], "summary_live")
            .await?;
        stats.total_api_cost_usd += response.top_level_cost;
        let Some(task_result) = response.tasks.into_iter().next() else {
            return Ok(());
        };
        stats.total_api_cost_usd += task_result.task_cost;
        if !task_result.task_ok {
            stats.tasks_error += 1;
            return Ok(());
        }
        stats.tasks_ok += 1;
        let Some(result) = task_result.result_body else {
            return Ok(());
        };
        if let Some(row) = parse_summary_row(
            &result,
            &entity.parse_context(run_date),
            task_result.task_cost,
        ) {
            stats.add_rows(StreamKind::Summary.as_str(), 1);
            self.submit_rows(ctx.as_ref(), NAMESPACE_SUMMARY_DAILY, run_date, vec![row])?;
        }
        Ok(())
    }

    async fn post_paginated_stream(
        &self,
        stream: StreamKind,
        tasks: Vec<Value>,
        fixture: &str,
    ) -> Result<crate::client::LiveApiResponse, std::io::Error> {
        match stream {
            StreamKind::ReferringDomains => {
                self.client
                    .post_referring_domains_live(tasks, fixture)
                    .await
            }
            StreamKind::Anchors => self.client.post_anchors_live(tasks, fixture).await,
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "not a paginated target stream",
            )),
        }
    }

    async fn sync_target_paginated(
        &self,
        ctx: &Arc<dyn SourceSyncContext>,
        entity: &SyncEntity,
        run_date: &str,
        discover: bool,
        stream: StreamKind,
        namespace: &str,
        fixture_prefix: &str,
        stats: &mut RunStats,
    ) -> Result<(), std::io::Error> {
        let job_id = stream.as_str();
        let cp_key = checkpoint_key(stream.as_str(), &entity.checkpoint_key(), job_id, run_date);
        let mut checkpoint = if discover {
            JobPaginationCheckpoint::default()
        } else {
            Self::load_job_checkpoint(ctx.as_ref(), &cp_key)
        };

        let (limit, max_pages) = self.paginated_limit_max_pages(discover);
        let query = self.target_query_for_entity(entity);
        let rank_scale = self.config.rank_scale.as_deref();
        let mut all_rows = Vec::new();

        loop {
            let fixture = format!("{fixture_prefix}_{}", checkpoint.pages_completed);
            let task = build_target_task(
                &entity.target,
                limit,
                checkpoint.offset,
                checkpoint.search_after_token.as_deref(),
                rank_scale,
                &query,
                Map::new(),
            );
            let response = self
                .post_paginated_stream(stream, vec![task], &fixture)
                .await?;
            stats.total_api_cost_usd += response.top_level_cost;

            let Some(task_result) = response.tasks.into_iter().next() else {
                break;
            };
            stats.total_api_cost_usd += task_result.task_cost;
            if !task_result.task_ok {
                stats.tasks_error += 1;
                break;
            }
            stats.tasks_ok += 1;

            let entity_ctx = entity.parse_context(run_date);
            let page_rows = match stream {
                StreamKind::ReferringDomains => parse_referring_domain_items(
                    &task_result.items,
                    &entity_ctx,
                    task_result.task_cost,
                ),
                StreamKind::Anchors => {
                    parse_anchor_items(&task_result.items, &entity_ctx, task_result.task_cost)
                }
                _ => Vec::new(),
            };
            all_rows.extend(page_rows);

            let advance = next_pagination_state(
                limit,
                checkpoint.offset,
                task_result.items_count,
                task_result.search_after_token,
                checkpoint.pages_completed,
                max_pages,
            );
            match advance {
                PaginationAdvance::Done => break,
                PaginationAdvance::ContinueWithOffset {
                    offset,
                    pages_completed,
                } => {
                    checkpoint = JobPaginationCheckpoint {
                        offset,
                        search_after_token: None,
                        pages_completed,
                    };
                }
                PaginationAdvance::ContinueWithToken {
                    search_after_token,
                    pages_completed,
                } => {
                    checkpoint = JobPaginationCheckpoint {
                        offset: 0,
                        search_after_token: Some(search_after_token),
                        pages_completed,
                    };
                }
            }

            if !discover {
                Self::store_job_checkpoint(ctx.as_ref(), &cp_key, &checkpoint)?;
            }
        }

        if !all_rows.is_empty() {
            let count = all_rows.len() as u32;
            stats.add_rows(stream.as_str(), count);
            self.submit_rows(ctx.as_ref(), namespace, run_date, all_rows)?;
        }

        if !discover {
            Self::store_job_checkpoint(ctx.as_ref(), &cp_key, &JobPaginationCheckpoint::default())?;
        }

        Ok(())
    }

    async fn sync_referring_domains(
        &self,
        ctx: &Arc<dyn SourceSyncContext>,
        entity: &SyncEntity,
        run_date: &str,
        discover: bool,
        stats: &mut RunStats,
    ) -> Result<(), std::io::Error> {
        self.sync_target_paginated(
            ctx,
            entity,
            run_date,
            discover,
            StreamKind::ReferringDomains,
            NAMESPACE_REFERRING_DOMAIN_DAILY,
            "referring_domains_live",
            stats,
        )
        .await
    }

    async fn sync_anchors(
        &self,
        ctx: &Arc<dyn SourceSyncContext>,
        entity: &SyncEntity,
        run_date: &str,
        discover: bool,
        stats: &mut RunStats,
    ) -> Result<(), std::io::Error> {
        self.sync_target_paginated(
            ctx,
            entity,
            run_date,
            discover,
            StreamKind::Anchors,
            NAMESPACE_ANCHOR_DAILY,
            "anchors_live",
            stats,
        )
        .await
    }

    async fn sync_history(
        &self,
        ctx: &Arc<dyn SourceSyncContext>,
        entity: &SyncEntity,
        run_date: &str,
        discover: bool,
        stats: &mut RunStats,
    ) -> Result<(), std::io::Error> {
        let query = self.target_query_for_entity(entity);
        let mut extra = Map::new();
        if discover {
            extra.insert("date_from".into(), json!("2024-01-01"));
            extra.insert("date_to".into(), json!("2024-03-01"));
        } else if let Some(history) = &self.config.history {
            if let Some(from) = &history.date_from {
                extra.insert("date_from".into(), json!(from));
            }
            if let Some(to) = &history.date_to {
                extra.insert("date_to".into(), json!(to));
            }
        }
        let task = build_target_task(
            &entity.target,
            0,
            0,
            None,
            self.config.rank_scale.as_deref(),
            &query,
            extra,
        );
        let response = self
            .client
            .post_history_live(vec![task], "history_live")
            .await?;
        stats.total_api_cost_usd += response.top_level_cost;
        let Some(task_result) = response.tasks.into_iter().next() else {
            return Ok(());
        };
        stats.total_api_cost_usd += task_result.task_cost;
        if !task_result.task_ok {
            stats.tasks_error += 1;
            return Ok(());
        }
        stats.tasks_ok += 1;
        let rows = parse_history_items(
            &task_result.items,
            &entity.parse_context(run_date),
            task_result.task_cost,
        );
        if !rows.is_empty() {
            let count = rows.len() as u32;
            stats.add_rows(StreamKind::History.as_str(), count);
            self.submit_rows(ctx.as_ref(), NAMESPACE_HISTORY_DAILY, run_date, rows)?;
        }
        Ok(())
    }

    async fn sync_intersection_job(
        &self,
        ctx: &Arc<dyn SourceSyncContext>,
        entity: &SyncEntity,
        job: &IntersectionJob,
        run_date: &str,
        discover: bool,
        stats: &mut RunStats,
    ) -> Result<(), std::io::Error> {
        let (targets, excludes) = normalize_intersection_job(job)?;
        let targets_config = serde_json::to_value(&targets).map_err(std::io::Error::other)?;
        let job_id = job.name.clone();
        let cp_key = checkpoint_key(
            StreamKind::PageIntersection.as_str(),
            &entity.checkpoint_key(),
            &job_id,
            run_date,
        );
        let mut checkpoint = if discover {
            JobPaginationCheckpoint::default()
        } else {
            Self::load_job_checkpoint(ctx.as_ref(), &cp_key)
        };

        let limit = job.effective_limit(discover);
        let max_pages = job.effective_max_pages(discover);
        let rank_scale = self.config.rank_scale.as_deref();
        let mut all_rows = Vec::new();
        let fixture = format!("page_intersection_live_{}", checkpoint.pages_completed);

        loop {
            let task = build_intersection_task(
                job,
                &targets,
                &excludes,
                limit,
                checkpoint.offset,
                checkpoint.search_after_token.as_deref(),
                rank_scale,
            );
            let response = self
                .client
                .post_page_intersection_live(vec![task], &fixture)
                .await?;
            stats.total_api_cost_usd += response.top_level_cost;

            let Some(task_result) = response.tasks.into_iter().next() else {
                break;
            };
            stats.total_api_cost_usd += task_result.task_cost;

            if !task_result.task_ok {
                stats.tasks_error += 1;
                warn!(
                    job = %job.name,
                    status_code = task_result.task_status_code,
                    message = %task_result.task_status_message,
                    "DataForSEO page_intersection task error"
                );
                break;
            }
            stats.tasks_ok += 1;

            let ctx_parse = IntersectionParseContext {
                entity: entity.parse_context(run_date),
                job_name: &job.name,
                intersection_mode: job.intersection_mode.as_deref(),
                targets_config: &targets_config,
                intersections_count: task_result.total_count,
                api_cost_usd: task_result.task_cost,
            };
            all_rows.extend(parse_intersection_items(&task_result.items, &ctx_parse));

            let advance = next_pagination_state(
                limit,
                checkpoint.offset,
                task_result.items_count,
                task_result.search_after_token,
                checkpoint.pages_completed,
                max_pages,
            );
            match advance {
                PaginationAdvance::Done => break,
                PaginationAdvance::ContinueWithOffset {
                    offset,
                    pages_completed,
                } => {
                    checkpoint = JobPaginationCheckpoint {
                        offset,
                        search_after_token: None,
                        pages_completed,
                    };
                }
                PaginationAdvance::ContinueWithToken {
                    search_after_token,
                    pages_completed,
                } => {
                    checkpoint = JobPaginationCheckpoint {
                        offset: 0,
                        search_after_token: Some(search_after_token),
                        pages_completed,
                    };
                }
            }

            if !discover {
                Self::store_job_checkpoint(ctx.as_ref(), &cp_key, &checkpoint)?;
            }
        }

        if !all_rows.is_empty() {
            let count = all_rows.len() as u32;
            stats.add_rows(StreamKind::PageIntersection.as_str(), count);
            self.submit_rows(
                ctx.as_ref(),
                NAMESPACE_PAGE_INTERSECTION_DAILY,
                run_date,
                all_rows,
            )?;
        }

        if !discover {
            Self::store_job_checkpoint(ctx.as_ref(), &cp_key, &JobPaginationCheckpoint::default())?;
        }

        Ok(())
    }

    fn primary_entity_for_intersection(&self, entities: &[SyncEntity]) -> SyncEntity {
        entities
            .iter()
            .find(|e| e.entity_kind == EntityKind::Primary)
            .cloned()
            .unwrap_or_else(|| SyncEntity {
                site: self.site.clone(),
                target: self.config.site_label(),
                entity_kind: EntityKind::Primary,
                competitor_name: None,
                backlink_jobs: vec![],
            })
    }

    fn site_run_row(&self, run_date: &str, stats: &RunStats) -> Value {
        json!({
            "site": self.site,
            "run_date": run_date,
            "total_api_cost_usd": stats.total_api_cost_usd,
            "tasks_ok": stats.tasks_ok,
            "tasks_error": stats.tasks_error,
            "rows_by_stream": self.rows_by_stream_payload(stats),
        })
    }

    /// Always emit every enabled stream key so Iceberg/Glue never discover `struct<>`.
    fn rows_by_stream_payload(&self, stats: &RunStats) -> Value {
        let mut map = serde_json::Map::new();
        for stream in self.config.enabled_streams() {
            if stream == StreamKind::PageIntersection {
                continue;
            }
            let count = stats
                .rows_by_stream
                .get(stream.as_str())
                .copied()
                .unwrap_or(0);
            map.insert(stream.as_str().to_string(), json!(count));
        }
        Value::Object(map)
    }

    async fn sync_entity_streams(
        &self,
        ctx: &Arc<dyn SourceSyncContext>,
        entity: &SyncEntity,
        run_date: &str,
        discover: bool,
        stats: &mut RunStats,
    ) -> Result<(), std::io::Error> {
        if self.config.stream_enabled(StreamKind::Backlinks)
            && self.config.run_mode != RunMode::PageIntersection
        {
            let jobs: Vec<BacklinkJob> = if discover {
                entity.backlink_jobs.iter().take(1).cloned().collect()
            } else {
                entity.backlink_jobs.clone()
            };
            for job in jobs {
                self.sync_backlink_job(ctx, entity, &job, run_date, discover, stats)
                    .await?;
            }
        }

        if self.config.stream_enabled(StreamKind::Summary) {
            self.sync_summary(ctx, entity, run_date, stats).await?;
        }
        if self.config.stream_enabled(StreamKind::ReferringDomains) {
            self.sync_referring_domains(ctx, entity, run_date, discover, stats)
                .await?;
        }
        if self.config.stream_enabled(StreamKind::Anchors) {
            self.sync_anchors(ctx, entity, run_date, discover, stats)
                .await?;
        }
        if self.config.stream_enabled(StreamKind::History) {
            self.sync_history(ctx, entity, run_date, discover, stats)
                .await?;
        }

        Ok(())
    }
}

#[async_trait]
impl DataSource for DataForSeoBacklinksPlugin {
    fn execution_contract(&self) -> SourceExecutionContract {
        SourceExecutionContract::stream(SourceOnceContract::Finite)
    }

    fn source_namespace_contracts(&self) -> Vec<SourceNamespaceContract> {
        let mut namespaces = Vec::new();
        if self.config.run_mode != RunMode::PageIntersection {
            if self.config.stream_enabled(StreamKind::Backlinks) {
                namespaces.push(Self::namespace_contract(NAMESPACE_BACKLINK_DAILY));
            }
            if self.config.stream_enabled(StreamKind::Summary) {
                namespaces.push(Self::namespace_contract(NAMESPACE_SUMMARY_DAILY));
            }
            if self.config.stream_enabled(StreamKind::ReferringDomains) {
                namespaces.push(Self::namespace_contract(NAMESPACE_REFERRING_DOMAIN_DAILY));
            }
            if self.config.stream_enabled(StreamKind::Anchors) {
                namespaces.push(Self::namespace_contract(NAMESPACE_ANCHOR_DAILY));
            }
            if self.config.stream_enabled(StreamKind::History) {
                namespaces.push(Self::namespace_contract(NAMESPACE_HISTORY_DAILY));
            }
        }
        if self.config.run_mode != RunMode::Backlinks
            && self.config.stream_enabled(StreamKind::PageIntersection)
        {
            namespaces.push(Self::namespace_contract(NAMESPACE_PAGE_INTERSECTION_DAILY));
        }
        namespaces.push(Self::namespace_contract(NAMESPACE_SITE_RUN_DAILY));
        for contract in &namespaces {
            contract
                .validate()
                .expect("invalid DataForSEO namespace contract");
        }
        namespaces
    }

    async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        self.config.validate()?;
        for contract in self.source_namespace_contracts() {
            contract
                .validate()
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }

        let discover = runtime_is_discover_mode();
        let run_date = Self::run_date();
        if discover {
            info!(
                run_date = %run_date,
                "DataForSEO discover: limit=5, max_pages=1, primary entity only"
            );
        }

        let mut stats = RunStats::default();
        let mut auth_failed = false;

        let mut entities = self.config.sync_entities()?;
        if discover {
            entities.retain(|e| e.entity_kind == EntityKind::Primary);
            entities.truncate(1);
        }

        for entity in &entities {
            match self
                .sync_entity_streams(&ctx, entity, &run_date, discover, &mut stats)
                .await
            {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    auth_failed = true;
                    break;
                }
                Err(e) => {
                    warn!(
                        target = %entity.target,
                        entity_kind = %entity.entity_kind.as_str(),
                        error = %e,
                        "DataForSEO entity sync failed"
                    );
                    stats.tasks_error += 1;
                }
            }
        }

        if !auth_failed
            && self.config.run_mode != RunMode::Backlinks
            && self.config.stream_enabled(StreamKind::PageIntersection)
        {
            let intersection_entity = self.primary_entity_for_intersection(&entities);
            let jobs: Vec<IntersectionJob> = if discover {
                self.config
                    .intersection_jobs
                    .iter()
                    .take(1)
                    .cloned()
                    .collect()
            } else {
                self.config.intersection_jobs.clone()
            };
            for job in jobs {
                match self
                    .sync_intersection_job(
                        &ctx,
                        &intersection_entity,
                        &job,
                        &run_date,
                        discover,
                        &mut stats,
                    )
                    .await
                {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                        auth_failed = true;
                        break;
                    }
                    Err(e) => {
                        warn!(job = %job.name, error = %e, "intersection job failed");
                        stats.tasks_error += 1;
                    }
                }
            }
        }

        if auth_failed {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "DataForSEO authentication failed",
            ));
        }

        self.submit_rows(
            ctx.as_ref(),
            NAMESPACE_SITE_RUN_DAILY,
            &run_date,
            vec![self.site_run_row(&run_date, &stats)],
        )?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::parse_live_response;
    use crate::config::{BacklinkJob, CompetitorEntry, IntersectionJob, RunMode};
    use crate::parse_summary::parse_summary_row;
    use std::collections::HashMap;

    fn fixture_bytes(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/fixtures/{name}.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap_or_else(|e| panic!("read fixture {name}: {e}"))
    }

    #[test]
    fn parse_backlinks_items_from_fixture() {
        let body: serde_json::Value =
            serde_json::from_slice(&fixture_bytes("backlinks_live_page1")).unwrap();
        let parsed = parse_live_response(&body).unwrap();
        let task = &parsed.tasks[0];
        assert!(task.task_ok);
        let entity = SyncEntity {
            site: "example".into(),
            target: "example.com".into(),
            entity_kind: EntityKind::Primary,
            competitor_name: None,
            backlink_jobs: vec![],
        };
        let rows = parse_backlinks_items(
            &task.items,
            &BacklinkParseContext {
                entity: entity.parse_context("2024-06-01"),
                job_tag: "main",
                backlinks_status_type: Some("live"),
                mode: None,
                api_cost_usd: 0.02,
            },
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["entity_kind"], "target");
    }

    #[test]
    fn parse_intersection_items_from_fixture() {
        let body: serde_json::Value =
            serde_json::from_slice(&fixture_bytes("page_intersection_live_partial")).unwrap();
        let parsed = parse_live_response(&body).unwrap();
        let task = &parsed.tasks[0];
        let targets_config = serde_json::json!({ "1": "a.com", "2": "b.com" });
        let entity = SyncEntity {
            site: "example".into(),
            target: "example.com".into(),
            entity_kind: EntityKind::Primary,
            competitor_name: None,
            backlink_jobs: vec![],
        };
        let rows = parse_intersection_items(
            &task.items,
            &IntersectionParseContext {
                entity: entity.parse_context("2024-06-01"),
                job_name: "gap",
                intersection_mode: Some("partial"),
                targets_config: &targets_config,
                intersections_count: Some(10),
                api_cost_usd: 0.03,
            },
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["site"], "example");
        assert_eq!(rows[0]["targets_linked"]["1"], true);
    }

    #[test]
    fn parse_summary_from_fixture() {
        let body: serde_json::Value =
            serde_json::from_slice(&fixture_bytes("summary_live")).unwrap();
        let parsed = parse_live_response(&body).unwrap();
        let task = &parsed.tasks[0];
        let entity = SyncEntity {
            site: "example".into(),
            target: "example.com".into(),
            entity_kind: EntityKind::Primary,
            competitor_name: None,
            backlink_jobs: vec![],
        };
        let row = parse_summary_row(
            task.result_body.as_ref().unwrap(),
            &entity.parse_context("2024-06-01"),
            task.task_cost,
        )
        .unwrap();
        assert_eq!(row["backlinks"], 41245);
        assert_eq!(row["referring_domains"], 12372);
    }

    #[test]
    fn site_run_row_emits_zeroed_stream_keys() {
        let cfg = DataForSeoBacklinksPluginConfig {
            login: Some("fixture".into()),
            password: Some("fixture".into()),
            site: Some("example.com".into()),
            run_mode: RunMode::Backlinks,
            backlink_jobs: vec![BacklinkJob {
                target: "example.com".into(),
                job_tag: None,
                limit: Some(10),
                mode: None,
                backlinks_status_type: None,
                filters: None,
                order_by: None,
                max_pages: Some(1),
                include_subdomains: None,
                exclude_internal_backlinks: None,
            }],
            intersection_jobs: vec![],
            competitors: vec![],
            streams: vec![
                StreamKind::Backlinks,
                StreamKind::Summary,
                StreamKind::ReferringDomains,
            ],
            history: None,
            rank_scale: None,
            request_interval_ms: 0,
            max_api_retries: 1,
        };
        let plugin = DataForSeoBacklinksPlugin::new(cfg).unwrap();
        let row = plugin.site_run_row("2024-06-01", &RunStats::default());
        let rows_by_stream = row["rows_by_stream"]
            .as_object()
            .expect("rows_by_stream object");
        assert_eq!(
            rows_by_stream.get("backlinks").and_then(|v| v.as_u64()),
            Some(0)
        );
        assert_eq!(
            rows_by_stream.get("summary").and_then(|v| v.as_u64()),
            Some(0)
        );
        assert_eq!(
            rows_by_stream
                .get("referring_domains")
                .and_then(|v| v.as_u64()),
            Some(0)
        );
    }

    #[test]
    fn sync_entities_includes_competitor() {
        let cfg = DataForSeoBacklinksPluginConfig {
            login: None,
            password: None,
            site: Some("picnic".into()),
            run_mode: RunMode::Backlinks,
            backlink_jobs: vec![BacklinkJob {
                target: "picnic.com".into(),
                job_tag: Some("main".into()),
                limit: Some(10),
                mode: None,
                backlinks_status_type: None,
                filters: None,
                order_by: None,
                max_pages: Some(1),
                include_subdomains: None,
                exclude_internal_backlinks: None,
            }],
            intersection_jobs: vec![],
            competitors: vec![CompetitorEntry {
                name: "Rival".into(),
                target: "rival.com".into(),
                limit: None,
                max_pages: None,
                include_subdomains: None,
                backlinks_status_type: None,
            }],
            streams: vec![],
            history: None,
            rank_scale: None,
            request_interval_ms: 0,
            max_api_retries: 1,
        };
        let entities = cfg.sync_entities().unwrap();
        assert_eq!(entities.len(), 2);
        assert!(entities
            .iter()
            .any(|e| e.entity_kind == EntityKind::Competitor));
        let rival = entities
            .iter()
            .find(|e| e.competitor_name.as_deref() == Some("Rival"))
            .unwrap();
        assert_eq!(rival.target, "rival.com");
    }

    #[test]
    fn discover_limits_applied_to_jobs() {
        let job = BacklinkJob {
            target: "example.com".into(),
            job_tag: None,
            limit: Some(1000),
            mode: None,
            backlinks_status_type: None,
            filters: None,
            order_by: None,
            max_pages: Some(20),
            include_subdomains: None,
            exclude_internal_backlinks: None,
        };
        assert_eq!(job.effective_limit(true), 5);
        assert_eq!(job.effective_max_pages(true), 1);
    }

    #[test]
    fn config_rejects_too_many_intersection_targets() {
        let mut targets = HashMap::new();
        for i in 0..21 {
            targets.insert(i.to_string(), format!("site{i}.com"));
        }
        let cfg = DataForSeoBacklinksPluginConfig {
            login: None,
            password: None,
            site: None,
            run_mode: RunMode::PageIntersection,
            backlink_jobs: vec![],
            intersection_jobs: vec![IntersectionJob {
                name: "big".into(),
                targets,
                exclude_targets: None,
                intersection_mode: None,
                limit: None,
                order_by: None,
                max_pages: None,
                filters: None,
                internal_list_limit: None,
            }],
            competitors: vec![],
            streams: vec![],
            history: None,
            rank_scale: None,
            request_interval_ms: 0,
            max_api_retries: 1,
        };
        assert!(cfg.validate().is_err());
    }
}
