use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{json, Value};
use skippr_runtime_sdk::helpers::offsets::OffsetKey;
use skippr_runtime_sdk::plugins::source_contract::SourceNamespaceContract;
use skippr_runtime_sdk::plugins::{
    DataSource, SourceExecutionContract, SourceOnceContract, SourceSyncContext,
};
use skippr_runtime_sdk::protocol::SKIPPR_RUNTIME_EXECUTION_MODE_ENV;
use skippr_runtime_sdk::source_compat::{submit_payload_batches, IngestBatch};
use tracing::info;

use crate::checkpoint::{
    load_prompt_checkpoint, store_prompt_checkpoint, PromptCheckpoint,
};
use crate::checks::map_checks;
use crate::client::{response_hash, CitationClient};
use crate::config::{DataSourceAiCitationsPluginConfig, TrackedPrompt};
use crate::extract::{
    citations_from_json, extract_mentions, links_from_json, links_from_text, merge_url_refs,
    url_matches_target_site, MentionSpan, UrlRef,
};
use crate::origin::normalize_site_origin;
use crate::streams::{
    all_namespace_contracts, NAMESPACE_CHECK_DAILY, NAMESPACE_CITATION, NAMESPACE_LINK,
    NAMESPACE_MENTION, NAMESPACE_PROMPT_RESPONSE_DAILY, NAMESPACE_RUN_DAILY,
};

fn runtime_is_discover_mode() -> bool {
    std::env::var(SKIPPR_RUNTIME_EXECUTION_MODE_ENV)
        .map(|mode| mode.eq_ignore_ascii_case("discover"))
        .unwrap_or(false)
}

pub struct DataSourceAiCitationsPlugin {
    config: DataSourceAiCitationsPluginConfig,
    origin: String,
    brand_names: Vec<String>,
}

impl DataSourceAiCitationsPlugin {
    pub fn new(config: DataSourceAiCitationsPluginConfig) -> Result<Self, std::io::Error> {
        config.validate()?;
        let origin = normalize_site_origin(&config.site)?;
        let brand_names = config.resolved_brand_names(&origin);
        Ok(Self {
            config,
            origin,
            brand_names,
        })
    }

    fn run_date(&self) -> String {
        Utc::now().format("%Y-%m-%d").to_string()
    }

    fn resolve_prompts(&self, discover: bool) -> Vec<TrackedPrompt> {
        let mut prompts: Vec<TrackedPrompt> = self
            .config
            .prompt_list
            .iter()
            .take(self.config.max_prompts_per_run as usize)
            .cloned()
            .collect();
        if discover {
            prompts.truncate(1);
        }
        prompts
    }

    fn resolve_models(&self, discover: bool) -> Vec<String> {
        let mut models = self.config.models.clone();
        if discover {
            models.truncate(1);
        }
        models
    }

    async fn throttle_delay(&self) {
        if self.config.requests_per_minute == 0 {
            return;
        }
        let ms = 60_000 / self.config.requests_per_minute as u64;
        if ms > 0 {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
    }

    fn submit_namespace(
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
        let offset_key = OffsetKey::new(namespace, run_date.to_string());
        submit_payload_batches(
            ctx,
            vec![IngestBatch {
                offset_key,
                data: payload,
                bytes,
                source_uri: format!("ai-citations://{}/{}", self.origin, namespace),
                namespace: Some(namespace.to_string()),
                cdc_rows: None,
            }],
        )?;
        Ok(())
    }

    fn mention_rows(
        &self,
        run_date: &str,
        prompt: &TrackedPrompt,
        model: &str,
        mentions: &[MentionSpan],
    ) -> Vec<Value> {
        mentions
            .iter()
            .map(|m| {
                json!({
                    "site": self.origin,
                    "prompt_id": prompt.id,
                    "prompt_text": prompt.text,
                    "model": model,
                    "run_date": run_date,
                    "mention_id": m.mention_id,
                    "brand_name": m.brand_name,
                    "matched_text": m.matched_text,
                    "start_offset": m.start_offset,
                    "end_offset": m.end_offset,
                    "category": prompt.category,
                    "intent": prompt.intent,
                })
            })
            .collect()
    }

    fn citation_rows(
        &self,
        run_date: &str,
        prompt: &TrackedPrompt,
        model: &str,
        citations: &[UrlRef],
    ) -> Vec<Value> {
        citations
            .iter()
            .map(|c| {
                json!({
                    "site": self.origin,
                    "prompt_id": prompt.id,
                    "model": model,
                    "run_date": run_date,
                    "citation_id": c.ref_id,
                    "cited_url": c.url,
                    "title": c.title,
                    "snippet": c.snippet,
                    "source": c.source,
                    "targets_site": url_matches_target_site(&c.url, &self.origin),
                    "category": prompt.category,
                })
            })
            .collect()
    }

    fn link_rows(
        &self,
        run_date: &str,
        prompt: &TrackedPrompt,
        model: &str,
        links: &[UrlRef],
    ) -> Vec<Value> {
        links
            .iter()
            .map(|l| {
                json!({
                    "site": self.origin,
                    "prompt_id": prompt.id,
                    "model": model,
                    "run_date": run_date,
                    "link_id": l.ref_id,
                    "link_url": l.url,
                    "anchor_text": l.anchor_text,
                    "source": l.source,
                    "targets_site": url_matches_target_site(&l.url, &self.origin),
                    "category": prompt.category,
                })
            })
            .collect()
    }
}

#[async_trait]
impl DataSource for DataSourceAiCitationsPlugin {
    fn execution_contract(&self) -> SourceExecutionContract {
        SourceExecutionContract::stream(SourceOnceContract::Finite)
    }

    fn source_namespace_contracts(&self) -> Vec<SourceNamespaceContract> {
        all_namespace_contracts()
            .into_iter()
            .inspect(|c| {
                c.validate()
                    .expect("invalid ai_citations namespace contract");
            })
            .collect()
    }

    async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        self.config.validate()?;
        if !self.config.api_active() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "OPENAI_API_KEY is required (or set SKIPPR_AI_CITATIONS_FIXTURE_DIR for offline runs)",
            ));
        }

        for contract in self.source_namespace_contracts() {
            contract
                .validate()
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }

        let discover = runtime_is_discover_mode();
        let run_date = self.run_date();
        let prompts = self.resolve_prompts(discover);
        let models = self.resolve_models(discover);
        let job_count = prompts.len().saturating_mul(models.len());

        info!(
            site = %self.origin,
            discover,
            prompt_count = prompts.len(),
            model_count = models.len(),
            job_count,
            "AI Citations sync: starting prompt × model enumeration"
        );

        let client = CitationClient::from_config(self.config.openai_base_url.as_deref())?;

        let mut response_rows = Vec::new();
        let mut mention_rows = Vec::new();
        let mut citation_rows = Vec::new();
        let mut link_rows = Vec::new();
        let mut check_rows = Vec::new();
        let mut prompts_ok = 0u32;
        let mut prompts_failed = 0u32;
        let mut prompts_skipped = 0u32;

        for prompt in &prompts {
            for model in &models {
                let prior = if discover {
                    None
                } else {
                    load_prompt_checkpoint(ctx.as_ref(), &prompt.id, model)
                };

                let skip_api = !discover
                    && self.config.skip_unchanged_responses
                    && prior.is_some();

                if skip_api {
                    prompts_skipped += 1;
                }

                let result = client
                    .query_prompt(model, &prompt.id, &prompt.text, skip_api)
                    .await;

                if !skip_api {
                    self.throttle_delay().await;
                }

                let content_unchanged = result.skipped_unchanged || skip_api;
                let answer_text = result.answer.clone().unwrap_or_default();
                let hash = if content_unchanged {
                    prior
                        .as_ref()
                        .map(|cp| cp.response_hash.clone())
                        .unwrap_or_else(|| response_hash(&answer_text, result.raw_json.as_ref()))
                } else {
                    response_hash(&answer_text, result.raw_json.as_ref())
                };

                let citations = if content_unchanged {
                    Vec::new()
                } else {
                    result
                        .raw_json
                        .as_ref()
                        .map(citations_from_json)
                        .unwrap_or_default()
                };
                let links = if content_unchanged {
                    Vec::new()
                } else {
                    let json_links = result
                        .raw_json
                        .as_ref()
                        .map(links_from_json)
                        .unwrap_or_default();
                    let text_links = if answer_text.is_empty() {
                        Vec::new()
                    } else {
                        links_from_text(&answer_text)
                    };
                    merge_url_refs(json_links, text_links)
                };

                let mentions = if content_unchanged || answer_text.is_empty() {
                    Vec::new()
                } else {
                    extract_mentions(&answer_text, &self.brand_names)
                };

                let brand_mentioned = if content_unchanged {
                    prior.as_ref().is_some_and(|cp| cp.brand_mentioned)
                } else {
                    !mentions.is_empty()
                };
                let target_domain_linked = if content_unchanged {
                    prior.as_ref().is_some_and(|cp| cp.target_domain_linked)
                } else {
                    citations
                        .iter()
                        .chain(links.iter())
                        .any(|r| url_matches_target_site(&r.url, &self.origin))
                };

                response_rows.push(json!({
                    "site": self.origin,
                    "prompt_id": prompt.id,
                    "prompt_text": prompt.text,
                    "model": model,
                    "run_date": run_date,
                    "category": prompt.category,
                    "intent": prompt.intent,
                    "status": if result.ok { "ok" } else { "error" },
                    "answer_text": if answer_text.is_empty() { Value::Null } else { Value::String(answer_text.clone()) },
                    "response_hash": hash,
                    "content_unchanged": content_unchanged,
                    "elapsed_ms": result.elapsed_ms,
                    "error_code": result.error_code,
                    "error_message": result.error_message,
                    "mention_count": mentions.len(),
                    "citation_count": citations.len(),
                    "link_count": links.len(),
                }));

                if result.ok && !content_unchanged {
                    mention_rows.extend(self.mention_rows(&run_date, prompt, model, &mentions));
                    citation_rows.extend(self.citation_rows(&run_date, prompt, model, &citations));
                    link_rows.extend(self.link_rows(&run_date, prompt, model, &links));
                }

                check_rows.extend(map_checks(
                    &self.origin,
                    &prompt.id,
                    model,
                    &run_date,
                    result.ok,
                    brand_mentioned,
                    target_domain_linked,
                    result.error_message.as_deref(),
                ));

                if result.ok {
                    prompts_ok += 1;
                } else {
                    prompts_failed += 1;
                }

                if !discover && result.ok && !content_unchanged {
                    let cp = PromptCheckpoint {
                        response_hash: hash,
                        brand_mentioned,
                        target_domain_linked,
                        mention_count: mentions.len() as u32,
                        citation_count: citations.len() as u32,
                        link_count: links.len() as u32,
                    };
                    store_prompt_checkpoint(ctx.as_ref(), &prompt.id, model, &cp)?;
                }
            }
        }

        self.submit_namespace(
            ctx.as_ref(),
            NAMESPACE_PROMPT_RESPONSE_DAILY,
            &run_date,
            response_rows,
        )?;
        self.submit_namespace(ctx.as_ref(), NAMESPACE_MENTION, &run_date, mention_rows)?;
        self.submit_namespace(ctx.as_ref(), NAMESPACE_CITATION, &run_date, citation_rows)?;
        self.submit_namespace(ctx.as_ref(), NAMESPACE_LINK, &run_date, link_rows)?;
        self.submit_namespace(ctx.as_ref(), NAMESPACE_CHECK_DAILY, &run_date, check_rows)?;

        let run_row = json!({
            "site": self.origin,
            "run_date": run_date,
            "prompts_tracked": prompts.len() as u32,
            "models_tracked": models.len() as u32,
            "jobs_enumerated": job_count as u32,
            "prompts_ok": prompts_ok,
            "prompts_failed": prompts_failed,
            "prompts_skipped_unchanged": prompts_skipped,
            "brand_names": self.brand_names,
        });
        self.submit_namespace(
            ctx.as_ref(),
            NAMESPACE_RUN_DAILY,
            &run_date,
            vec![run_row],
        )?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_config() -> DataSourceAiCitationsPluginConfig {
        DataSourceAiCitationsPluginConfig {
            site: "https://example.com".into(),
            brand_names: vec!["Example".into()],
            prompt_list: vec![TrackedPrompt {
                id: "p1".into(),
                text: "question".into(),
                category: None,
                intent: None,
            }],
            models: vec!["gpt-4.1-mini".into()],
            requests_per_minute: 10,
            max_prompts_per_run: 10,
            skip_unchanged_responses: true,
            openai_base_url: None,
        }
    }

    #[test]
    fn discover_emits_six_contracts() {
        let plugin = DataSourceAiCitationsPlugin::new(minimal_config()).unwrap();
        assert_eq!(plugin.source_namespace_contracts().len(), 6);
    }

}
