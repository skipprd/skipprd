use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{json, Value};
use skippr_plugin_shared_api_source::OpenAiChatClient;
use skippr_runtime_sdk::helpers::offsets::OffsetKey;
use skippr_runtime_sdk::plugins::{
    DataSource, SourceExecutionContract, SourceOnceContract, SourceSyncContext,
};
use skippr_runtime_sdk::source_compat::{submit_payload_batches, IngestBatch};
use tracing::info;

use crate::checkpoint::{
    blocks_needing_analysis, content_unchanged, load_page_checkpoint, store_page_checkpoint,
    PageContentCheckpoint, PageScores,
};
use crate::config::DataSourceContentQualityPluginConfig;
use crate::crawler::{crawl_site, seed_origin};
use crate::fetch::HttpFetcher;
use crate::html::{mock_block_analysis, rollup_page_scores, ContentBlock};
use crate::openai_blocks::analyze_blocks_batch;
use crate::origin::SiteOrigin;
use crate::scorecard::page_check_rows;
use crate::streams::{
    all_namespace_contracts, NAMESPACE_CHECK_DAILY, NAMESPACE_CONTENT_BLOCK, NAMESPACE_PAGE_DAILY,
    NAMESPACE_SITE_RUN_DAILY, NAMESPACE_VECTOR_CHUNK,
};

pub struct DataSourceContentQualityPlugin {
    config: DataSourceContentQualityPluginConfig,
    origin: SiteOrigin,
    fetcher: HttpFetcher,
    openai: Option<OpenAiChatClient>,
}

impl DataSourceContentQualityPlugin {
    pub fn new(config: DataSourceContentQualityPluginConfig) -> Result<Self, std::io::Error> {
        config.validate()?;
        if config.render_js {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "render_js is not supported in content_quality",
            ));
        }
        let origin = seed_origin(&config.site)?;
        let openai = if config.openai_active() {
            if std::env::var("SKIPPR_CONTENT_QUALITY_FIXTURE_DIR")
                .or_else(|_| std::env::var("SKIPPR_OPENAI_FIXTURE_DIR"))
                .map(|d| !d.trim().is_empty())
                .unwrap_or(false)
            {
                Some(OpenAiChatClient::new(
                    "fixture",
                    "https://api.openai.com/v1",
                ))
            } else {
                OpenAiChatClient::from_env().ok()
            }
        } else {
            None
        };
        let user_agent = config.user_agent.clone();
        Ok(Self {
            config,
            origin,
            fetcher: HttpFetcher::new(&user_agent),
            openai,
        })
    }

    fn run_date() -> String {
        Utc::now().format("%Y-%m-%d").to_string()
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
                source_uri: format!("content-quality://{}", self.origin.origin),
                namespace: Some(namespace.to_string()),
                cdc_rows: None,
            }],
        )?;
        Ok(())
    }
}

#[async_trait]
impl DataSource for DataSourceContentQualityPlugin {
    fn execution_contract(&self) -> SourceExecutionContract {
        SourceExecutionContract::stream(SourceOnceContract::Finite)
    }

    fn source_namespace_contracts(
        &self,
    ) -> Vec<skippr_runtime_sdk::plugins::source_contract::SourceNamespaceContract> {
        all_namespace_contracts()
    }

    async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        self.config.validate()?;
        let run_date = Self::run_date();
        let run_id = Utc::now().to_rfc3339();
        let site = self.origin.origin.clone();

        let pages = crawl_site(
            &self.origin,
            &self.fetcher,
            &self.config.user_agent,
            self.config.max_urls,
            self.config.max_depth,
            self.config.respect_robots,
            &self.config.seed_urls,
        )
        .await?;

        let mut page_rows = Vec::new();
        let mut block_rows = Vec::new();
        let mut vector_rows = Vec::new();
        let mut issue_rows = Vec::new();
        let mut seo_scores = Vec::new();
        let mut aio_scores = Vec::new();

        for page in pages {
            let url = page.parsed.canonical_url.clone();
            let checkpoint = load_page_checkpoint(ctx.as_ref(), &url);
            let unchanged = content_unchanged(
                self.config.skip_unchanged_content,
                checkpoint.as_ref(),
                &page.parsed.content_hash,
            );

            let mut block_scores_json: Vec<Value> = if unchanged {
                checkpoint
                    .as_ref()
                    .map(|cp| cp.block_scores.values().cloned().collect())
                    .unwrap_or_default()
            } else {
                let to_analyze: Vec<ContentBlock> =
                    blocks_needing_analysis(&page.parsed.blocks, checkpoint.as_ref())
                        .into_iter()
                        .cloned()
                        .collect();
                let cap = self.config.openai_max_blocks_per_page.max(1) as usize;
                let to_analyze: Vec<ContentBlock> = to_analyze.into_iter().take(cap).collect();
                if self.config.openai_active() && !to_analyze.is_empty() {
                    if let Some(client) = &self.openai {
                        analyze_blocks_batch(
                            client,
                            &self.config.openai_model,
                            &site,
                            &url,
                            &to_analyze,
                        )
                        .await?
                    } else {
                        to_analyze.iter().map(mock_block_analysis).collect()
                    }
                } else if !to_analyze.is_empty() {
                    to_analyze.iter().map(mock_block_analysis).collect()
                } else {
                    Vec::new()
                }
            };

            if !unchanged && block_scores_json.is_empty() && !page.parsed.blocks.is_empty() {
                block_scores_json = page.parsed.blocks.iter().map(mock_block_analysis).collect();
            }

            let rollup = rollup_page_scores(&block_scores_json);
            let mut page_scores = PageScores {
                seo_content_score: rollup.seo_content_score,
                aio_score: rollup.aio_score,
                eeat_proxy_score: rollup.eeat_proxy_score,
            };
            if unchanged {
                if let Some(cp) = checkpoint.as_ref() {
                    page_scores = cp.page_scores.clone();
                }
            }
            seo_scores.push(page_scores.seo_content_score);
            aio_scores.push(page_scores.aio_score);

            page_rows.push(json!({
                "site": site,
                "run_date": run_date,
                "run_id": run_id,
                "canonical_url": url,
                "final_url": page.parsed.canonical_url,
                "status": page.status,
                "page_type": page.parsed.page_type,
                "title": page.parsed.title,
                "content_hash": page.parsed.content_hash,
                "content_unchanged": unchanged,
                "word_count": page.parsed.word_count,
                "has_faq_schema": page.parsed.has_faq_schema,
                "question_heading_count": page.parsed.question_heading_count,
                "seo_content_score": page_scores.seo_content_score,
                "aio_score": page_scores.aio_score,
                "eeat_proxy_score": page_scores.eeat_proxy_score,
                "block_count": page.parsed.blocks.len(),
            }));

            for (block, score) in page.parsed.blocks.iter().zip(block_scores_json.iter()) {
                let chunk_text = format!("{}\n\n{}", block.heading_path.join(" > "), block.text);
                block_rows.push(json!({
                    "site": site,
                    "run_date": run_date,
                    "run_id": run_id,
                    "page_url": url,
                    "block_id": block.block_id,
                    "block_type": block.block_type,
                    "heading_path": block.heading_path,
                    "char_count": block.char_count,
                    "word_count": block.word_count,
                    "text_hash": block.text_hash,
                    "content_unchanged": unchanged,
                    "extractability_score": score.get("extractability_score"),
                    "answer_clarity_score": score.get("answer_clarity_score"),
                    "citation_worthiness_score": score.get("citation_worthiness_score"),
                    "helpfulness_score": score.get("helpfulness_score"),
                    "trust_score": score.get("trust_score"),
                    "is_self_contained": score.get("is_self_contained"),
                    "confidence": score.get("confidence"),
                }));
                vector_rows.push(json!({
                    "site": site,
                    "run_date": run_date,
                    "run_id": run_id,
                    "page_url": url,
                    "block_id": block.block_id,
                    "chunk_id": block.block_id,
                    "page_type": page.parsed.page_type,
                    "has_faq_schema": page.parsed.has_faq_schema,
                    "question_heading_count": page.parsed.question_heading_count,
                    "text": chunk_text,
                }));
            }

            issue_rows.extend(page_check_rows(
                &site,
                &run_date,
                &url,
                &page.parsed,
                &page_scores,
                &block_scores_json,
            ));

            let mut block_hashes = HashMap::new();
            let mut block_scores = HashMap::new();
            for block in &page.parsed.blocks {
                block_hashes.insert(block.block_id.clone(), block.text_hash.clone());
            }
            for (block, score) in page.parsed.blocks.iter().zip(block_scores_json.iter()) {
                block_scores.insert(block.block_id.clone(), score.clone());
            }
            store_page_checkpoint(
                ctx.as_ref(),
                &url,
                &PageContentCheckpoint {
                    content_hash: page.parsed.content_hash.clone(),
                    last_run_date: run_date.clone(),
                    page_scores,
                    block_hashes,
                    block_scores,
                    openai_model: self.config.openai_model.clone(),
                },
            )?;
        }

        let mean_seo = if seo_scores.is_empty() {
            0.0
        } else {
            seo_scores.iter().sum::<f64>() / seo_scores.len() as f64
        };
        let mean_aio = if aio_scores.is_empty() {
            0.0
        } else {
            aio_scores.iter().sum::<f64>() / aio_scores.len() as f64
        };

        let site_row = json!({
            "site": site,
            "run_date": run_date,
            "run_id": run_id,
            "urls_fetched": page_rows.len(),
            "mean_seo_content_score": mean_seo,
            "mean_aio_score": mean_aio,
        });

        let page_count = page_rows.len();
        let block_count = block_rows.len();

        self.submit_rows(ctx.as_ref(), NAMESPACE_PAGE_DAILY, &run_date, page_rows)?;
        self.submit_rows(ctx.as_ref(), NAMESPACE_CONTENT_BLOCK, &run_date, block_rows)?;
        self.submit_rows(ctx.as_ref(), NAMESPACE_VECTOR_CHUNK, &run_date, vector_rows)?;
        self.submit_rows(ctx.as_ref(), NAMESPACE_CHECK_DAILY, &run_date, issue_rows)?;
        self.submit_rows(
            ctx.as_ref(),
            NAMESPACE_SITE_RUN_DAILY,
            &run_date,
            vec![site_row],
        )?;

        info!(
            site = %site,
            run_date = %run_date,
            pages = page_count,
            blocks = block_count,
            "ContentQuality sync complete"
        );
        Ok(())
    }
}
